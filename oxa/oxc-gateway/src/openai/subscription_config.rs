//! Owner-only factory configuration shared by the production composition.
//! No credential reads, routing, admission or additional cap setting.
use super::client::Refresh;
use serde_json::Value;
use std::{fmt, time::Duration};

/// Public experimental identity approved in #252/#726, not a credential.
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const REFRESH_ENDPOINT: &str = "https://auth.openai.com/oauth/token";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Error;
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Invalid user cloud subscription configuration.")
    }
}
impl std::error::Error for Error {}

/// Parsed inputs, not admission authority. The composition converts the positive
/// pair into its existing SubscriptionLimits; None MUST NOT mean unlimited.
/// Other subscription providers share limits without needing ChatGPT refresh.
#[derive(Clone, Debug)]
pub struct Config<'a> {
    pub refresh: Option<Refresh<'a>>,
    pub limits: Option<(u64, u64)>,
}

/// Parse the user-only `oxa.cloud` object. Limits come only from
/// `subscription_limits`; optional refresh identity/timeouts are separate.
/// Production refresh always uses the fixed issuer. Loopback transport injection
/// belongs to client tests, never a setting-selected token destination.
pub fn parse(cloud: &Value, needs_refresh: bool) -> Result<Config<'_>, Error> {
    let limits = cloud
        .get("subscription_limits")
        .map(|value| {
            let positive = |field| value[field].as_u64().filter(|n| *n > 0).ok_or(Error);
            Ok::<_, Error>((positive("daily_requests")?, positive("daily_tokens")?))
        })
        .transpose()?;
    if !needs_refresh {
        return Ok(Config {
            refresh: None,
            limits,
        });
    }
    let config = cloud.get("subscription");
    if config.is_some_and(|value| !value.is_object() || value.get("refresh_endpoint").is_some()) {
        return Err(Error);
    }
    let client_id = match config.and_then(|value| value.get("client_id")) {
        None => CLIENT_ID,
        Some(value) => value.as_str().ok_or(Error)?,
    };
    if client_id.is_empty()
        || client_id.len() > 128
        || !client_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
    {
        return Err(Error);
    }
    let timeout = match config.and_then(|value| value.get("refresh_timeout_ms")) {
        None => 15_000,
        Some(value) => value.as_u64().filter(|n| *n > 0).ok_or(Error)?,
    };
    Ok(Config {
        refresh: Some(Refresh {
            endpoint: REFRESH_ENDPOINT,
            client_id,
            timeout: Duration::from_millis(timeout.min(30_000)),
        }),
        limits,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn canonical_caps_and_public_refresh_defaults_are_separate() {
        let cloud = json!({"subscription_limits":{"daily_requests":3,"daily_tokens":100000}});
        let config = parse(&cloud, true).unwrap();
        let refresh = config.refresh.unwrap();
        assert_eq!(refresh.client_id, CLIENT_ID);
        assert_eq!(refresh.endpoint, REFRESH_ENDPOINT);
        assert_eq!(refresh.timeout, Duration::from_secs(15));
        assert_eq!(config.limits, Some((3, 100000)));
        let cloud = json!({"subscription":{"daily_requests":99,"daily_tokens":999}});
        let config = parse(&cloud, true).unwrap();
        assert!(config.refresh.is_some());
        assert!(config.limits.is_none());
        let cloud = json!({"subscription_limits":{"daily_requests":3,"daily_tokens":100000}});
        let config = parse(&cloud, false).unwrap();
        assert!(config.refresh.is_none());
        assert_eq!(config.limits, Some((3, 100000)));
        let cloud = json!({"subscription":{"client_id":"synthetic-client_1.2","refresh_timeout_ms":u64::MAX},"subscription_limits":{"daily_requests":u64::MAX,"daily_tokens":u64::MAX}});
        let config = parse(&cloud, true).unwrap();
        assert_eq!(config.refresh.as_ref().unwrap().timeout, Duration::from_secs(30));
        assert!(!format!("{config:?}").contains("synthetic-client_1.2"));
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
            assert!(parse(&json!({"subscription_limits":value}), true).is_err());
        }
        for id in ["", "bad id", "bad/id", "bad\nid", "λ", &"a".repeat(129)] {
            assert!(parse(&json!({"subscription":{"client_id":id}}), true).is_err());
        }
        for id in ["A", "synthetic-client_1.2", &"a".repeat(128)] {
            let cloud = json!({"subscription":{"client_id":id}});
            assert_eq!(parse(&cloud, true).unwrap().refresh.unwrap().client_id, id);
        }
        for value in [json!(0), json!(-1), json!(1.5), json!("1000")] {
            assert!(parse(&json!({"subscription":{"refresh_timeout_ms":value}}), true).is_err());
        }
        for endpoint in [
            "https://example.invalid/token",
            "http://127.0.0.1:9/token",
            REFRESH_ENDPOINT,
        ] {
            assert!(parse(&json!({"subscription":{"refresh_endpoint":endpoint}}), true).is_err());
        }
        assert!(parse(&json!({"subscription":false}), true).is_err());
        assert_eq!(
            format!("{} {:?}", Error, Error),
            "Invalid user cloud subscription configuration. Error"
        );
    }
}
