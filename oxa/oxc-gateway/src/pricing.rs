//! Catalog pricing and full-window reservation bounds. Actual charges use
//! decimal arithmetic; reservation math retains a conservative
//! floating-point bound. Neither operation admits or forwards a request.
use bigdecimal::BigDecimal;
use num_traits::ToPrimitive;
use serde_json::{json, Map, Value};
use std::str::FromStr;

#[derive(Clone, Debug)]
pub struct Price(Value);
#[derive(Clone, Copy, Debug)]
pub enum Cache {
    Tokens { read: u64, write: u64 },
    Invalid,
}
fn rates(raw: &Value) -> Option<Value> {
    let mut out = Map::new();
    for (name, alias) in [
        ("input_per_million", "input"),
        ("output_per_million", "output"),
        ("cache_read_per_million", "cache_read_per_million"),
        ("cache_write_per_million", "cache_write_per_million"),
    ] {
        let value = raw.get(name).or_else(|| raw.get(alias));
        if value.is_none_or(Value::is_null) && name.starts_with("cache_") {
            continue;
        }
        let number = value?.as_f64()?;
        if !(0.0..=1e12).contains(&number) {
            return None;
        }
        out.insert(name.into(), json!(number));
    }
    Some(Value::Object(out))
}
fn cache_rates(price: &Value, provider: &str, upstream: &str) -> (f64, f64) {
    let base = price["input_per_million"]
        .as_f64()
        .expect("normalized input rate");
    let anthropic = provider == "anthropic" || upstream.starts_with("anthropic/");
    let write = price["cache_write_per_million"]
        .as_f64()
        .unwrap_or(base * if anthropic { 2.0 } else { 1.0 });
    let read = price["cache_read_per_million"]
        .as_f64()
        .unwrap_or(base * if provider == "anthropic" { 0.1 } else { 1.0 });
    (read, write)
}
fn decimal(number: f64) -> BigDecimal {
    BigDecimal::from_str(&number.to_string()).expect("validated finite rate")
}
impl Price {
    /// Parse the model's pricing/free fields. Unknown pricing is never free.
    pub fn from_model(raw: &Value) -> Option<Self> {
        let price = &raw["pricing"];
        if let Some(object) = price.as_object() {
            if let Some(tiers) = price.get("tiers") {
                let tiers = tiers.as_array()?;
                if tiers.is_empty() || tiers.len() > 64 {
                    return None;
                }
                let mut result = Vec::new();
                let mut previous = 0;
                for (index, tier) in tiers.iter().enumerate() {
                    let mut merged = object.clone();
                    merged.extend(tier.as_object()?.clone());
                    let mut normalized = rates(&Value::Object(merged))?;
                    let bound = &tier["max_prompt_tokens"];
                    if index == tiers.len() - 1 {
                        if !bound.is_null() {
                            return None;
                        }
                    } else {
                        let bound = bound.as_u64()?;
                        if bound <= previous || bound > 100_000_000 {
                            return None;
                        }
                        previous = bound;
                    }
                    normalized["max_prompt_tokens"] = bound.clone();
                    result.push(normalized);
                }
                return Some(Self(json!({"tiers":result})));
            }
            if let Some(price) = rates(price) {
                return Some(Self(price));
            }
        }
        (raw["free"] == true).then(|| Self(json!({"input_per_million":0.0,"output_per_million":0.0})))
    }
    pub fn normalized(&self) -> &Value {
        &self.0
    }
    /// The live list publishes short-context rates. Keep configured tiers and
    /// use configured rates as floors when applying live pricing.
    #[cfg(feature = "anthropic-gateway")]
    pub(crate) fn overlay_discovered(mut self, live: &Self) -> Self {
        let overlay = |tier: &mut Value| {
            for (field, rate) in live.0.as_object().into_iter().flatten() {
                let Some(rate) = rate.as_f64() else {
                    continue;
                };
                let rate = tier[field]
                    .as_f64()
                    .map_or(rate, |configured| configured.max(rate));
                tier[field] = json!(rate);
            }
        };
        if let Some(tiers) = self.0.get_mut("tiers").and_then(Value::as_array_mut) {
            let mut tiers = tiers.iter_mut();
            let mut previous = tiers.next().expect("validated price has a tier");
            overlay(previous);
            for tier in tiers {
                overlay(tier);
                for (field, rate) in previous.as_object().into_iter().flatten() {
                    if field.ends_with("_per_million")
                        && tier[field]
                            .as_f64()
                            .is_none_or(|current| rate.as_f64().is_some_and(|rate| rate > current))
                    {
                        tier[field] = rate.clone();
                    }
                }
                previous = tier;
            }
        } else {
            overlay(&mut self.0);
        }
        self
    }
    pub fn cost(
        &self,
        provider: &str,
        upstream: &str,
        prompt: u64,
        output: u64,
        cache: Cache,
    ) -> Option<f64> {
        if prompt > 100_000_000 || output > 10_000_000 {
            return None;
        }
        let price = if let Some(tiers) = self.0["tiers"].as_array() {
            tiers.iter().find(|tier| {
                tier["max_prompt_tokens"].is_null()
                    || tier["max_prompt_tokens"].as_u64().is_some_and(|n| prompt <= n)
            })?
        } else {
            &self.0
        };
        let input = price["input_per_million"].as_f64()?;
        let out = price["output_per_million"].as_f64()?;
        let (read_rate, write_rate) = cache_rates(price, provider, upstream);
        let input_cost = match cache {
            Cache::Tokens { read, write } if read.saturating_add(write) <= prompt => {
                BigDecimal::from(prompt - read - write) * decimal(input)
                    + BigDecimal::from(read) * decimal(read_rate)
                    + BigDecimal::from(write) * decimal(write_rate)
            }
            _ => BigDecimal::from(prompt) * decimal(input.max(read_rate).max(write_rate)),
        };
        ((input_cost + BigDecimal::from(output) * decimal(out)) / BigDecimal::from(1_000_000)).to_f64()
    }
    /// Bound input by the entire catalog window, across every pricing tier and
    /// cache-write rate. OpenRouter adds 25% routing headroom.
    pub fn reservation(&self, provider: &str, upstream: &str, context: u64, output: u64) -> Option<f64> {
        if context > 100_000_000 || output > 10_000_000 {
            return None;
        }
        let prices = self.0["tiers"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(std::slice::from_ref(&self.0));
        let mut input = 0.0_f64;
        let mut out = 0.0_f64;
        for price in prices {
            let (read, write) = cache_rates(price, provider, upstream);
            input = input
                .max(price["input_per_million"].as_f64()?)
                .max(read)
                .max(write);
            out = out.max(price["output_per_million"].as_f64()?);
        }
        let cost = (context as f64 * input + output as f64 * out) / 1_000_000.0;
        Some(cost * if provider == "openrouter" { 1.25 } else { 1.0 })
    }
}
