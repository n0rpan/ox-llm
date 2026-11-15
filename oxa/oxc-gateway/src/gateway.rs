//! One shared Anthropic request path, used by in-process clients and test peers.
//!
//! Enable `anthropic-gateway` and retain one Gateway across requests. Trusted
//! user configuration supplies the catalog and resolved private paths. Discovery
//! reads no key bytes and opens no ledger or provider connection. Every chat
//! reads the current file key, uses one admission hold, and settles before Done.
//! Caller callbacks must be brief; Stop cancels delivery and settles the request.
//! Settled signed thinking blocks stay in a private LRU (4096 entries / 64 MiB),
//! scoped to the exact history, alias, upstream, endpoint and credential. They
//! are never written to the ledger or exposed as public stream diagnostics.
//!
//! ```no_run
//! use oxc_gateway::{
//!     anthropic_client::{Cancellation, Control},
//!     gateway::{Error, Event, Gateway},
//!     ledger::Context,
//!     runtime::{Options, Runtime},
//! };
//! use serde_json::Value;
//! use std::path::PathBuf;
//!
//! async fn request(config: Value, key_file: PathBuf, ledger: PathBuf, payload: Value)
//!     -> Result<(), Error>
//! {
//!     let runtime = Runtime::new(config, key_file, ledger, Options::default())
//!         .map_err(Error::Refused)?;
//!     let gateway = Gateway::new(runtime)?;
//!     let cancel = Cancellation::default();
//!     let mut chunks = Vec::new();
//!     gateway.chat(&payload, Context::default(), &cancel, &mut |event| {
//!         if let Event::Chunk(chunk) = event { chunks.push(chunk); }
//!         Control::Continue
//!     }).await?;
//!     // chunks contain normalized, redacted stream data. Keep any returned
//!     // opaque native blocks private; they are not user-visible diagnostics.
//!     Ok(())
//! }
//! ```
use crate::{
    accounting::Outcome,
    anthropic_client::{self, Cancellation, Chat, Client, Control},
    anthropic_replay::{History, Replay},
    anthropic_stream::Completed,
    ledger::Context,
    policy::Refusal,
    runtime::{Discovery, Runtime},
};
use serde_json::{json, Value};
use std::{
    fmt,
    sync::Mutex,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

pub enum Event {
    Chunk(Value),
    Done,
}

#[derive(Debug)]
pub enum Error {
    Refused(Refusal),
    Upstream(anthropic_client::Error),
}
impl Error {
    pub fn refusal(&self) -> Refusal {
        let error = match self {
            Self::Refused(refusal) => return refusal.clone(),
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
        Ok(Self {
            runtime,
            client: Client::new().map_err(Error::Upstream)?,
            replay: Mutex::new(Replay::default()),
        })
    }
    pub fn discovery(&self) -> Discovery {
        self.runtime.discovery()
    }

    /// Admit, read the current key, reserve, forward, meter, and settle exactly
    /// once. Done is sent only after the private ledger writer has succeeded.
    pub async fn chat<F>(
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
        let started = Instant::now();
        let mut call = self
            .runtime
            .begin(payload.clone(), context)
            .map_err(Error::Refused)?;
        let model = call.model().clone();
        let bounded = call.payload().clone();
        let key = call.key_handle();
        let history = History::new(&model, key.expose(), &bounded);
        let request_bytes = serde_json::to_vec(&bounded).map_or(0, |v| v.len());
        let mut streamed_bytes = 0;
        let mut usage_final = false;
        let mut terminal_usage = None;
        let request = Chat {
            base_url: model["base_url"]
                .as_str()
                .unwrap_or("https://api.anthropic.com/v1"),
            upstream_model: model["upstream_model"].as_str().unwrap_or(""),
            alias: model["id"].as_str().unwrap_or(""),
            max_output: model["max_output"].as_u64(),
            payload: &bounded,
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
                        streamed_bytes += serde_json::to_vec(&chunk).map_or(0, |v| v.len());
                        usage_final |= chunk["usage"].is_object();
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
            call.observe(&json!({"usage":usage.openai()}));
            usage_final = true;
        }
        let (outcome, status, code) = match &result {
            Ok(_) => (Outcome::Complete, 200, None),
            Err(anthropic_client::Error::Cancelled) => (
                Outcome::Cancelled {
                    request_bytes,
                    streamed_bytes,
                    usage_final,
                    elapsed: started.elapsed(),
                },
                499,
                Some("request_cancelled"),
            ),
            Err(error) => {
                let refusal = Error::Upstream(*error).refusal();
                (Outcome::Failed, refusal.status, Some(refusal.code))
            }
        };
        call.finish(outcome, status, code).map_err(Error::Refused)?;
        let completed = result.map_err(Error::Upstream)?;
        if cancel.is_cancelled() {
            return Err(Error::Upstream(anthropic_client::Error::Cancelled));
        }
        // Opaque signatures are bound to the exact safe assistant turn the
        // client will echo. Keep them only after settlement, before Done lets
        // another request continue the conversation. A miss never crosses a
        // model/endpoint/credential/history boundary.
        if let Ok(mut replay) = self.replay.lock() {
            replay.remember(history.after(&completed.assistant), &completed.native_blocks);
        }
        if on(Event::Done) == Control::Stop {
            return Err(Error::Upstream(anthropic_client::Error::Cancelled));
        }
        Ok(completed)
    }
}
