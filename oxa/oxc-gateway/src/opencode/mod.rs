//! OpenCode Go protocol fields. Admission, key reads and subscription billing
//! stay with the native gateway; no sign-in, credential store or paid fallback.
use bigdecimal::BigDecimal;
use num_traits::{Signed, Zero};
use reqwest::{
    header::{HeaderMap, HeaderValue, USER_AGENT},
    Url,
};
use serde_json::Value;
use std::{collections::BTreeMap, str::FromStr};

pub const GO_BASE: &str = "https://opencode.ai/inference/go";
const MAX_MODELS: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Discovery,
    Endpoint,
    UnsupportedShape,
    Usage,
    SessionHint,
    Request,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Discovery => "Invalid OpenCode Go discovery response",
            Self::Endpoint => "Invalid OpenCode Go endpoint",
            Self::UnsupportedShape => "This OpenCode Go model does not offer chat completions",
            Self::Usage => "Invalid OpenCode Go subscription usage",
            Self::SessionHint => "Invalid private OpenCode Go session hint",
            Self::Request => "Invalid OpenCode Go chat request",
        })
    }
}
impl std::error::Error for Error {}

/// Unknown API shapes and their hints survive discovery for later adapters.
#[derive(Clone, Debug, PartialEq)]
pub struct Api {
    pub base_url: String,
    pub hints: Value,
}
#[derive(Clone, Debug, PartialEq)]
pub struct Model {
    pub id: String,
    pub canonical_id: String,
    pub context_window: u32,
    pub max_output: u32,
    pub api: BTreeMap<String, Api>,
    pub cost: Value,
    pub capabilities: Value,
    pub modalities: Value,
}
fn endpoint(base: &str) -> Result<Url, Error> {
    let url = Url::parse(base).map_err(|_| Error::Endpoint)?;
    let loopback = match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => {
            ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|ip| ip.is_loopback())
        }
        Some(url::Host::Domain(host)) => host == "localhost",
        None => false,
    };
    if !(url.scheme() == "https" || url.scheme() == "http" && loopback)
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::Endpoint);
    }
    Ok(url)
}
/// The trusted Go origin constrains every discovered API URL before any key
/// can be sent. A malformed record never yields a partial catalog.
pub fn models(value: &Value, trusted_base: &str) -> Result<Vec<Model>, Error> {
    let base = endpoint(trusted_base)?;
    let data = value["data"]
        .as_array()
        .filter(|v| v.len() <= MAX_MODELS)
        .ok_or(Error::Discovery)?;
    let mut out = BTreeMap::new();
    for item in data {
        let text = |key| {
            item[key]
                .as_str()
                .filter(|s| {
                    !s.is_empty()
                        && s.len() <= 128
                        && s.bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"._:/-".contains(&b))
                })
                .map(str::to_owned)
                .ok_or(Error::Discovery)
        };
        let limit = |key| {
            item["limit"][key]
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .filter(|n| *n > 0)
                .ok_or(Error::Discovery)
        };
        let context_window = limit("context")?;
        let max_output = limit("output")?;
        if max_output > context_window {
            return Err(Error::Discovery);
        }
        let mut api = BTreeMap::new();
        for (shape, hints) in item["api"]
            .as_object()
            .filter(|v| !v.is_empty() && v.len() <= 32)
            .ok_or(Error::Discovery)?
        {
            if shape.is_empty() || shape.len() > 64 || shape.chars().any(char::is_control) {
                return Err(Error::Discovery);
            }
            let raw = hints["base_url"].as_str().ok_or(Error::Discovery)?;
            let url = endpoint(raw)?;
            let prefix = base.path().trim_end_matches('/');
            if url.origin() != base.origin()
                || !(url.path() == prefix || url.path().starts_with(&format!("{prefix}/")))
            {
                return Err(Error::Endpoint);
            }
            api.insert(
                shape.clone(),
                Api {
                    base_url: url.into(),
                    hints: hints.clone(),
                },
            );
        }
        let model = Model {
            id: text("id")?,
            canonical_id: text("canonical_id")?,
            context_window,
            max_output,
            api,
            cost: item["cost"].clone(),
            capabilities: item["capabilities"].clone(),
            modalities: item["modalities"].clone(),
        };
        if out.insert(model.id.clone(), model).is_some() {
            return Err(Error::Discovery);
        }
    }
    Ok(out.into_values().collect())
}
impl Model {
    pub fn chat_endpoint(&self) -> Result<Url, Error> {
        let api = self.api.get("chat-completions").ok_or(Error::UnsupportedShape)?;
        let mut url = endpoint(&api.base_url)?;
        url.path_segments_mut()
            .map_err(|_| Error::Endpoint)?
            .pop_if_empty()
            .push("chat")
            .push("completions");
        Ok(url)
    }
    /// Canonical chat-shaped messages/tools pass through; no model-name table.
    /// Route/usage controls come from the trusted caller, never model JSON.
    pub fn request(&self, payload: &Value) -> Result<Value, Error> {
        self.chat_endpoint()?;
        let mut body = payload.as_object().cloned().ok_or(Error::Request)?;
        if !body.get("messages").is_some_and(Value::is_array) {
            return Err(Error::Request);
        }
        body.insert("model".into(), self.id.clone().into());
        body.insert("stream".into(), true.into());
        body.insert("stream_options".into(), serde_json::json!({"include_usage":true}));
        let mut output = u64::from(self.max_output);
        for field in ["max_tokens", "max_completion_tokens"] {
            if let Some(v) = body.remove(field) {
                output = output.min(v.as_u64().filter(|n| *n > 0).ok_or(Error::Request)?);
            }
        }
        body.insert("max_tokens".into(), output.into());
        // Local-engine hints and paid/provider-routing overrides have no meaning
        // on the Go subscription endpoint and must not select another balance.
        for field in [
            "provider",
            "cache_prompt",
            "id_slot",
            "n_predict",
            "slot_id",
            "prefill_only",
        ] {
            body.remove(field);
        }
        Ok(Value::Object(body))
    }
}

/// The gateway supplies a persistent, private HMAC, never a raw session id or
/// workspace path. Keeping construction typed avoids a free-form header value.
#[derive(Clone)]
pub struct SessionHint([u8; 32]);
impl SessionHint {
    pub fn from_hmac(digest: [u8; 32]) -> Self {
        Self(digest)
    }
    pub fn headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            USER_AGENT,
            HeaderValue::from_static(concat!("oxa/", env!("CARGO_PKG_VERSION"))),
        );
        let mut text = String::with_capacity(64);
        use std::fmt::Write;
        for byte in self.0 {
            write!(&mut text, "{byte:02x}").expect("String write");
        }
        let mut hint = HeaderValue::from_str(&text).expect("hex header");
        hint.set_sensitive(true);
        headers.insert("x-opencode-session", hint);
        headers
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Usage {
    pub prompt: u64,
    pub completion: u64,
    pub allowance: Value,
    pub list_cost: Value,
    pub tokens: BTreeMap<String, u64>,
}
fn money(value: &Value) -> Result<BigDecimal, Error> {
    let text = value.as_number().ok_or(Error::Usage)?.to_string();
    if text.len() > 128 {
        return Err(Error::Usage);
    }
    let number = BigDecimal::from_str(&text).map_err(|_| Error::Usage)?;
    if number.is_negative() || number.as_bigint_and_exponent().1.unsigned_abs() > 1024 {
        return Err(Error::Usage);
    }
    Ok(number)
}
impl Usage {
    pub fn parse(value: &Value) -> Result<Self, Error> {
        let go = &value["opencode"];
        if go["billing_source"] != "go" || !money(&value["cost"])?.is_zero() {
            return Err(Error::Usage);
        }
        money(&go["allowance"])?;
        money(&go["list_cost"])?;
        let prompt = value["prompt_tokens"].as_u64().ok_or(Error::Usage)?;
        let completion = value["completion_tokens"].as_u64().ok_or(Error::Usage)?;
        prompt.checked_add(completion).ok_or(Error::Usage)?;
        let mut tokens = BTreeMap::new();
        for key in [
            "input",
            "output",
            "reasoning",
            "cache_read",
            "cache_write_5m",
            "cache_write_1h",
        ] {
            tokens.insert(key.into(), go["tokens"][key].as_u64().ok_or(Error::Usage)?);
        }
        Ok(Self {
            prompt,
            completion,
            allowance: go["allowance"].clone(),
            list_cost: go["list_cost"].clone(),
            tokens,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    RegionBlocked,
    LimitReached,
    Unauthenticated,
    Forbidden,
    Other(u16),
}
pub fn refusal(status: u16, body: &Value) -> Refusal {
    if status == 400
        && body["error"]["type"] == "invalid_request_error"
        && body["error"]["message"].as_str().is_some_and(|s| {
            s.to_ascii_lowercase()
                .contains("hosted outside europe and the united states")
        })
    {
        return Refusal::RegionBlocked;
    }
    match status {
        401 => Refusal::Unauthenticated,
        403 => Refusal::Forbidden,
        429 => Refusal::LimitReached,
        other => Refusal::Other(other),
    }
}
impl Refusal {
    pub fn code(self) -> &'static str {
        match self {
            Self::RegionBlocked => "region_blocked",
            Self::LimitReached => "subscription_limit_reached",
            Self::Unauthenticated => "cloud_credentials_unavailable",
            Self::Forbidden => "cloud_forbidden",
            Self::Other(_) => "cloud_upstream_error",
        }
    }
}
