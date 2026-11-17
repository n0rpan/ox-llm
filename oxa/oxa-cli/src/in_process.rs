//! User-only selection and production composition of the native cloud gateway.
use oxa_config::{Dirs, Settings};
use oxa_engine::EmbeddedEngine;
use oxa_proto::{codes, Engine, OxaError};
use oxc_gateway::{
    billing::{Billing, SubscriptionLimits},
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

/// Consume private facts only at a trusted terminal failure boundary.
pub(crate) fn report_diagnostics(
    diagnostics: &oxc_gateway::diagnostics::Diagnostics,
    dirs: &Dirs,
    stderr: &mut dyn std::io::Write,
) {
    use oxc_gateway::diagnostics::Stage;
    let Some(snapshot) = diagnostics.take() else {
        return;
    };
    if snapshot.stage == Stage::LocalCredentialRead {
        let _ = writeln!(stderr,
            "Local cloud credentials unavailable; attempted protected store: {:?}. Check the provider entry and file protections.",
            credential_path(dirs));
        return;
    }
    let stage = match snapshot.stage {
        Stage::DiscoveryHttp => "discovery_http",
        Stage::ChatHttp => "chat_http",
        Stage::LocalCredentialRead => unreachable!(),
    };
    let _ = write!(stderr, "oxa native diagnostic: stage={stage}");
    if let Some(status) = snapshot.status {
        let _ = write!(stderr, " status={status}");
    }
    if let Some(code) = snapshot.code {
        let _ = write!(stderr, " code={code}");
    }
    for (name, value) in ["retry_after", "rate_limit", "rate_remaining", "rate_reset"]
        .into_iter()
        .zip(snapshot.rate)
    {
        if let Some(value) = value {
            let _ = write!(stderr, " {name}={value}");
        }
    }
    let _ = writeln!(stderr);
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
    select_observed(user, effective, dirs, None, http)
}

pub(crate) fn select_observed(
    user: &Settings,
    effective: &Settings,
    dirs: &Dirs,
    diagnostics: Option<oxc_gateway::diagnostics::Diagnostics>,
    http: impl FnOnce() -> Result<Arc<dyn Engine>, OxaError>,
) -> Result<Arc<dyn Engine>, OxaError> {
    if !user.bool(SETTING, false) {
        return http();
    }
    if oxa_proto::offline() {
        return Err(oxa_proto::offline_error("the in-process cloud gateway"));
    }
    open(user, effective, dirs, diagnostics)
}

fn open(
    user: &Settings,
    effective: &Settings,
    dirs: &Dirs,
    diagnostics: Option<oxc_gateway::diagnostics::Diagnostics>,
) -> Result<Arc<dyn Engine>, OxaError> {
    let cloud = user.get("oxa.cloud").ok_or_else(invalid)?;
    let go_enabled = cloud.pointer("/opencode/enabled") == Some(&Value::Bool(true));
    let empty = serde_json::Map::new();
    let models = match cloud.get("models") {
        None | Some(Value::Null) if go_enabled => &empty,
        Some(value) => value.as_object().ok_or_else(invalid)?,
        None => return Err(invalid()),
    };
    if models.is_empty() && !go_enabled {
        return Err(invalid());
    }
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
        routes.push((model, Price::from_model(value).map(Billing::Metered)));
    }
    let cap = match cloud.get("daily_budget_usd") {
        None | Some(Value::Null) => None,
        Some(value) => Some(value.as_f64().ok_or_else(invalid)?),
    };
    // These existing private locations are denied to both file tools and bash.
    // Anthropic discovery stays credential-free; explicit Go discovery uses
    // only the existing protected key reader and never opens the ledger.
    let subscription_limits = if go_enabled {
        let limits = &cloud["subscription_limits"];
        Some(
            SubscriptionLimits::new(
                limits["daily_requests"].as_u64().ok_or_else(invalid)?,
                limits["daily_tokens"].as_u64().ok_or_else(invalid)?,
            )
            .map_err(|e| OxaError::new(e.code, e.message))?,
        )
    } else {
        None
    };
    let mut runtime = Runtime::new(
        routes,
        credential_path(dirs),
        dirs.agent_dir.join("cloud-ledger"),
        Options {
            access: Access::Enabled,
            daily_budget_usd: cap,
            ledger_enabled: true,
            subscription: None,
            subscription_limits,
        },
    )
    .map_err(|e| OxaError::new(e.code, e.message))?;
    if let Some(diagnostics) = diagnostics {
        runtime = runtime.with_diagnostics(diagnostics);
    }
    let timeout = crate::dispatch::stream_silence_timeout(effective).map(Duration::from_millis);
    let mut gateway = Gateway::with_read_timeout(runtime, timeout).map_err(|e| {
        let refusal = e.refusal();
        OxaError::new(refusal.code, refusal.message)
    })?;
    if go_enabled {
        let base = cloud
            .pointer("/opencode/base_url")
            .map_or(Some(oxc_gateway::opencode::GO_BASE), Value::as_str)
            .ok_or_else(invalid)?;
        gateway
            .discover_opencode(base, Duration::from_secs(30))
            .map_err(|e| {
                let refusal = e.refusal();
                OxaError::new(refusal.code, refusal.message)
            })?;
    }
    Ok(Arc::new(EmbeddedEngine::from_gateway(gateway)))
}
