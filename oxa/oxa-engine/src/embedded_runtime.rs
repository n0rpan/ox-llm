//! Caller-thread delivery with a lazy I/O worker and no cancellation polling.
use crate::{EmbeddedEngine, EmbeddedGateway, Flow, GatewayContext, GatewayDiscovery, GatewayEvent};
use oxa_proto::{codes, CancelToken, ChatRequest, OxaError};
use oxc_gateway::{
    anthropic_client::{Control, Error as ClientError},
    gateway::{Error, Event, Gateway},
    ledger,
};
use serde_json::{json, Map, Value};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

#[cfg(test)]
mod tests {
    use super::*;
    use oxc_gateway::policy::Refusal;

    #[test]
    fn local_credential_context_never_changes_public_or_upstream_failures() {
        let path = Path::new("/synthetic/home/.config/oxc/llm/cloud-secrets.json");
        let refusal = Refusal::credentials();
        let public = refusal.body();
        let ordinary = normalize(Error::Refused(refusal.clone()), None);
        assert!(!ordinary.message.contains("/synthetic"));
        let local = normalize(Error::Refused(refusal.clone()), Some(path));
        assert_eq!(local.code, ordinary.code);
        assert!(local.message.contains(path.to_str().unwrap()));
        assert_eq!(refusal.body(), public);
        assert!(!public.to_string().contains("/synthetic"));
        for status in [401, 403] {
            assert_eq!(
                normalize(Error::Upstream(ClientError::Http(status)), Some(path)),
                normalize(Error::Upstream(ClientError::Http(status)), None)
            );
        }
        assert_eq!(
            normalize(Error::Refused(Refusal::ledger()), Some(path)),
            normalize(Error::Refused(Refusal::ledger()), None)
        );
    }
}

struct Backend {
    gateway: Gateway,
    local_credential_path: Option<PathBuf>,
    driver: OnceLock<Result<tokio::runtime::Runtime, OxaError>>,
}
impl EmbeddedEngine {
    pub fn from_gateway(gateway: Gateway) -> Self {
        Self::gateway_backend(gateway, None)
    }
    /// Trusted local CLI diagnostics only. This is the path already supplied to
    /// the runtime, not a selector or an additional credential read. Never use
    /// this constructor for remote/API error delivery.
    pub fn from_gateway_with_local_diagnostics(gateway: Gateway, credential_path: PathBuf) -> Self {
        Self::gateway_backend(gateway, Some(credential_path))
    }
    fn gateway_backend(gateway: Gateway, local_credential_path: Option<PathBuf>) -> Self {
        Self::new(Arc::new(Backend {
            gateway,
            local_credential_path,
            driver: OnceLock::new(),
        }))
    }
}
fn normalize(error: Error, local_credential_path: Option<&Path>) -> OxaError {
    if matches!(error, Error::Upstream(ClientError::Cancelled)) {
        return OxaError::new(codes::ABORTED, "request cancelled");
    }
    let mut refusal = error.refusal();
    if refusal.code == "budget_not_configured" {
        refusal.message = "Cloud models need a positive oxa.cloud.daily_budget_usd in oxa user settings.";
    }
    let mut normalized =
        crate::info::http_error_with(refusal.status, refusal.body().to_string().as_bytes(), None);
    // Only admission's local reader failure may name the attempted store.
    // Upstream authentication errors and the public refusal remain unchanged.
    if matches!(&error, Error::Refused(r) if r.code == "cloud_credentials_unavailable") {
        if let Some(path) = local_credential_path {
            normalized.message = format!(
                "{} Checked local credential store: {}. Verify the provider entry and owner-only file protections.",
                normalized.message,
                path.display()
            );
        }
    }
    if normalized.code == codes::BUDGET_EXHAUSTED {
        normalized.with_hint("adjust oxa.cloud.daily_budget_usd in oxa user settings, or wait for the next spend day; this is not retried")
    } else {
        normalized
    }
}
impl EmbeddedGateway for Backend {
    fn discovery(&self) -> Result<GatewayDiscovery, OxaError> {
        let mut limits = Map::new();
        let models: Vec<_> = self.gateway.discovery().map(|model| {
            limits.insert(model.id.into(), json!({"cap":model.context_window,"max_output":model.max_output,
                "capabilities":{"remote":true,"remote_provider":model.provider.as_str(),
                "context_window":model.context_window,"echo":false,"cache_hints":false,"prefill":false,"cancel":true}}));
            json!({"id":model.id,"remote":true,"remote_provider":model.provider.as_str(),
                "context_length":model.context_window,"max_output_tokens":model.max_output})
        }).collect();
        let selected = models.first().map_or(Value::Null, |model| model["id"].clone());
        Ok(GatewayDiscovery {
            models: json!({"data":models}),
            status: json!({"engine_mode":"none","loaded":false,"sleeping":true,"model":selected,"model_limits":limits}),
        })
    }
    fn chat(
        &self,
        request: &ChatRequest<'_>,
        context: GatewayContext<'_>,
        cancel: &CancelToken,
        on: &mut dyn FnMut(GatewayEvent) -> Flow,
    ) -> Result<(), OxaError> {
        // Async callers should use Gateway::chat; nesting a runtime cannot drive
        // this synchronous Engine boundary safely.
        if tokio::runtime::Handle::try_current().is_ok() {
            return Err(OxaError::new(
                codes::ENGINE_DOWN,
                "call the synchronous cloud Engine outside an async runtime",
            ));
        }
        // One driver retains pooled connections and supports concurrent callers.
        // Neither construction nor discovery creates its parked I/O worker.
        let driver = self
            .driver
            .get_or_init(|| {
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(1)
                    .enable_all()
                    .build()
                    .map_err(|_| OxaError::new(codes::ENGINE_DOWN, "the cloud runtime is unavailable"))
            })
            .as_ref()
            .map_err(Clone::clone)?;
        let context = ledger::Context {
            origin: ledger::Origin::Oxa,
            session: Some(context.session),
            idempotency_key: Some(context.idempotency_key),
            guest_key_name: None,
        };
        let mut deliver = |event| {
            let settled = matches!(event, Event::Done);
            let event = match event {
                Event::Chunk(chunk) => GatewayEvent::Chunk(chunk),
                Event::Done => GatewayEvent::Done,
            };
            if !settled && cancel.is_cancelled() {
                return Control::Stop;
            }
            if on(event) == Flow::Stop {
                Control::Stop
            } else {
                Control::Continue
            }
        };
        driver
            .block_on(self.gateway.chat(request, context, cancel, &mut deliver))
            .map(|_| ())
            .map_err(|error| normalize(error, self.local_credential_path.as_deref()))
    }
}
