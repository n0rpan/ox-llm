//! In-process access to the controller's normalized cloud pipeline (#289).
//!
//! The gateway implementation owns admission, credentials, provider I/O, budget
//! reservations and ledger settlement. This adapter reuses the HTTP engine's
//! request builder and reply assembler; it opens no socket and reads no key.
use crate::{build_body_for, new_idem_key, parse_info_for, Flow, ReplyAssembler, SseEvent};
use oxa_proto::{
    codes, AssistantTurn, CancelToken, ChatRequest, Engine, EngineInfo, OxaError, StreamEvent, ThinkingLevel,
    Value,
};
use std::{sync::Arc, time::Instant};

/// The same public discovery documents served by the daemon.
pub struct GatewayDiscovery {
    pub models: Value,
    pub status: Value,
}

/// Caller metadata shared with the daemon's normalized chat entry point.
/// These values are not provider headers or local engine cache hints.
pub struct GatewayContext<'a> {
    pub origin: &'static str,
    pub session: &'a str,
    pub idempotency_key: &'a str,
}

/// Transport-independent access to one gateway instance.
///
/// Implementations must use the daemon's admission, key, budget and ledger code,
/// honor `cancel` during blocking work and stop delivery when `on` returns Stop.
/// `Ok(())` means processing and ledger settlement have completed successfully;
/// delivering Done alone must not release a reservation or commit success.
/// Normalized refusal errors retain their oxa classification, including budget
/// exhaustion. No provider-specific translation belongs in the caller.
pub trait EmbeddedGateway: Send + Sync {
    fn discovery(&self) -> Result<GatewayDiscovery, OxaError>;
    fn chat(
        &self,
        body: &[u8],
        context: GatewayContext<'_>,
        cancel: &CancelToken,
        on: &mut dyn FnMut(SseEvent) -> Flow,
    ) -> Result<(), OxaError>;
}

/// An explicitly constructed cloud-only Engine; the default remains HttpEngine.
/// The shared gateway implementation is injected by the binary's composition.
pub struct EmbeddedEngine {
    gateway: Arc<dyn EmbeddedGateway>,
}

impl EmbeddedEngine {
    pub fn new(gateway: Arc<dyn EmbeddedGateway>) -> Self {
        Self { gateway }
    }
}

fn online() -> Result<(), OxaError> {
    if oxa_proto::offline() {
        return Err(OxaError::new(
            codes::OFFLINE,
            "cloud models are unavailable in offline mode",
        ));
    }
    Ok(())
}
fn aborted() -> OxaError {
    OxaError::new(codes::ABORTED, "request cancelled")
}
fn broken(message: impl Into<String>) -> OxaError {
    OxaError::new(codes::STREAM_BROKEN, message)
}

impl Engine for EmbeddedEngine {
    fn info(&self, target: Option<&str>) -> Result<EngineInfo, OxaError> {
        online()?;
        let discovery = self.gateway.discovery()?;
        let info = parse_info_for("in-process", &discovery.models, &discovery.status, target)?;
        if !info.caps.remote {
            return Err(OxaError::new(
                codes::UNSUPPORTED_IN_OXA,
                "the in-process gateway requires a cloud model",
            ));
        }
        Ok(info)
    }

    fn count_tokens(&self, _text: &str) -> Result<u32, OxaError> {
        online()?;
        Err(OxaError::new(
            codes::UNSUPPORTED_IN_OXA,
            "exact tokenization is unavailable for cloud models",
        ))
    }

    fn chat(
        &self,
        request: &ChatRequest,
        cancel: &CancelToken,
        on: &mut dyn FnMut(StreamEvent),
    ) -> Result<AssistantTurn, OxaError> {
        online()?;
        if cancel.is_cancelled() {
            return Err(aborted());
        }
        let mut body = Vec::new();
        build_body_for(request, true, &mut body);
        let key = if request.hints.idem_key.is_empty() {
            new_idem_key()
        } else {
            request.hints.idem_key.to_string()
        };
        let context = GatewayContext {
            origin: "oxa",
            session: request.hints.session,
            idempotency_key: &key,
        };
        let mut assembler = ReplyAssembler::new();
        let started = Instant::now();
        let mut done = false;
        let mut emitted = false;
        let mut failure = None;
        let outcome = self.gateway.chat(&body, context, cancel, &mut |event| {
            if failure.is_some() {
                return Flow::Stop;
            }
            if cancel.is_cancelled() {
                failure = Some(aborted());
                return Flow::Stop;
            }
            if done {
                failure = Some(broken("the gateway delivered an event after [DONE]"));
                return Flow::Stop;
            }
            match event {
                SseEvent::Data(data) => match assembler.chunk(&data) {
                    Ok(events) => {
                        for event in events {
                            if cancel.is_cancelled() {
                                failure = Some(aborted());
                                return Flow::Stop;
                            }
                            emitted = true;
                            on(event);
                        }
                    }
                    Err(error) => {
                        // A malformed chunk or refusal after reply data is a cut reply that
                        // must never be replayed.
                        failure = Some(if emitted {
                            broken(format!("{}{}", oxa_proto::CUT_BY_REFUSAL, error.message))
                        } else {
                            error
                        });
                        return Flow::Stop;
                    }
                },
                SseEvent::Comment(comment) => {
                    let seconds = started.elapsed().as_secs().min(u64::from(u32::MAX)) as u32;
                    if let Some(event) = assembler.comment(&comment, seconds) {
                        on(event);
                    }
                }
                SseEvent::Done => done = true,
            }
            Flow::Continue
        });
        if cancel.is_cancelled() {
            return Err(aborted());
        }
        if let Some(error) = failure.or_else(|| outcome.err()) {
            // As with HTTP, a refusal after visible reply data is a cut stream;
            // it must never invite replay of an already visible tool or answer.
            return Err(
                if emitted
                    && error.code != codes::STREAM_BROKEN
                    && !error.message.starts_with(oxa_proto::CUT_BY_REFUSAL)
                {
                    broken(format!("{}{}", oxa_proto::CUT_BY_REFUSAL, error.message))
                } else {
                    error
                },
            );
        }
        if !done {
            return Err(broken("the gateway closed the stream before [DONE]"));
        }
        for event in assembler.flush() {
            if cancel.is_cancelled() {
                return Err(aborted());
            }
            on(event);
        }
        if cancel.is_cancelled() {
            return Err(aborted());
        }
        let level = if request.thinking {
            ThinkingLevel::Medium
        } else {
            ThinkingLevel::Off
        };
        let mut turn = assembler.finish(level)?;
        if turn.model.is_empty() {
            turn.model = request.model.to_string();
        }
        if cancel.is_cancelled() {
            return Err(aborted());
        }
        Ok(turn)
    }
}
