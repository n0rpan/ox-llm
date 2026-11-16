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
    let (subscription, subscription_limits) = subscription_options(cloud, needs_subscription)?;
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

// One canonical cap object shared with the other subscription routes. Refresh
// identity/timeouts are separate and never supply an implicit/unlimited cap.
fn subscription_options(
    cloud: &Value,
    needed: bool,
) -> Result<(Option<Subscription>, Option<SubscriptionLimits>), OxaError> {
    let limits = cloud
        .get("subscription_limits")
        .map(|value| {
            SubscriptionLimits::new(
                value["daily_requests"].as_u64().ok_or_else(invalid)?,
                value["daily_tokens"].as_u64().ok_or_else(invalid)?,
            )
            .map_err(|_| invalid())
        })
        .transpose()?;
    if !needed {
        return Ok((None, limits));
    }
    let config = cloud.get("subscription");
    if config.is_some_and(|value| !value.is_object() || value.get("refresh_endpoint").is_some()) {
        return Err(invalid());
    }
    // Public experimental identity approved in #252/#726, not a credential.
    let client_id = match config.and_then(|value| value.get("client_id")) {
        None => "app_EMoamEEZ73f0CkXaXp7hrann",
        Some(value) => value.as_str().ok_or_else(invalid)?,
    };
    if client_id.is_empty()
        || client_id.len() > 128
        || !client_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
    {
        return Err(invalid());
    }
    let timeout = match config.and_then(|value| value.get("refresh_timeout_ms")) {
        None => 15_000,
        Some(value) => value.as_u64().filter(|n| *n > 0).ok_or_else(invalid)?,
    };
    Ok((
        Some(Subscription {
            client_id: client_id.into(),
            // Production refresh cannot send a token to a setting-selected origin.
            // The client contract's loopback injection belongs only to its tests.
            refresh_endpoint: "https://auth.openai.com/oauth/token".into(),
            timeout: Duration::from_millis(timeout.min(30_000)),
        }),
        limits,
    ))
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
