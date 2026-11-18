//! Bridge bounded guest HTTP requests to a synchronous native engine entry point.
use super::listener::{
    Access, Answer, CancelToken, Completed, Failure, Finish, Guest, Model, Output, Request, Usage,
};
use oxa_proto::{AssistantTurn, ChatRequest, FrozenPrefix, OxaError, Role, Sampling, StreamEvent, WireMsg};
use sha2::{Digest, Sha256};
use std::sync::Arc;

pub struct Native<C> {
    models: Vec<Model>,
    chat: Arc<C>,
    #[cfg(feature = "ledger")]
    ledger: Option<crate::ledger::Ledger>,
    #[cfg(feature = "anthropic-gateway")]
    metered: Option<Arc<crate::gateway::Gateway>>,
}

impl<C> Native<C> {
    pub fn new(models: Vec<Model>, chat: C) -> Result<Self, Failure> {
        if models.is_empty() || models.iter().any(|model| model.access != Access::Local) {
            return Err(Failure::Forbidden);
        }
        Ok(Self {
            models,
            chat: Arc::new(chat),
            #[cfg(feature = "ledger")]
            ledger: None,
            #[cfg(feature = "anthropic-gateway")]
            metered: None,
        })
    }

    #[cfg(feature = "anthropic-gateway")]
    pub fn new_metered(
        mut models: Vec<Model>,
        chat: C,
        gateway: Arc<crate::gateway::Gateway>,
    ) -> Result<Self, Failure> {
        if models.iter().any(|model| model.access != Access::Local) {
            return Err(Failure::Forbidden);
        }
        let mut remote = Vec::new();
        for model in gateway.metered_guest_models() {
            if models.iter().any(|local| local.alias == model.id) {
                return Err(Failure::Forbidden);
            }
            remote.push(Model {
                alias: model.id.to_owned(),
                context_window: model.context_window,
                max_output: model.max_output,
                access: Access::Metered,
            });
        }
        if remote.is_empty() {
            return Err(Failure::Forbidden);
        }
        models.extend(remote);
        Ok(Self {
            models,
            chat: Arc::new(chat),
            #[cfg(feature = "ledger")]
            ledger: None,
            metered: Some(gateway),
        })
    }

    #[cfg(feature = "ledger")]
    pub fn with_ledger(mut self, ledger: crate::ledger::Ledger) -> Self {
        self.ledger = Some(ledger);
        self
    }
}

impl<C> Answer for Native<C>
where
    C: Fn(&ChatRequest<'_>, &CancelToken, &mut dyn FnMut(StreamEvent)) -> Result<AssistantTurn, OxaError>
        + Send
        + Sync
        + 'static,
{
    fn models(&self) -> &[Model] {
        &self.models
    }

    async fn chat(
        &self,
        request: Request,
        guest: Guest<'_>,
        output: Output,
        cancel: CancelToken,
    ) -> Result<Completed, Failure> {
        let runtime = tokio::runtime::Handle::current();
        let model = self
            .models
            .iter()
            .find(|model| model.alias == request.model)
            .ok_or(Failure::NotFound)?;
        #[cfg(feature = "anthropic-gateway")]
        if model.access == Access::Metered {
            let gateway = self.metered.as_deref().ok_or(Failure::Forbidden)?;
            return metered_chat(gateway, request, guest, output, cancel, &runtime).await;
        }
        if model.access != Access::Local {
            return Err(Failure::Forbidden);
        }
        #[cfg(feature = "ledger")]
        let mut audit = self
            .ledger
            .as_ref()
            .map(|ledger| {
                ledger.today()?.entry(
                    &request.model,
                    crate::ledger::Context {
                        origin: crate::ledger::Origin::Api,
                        guest_key_name: Some(guest.key_name()),
                        ..crate::ledger::Context::default()
                    },
                )
            })
            .transpose()
            .map_err(|_| Failure::Failed)?;
        #[cfg(not(feature = "ledger"))]
        let _ = guest;
        let chat = self.chat.clone();
        // The listener joins this future even after disconnect or shutdown. The
        // synchronous engine keeps its lease until cancellation has finished.
        tokio::task::spawn_blocking(move || {
            #[cfg(feature = "ledger")]
            if let Some(entry) = &mut audit {
                entry.admitted();
            }
            let result = (|| {
                let prepared = GuestChat::prepare(request)?;
                let request = prepared.chat();
                let reply = chat(&request, &cancel, &mut |event| match event {
                    StreamEvent::Text(text) => {
                        #[cfg(feature = "ledger")]
                        if let Some(entry) = &mut audit {
                            entry.observe();
                        }
                        if runtime.block_on(output.text(&text)).is_err() {
                            cancel.cancel();
                        }
                    }
                    StreamEvent::ToolArgs { .. } => cancel.cancel(),
                    _ => {}
                })
                .map_err(|_| Failure::Failed)?;
                if cancel.is_cancelled() || !reply.msg.tool_calls.is_empty() {
                    return Err(Failure::Failed);
                }
                let finish = match reply.finish {
                    oxa_proto::Finish::Stop => Finish::Stop,
                    oxa_proto::Finish::Length => Finish::Length,
                    oxa_proto::Finish::ContentFilter => Finish::Refusal,
                    oxa_proto::Finish::ToolCalls => return Err(Failure::Failed),
                };
                Ok(Completed {
                    finish,
                    usage: (reply.usage.prompt > 0).then_some(Usage {
                        input: u64::from(reply.usage.prompt),
                        output: u64::from(reply.usage.completion),
                    }),
                })
            })();
            #[cfg(feature = "ledger")]
            if let Some(entry) = audit {
                use crate::ledger::LocalGuestOutcome;
                let outcome = match &result {
                    Ok(completed) => LocalGuestOutcome::Completed {
                        tokens: completed.usage.map(|usage| (usage.input, usage.output)),
                    },
                    Err(_) if cancel.is_cancelled() => LocalGuestOutcome::Cancelled,
                    Err(_) => LocalGuestOutcome::Failed,
                };
                entry.append_local_guest(outcome).map_err(|_| Failure::Failed)?;
            }
            result
        })
        .await
        .map_err(|_| Failure::Failed)?
    }
}

struct GuestChat {
    model: String,
    max_output: u32,
    prefix: FrozenPrefix,
    messages: Vec<WireMsg>,
    first_chat: usize,
    sampling: Option<Sampling>,
}

impl GuestChat {
    fn prepare(request: Request) -> Result<Self, Failure> {
        let messages = request
            .messages
            .into_iter()
            .map(|message| {
                let content = message["content"].as_str().ok_or(Failure::BadRequest)?;
                match message["role"].as_str() {
                    Some("system") => Ok(WireMsg::system(content)),
                    Some("user") => Ok(WireMsg::user(content)),
                    Some("assistant") => Ok(WireMsg::assistant(content)),
                    _ => Err(Failure::BadRequest),
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        // The native serializer emits its prefix as the first system
        // message. Move leading guest instructions into that prefix.
        let first_chat = messages
            .iter()
            .take_while(|message| message.role == Role::System)
            .count();
        let (system, _) = messages.split_at(first_chat);
        let mut prefix = FrozenPrefix {
            sections: system
                .iter()
                .filter_map(|message| message.content.clone().map(|text| ("system".into(), text)))
                .collect(),
            tools_json: "[]".into(),
            sha256: [0; 32],
        };
        prefix.sha256 = Sha256::digest(prefix.hash_input()).into();
        let sampling = (request.temperature.is_some() || request.top_p.is_some()).then_some(Sampling {
            temperature: request.temperature.unwrap_or(1.0) as f32,
            top_p: request.top_p.unwrap_or(1.0) as f32,
            top_k: 0,
            min_p: 0.0,
            presence_penalty: 0.0,
            seed: None,
        });
        Ok(Self {
            model: request.model,
            max_output: request.max_output,
            prefix,
            messages,
            first_chat,
            sampling,
        })
    }
    fn chat(&self) -> ChatRequest<'_> {
        ChatRequest {
            max_tokens: self.max_output,
            sampling: self.sampling,
            ..ChatRequest::new(&self.prefix, &self.messages[self.first_chat..], &self.model)
        }
    }
}

#[cfg(feature = "anthropic-gateway")]
async fn metered_chat(
    gateway: &crate::gateway::Gateway,
    request: Request,
    guest: Guest<'_>,
    output: Output,
    cancel: CancelToken,
    runtime: &tokio::runtime::Handle,
) -> Result<Completed, Failure> {
    use crate::{anthropic_client::Control, gateway::Event};

    // Backpressure must not block the single-threaded local-only listener.
    if runtime.runtime_flavor() != tokio::runtime::RuntimeFlavor::MultiThread {
        return Err(Failure::Failed);
    }
    let prepared = GuestChat::prepare(request)?;
    let reply = gateway
        .chat_authenticated_guest(&prepared.chat(), &guest, &cancel, &mut |event| {
            let Event::Chunk(chunk) = event else {
                return Control::Continue;
            };
            let delta = &chunk["choices"][0]["delta"];
            if delta["tool_calls"]
                .as_array()
                .is_some_and(|calls| !calls.is_empty())
            {
                cancel.cancel();
                return Control::Stop;
            }
            if let Some(text) = delta["content"].as_str().filter(|text| !text.is_empty()) {
                let sent = tokio::task::block_in_place(|| runtime.block_on(output.text(text)));
                if sent.is_err() {
                    cancel.cancel();
                    return Control::Stop;
                }
            }
            Control::Continue
        })
        .await
        .map_err(|_| Failure::Failed)?;
    if cancel.is_cancelled()
        || reply.assistant["tool_calls"]
            .as_array()
            .is_some_and(|calls| !calls.is_empty())
    {
        return Err(Failure::Failed);
    }
    let finish = match reply.finish_reason {
        "stop" => Finish::Stop,
        "length" => Finish::Length,
        "content_filter" => Finish::Refusal,
        _ => return Err(Failure::Failed),
    };
    Ok(Completed {
        finish,
        usage: reply
            .usage
            .filter(|usage| usage.total_tokens().is_some())
            .and_then(|usage| usage.prompt.zip(usage.output))
            .map(|(input, output)| Usage { input, output }),
    })
}

/// Run the listener to completion in the caller's foreground process.
pub fn serve<A: Answer, T: super::listener::Tailnet>(
    keys: Arc<super::keys::Store>,
    answer: Arc<A>,
    tailnet: Arc<T>,
    config: super::listener::Config,
    stop: CancelToken,
    ready: impl FnOnce(std::net::SocketAddr) -> Result<(), Failure>,
) -> Result<(), Failure> {
    let mut builder = if config.remote_models.is_empty() {
        tokio::runtime::Builder::new_current_thread()
    } else {
        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder.worker_threads(1);
        builder
    };
    let runtime = builder.enable_all().build().map_err(|_| Failure::Failed)?;
    runtime.block_on(async {
        if let super::listener::Binding::Explicit(ip) = config.binding {
            if ip.is_unspecified() || ip.is_multicast() {
                return Err(Failure::Forbidden);
            }
            if !ip.is_loopback() {
                // This adapter serves HTTP. Outside loopback, only addresses
                // assigned by the trusted Tailscale daemon provide encryption.
                let addresses = tokio::time::timeout(config.read_timeout, tailnet.addresses())
                    .await
                    .map_err(|_| Failure::Busy)??;
                if !addresses.contains(&ip) {
                    return Err(Failure::Forbidden);
                }
            }
        }
        let server = super::listener::Server::bind(keys, answer, tailnet, config).await?;
        ready(server.address()?)?;
        server.serve(stop).await
    })
}
