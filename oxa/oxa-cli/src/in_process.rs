//! User-only selection and production composition of the native cloud gateway.
use oxa_config::{Dirs, Settings};
use oxa_engine::EmbeddedEngine;
use oxa_proto::{codes, Engine, OxaError};
use oxc_gateway::{
    billing::SubscriptionLimits,
    catalog::{Access, Model, Provider},
    gateway::Gateway,
    pricing::Price,
    runtime::{Billing, Options, Runtime, Subscription},
};
use serde_json::Value;
use std::{sync::Arc, time::Duration};

pub(crate) const SETTING: &str = "oxa.engine.inProcessCloud";

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
    let mut needs_subscription = false;
    for (alias, value) in models {
        let (provider, default_endpoint) = match text(value, "provider")? {
            "anthropic" => (Provider::Anthropic, "https://api.anthropic.com"),
            "openai" => (Provider::OpenAi, oxc_gateway::openai::client::API_BASE),
            "chatgpt" => (Provider::ChatGpt, oxc_gateway::openai::client::SUBSCRIPTION_BASE),
            _ => return Err(OxaError::unsupported("this in-process cloud provider")),
        };
        let endpoint = match value.get("base_url") {
            None => default_endpoint,
            Some(value) => value.as_str().ok_or_else(invalid)?,
        };
        let model = Model::new(
            alias,
            provider,
            text(value, "upstream_model")?,
            endpoint,
            limit(value, "context_window")?,
            limit(value, "max_output")?,
        )
        .map_err(|_| invalid())?;
        let billing = if provider == Provider::ChatGpt {
            needs_subscription = true;
            Some(Billing::Subscription)
        } else {
            Price::from_model(value).map(Billing::Metered)
        };
        routes.push((model, billing));
    }
    let subscription_config = cloud.get("subscription").filter(|value| !value.is_null());
    let subscription_limits = subscription_config
        .map(|value| {
            SubscriptionLimits::new(
                value["daily_requests"].as_u64().ok_or_else(invalid)?,
                value["daily_tokens"].as_u64().ok_or_else(invalid)?,
            )
            .map_err(|_| invalid())
        })
        .transpose()?;
    let subscription = if needs_subscription {
        subscription_config
            .map(|value| {
                Ok::<_, OxaError>(Subscription {
                    client_id: text(value, "client_id")?.into(),
                    refresh_endpoint: match value.get("refresh_endpoint") {
                        None => "https://auth.openai.com/oauth/token",
                        Some(value) => value.as_str().ok_or_else(invalid)?,
                    }
                    .into(),
                    timeout: Duration::from_millis(match value.get("refresh_timeout_ms") {
                        None => 15_000,
                        Some(value) => value.as_u64().ok_or_else(invalid)?,
                    }),
                })
            })
            .transpose()?
    } else {
        None
    };
    let cap = match cloud.get("daily_budget_usd") {
        None | Some(Value::Null) => None,
        Some(value) => Some(value.as_f64().ok_or_else(invalid)?),
    };
    // These existing private locations are denied to both file tools and bash.
    // Discovery prepares the key parent only; it reads no key and opens no ledger.
    let runtime = Runtime::new(
        routes,
        dirs.home.join(".config/oxc/llm/cloud-secrets.json"),
        dirs.agent_dir.join("cloud-ledger"),
        Options {
            access: Access::Enabled,
            daily_budget_usd: cap,
            ledger_enabled: true,
            subscription,
            subscription_limits,
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
