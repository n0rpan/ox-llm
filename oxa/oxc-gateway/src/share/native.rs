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
}

impl<C> Native<C> {
    pub fn new(models: Vec<Model>, chat: C) -> Result<Self, Failure> {
        if models.is_empty() || models.iter().any(|model| model.access != Access::Local) {
            return Err(Failure::Forbidden);
        }
        Ok(Self {
            models,
            chat: Arc::new(chat),
        })
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
        _guest: Guest<'_>,
        output: Output,
        cancel: CancelToken,
    ) -> Result<Completed, Failure> {
        let chat = self.chat.clone();
        let runtime = tokio::runtime::Handle::current();
        // The listener joins this future even after disconnect or shutdown. The
        // synchronous engine keeps its lease until cancellation has finished.
        tokio::task::spawn_blocking(move || {
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
            let (system, messages) = messages.split_at(first_chat);
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
            let request = ChatRequest {
                max_tokens: request.max_output,
                sampling,
                ..ChatRequest::new(&prefix, messages, &request.model)
            };
            let reply = chat(&request, &cancel, &mut |event| match event {
                StreamEvent::Text(text) => {
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
        })
        .await
        .map_err(|_| Failure::Failed)?
    }
}

/// Run the listener to completion in the caller's foreground process.
pub fn serve<A: Answer, T: super::listener::Tailnet>(
    keys: Arc<super::keys::Store>,
    answer: Arc<A>,
    tailnet: Arc<T>,
    config: super::listener::Config,
    stop: CancelToken,
    ready: impl FnOnce(std::net::SocketAddr, super::status::Status) -> Result<(), Failure>,
) -> Result<(), Failure> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| Failure::Failed)?;
    runtime.block_on(async {
        let server = super::listener::Server::bind(keys, answer, tailnet, config).await?;
        ready(server.address()?, server.status())?;
        server.serve(stop).await
    })
}
