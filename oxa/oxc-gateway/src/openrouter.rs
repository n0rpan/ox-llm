//! Opt-in OpenRouter account and discovery client; no cache or request-path wiring.
//! Available with `anthropic-client,pricing`, reusing the gateway's transport,
//! secret handling, errors and exact decimal dependency.
use crate::{anthropic_client::Error, secrets::Key};
use bigdecimal::BigDecimal;
use std::{fmt, time::Duration};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Balance {
    pub limit: Option<BigDecimal>,
    pub limit_remaining: Option<BigDecimal>,
    pub usage: BigDecimal,
    pub usage_daily: BigDecimal,
    pub usage_weekly: BigDecimal,
    pub usage_monthly: BigDecimal,
    pub limit_reset: Option<String>,
    pub account_remaining: BigDecimal,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Model {
    pub id: String,
    pub context_length: u64,
    pub max_completion_tokens: Option<u64>,
    /// `None` when OpenRouter reports the unavailable-price sentinel `-1`.
    pub prompt_price_per_token: Option<BigDecimal>,
    /// `None` when OpenRouter reports the unavailable-price sentinel `-1`.
    pub completion_price_per_token: Option<BigDecimal>,
    pub tools: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefusalKind {
    CreditExhausted,
    KeyLimit,
    RateLimit,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub kind: RefusalKind,
    /// Provider metadata, with any echoed credential redacted.
    pub limit_source: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub error: Error,
    pub refusal: Option<Refusal>,
}
impl From<Error> for Failure {
    fn from(error: Error) -> Self {
        Self { error, refusal: None }
    }
}
impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "OpenRouter status/discovery failed: {:?}", self.error)
    }
}
impl std::error::Error for Failure {}

/// Reusable reqwest connection pool, just like the gateway's streaming client.
/// Keys are supplied per call, never retained. Redirects and proxies are disabled.
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base: reqwest::Url,
    timeout: Duration,
}
impl Client {
    /// `base` is the injected API root (for example `https://example.invalid/api/v1`).
    /// HTTP is allowed only on loopback, for tests. The deadline bounds each GET,
    /// including its body, rather than just waiting for response headers.
    pub fn new(base: &str, timeout: Duration) -> Result<Self, Failure> {
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| Error::Setup)?;
        Self::with_transport(base, timeout, http)
    }

    #[cfg(feature = "anthropic-gateway")]
    pub(crate) fn from_messages_transport(
        base: &str,
        timeout: Duration,
        client: &crate::anthropic_client::Client,
    ) -> Result<Self, Failure> {
        Self::with_transport(base, timeout, client.transport().clone())
    }

    fn with_transport(base: &str, timeout: Duration, http: reqwest::Client) -> Result<Self, Failure> {
        let mut base = reqwest::Url::parse(base).map_err(|_| Error::Endpoint)?;
        let loopback = match base.host() {
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => {
                ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|ip| ip.is_loopback())
            }
            Some(url::Host::Domain(host)) => host == "localhost",
            None => false,
        };
        if !(base.scheme() == "https" || base.scheme() == "http" && loopback)
            || base.host_str().is_none()
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
            || timeout.is_zero()
        {
            return Err(Error::Endpoint.into());
        }
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        Ok(Self { http, base, timeout })
    }

    pub async fn balance(&self, key: &Key) -> Result<Balance, Failure> {
        let key_data = self.get("key", key).await?;
        let credits = self.get("credits", key).await?;
        let data = &key_data["data"];
        let credits = &credits["data"];
        Ok(Balance {
            limit: optional_decimal(&data["limit"])?,
            limit_remaining: optional_decimal(&data["limit_remaining"])?,
            usage: decimal(&data["usage"])?,
            usage_daily: decimal(&data["usage_daily"])?,
            usage_weekly: decimal(&data["usage_weekly"])?,
            usage_monthly: decimal(&data["usage_monthly"])?,
            limit_reset: match &data["limit_reset"] {
                serde_json::Value::Null => None,
                serde_json::Value::String(text) => Some(text.clone()),
                _ => return Err(Error::Protocol.into()),
            },
            account_remaining: decimal(&credits["total_credits"])? - decimal(&credits["total_usage"])?,
        })
    }

    pub async fn models(&self, key: &Key) -> Result<Vec<Model>, Failure> {
        let response = self.get("models/user", key).await?;
        response["data"]
            .as_array()
            .ok_or(Error::Protocol)?
            .iter()
            .map(|model| {
                let pricing = &model["pricing"];
                // Prices are decimal strings in this API, never binary floats.
                let rate = |value: &serde_json::Value| match value {
                    serde_json::Value::Null => Ok(None),
                    serde_json::Value::String(text) => parse_price(text),
                    _ => Err(Error::Protocol.into()),
                };
                let parameters = match &model["supported_parameters"] {
                    serde_json::Value::Null => &[][..],
                    serde_json::Value::Array(parameters) => parameters.as_slice(),
                    _ => return Err(Error::Protocol.into()),
                };
                if parameters.iter().any(|parameter| !parameter.is_string()) {
                    return Err(Error::Protocol.into());
                }
                Ok(Model {
                    id: model["id"]
                        .as_str()
                        .filter(|id| !id.is_empty())
                        .ok_or(Error::Protocol)?
                        .into(),
                    context_length: model["context_length"].as_u64().ok_or(Error::Protocol)?,
                    max_completion_tokens: match &model["top_provider"]["max_completion_tokens"] {
                        serde_json::Value::Null => None,
                        value => Some(value.as_u64().ok_or(Error::Protocol)?),
                    },
                    prompt_price_per_token: rate(&pricing["prompt"])?,
                    completion_price_per_token: rate(&pricing["completion"])?,
                    tools: parameters
                        .iter()
                        .any(|parameter| parameter.as_str() == Some("tools")),
                })
            })
            .collect()
    }

    async fn get(&self, path: &str, key: &Key) -> Result<serde_json::Value, Failure> {
        tokio::time::timeout(self.timeout, self.get_inner(path, key))
            .await
            .map_err(|_| Error::Transport)?
    }

    async fn get_inner(&self, path: &str, key: &Key) -> Result<serde_json::Value, Failure> {
        use reqwest::header::{HeaderValue, ACCEPT, AUTHORIZATION};
        const MAX_BODY: usize = 8 * 1024 * 1024;
        let endpoint = self.base.join(path).map_err(|_| Error::Endpoint)?;
        let mut bearer =
            HeaderValue::from_str(&format!("Bearer {}", key.expose())).map_err(|_| Error::Setup)?;
        bearer.set_sensitive(true);
        let mut response = self
            .http
            .get(endpoint)
            .header(AUTHORIZATION, bearer)
            .header(ACCEPT, "application/json")
            .send()
            .await
            .map_err(|_| Error::Transport)?;
        let status = response.status().as_u16();
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| Error::Transport)? {
            if chunk.len() > MAX_BODY - body.len() {
                return Err(Error::TooLarge.into());
            }
            body.extend_from_slice(&chunk);
        }
        if status != 200 {
            return Err(Failure {
                error: Error::Http(status),
                refusal: Some(classify_refusal(status, &body, key)),
            });
        }
        let mut value = serde_json::from_slice(&body).map_err(|_| Error::Protocol)?;
        // Even successful provider replies must not echo a credential in Debug.
        crate::redaction::value(&mut value, key);
        Ok(value)
    }
}

/// Classify one refusal without retaining its untrusted message or body. Unknown
/// limit sources are preserved (redacted) rather than guessed to be account credit.
pub fn classify_refusal(status: u16, body: &[u8], key: &Key) -> Refusal {
    let mut value: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
    crate::redaction::value(&mut value, key);
    let limit_source = value
        .pointer("/error/metadata/limit_source")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let kind = match (status, limit_source.as_deref()) {
        (429, _) => RefusalKind::RateLimit,
        (402, Some("openrouter_key_limit")) => RefusalKind::KeyLimit,
        (402, Some("openrouter_credits")) => RefusalKind::CreditExhausted,
        _ => RefusalKind::Other,
    };
    Refusal { kind, limit_source }
}

fn parse_price(raw: &str) -> Result<Option<BigDecimal>, Failure> {
    // The provider's sentinel is not a free price or a malformed model.
    if raw == "-1" {
        Ok(None)
    } else {
        parse_decimal(raw).map(Some)
    }
}

fn parse_decimal(raw: &str) -> Result<BigDecimal, Failure> {
    // Bound adversarial coefficients/exponents before arithmetic, without ever
    // passing through f32/f64. Only parse_price accepts the unavailable sentinel.
    if raw.len() > 1024 {
        return Err(Error::Protocol.into());
    }
    let value: BigDecimal = raw.parse().map_err(|_| Error::Protocol)?;
    let (_, scale) = value.as_bigint_and_exponent();
    if !(-1000..=1000).contains(&scale) || value < 0 {
        return Err(Error::Protocol.into());
    }
    Ok(value)
}
fn decimal(raw: &serde_json::Value) -> Result<BigDecimal, Failure> {
    match raw {
        serde_json::Value::Number(number) => parse_decimal(&number.to_string()),
        serde_json::Value::String(text) => parse_decimal(text),
        _ => Err(Error::Protocol.into()),
    }
}
fn optional_decimal(raw: &serde_json::Value) -> Result<Option<BigDecimal>, Failure> {
    if raw.is_null() {
        Ok(None)
    } else {
        decimal(raw).map(Some)
    }
}
impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OpenRouterClient([configured])")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets;
    use serde_json::json;
    use std::{
        fs,
        io::{Read, Write},
        net::TcpListener,
        os::unix::fs::OpenOptionsExt,
        path::PathBuf,
        sync::atomic::{AtomicUsize, Ordering},
        thread,
    };

    const CREDENTIAL: &str = "synthetic-discovery-credential";
    static NEXT: AtomicUsize = AtomicUsize::new(0);

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "gateway-discovery-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path.join("credentials.json"))
                .unwrap();
            write!(file, "{}", json!({"openrouter": CREDENTIAL})).unwrap();
            Self(path)
        }
        fn key(&self) -> Key {
            secrets::read(&self.0.join("credentials.json"), "openrouter").unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    // Only numeric loopback addresses; assert paths and authentication on every GET.
    fn mock(replies: Vec<(&'static str, u16, String, Duration)>) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}/api/v1/", listener.local_addr().unwrap());
        let task = thread::spawn(move || {
            for (path, status, body, delay) in replies {
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                let mut socket = loop {
                    match listener.accept() {
                        Ok((socket, _)) => break socket,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(std::time::Instant::now() < deadline, "expected request");
                            thread::sleep(Duration::from_millis(2));
                        }
                        Err(e) => panic!("{e}"),
                    }
                };
                socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    socket.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                let request = String::from_utf8(request).unwrap();
                assert!(request.starts_with(&format!("GET /api/v1/{path} HTTP/1.1\r\n")));
                assert!(request
                    .to_ascii_lowercase()
                    .contains(&format!("authorization: bearer {CREDENTIAL}")));
                thread::sleep(delay);
                let _ = write!(socket, "HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            }
        });
        (base, task)
    }
    fn reply(path: &'static str, body: &str) -> (&'static str, u16, String, Duration) {
        (path, 200, body.into(), Duration::ZERO)
    }
    fn decimal(s: &str) -> BigDecimal {
        s.parse().unwrap()
    }

    #[tokio::test]
    async fn balance_null_limit_and_exact_account_credit() {
        let fixture = Fixture::new();
        let (base, task) = mock(vec![
            reply(
                "key",
                r#"{"data":{"limit":null,"limit_remaining":null,"usage":1.000000000000000001,"usage_daily":0.1,"usage_weekly":0.2,"usage_monthly":0.3,"limit_reset":null}}"#,
            ),
            reply(
                "credits",
                r#"{"data":{"total_credits":10.000000000000000003,"total_usage":1.000000000000000001}}"#,
            ),
        ]);
        let client = Client::new(&base, Duration::from_secs(2)).unwrap();
        let balance = client.balance(&fixture.key()).await.unwrap();
        assert_eq!(balance.limit, None);
        assert_eq!(balance.limit_remaining, None);
        assert_eq!(balance.limit_reset, None);
        assert_eq!(balance.usage, decimal("1.000000000000000001"));
        assert_eq!(balance.usage_daily, decimal("0.1"));
        assert_eq!(balance.usage_weekly, decimal("0.2"));
        assert_eq!(balance.usage_monthly, decimal("0.3"));
        assert_eq!(balance.account_remaining, decimal("9.000000000000000002"));
        task.join().unwrap();
    }

    #[tokio::test]
    async fn models_exact_prices_and_tools() {
        let fixture = Fixture::new();
        let (base, task) = mock(vec![reply(
            "models/user",
            r#"{"data":[{"id":"synthetic/tool-model","context_length":128000,"top_provider":{"max_completion_tokens":8192},"pricing":{"prompt":"0.0000001234567890123456789","completion":"2.5e-7"},"supported_parameters":["tools","temperature"]},{"id":"synthetic/plain","context_length":1024,"top_provider":{"max_completion_tokens":null},"pricing":{"prompt":"0","completion":"0"},"supported_parameters":[]}]}"#,
        )]);
        let models = Client::new(&base, Duration::from_secs(2))
            .unwrap()
            .models(&fixture.key())
            .await
            .unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "synthetic/tool-model");
        assert_eq!(models[0].context_length, 128000);
        assert_eq!(models[0].max_completion_tokens, Some(8192));
        assert_eq!(
            models[0].prompt_price_per_token,
            Some(decimal("0.0000001234567890123456789"))
        );
        assert_eq!(models[0].completion_price_per_token, Some(decimal("0.00000025")));
        assert!(models[0].tools);
        assert!(!models[1].tools);
        assert_eq!(models[1].max_completion_tokens, None);
        task.join().unwrap();
    }

    #[tokio::test]
    async fn models_unavailable_prices_do_not_discard_priced_models() {
        let fixture = Fixture::new();
        let body = json!({"data": [
            {"id":"synthetic/router", "context_length":128000,
             "pricing":{"prompt":"-1", "completion":"-1"}, "supported_parameters":["tools"]},
            {"id":"synthetic/priced", "context_length":8192,
             "pricing":{"prompt":"0.000001", "completion":"0"}, "supported_parameters":[]},
            {"id":"synthetic/partial", "context_length":8192,
             "pricing":{"prompt":"0", "completion":"-1"}, "supported_parameters":[]}
        ]})
        .to_string();
        let (base, task) = mock(vec![("models/user", 200, body, Duration::ZERO)]);
        let models = Client::new(&base, Duration::from_secs(2))
            .unwrap()
            .models(&fixture.key())
            .await
            .unwrap();
        assert_eq!(models.len(), 3);
        assert_eq!(models[0].id, "synthetic/router");
        assert_eq!(models[0].prompt_price_per_token, None);
        assert_eq!(models[0].completion_price_per_token, None);
        assert!(models[0].tools);
        assert_eq!(models[1].id, "synthetic/priced");
        assert_eq!(models[1].prompt_price_per_token, Some(decimal("0.000001")));
        assert_eq!(models[1].completion_price_per_token, Some(decimal("0")));
        assert_eq!(models[2].id, "synthetic/partial");
        assert_eq!(models[2].prompt_price_per_token, Some(decimal("0")));
        assert_eq!(models[2].completion_price_per_token, None);
        task.join().unwrap();
    }

    #[tokio::test]
    async fn malformed_json_and_schema_are_safe() {
        let fixture = Fixture::new();
        let key = fixture.key();
        for body in [CREDENTIAL, r#"{"data":[{"id":"incomplete"}]}"#] {
            let (base, task) = mock(vec![reply("models/user", body)]);
            let client = Client::new(&base, Duration::from_secs(2)).unwrap();
            let error = client.models(&key).await.unwrap_err();
            assert_eq!(error.error, Error::Protocol);
            assert!(!format!("{error} {error:?} {client:?} {key:?}").contains(CREDENTIAL));
            task.join().unwrap();
        }
    }

    #[tokio::test]
    async fn non_200_preserves_limit_source_without_echoing_key() {
        let fixture = Fixture::new();
        for (status, source, kind) in [
            (402, "openrouter_key_limit", RefusalKind::KeyLimit),
            (402, "openrouter_credits", RefusalKind::CreditExhausted),
            (402, "unknown_limit", RefusalKind::Other),
            (429, "provider", RefusalKind::RateLimit),
            (503, CREDENTIAL, RefusalKind::Other),
        ] {
            let body = json!({"error":{"message":CREDENTIAL,"metadata":{"limit_source":source}}}).to_string();
            let (base, task) = mock(vec![("models/user", status, body, Duration::ZERO)]);
            let error = Client::new(&base, Duration::from_secs(2))
                .unwrap()
                .models(&fixture.key())
                .await
                .unwrap_err();
            assert_eq!(error.error, Error::Http(status));
            let refusal = error.refusal.as_ref().unwrap();
            assert_eq!(refusal.kind, kind);
            assert_eq!(
                refusal.limit_source.as_deref(),
                Some(if source == CREDENTIAL {
                    "[redacted]"
                } else {
                    source
                })
            );
            assert!(!format!("{error} {error:?}").contains(CREDENTIAL));
            task.join().unwrap();
        }
    }

    #[test]
    fn endpoint_and_decimal_validation_and_unknown_refusals() {
        for base in [
            format!("https://{CREDENTIAL}@example.invalid/api/v1"),
            format!("https://example.invalid/?key={CREDENTIAL}"),
            "http://example.invalid/api/v1".into(),
            "file:///api/v1".into(),
        ] {
            let error = Client::new(&base, Duration::from_secs(1)).unwrap_err();
            assert_eq!(error.error, Error::Endpoint);
            assert!(!format!("{error} {error:?}").contains(CREDENTIAL));
        }
        let client = Client::new(
            &format!("https://example.invalid/{CREDENTIAL}"),
            Duration::from_secs(1),
        )
        .unwrap();
        assert!(!format!("{client:?}").contains(CREDENTIAL));
        assert_eq!(parse_price("-1").unwrap(), None);
        assert_eq!(parse_decimal("-1").unwrap_err().error, Error::Protocol);
        for raw in ["NaN", "-2", "1e1000000000", "1e-1000000000", "not a price"] {
            assert_eq!(parse_decimal(raw).unwrap_err().error, Error::Protocol);
            assert_eq!(parse_price(raw).unwrap_err().error, Error::Protocol);
        }
        let fixture = Fixture::new();
        let refusal = classify_refusal(402, CREDENTIAL.as_bytes(), &fixture.key());
        assert_eq!(refusal.kind, RefusalKind::Other);
        assert_eq!(refusal.limit_source, None);
        assert!(!format!("{refusal:?}").contains(CREDENTIAL));
    }

    #[tokio::test]
    async fn finite_key_limit_and_reset() {
        let fixture = Fixture::new();
        let (base, task) = mock(vec![
            reply(
                "key",
                r#"{"data":{"limit":20,"limit_remaining":12.75,"usage":7.25,"usage_daily":1,"usage_weekly":2,"usage_monthly":3,"limit_reset":"monthly"}}"#,
            ),
            reply("credits", r#"{"data":{"total_credits":10,"total_usage":11}}"#),
        ]);
        let balance = Client::new(base.trim_end_matches('/'), Duration::from_secs(2))
            .unwrap()
            .balance(&fixture.key())
            .await
            .unwrap();
        assert_eq!(balance.limit, Some(decimal("20")));
        assert_eq!(balance.limit_remaining, Some(decimal("12.75")));
        assert_eq!(balance.limit_reset.as_deref(), Some("monthly"));
        assert_eq!(balance.account_remaining, decimal("-1"));
        task.join().unwrap();
    }

    #[tokio::test]
    async fn successful_response_cannot_echo_key_in_debug() {
        let fixture = Fixture::new();
        let body = json!({"data":[{"id":CREDENTIAL,"context_length":100,
            "top_provider":{"max_completion_tokens":null},
            "pricing":{"prompt":"0","completion":"0"},"supported_parameters":[]}]})
        .to_string();
        let (base, task) = mock(vec![("models/user", 200, body, Duration::ZERO)]);
        let models = Client::new(&base, Duration::from_secs(2))
            .unwrap()
            .models(&fixture.key())
            .await
            .unwrap();
        assert_eq!(models[0].id, "[redacted]");
        assert!(!format!("{models:?}").contains(CREDENTIAL));
        task.join().unwrap();
    }

    #[tokio::test]
    async fn non_200_malformed_body_keeps_http_status() {
        let fixture = Fixture::new();
        let (base, task) = mock(vec![("key", 401, CREDENTIAL.into(), Duration::ZERO)]);
        let error = Client::new(&base, Duration::from_secs(2))
            .unwrap()
            .balance(&fixture.key())
            .await
            .unwrap_err();
        assert_eq!(error.error, Error::Http(401));
        assert_eq!(error.refusal.as_ref().unwrap().limit_source, None);
        assert!(!format!("{error} {error:?}").contains(CREDENTIAL));
        task.join().unwrap();
    }

    #[tokio::test]
    async fn timeout_is_bounded_and_safe() {
        let fixture = Fixture::new();
        let (base, task) = mock(vec![("key", 200, "{}".into(), Duration::from_millis(200))]);
        let error = Client::new(&base, Duration::from_millis(50))
            .unwrap()
            .balance(&fixture.key())
            .await
            .unwrap_err();
        assert_eq!(error.error, Error::Transport);
        assert!(!format!("{error} {error:?}").contains(CREDENTIAL));
        task.join().unwrap();
    }
}
