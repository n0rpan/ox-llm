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
    if user.bool(oxc_local::LOCAL_SETTING, false) {
        return oxc_local::configured_engine(
            user.get("oxa.local"),
            dirs.agent_dir.join("local-engine"),
            oxa_engine::StreamOpts {
                silence_limit_ms: crate::dispatch::stream_silence_timeout(effective)
                    .unwrap_or(365 * 24 * 3600 * 1000),
                ..oxa_engine::StreamOpts::default()
            },
        );
    }
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
    let mut needs_subscription = false;
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
    let (subscription, subscription_limits) = subscription_options(cloud, needs_subscription)?;
    let cap = match cloud.get("daily_budget_usd") {
        None | Some(Value::Null) => None,
        Some(value) => Some(value.as_f64().ok_or_else(invalid)?),
    };
    // These existing private locations are denied to both file tools and bash.
    // Anthropic discovery stays credential-free; explicit Go discovery uses
    // only the existing protected key reader and never opens the ledger.
    if go_enabled && subscription_limits.is_none() {
        return Err(invalid());
    }
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
    let mut gateway = Gateway::with_read_timeout(runtime, timeout).map_err(|e| {
        let refusal = e.refusal();
        OxaError::new(refusal.code, e.to_string())
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
                OxaError::new(refusal.code, e.to_string())
            })?;
    }
    Ok(Arc::new(EmbeddedEngine::from_gateway(gateway)))
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
