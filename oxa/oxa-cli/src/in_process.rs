//! User-only selection and production composition of the native cloud gateway.
use oxa_config::{Dirs, Settings};
use oxa_engine::EmbeddedEngine;
use oxa_proto::{codes, Engine, OxaError};
use oxc_gateway::{
    catalog::{Access, Model, Provider},
    gateway::Gateway,
    pricing::Price,
    runtime::{Options, Runtime},
};
use serde_json::Value;
use std::{sync::Arc, time::Duration};

pub(crate) const SETTING: &str = "oxa.engine.inProcessCloud";

pub(crate) fn credential_path(dirs: &Dirs) -> std::path::PathBuf {
    dirs.home.join(".config/oxc/llm/cloud-secrets.json")
}

/// Terminal-only context for the native reader's local refusal. Never enrich an
/// engine error: those also reach session history, JSON/RPC events and API callers.
/// Native upstream authentication failures have a different typed error class.
pub(crate) fn report_credentials(
    result: &Result<oxa_core::RunOutcome, OxaError>,
    native_cloud: bool,
    dirs: &Dirs,
    stderr: &mut dyn std::io::Write,
) {
    let local_refusal = match result {
        Ok(oxa_core::RunOutcome::Blocked("cloud_credentials_unavailable")) => true,
        Err(error) => error.code == "cloud_credentials_unavailable",
        _ => false,
    };
    if native_cloud && local_refusal {
        // Debug quoting keeps unusual HOME characters on one diagnostic line.
        let _ = writeln!(
            stderr,
            "Local cloud credentials unavailable; attempted protected store: {:?}. Check the provider entry and file protections.",
            credential_path(dirs),
        );
    }
}

fn invalid() -> OxaError {
    OxaError::new(codes::INVALID, "Invalid user cloud configuration in oxa.cloud.")
}
fn text<'a>(value: &'a Value, field: &str) -> Result<&'a str, OxaError> {
    value[field]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(invalid)
}
fn limit(value: &Value, field: &str) -> Result<u32, OxaError> {
    value[field]
        .as_u64()
        .and_then(|v| u32::try_from(v).ok())
        .ok_or_else(invalid)
}

pub(crate) fn select(
    user: &Settings,
    effective: &Settings,
    dirs: &Dirs,
    http: impl FnOnce() -> Result<Arc<dyn Engine>, OxaError>,
) -> Result<Arc<dyn Engine>, OxaError> {
    if !user.bool(SETTING, false) {
        return http();
    }
    if oxa_proto::offline() {
        return Err(oxa_proto::offline_error("the in-process cloud gateway"));
    }
    open(user, effective, dirs)
}

fn open(user: &Settings, effective: &Settings, dirs: &Dirs) -> Result<Arc<dyn Engine>, OxaError> {
    let cloud = user.get("oxa.cloud").ok_or_else(invalid)?;
    let models = cloud["models"]
        .as_object()
        .filter(|m| !m.is_empty())
        .ok_or_else(invalid)?;
    let mut routes = Vec::with_capacity(models.len());
    for (alias, value) in models {
        if text(value, "provider")? != "anthropic" {
            return Err(OxaError::unsupported("this in-process cloud provider"));
        }
        let endpoint = match value.get("base_url") {
            None => "https://api.anthropic.com",
            Some(value) => value.as_str().ok_or_else(invalid)?,
        };
        let model = Model::new(
            alias,
            Provider::Anthropic,
            text(value, "upstream_model")?,
            endpoint,
            limit(value, "context_window")?,
            limit(value, "max_output")?,
        )
        .map_err(|_| invalid())?;
        routes.push((model, Price::from_model(value)));
    }
    let cap = match cloud.get("daily_budget_usd") {
        None | Some(Value::Null) => None,
        Some(value) => Some(value.as_f64().ok_or_else(invalid)?),
    };
    // These existing private locations are denied to both file tools and bash.
    // Discovery prepares the key parent only; it reads no key and opens no ledger.
    let runtime = Runtime::new(
        routes,
        credential_path(dirs),
        dirs.agent_dir.join("cloud-ledger"),
        Options {
            access: Access::Enabled,
            daily_budget_usd: cap,
            ledger_enabled: true,
        },
    )
    .map_err(|e| OxaError::new(e.code, e.message))?;
    let timeout = crate::dispatch::stream_silence_timeout(effective).map(Duration::from_millis);
    let gateway = Gateway::with_read_timeout(runtime, timeout).map_err(|e| {
        let refusal = e.refusal();
        OxaError::new(refusal.code, refusal.message)
    })?;
    Ok(Arc::new(EmbeddedEngine::from_gateway(gateway)))
}
