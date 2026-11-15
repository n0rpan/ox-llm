//! Synchronous Engine bridge to the shared Anthropic gateway (#331).
use crate::{EmbeddedEngine, EmbeddedGateway, Flow, GatewayContext, GatewayDiscovery, SseEvent};
use oxa_proto::{codes, CancelToken, OxaError};
use oxc_gateway::{
    anthropic_client::{Cancellation, Control, Error as ClientError},
    gateway::{Error, Event},
    ledger, Gateway,
};
use std::{sync::Arc, time::Duration};

struct Backend {
    gateway: Gateway,
    driver: tokio::runtime::Runtime,
}

impl EmbeddedEngine {
    /// Explicitly opt into the shared cloud gateway. The caller supplies trusted
    /// config and private paths through GatewayRuntime; construction reads no key.
    ///
    /// Like Engine::chat, this bridge is synchronous. Async consumers should call
    /// Gateway::chat directly instead of invoking it inside a Tokio task.
    pub fn from_gateway(gateway: Gateway) -> Result<Self, OxaError> {
        let driver = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .map_err(|_| OxaError::new(codes::ENGINE_DOWN, "the cloud runtime is unavailable"))?;
        Ok(Self::new(Arc::new(Backend { gateway, driver })))
    }
}

fn normalize(error: Error) -> OxaError {
    if matches!(error, Error::Upstream(ClientError::Cancelled)) {
        return OxaError::new(codes::ABORTED, "request cancelled");
    }
    let refusal = error.refusal();
    crate::info::http_error_with(refusal.status, refusal.body().to_string().as_bytes(), None)
}

impl EmbeddedGateway for Backend {
    fn discovery(&self) -> Result<GatewayDiscovery, OxaError> {
        let discovery = self.gateway.discovery();
        Ok(GatewayDiscovery {
            models: discovery.models,
            status: discovery.status,
        })
    }

    fn chat(
        &self,
        body: &[u8],
        context: GatewayContext<'_>,
        cancel: &CancelToken,
        on: &mut dyn FnMut(SseEvent) -> Flow,
    ) -> Result<(), OxaError> {
        let payload = serde_json::from_slice(body)
            .map_err(|_| OxaError::new(codes::ENGINE_HTTP, "invalid cloud request JSON"))?;
        let cancellation = Cancellation::default();
        if cancel.is_cancelled() {
            cancellation.cancel();
        }
        let context = ledger::Context {
            origin: Some(context.origin),
            session: Some(context.session),
            idempotency_key: Some(context.idempotency_key),
        };
        let mut deliver = |event| {
            if cancel.is_cancelled() {
                cancellation.cancel();
                return Control::Stop;
            }
            let event = match event {
                Event::Chunk(value) => SseEvent::Data(value.to_string()),
                Event::Done => SseEvent::Done,
            };
            if on(event) == Flow::Stop || cancel.is_cancelled() {
                cancellation.cancel();
                Control::Stop
            } else {
                Control::Continue
            }
        };
        self.driver.block_on(async {
            let request = self.gateway.chat(&payload, context, &cancellation, &mut deliver);
            tokio::pin!(request);
            let mut poll = tokio::time::interval(Duration::from_millis(20));
            loop {
                tokio::select! {
                    result = &mut request => return result.map(|_| ()).map_err(normalize),
                    _ = poll.tick() => {
                        // Keep driving the request after cancellation so its
                        // existing reservation is settled before we return.
                        if cancel.is_cancelled() {
                            cancellation.cancel();
                        }
                    }
                }
            }
        })
    }
}
