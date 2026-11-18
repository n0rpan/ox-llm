//! User-only selection and production composition of the native cloud gateway.
use oxa_config::{Dirs, Settings};
use oxa_engine::EmbeddedEngine;
use oxa_proto::{codes, Engine, OxaError};
use oxc_gateway::{
    billing::SubscriptionLimits,
    catalog::{Access, Model, Provider},
    gateway::Gateway,
    pricing::Price,
    routing::ProviderRouting,
    runtime::{Billing, Options, Runtime, Subscription},
};
use serde_json::Value;
use std::{collections::BTreeMap, sync::Arc, time::Duration};

mod routing;

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
    if let Some(plan) = snapshot.billing_source {
        let _ = write!(stderr, " billing_source={}", plan.as_str());
    }
    if let Some(cause) = snapshot.cause {
        let _ = write!(stderr, " cause={}", cause.as_str());
    }
    if snapshot.cause == Some(oxc_gateway::diagnostics::StreamCause::TerminalUsage) {
        if let Some(check) = snapshot.terminal_check {
            let _ = write!(stderr, " terminal_check={}", check.as_str());
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

/// O-384: one refused OpenRouter provider preference, named so a settings typo is fixable.
fn routing_error(error: oxc_gateway::routing::Error) -> OxaError {
    OxaError::new(codes::INVALID, format!("Invalid oxa.cloud {error}."))
}

/// O-384: the global preferences (`oxa.cloud.provider_routing`); absent means none.
fn global_routing(cloud: &Value) -> Result<ProviderRouting, OxaError> {
    match cloud.get("provider_routing") {
        None | Some(Value::Null) => Ok(ProviderRouting::default()),
        Some(value) => ProviderRouting::parse(value).map_err(routing_error),
    }
}

/// O-384: the active OpenRouter provider routing per model alias, for the model view.
/// Display only: a malformed entry is skipped here and refused by [`select`].
pub(crate) fn routing_descriptions(user: &Settings) -> BTreeMap<String, String> {
    let Some(cloud) = user.get("oxa.cloud") else {
        return BTreeMap::new();
    };
    let global = global_routing(cloud).unwrap_or_default();
    let Some(models) = cloud.get("models").and_then(Value::as_object) else {
        return BTreeMap::new();
    };
    models
        .iter()
        .filter(|(_, value)| value.get("provider").and_then(Value::as_str) == Some("openrouter"))
        .filter_map(|(alias, value)| {
            let model = value
                .get("provider_routing")
                .and_then(|value| ProviderRouting::parse(value).ok())
                .unwrap_or_default();
            let routing = global.overridden_by(&model);
            (!routing.is_empty()).then(|| (alias.clone(), routing.describe()))
        })
        .collect()
}

pub(crate) fn select(
    user: &Settings,
    effective: &Settings,
    dirs: &Dirs,
    requested: Option<&str>,
    http: impl FnOnce() -> Result<Arc<dyn Engine>, OxaError>,
) -> Result<Arc<dyn Engine>, OxaError> {
    select_observed(
        user,
        effective,
        dirs,
        requested,
        None,
        SharedEngines::default(),
        http,
    )
}

/// Native launch configuration is user-only. Stream policy follows the effective session.
pub(crate) fn local_engine_settings<'a>(
    user: &'a Settings,
    effective: &Settings,
) -> (Option<&'a Value>, oxa_engine::StreamOpts) {
    let stream = oxa_engine::StreamOpts {
        silence_limit_ms: crate::dispatch::stream_silence_timeout(effective)
            .unwrap_or(365 * 24 * 3600 * 1000),
        ..oxa_engine::StreamOpts::default()
    };
    (user.get("oxa.local"), stream)
}

#[test]
fn shared_local_engine_uses_effective_stream_timeout() {
    let user_values = serde_json::json!({"httpIdleTimeoutMs": 1700});
    let user = Settings::from_values(user_values.clone(), serde_json::json!({}));
    let effective = Settings::from_values(user_values, serde_json::json!({"httpIdleTimeoutMs": 2400}));
    let (_, stream) = local_engine_settings(&user, &effective);
    assert_eq!(stream.silence_limit_ms, 2400);
}

#[derive(Clone, Default)]
pub(crate) struct SharedEngines {
    pub local: Option<Arc<oxc_local::native::LocalEngine>>,
    pub cloud: Option<Arc<Gateway>>,
}

pub(crate) fn select_observed(
    user: &Settings,
    effective: &Settings,
    dirs: &Dirs,
    requested: Option<&str>,
    diagnostics: Option<oxc_gateway::diagnostics::Diagnostics>,
    shared: SharedEngines,
    http: impl FnOnce() -> Result<Arc<dyn Engine>, OxaError>,
) -> Result<Arc<dyn Engine>, OxaError> {
    let local_engine = || match shared.local {
        Some(engine) => Ok(engine as Arc<dyn Engine>),
        None => {
            let (launch, stream) = local_engine_settings(user, effective);
            oxc_local::configured_engine(launch, dirs.agent_dir.join("local-engine"), stream)
        }
    };
    let cloud_engine = || {
        if oxa_proto::offline() {
            return Err(oxa_proto::offline_error("the in-process cloud gateway"));
        }
        let gateway = match shared.cloud {
            Some(gateway) => gateway,
            None => Arc::new(open_gateway(user, effective, dirs, diagnostics, Ok)?),
        };
        Ok(Arc::new(EmbeddedEngine::from_shared_gateway(gateway)) as Arc<dyn Engine>)
    };
    if user.bool(oxc_local::LOCAL_SETTING, false) && user.bool(SETTING, false) {
        let aliases = |path| {
            user.get(path)
                .and_then(Value::as_object)
                .map(|models| models.keys().cloned().collect())
                .unwrap_or_default()
        };
        return routing::Routed {
            local: local_engine(),
            cloud: cloud_engine(),
            local_models: aliases("oxa.local.models"),
            cloud_models: aliases("oxa.cloud.models"),
            dynamic_go: user.get("oxa.cloud.opencode.enabled") == Some(&Value::Bool(true)),
            default: effective.string("defaultModel"),
        }
        .into_engine(requested);
    }
    if user.bool(oxc_local::LOCAL_SETTING, false) {
        return local_engine();
    }
    if !user.bool(SETTING, false) {
        return http();
    }
    cloud_engine()
}

pub(crate) fn open_gateway(
    user: &Settings,
    effective: &Settings,
    dirs: &Dirs,
    diagnostics: Option<oxc_gateway::diagnostics::Diagnostics>,
    configure: impl FnOnce(Gateway) -> Result<Gateway, OxaError>,
) -> Result<Gateway, OxaError> {
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
    let mut needs_subscription = false;
    let global = global_routing(cloud)?;
    for (alias, value) in models {
        let (provider, default_endpoint) = match text(value, "provider")? {
            "anthropic" => (Provider::Anthropic, "https://api.anthropic.com"),
            "openrouter" => (Provider::OpenRouter, "https://openrouter.ai/api/v1"),
            "openai" => (Provider::OpenAi, oxc_gateway::openai::client::API_BASE),
            "chatgpt" => (Provider::ChatGpt, oxc_gateway::openai::client::SUBSCRIPTION_BASE),
            _ => return Err(OxaError::unsupported("this in-process cloud provider")),
        };
        let endpoint = match value.get("base_url") {
            None => default_endpoint,
            Some(value) => value.as_str().ok_or_else(invalid)?,
        };
        // O-384: provider routing is an OpenRouter preference; on another route it would
        // be silently ignored, so it is refused instead.
        let routing = match value.get("provider_routing") {
            None | Some(Value::Null) => global.clone(),
            Some(value) if provider == Provider::OpenRouter => {
                global.overridden_by(&ProviderRouting::parse(value).map_err(routing_error)?)
            }
            Some(_) => {
                return Err(OxaError::new(
                    codes::INVALID,
                    format!("Invalid oxa.cloud.models.{alias}.provider_routing: provider routing applies only to an openrouter route."),
                ))
            }
        };
        let model = Model::new(
            alias,
            provider,
            text(value, "upstream_model")?,
            endpoint,
            limit(value, "context_window")?,
            limit(value, "max_output")?,
        )
        .map_err(|_| invalid())?
        .with_routing(Some(routing));
        let billing = if provider == Provider::ChatGpt {
            needs_subscription = true;
            Some(Billing::Subscription)
        } else {
            Price::from_model(value).map(Billing::Metered)
        };
        routes.push((model, billing));
    }
    let (subscription, subscription_limits) = subscription_options(cloud, needs_subscription)?;
    if go_enabled && subscription_limits.is_none() {
        return Err(invalid());
    }
    let cap = match cloud.get("daily_budget_usd") {
        None | Some(Value::Null) => None,
        Some(value) => Some(value.as_f64().ok_or_else(invalid)?),
    };
    // These existing private locations are denied to both file tools and bash.
    // Configured-route discovery reads no key or ledger. Explicit Go discovery
    // uses the existing protected key reader and this same gateway HTTP pool.
    let mut runtime = Runtime::new(
        routes,
        credential_path(dirs),
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
    if let Some(diagnostics) = diagnostics {
        runtime = runtime.with_diagnostics(diagnostics);
    }
    let timeout = crate::dispatch::stream_silence_timeout(effective).map(Duration::from_millis);
    let gateway = Gateway::with_read_timeout(runtime, timeout).map_err(|e| {
        let refusal = e.refusal();
        OxaError::new(refusal.code, e.to_string())
    })?;
    let mut gateway = configure(gateway)?;
    if go_enabled {
        let base = cloud
            .pointer("/opencode/base_url")
            .map_or(Some(oxc_gateway::opencode::GO_BASE), Value::as_str)
            .ok_or_else(invalid)?;
        gateway
            .discover_opencode(base, Duration::from_secs(30))
            .map_err(|e| {
                let refusal = e.refusal();
                OxaError::new(refusal.code, e.to_string())
            })?;
    }
    Ok(gateway)
}

// One canonical cap object shared with the other subscription routes. Refresh
// identity/timeouts are separate and never supply an implicit/unlimited cap.
fn subscription_options(
    cloud: &Value,
    needed: bool,
) -> Result<(Option<Subscription>, Option<SubscriptionLimits>), OxaError> {
    let parsed = oxc_gateway::openai::subscription_config::parse(cloud, needed).map_err(|_| invalid())?;
    let subscription = parsed.refresh.map(|refresh| Subscription {
        client_id: refresh.client_id.into(),
        refresh_endpoint: refresh.endpoint.into(),
        timeout: refresh.timeout,
    });
    let limits = parsed
        .limits
        .map(|(requests, tokens)| SubscriptionLimits::new(requests, tokens).map_err(|_| invalid()))
        .transpose()?;
    Ok((subscription, limits))
}

#[cfg(test)]
mod subscription_factory_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn canonical_caps_and_public_refresh_defaults_are_separate() {
        let cloud = json!({"subscription_limits":{"daily_requests":3,"daily_tokens":100000}});
        let (refresh, caps) = subscription_options(&cloud, true).unwrap();
        let refresh = refresh.unwrap();
        assert_eq!(refresh.client_id, "app_EMoamEEZ73f0CkXaXp7hrann");
        assert_eq!(refresh.refresh_endpoint, "https://auth.openai.com/oauth/token");
        assert_eq!(refresh.timeout, Duration::from_secs(15));
        let caps = caps.unwrap();
        assert_eq!((caps.requests(), caps.tokens()), (3, 100000));
        let (refresh, caps) = subscription_options(
            &json!({"subscription":{"daily_requests":99,"daily_tokens":999}}),
            true,
        )
        .unwrap();
        assert!(refresh.is_some());
        assert!(caps.is_none(), "refresh object is never a second cap setting");
        let (refresh, caps) = subscription_options(&cloud, false).unwrap();
        assert!(refresh.is_none());
        assert!(
            caps.is_some(),
            "Go shares the same canonical limits without ChatGPT refresh"
        );
    }
    #[test]
    fn malformed_caps_ids_timeouts_and_refresh_redirects_refuse_locally() {
        for value in [
            json!(null),
            json!({}),
            json!({"daily_requests":0,"daily_tokens":1}),
            json!({"daily_requests":1,"daily_tokens":0}),
            json!({"daily_requests":-1,"daily_tokens":1}),
            json!({"daily_requests":1.5,"daily_tokens":1}),
            json!({"daily_requests":1,"daily_tokens":"1"}),
        ] {
            assert!(subscription_options(&json!({"subscription_limits":value}), true).is_err());
        }
        for id in ["", "bad id", "bad/id", "bad\nid", "λ", &"a".repeat(129)] {
            assert!(subscription_options(&json!({"subscription":{"client_id":id}}), true).is_err());
        }
        for id in ["A", "synthetic-client_1.2", &"a".repeat(128)] {
            let (refresh, _) = subscription_options(&json!({"subscription":{"client_id":id}}), true).unwrap();
            assert_eq!(refresh.unwrap().client_id, id);
        }
        for value in [json!(0), json!(-1), json!(1.5), json!("1000")] {
            assert!(
                subscription_options(&json!({"subscription":{"refresh_timeout_ms":value}}), true).is_err()
            );
        }
        for endpoint in [
            "https://example.invalid/token",
            "http://127.0.0.1:9/token",
            "https://auth.openai.com/oauth/token",
        ] {
            assert!(
                subscription_options(&json!({"subscription":{"refresh_endpoint":endpoint}}), true).is_err()
            );
        }
        assert!(subscription_options(&json!({"subscription":false}), true).is_err());
    }
}
