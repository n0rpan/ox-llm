//! One admission/stream/settlement path, with Done only after a durable row.
#[cfg(test)]
#[path = "../tests/common/gateway_cases.rs"]
mod tests;
use crate::{
    accounting::Outcome,
    anthropic_client::{self, Cancellation, Chat, Client, Control},
    anthropic_replay::{History, Replay},
    anthropic_stream::Completed,
    ledger::Context,
    policy::Refusal,
    runtime::Runtime,
};
use serde_json::{json, Value};
use std::{
    fmt,
    sync::Mutex,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub enum Event {
    Chunk(Value),
    Done,
}

#[derive(Debug)]
pub enum Error {
    Refused(Refusal),
    Upstream(anthropic_client::Error),
    OpenCode(crate::opencode::client::Error),
}
impl Error {
    pub fn refusal(&self) -> Refusal {
        let error = match self {
            Self::Refused(refusal) => return refusal.clone(),
            Self::OpenCode(error) => return go_refusal(*error),
            Self::Upstream(error) => error,
        };
        let (status, code, param) = match error {
            anthropic_client::Error::Request(param) => (400, "invalid_value", *param),
            anthropic_client::Error::Cancelled => (499, "request_cancelled", ""),
            anthropic_client::Error::Setup => (503, "cloud_transport_unavailable", ""),
            anthropic_client::Error::Endpoint => (503, "cloud_upstream_unavailable", "model"),
            anthropic_client::Error::Http(status) => (
                if (400..600).contains(status) { *status } else { 502 },
                "cloud_upstream_error",
                "",
            ),
            anthropic_client::Error::Protocol => (502, "cloud_upstream_stream_broken", ""),
            anthropic_client::Error::TooLarge => (502, "cloud_upstream_too_large", ""),
            anthropic_client::Error::Transport => (502, "cloud_upstream_unavailable", ""),
        };
        Refusal {
            status,
            code,
            param,
            message: match error {
                anthropic_client::Error::Cancelled => "Request cancelled.",
                anthropic_client::Error::Request(_) => "Invalid Anthropic request.",
                _ => "The Anthropic upstream request failed.",
            },
        }
    }
    pub fn body(&self) -> Value {
        self.refusal().body()
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.refusal().message)
    }
}
impl std::error::Error for Error {}

pub struct Gateway {
    runtime: Runtime,
    client: Client,
    replay: Mutex<Replay>,
}
impl Gateway {
    pub fn new(runtime: Runtime) -> Result<Self, Error> {
        Self::with_read_timeout(runtime, Some(Duration::from_secs(1800)))
    }
    pub fn with_read_timeout(runtime: Runtime, timeout: Option<Duration>) -> Result<Self, Error> {
        Ok(Self {
            runtime,
            client: Client::with_read_timeout(timeout).map_err(Error::Upstream)?,
            replay: Mutex::new(Replay::default()),
        })
    }
    /// Startup-only bounded live discovery. Reuses this gateway's HTTP pool;
    /// the temporary current-thread driver creates no second queue or daemon.
    pub fn discover_opencode(&mut self, base: &str, timeout: Duration) -> Result<(), Error> {
        if tokio::runtime::Handle::try_current().is_ok() {
            return Err(Error::OpenCode(crate::opencode::client::Error::Request));
        }
        let driver = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| Error::OpenCode(crate::opencode::client::Error::Transport))?;
        let result = driver.block_on(self.runtime.discover_go(self.client.transport(), base, timeout));
        driver.shutdown_timeout(Duration::from_secs(1));
        result.map_err(Error::OpenCode)
    }
    pub fn discovery(&self) -> impl Iterator<Item = crate::catalog::ModelInfo<'_>> {
        self.runtime.discovery()
    }

    /// Admit, read the current key, reserve, forward, meter, and settle exactly
    /// once. Done is sent only after the private ledger writer has succeeded.
    pub async fn chat<F>(
        &self,
        request: &oxa_proto::ChatRequest<'_>,
        context: Context<'_>,
        cancel: &Cancellation,
        on: &mut F,
    ) -> Result<Completed, Error>
    where
        F: FnMut(Event) -> Control + ?Sized,
    {
        if cancel.is_cancelled() {
            return Err(Error::Upstream(anthropic_client::Error::Cancelled));
        }
        let payload = match payload(request) {
            Ok(payload) => payload,
            Err(error) => {
                self.runtime.refused(request.model, context, &error);
                return Err(Error::Refused(error));
            }
        };
        self.chat_payload(&payload, context, cancel, on).await
    }
    async fn chat_payload<F>(
        &self,
        payload: &Value,
        context: Context<'_>,
        cancel: &Cancellation,
        on: &mut F,
    ) -> Result<Completed, Error>
    where
        F: FnMut(Event) -> Control + ?Sized,
    {
        if cancel.is_cancelled() {
            return Err(Error::Upstream(anthropic_client::Error::Cancelled));
        }
        let mut call = self.runtime.begin(payload, context).map_err(Error::Refused)?;
        let model = call.model();
        if model.info().provider == crate::catalog::Provider::OpenCode {
            return self.chat_go(call, payload, cancel, on).await;
        }
        // Transport dispatch is added with the provider client; an admitted
        // foreign route must never be sent using the Anthropic protocol.
        if model.info().provider != crate::catalog::Provider::Anthropic {
            let error = Refusal {
                status: 503,
                code: "cloud_provider_unavailable",
                param: "model",
                message: "This provider transport is not available in this build.",
            };
            call.finish(Outcome::Complete, error.status, Some(error.code))
                .map_err(Error::Refused)?;
            return Err(Error::Refused(error));
        }
        let key = call.key_handle().map_err(Error::Refused)?;
        let history = History::new(model, key.expose(), payload);
        let mut terminal_usage = None;
        let request = Chat {
            base_url: model.endpoint().as_str(),
            upstream_model: model.upstream(),
            alias: model.info().id,
            max_output: Some(call.max_output()),
            payload,
            created: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |t| t.as_secs()),
        };
        let result = self
            .client
            .stream_with_blocks(
                request,
                &key,
                cancel,
                &mut |event| match event {
                    anthropic_client::Event::Sending => {
                        if cancel.is_cancelled() {
                            return Control::Stop;
                        }
                        call.mark_forwarded();
                        Control::Continue
                    }
                    anthropic_client::Event::Chunk(chunk) => {
                        call.observe(&chunk);
                        on(Event::Chunk(chunk))
                    }
                },
                |index| {
                    let id = history.at(index)?;
                    self.replay.lock().ok()?.restore(id)
                },
                &mut terminal_usage,
            )
            .await;
        // This record survives cancellation during final chunk delivery and
        // public usage opt-out; both must settle the actual cache categories.
        if let Some(usage) = terminal_usage {
            call.metered(usage);
        }
        let (outcome, status, code) = match &result {
            Ok(_) => (Outcome::Complete, 200, None),
            Err(anthropic_client::Error::Cancelled) => (
                if terminal_usage.is_some() {
                    Outcome::Cancelled {
                        request_bytes: 0,
                        streamed_bytes: 0,
                        usage_final: true,
                        elapsed: Duration::ZERO,
                    }
                } else {
                    Outcome::Failed
                },
                499,
                Some("request_cancelled"),
            ),
            Err(error) => {
                let refusal = Error::Upstream(*error).refusal();
                // Documented Anthropic request/auth/limit refusals precede
                // generation. Timeouts, server and transport failures remain
                // ambiguous and retain their conservative reservation charge.
                let outcome = if matches!(
                    *error,
                    anthropic_client::Error::Http(400 | 401 | 403 | 404 | 413 | 429)
                ) {
                    Outcome::Refused
                } else {
                    Outcome::Failed
                };
                (outcome, refusal.status, Some(refusal.code))
            }
        };
        call.finish(outcome, status, code).map_err(Error::Refused)?;
        let completed = result.map_err(Error::Upstream)?;
        // Opaque signatures are bound to the exact safe assistant turn the
        // client will echo. Keep them only after settlement, before Done lets
        // another request continue the conversation. A miss never crosses a
        // model/endpoint/credential/history boundary.
        if let Ok(mut replay) = self.replay.lock() {
            replay.remember(history.after(&completed.assistant), &completed.native_blocks);
        }
        // Settlement committed success; a late cancel cannot retract a paid turn.
        let _ = on(Event::Done);
        Ok(completed)
    }
    async fn chat_go<F>(
        &self,
        mut call: crate::runtime::Call<'_>,
        payload: &Value,
        cancel: &Cancellation,
        on: &mut F,
    ) -> Result<Completed, Error>
    where
        F: FnMut(Event) -> Control + ?Sized,
    {
        use crate::opencode::client::{self, Error as GoError};
        let alias = call.model().info().id;
        let model = self
            .runtime
            .go_model(alias)
            .ok_or(Error::OpenCode(GoError::Endpoint))?;
        let hint = crate::opencode::SessionHint::from_provider_hint(
            call.provider_session_hint().map_err(Error::Refused)?,
        )
        .map_err(|_| Error::OpenCode(GoError::Request))?;
        let key = call.key_handle().map_err(Error::Refused)?;
        let mut payload = payload.clone();
        payload["max_tokens"] = call.max_output().into();
        let mut usage = None;
        let result = client::stream(
            self.client.transport(),
            client::Chat {
                model,
                alias,
                payload: &payload,
                key: &key,
                hint: &hint,
            },
            cancel,
            &mut |event| match event {
                client::Event::Sending => {
                    if cancel.is_cancelled() {
                        return Control::Stop;
                    }
                    call.mark_forwarded();
                    Control::Continue
                }
                client::Event::Chunk(chunk) => {
                    call.observe(&chunk);
                    on(Event::Chunk(chunk))
                }
            },
            &mut usage,
        )
        .await;
        let (outcome, status, code) = match &result {
            Ok(_) => (Outcome::Complete, 200, None),
            Err(GoError::Cancelled) => (
                if usage.is_some() {
                    Outcome::Cancelled {
                        request_bytes: 0,
                        streamed_bytes: 0,
                        usage_final: true,
                        elapsed: Duration::ZERO,
                    }
                } else {
                    Outcome::Failed
                },
                499,
                Some("request_cancelled"),
            ),
            Err(error) => {
                let refusal = go_refusal(*error);
                let outcome = if matches!(
                    error,
                    GoError::Refused(
                        crate::opencode::Refusal::RegionBlocked
                            | crate::opencode::Refusal::LimitReached
                            | crate::opencode::Refusal::Unauthenticated
                            | crate::opencode::Refusal::Forbidden
                    )
                ) {
                    Outcome::Refused
                } else {
                    Outcome::Failed
                };
                (outcome, refusal.status, Some(refusal.code))
            }
        };
        call.finish(outcome, status, code).map_err(Error::Refused)?;
        let completed = result.map_err(|error| {
            if error == GoError::Cancelled {
                Error::Upstream(anthropic_client::Error::Cancelled)
            } else {
                Error::OpenCode(error)
            }
        })?;
        let _ = on(Event::Done);
        Ok(completed)
    }
}

/// Provider conversion consumes the same borrowed request the core passed to
/// Engine. No request body or stream chunk is encoded/reparsed across that boundary.
fn payload(request: &oxa_proto::ChatRequest<'_>) -> Result<Value, Refusal> {
    let mut messages = Vec::with_capacity(request.messages.len() + 1);
    messages.push(json!({"role":"system","content":request.prefix.system_text()}));
    for message in request.messages {
        let mut value = json!({"role":message.role.as_str(),"content":message.content});
        if let Some(id) = &message.tool_call_id {
            value["tool_call_id"] = json!(id);
        }
        if let Some(reasoning) = &message.reasoning {
            value["reasoning_content"] = json!(reasoning);
        }
        if !message.tool_calls.is_empty() {
            value["tool_calls"] = message
                .tool_calls
                .iter()
                .map(|call| {
                    json!({"id":call.id,
                "type":"function","function":{"name":call.name,"arguments":call.args_raw}})
                })
                .collect();
        }
        messages.push(value);
    }
    let effort = match request.effective_level() {
        oxa_proto::ThinkingLevel::Off => "none",
        oxa_proto::ThinkingLevel::Minimal => "minimal",
        oxa_proto::ThinkingLevel::Low => "low",
        oxa_proto::ThinkingLevel::Medium => "medium",
        _ => "high",
    };
    // Tools are already stored as canonical JSON in FrozenPrefix. Only that
    // schema fragment needs decoding; message history stays typed at the boundary.
    let tools = serde_json::from_str::<Value>(&request.prefix.tools_json)
        .ok()
        .filter(Value::is_array)
        .ok_or(Refusal {
            status: 400,
            code: "invalid_value",
            param: "tools",
            message: "Invalid tool definitions.",
        })?;
    Ok(json!({"model":request.model,"messages":messages,"tools":tools,
        "max_tokens":request.max_tokens,"reasoning_effort":effort,
        "stream":true,"stream_options":{"include_usage":true}}))
}

fn go_refusal(error: crate::opencode::client::Error) -> Refusal {
    use crate::opencode::{client::Error as E, Refusal as R};
    let (status, code, message) = match error {
        E::Refused(R::RegionBlocked) => (
            403,
            "region_blocked",
            "This Go model is blocked by the workspace region policy.",
        ),
        E::Refused(R::LimitReached) => (
            429,
            "subscription_limit_reached",
            "The Go subscription limit was reached; no paid fallback was attempted.",
        ),
        E::Refused(R::Unauthenticated) => (
            503,
            "cloud_credentials_unavailable",
            "OpenCode Go credentials are unavailable.",
        ),
        E::Refused(R::Forbidden) => (403, "cloud_forbidden", "OpenCode Go refused this request."),
        E::Refused(R::Other(status)) => (
            if (400..600).contains(&status) { status } else { 502 },
            "cloud_upstream_error",
            "The Go upstream request failed.",
        ),
        E::Cancelled => (499, "request_cancelled", "Request cancelled."),
        E::Request => (400, "invalid_value", "Invalid Go configuration or request."),
        E::Endpoint => (
            503,
            "cloud_provider_unavailable",
            "This Go model has no supported chat route.",
        ),
        E::Protocol | E::TooLarge => (
            502,
            "cloud_upstream_stream_broken",
            "Invalid or incomplete Go response.",
        ),
        E::Transport => (502, "cloud_upstream_unavailable", "The Go connection failed."),
    };
    Refusal {
        status,
        code,
        message,
        param: "model",
    }
}
