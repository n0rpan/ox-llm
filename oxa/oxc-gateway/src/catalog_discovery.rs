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

pub struct Status {
    pub state: State,
    pub missing_price: usize,
    pub missing_limits: usize,
}

#[derive(Clone)]
pub struct Source {
    provider: Provider,
    base: String,
    filename: &'static str,
}
impl Source {
    pub fn new(provider: Provider, base: &str) -> Option<Self> {
        let filename = match provider {
            Provider::OpenRouter => "openrouter-catalog-cache.json",
            Provider::Anthropic => "anthropic-catalog-cache.json",
            #[cfg(feature = "upstream-openai")]
            Provider::OpenAi => "openai-catalog-cache.json",
            _ => return None,
        };
        Some(Self {
            provider,
            base: base.to_owned(),
            filename,
        })
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

    pub(crate) async fn fetch(
        &self,
        client: &Client,
        key: &Key,
        timeout: Duration,
        directory: &Path,
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
                let price = |rate: Option<bigdecimal::BigDecimal>| {
                    let rate = rate?;
                    let number = (&rate * 1_000_000i64).to_f64()?;
                    (number.is_finite() && (number > 0.0 || rate == 0)).then_some(number)
                };
                let prompt = price(model.prompt_price_per_token);
                let completion = price(model.completion_price_per_token);
                json!({
                    "id": model.id, "context": model.context_length,
                    "output": model.max_completion_tokens, "tools": model.tools,
                    "pricing": prompt.zip(completion).map(|(prompt, completion)| {
                        json!({"input_per_million": prompt, "output_per_million": completion})
                    })
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
                .map_err(|error| match error {
                    crate::openai::client::Error::Http {status,..} => Error::Http(status),
                    crate::openai::client::Error::Endpoint => Error::Endpoint,
                    crate::openai::client::Error::TooLarge => Error::TooLarge,
                    _ => Error::Transport,
                })?
                .into_iter()
                .map(|id| json!({"id": id}))
                .collect(),
            _ => return Err(Error::Request("provider")),
        };
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

pub(crate) struct Offer {
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
                id: id.into(),
                context: limit("context")?,
                max_output: limit("output")?,
                price: Price::from_model(row),
            })
        })
        .collect()
}
