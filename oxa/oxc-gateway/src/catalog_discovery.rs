//! Provider lists are resolved at catalog startup or refresh, outside inference.
use crate::{
    anthropic_client::{Client, Error},
    catalog::Provider,
    catalog_cache::Cache,
    pricing::Price,
    secrets::Key,
};
use num_traits::ToPrimitive;
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    path::Path,
    time::{Duration, SystemTime},
};

const MAX_LISTED_MODELS: usize = 8192;

pub use crate::catalog_cache::State;

/// What a ChatGPT-plan list needs besides the cache key: the owner's token file and the refresh
/// settings. Held by the gateway's runtime; the token itself is read only inside the transport.
pub struct Plan<'a> {
    pub token_path: &'a Path,
    pub refresh_endpoint: &'a str,
    pub client_id: &'a str,
    pub timeout: Duration,
}

pub struct Status {
    pub state: State,
    pub missing_price: usize,
    pub missing_limits: usize,
    pub estimated_price: usize,
    /// Offers whose limits or prices came from the public metadata catalog.
    pub public_metadata: usize,
}

#[derive(Clone)]
pub struct Source {
    provider: Provider,
    base: String,
    filename: &'static str,
    /// Public, keyless catalog that fills the limits and prices a provider's own model list lacks.
    metadata_base: Option<String>,
}

/// OpenRouter's public model catalog: no key is sent, and it carries the context window, output
/// limit and prices of the OpenAI and Anthropic ids.
pub const PUBLIC_METADATA_BASE: &str = "https://openrouter.ai/api/v1";
const PUBLIC_SOURCE: &str = "openrouter-public";
impl Source {
    pub fn new(provider: Provider, base: &str) -> Option<Self> {
        let filename = match provider {
            Provider::OpenRouter => "openrouter-catalog-cache.json",
            Provider::Anthropic => "anthropic-catalog-cache.json",
            #[cfg(feature = "upstream-openai")]
            Provider::OpenAi => "openai-catalog-cache.json",
            #[cfg(feature = "upstream-openai")]
            Provider::ChatGpt => "chatgpt-catalog-cache.json",
            _ => return None,
        };
        let metadata_base = matches!(
            provider,
            Provider::OpenAi | Provider::ChatGpt | Provider::Anthropic
        )
        .then(|| PUBLIC_METADATA_BASE.to_owned());
        Some(Self {
            provider,
            base: base.to_owned(),
            filename,
            metadata_base,
        })
    }

    /// Point the public metadata lookup elsewhere, or off with `None`.
    pub fn with_metadata_base(mut self, base: Option<&str>) -> Self {
        self.metadata_base = base.map(str::to_owned);
        self
    }

    pub fn provider(&self) -> Provider {
        self.provider
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    pub(crate) fn load(&self, directory: &Path, key: &Key) -> Option<(State, Vec<Offer>)> {
        let (state, snapshot) = Cache::new(directory, self.filename).load(key, SystemTime::now())?;
        if snapshot["base"].as_str()? != self.base {
            return None;
        }
        Some((state, parse(&snapshot["data"]).ok()?))
    }

    #[cfg_attr(not(feature = "upstream-openai"), allow(unused_variables))]
    pub(crate) async fn fetch(
        &self,
        client: &Client,
        key: &Key,
        timeout: Duration,
        directory: &Path,
        plan: Option<Plan<'_>>,
    ) -> Result<Vec<Offer>, Error> {
        let rows: Vec<Value> = match self.provider {
            Provider::OpenRouter => crate::openrouter::Client::from_messages_transport(
                &self.base, timeout, client,
            )
            .map_err(|error| error.error)?
            .models(key)
            .await
            .map_err(|error| error.error)?
            .into_iter()
            .map(|model| {
                let prices = price_fields(&model);
                json!({
                    "id": model.id, "context": model.context_length,
                    "output": model.max_completion_tokens, "tools": model.tools,
                    "pricing": prices
                })
            })
            .collect(),
            Provider::Anthropic => client
                .models(&self.base, key, timeout)
                .await?
                .into_iter()
                .map(|model| {
                    json!({"id": model.id, "context": model.max_input_tokens, "output": model.max_output_tokens})
                })
                .collect(),
            #[cfg(feature = "upstream-openai")]
            Provider::OpenAi => crate::openai::client::Client::from_messages_transport(client)
                .models(&self.base, key, timeout)
                .await
                .map_err(openai_error)?
                .into_iter()
                .map(|id| json!({"id": id}))
                .collect(),
            #[cfg(feature = "upstream-openai")]
            Provider::ChatGpt => {
                let plan = plan.ok_or(Error::Request("subscription"))?;
                // The cache key names the account; the request is pinned to that same account.
                let account = key
                    .expose()
                    .strip_prefix(crate::secrets::ACCOUNT_KEY_PREFIX)
                    .ok_or(Error::Request("subscription"))?;
                crate::openai::client::Client::from_messages_transport(client)
                    .plan_models(
                        &self.base,
                        plan.token_path,
                        account,
                        crate::openai::client::Refresh {
                            endpoint: plan.refresh_endpoint,
                            client_id: plan.client_id,
                            timeout: plan.timeout,
                        },
                        timeout,
                    )
                    .await
                    .map_err(openai_error)?
                    .into_iter()
                    // A window outside the accepted range stays unknown, never a refusal.
                    .map(|(id, context)| {
                        let context = context.filter(|n| *n > 0 && *n <= u64::from(u32::MAX));
                        json!({"id": id, "context": context})
                    })
                    .collect()
            }
            _ => return Err(Error::Request("provider")),
        };
        let mut rows = rows;
        self.fill_from_public_catalog(&mut rows, client, timeout).await;
        let rows = Value::Array(rows);
        if crate::redaction::contains_secret(&rows, key.expose()) {
            return Err(Error::Protocol);
        }
        let offers = parse(&rows)?;
        let _ = Cache::new(directory, self.filename).store(
            key,
            json!({"base": self.base, "data": rows}),
            SystemTime::now(),
        );
        Ok(offers)
    }
}

#[cfg(all(test, not(feature = "upstream-openai")))]
#[test]
fn chatgpt_catalog_source_requires_the_openai_upstream() {
    assert!(Source::new(Provider::ChatGpt, "https://example.invalid").is_none());
}

#[cfg(feature = "upstream-openai")]
fn openai_error(error: crate::openai::client::Error) -> Error {
    match error {
        crate::openai::client::Error::Http { status, .. } => Error::Http(status),
        crate::openai::client::Error::Endpoint => Error::Endpoint,
        crate::openai::client::Error::TooLarge => Error::TooLarge,
        _ => Error::Transport,
    }
}

impl Source {
    /// Fill the limits and prices the provider's own list lacks from the public catalog, matched
    /// by upstream id. Anything the provider already gave stays; a failed fetch changes nothing.
    async fn fill_from_public_catalog(&self, rows: &mut [Value], client: &Client, timeout: Duration) {
        let (Some(base), Some(prefix)) = (
            &self.metadata_base,
            match self.provider {
                Provider::OpenAi | Provider::ChatGpt => Some("openai/"),
                Provider::Anthropic => Some("anthropic/"),
                _ => None,
            },
        ) else {
            return;
        };
        let Ok(public) = crate::openrouter::Client::from_messages_transport(base, timeout, client) else {
            return;
        };
        let Ok(models) = public.public_models().await else {
            return;
        };
        let by_id: std::collections::BTreeMap<String, &crate::openrouter::Model> = models
            .iter()
            .filter_map(|model| Some((normalized(model.id.strip_prefix(prefix)?), model)))
            .collect();
        for row in rows {
            let Some(model) = row["id"].as_str().and_then(|id| by_id.get(&normalized(id))) else {
                continue;
            };
            let mut used = false;
            let mut fill = |field: &str, value: Option<u64>| {
                if let Some(value) = value.filter(|value| *value > 0 && *value <= u64::from(u32::MAX)) {
                    if row[field].is_null() {
                        row[field] = json!(value);
                        used = true;
                    }
                }
            };
            fill("context", Some(model.context_length));
            fill("output", model.max_completion_tokens);
            // A plan route is a subscription: it never carries a per-token price.
            if row["pricing"].is_null() && self.provider != Provider::ChatGpt {
                let prices = price_fields(model);
                if !prices.is_empty() {
                    row["pricing"] = Value::Object(prices);
                    used = true;
                }
            }
            if used {
                row["metadata_source"] = json!(PUBLIC_SOURCE);
            }
        }
    }
}

/// Ids differ in punctuation between catalogs (`claude-opus-5.5` against `claude-opus-5-5`) and
/// the provider may append a snapshot date.
fn normalized(id: &str) -> String {
    let id = id.to_ascii_lowercase().replace('.', "-");
    match id.rsplit_once('-') {
        Some((head, tail)) if tail.len() == 8 && tail.bytes().all(|byte| byte.is_ascii_digit()) => {
            head.to_owned()
        }
        _ => id,
    }
}

/// Per-million rates of one public-catalog model; empty when any listed rate is not usable.
fn price_fields(model: &crate::openrouter::Model) -> serde_json::Map<String, Value> {
    let mut prices = serde_json::Map::new();
    for (field, rate) in [
        ("input_per_million", &model.prompt_price_per_token),
        ("output_per_million", &model.completion_price_per_token),
        ("cache_read_per_million", &model.cache_read_price_per_token),
        ("cache_write_per_million", &model.cache_write_price_per_token),
    ] {
        let Some(rate) = rate else {
            continue;
        };
        if let Some(value) = (rate * 1_000_000i64)
            .to_f64()
            .filter(|value| value.is_finite() && (*value > 0.0 || *rate == 0))
        {
            prices.insert(field.into(), json!(value));
        } else {
            prices.clear();
            break;
        }
    }
    prices
}

pub(crate) struct Offer {
    /// The limits or prices came from the public catalog, not the provider's own list.
    pub public: bool,
    pub id: String,
    pub context: Option<u32>,
    pub max_output: Option<u32>,
    pub price: Option<Price>,
}

fn parse(rows: &Value) -> Result<Vec<Offer>, Error> {
    let rows = rows
        .as_array()
        .filter(|rows| rows.len() <= MAX_LISTED_MODELS)
        .ok_or(Error::Protocol)?;
    let mut seen = BTreeSet::new();
    rows.iter()
        .map(|row| {
            let id = row["id"]
                .as_str()
                .filter(|id| !id.is_empty() && id.len() <= 128)
                .filter(|id| !id.chars().any(char::is_control))
                .ok_or(Error::Protocol)?;
            if !seen.insert(id) {
                return Err(Error::Protocol);
            }
            let limit = |field: &str| {
                if row[field].is_null() {
                    return Ok(None);
                }
                row[field]
                    .as_u64()
                    .and_then(|n| u32::try_from(n).ok())
                    .filter(|n| *n > 0)
                    .map(Some)
                    .ok_or(Error::Protocol)
            };
            Ok(Offer {
                public: row["metadata_source"].as_str() == Some(PUBLIC_SOURCE),
                id: id.into(),
                context: limit("context")?,
                max_output: limit("output")?,
                price: Price::from_model(row),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };

    /// One-shot local fake of the public catalog; it fails the test if a credential is sent.
    fn public_catalog(status: u16, body: &str) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/api/v1", listener.local_addr().unwrap());
        let body = body.to_owned();
        let task = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                socket.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
            assert!(
                request.starts_with("get /api/v1/models http/1.1\r\n"),
                "{request}"
            );
            assert!(!request.contains("authorization"), "{request}");
            let _ = write!(
                socket,
                "HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        });
        (base, task)
    }

    const CATALOG: &str = r#"{"data":[
        {"id":"anthropic/claude-opus-5.5","context_length":1000000,"top_provider":{"max_completion_tokens":128000},
         "pricing":{"prompt":"0.000004","completion":"0.00002","input_cache_read":"0.0000002"}},
        {"id":"openai/gpt-6.2-sol","context_length":1050000,"top_provider":{"max_completion_tokens":128000},
         "pricing":{"prompt":"0.000002","completion":"0.00001"}},
        {"id":"vendor/odd","context_length":"not a number"}]}"#;

    async fn fill(provider: Provider, status: u16, body: &str, rows: Vec<Value>) -> Vec<Value> {
        let (base, task) = public_catalog(status, body);
        let source = Source::new(provider, "https://example.com")
            .unwrap()
            .with_metadata_base(Some(&base));
        let mut rows = rows;
        source
            .fill_from_public_catalog(&mut rows, &Client::new().unwrap(), Duration::from_secs(2))
            .await;
        task.join().unwrap();
        rows
    }

    #[tokio::test]
    async fn the_public_catalog_fills_what_the_provider_list_lacks() {
        let rows = fill(
            Provider::Anthropic,
            200,
            CATALOG,
            vec![json!({"id": "claude-opus-5-5"}), json!({"id": "claude-unlisted"})],
        )
        .await;
        let offers = parse(&Value::Array(rows)).unwrap();
        assert_eq!(offers[0].context, Some(1_000_000));
        assert_eq!(offers[0].max_output, Some(128_000));
        assert!(offers[0].price.is_some() && offers[0].public);
        assert!(offers[1].context.is_none() && offers[1].price.is_none() && !offers[1].public);
    }

    #[cfg(feature = "upstream-openai")]
    #[tokio::test]
    async fn provider_values_win_over_the_public_catalog() {
        let rows = fill(
            Provider::OpenAi,
            200,
            CATALOG,
            vec![json!({"id": "gpt-6.2-sol", "context": 4096, "output": 64})],
        )
        .await;
        let offers = parse(&Value::Array(rows)).unwrap();
        assert_eq!(offers[0].context, Some(4096));
        assert_eq!(offers[0].max_output, Some(64));
        assert!(offers[0].price.is_some());
    }

    #[cfg(feature = "upstream-openai")]
    #[tokio::test]
    async fn a_plan_model_gets_its_output_limit_but_never_a_price() {
        let rows = fill(
            Provider::ChatGpt,
            200,
            CATALOG,
            vec![json!({"id": "gpt-6.2-sol", "context": 272000})],
        )
        .await;
        let offers = parse(&Value::Array(rows)).unwrap();
        assert_eq!(offers[0].context, Some(272_000));
        assert_eq!(offers[0].max_output, Some(128_000));
        assert!(offers[0].price.is_none());
    }

    #[cfg(feature = "upstream-openai")]
    #[tokio::test]
    async fn a_failed_public_fetch_changes_nothing() {
        let rows = fill(Provider::OpenAi, 500, "{}", vec![json!({"id": "gpt-6.2-sol"})]).await;
        let offers = parse(&Value::Array(rows)).unwrap();
        assert!(offers[0].context.is_none() && offers[0].price.is_none() && !offers[0].public);
    }

    #[test]
    fn ids_match_across_punctuation_and_snapshot_dates() {
        assert_eq!(normalized("claude-opus-5.5"), normalized("claude-opus-5-5"));
        assert_eq!(normalized("claude-opus-5-5-20261101"), "claude-opus-5-5");
    }
}
