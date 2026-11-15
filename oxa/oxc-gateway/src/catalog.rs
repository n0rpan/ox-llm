//! Cloud-only discovery. The caller supplies configuration and offline policy;
//! this module never reads credentials, probes hardware or touches an engine lease.
use serde_json::{json, Map, Value};
use url::Url;

const PROVIDERS: [&str; 3] = ["openrouter", "anthropic", "openai"];

#[derive(Clone, Debug)]
struct Model {
    alias: String,
    provider: String,
    name: String,
    context: Value,
    output: Value,
    cache_pricing: &'static str,
}

fn positive(value: &Value) -> Option<Value> {
    let number = value.as_f64()?;
    if !number.is_finite() || number <= 0.0 {
        return None;
    }
    let raw = value.as_number()?.to_string();
    if !raw.contains(['.', 'e', 'E']) {
        return Some(value.clone());
    }
    // Python publishes int(float), including values above u64::MAX. Keep
    // the wire type integer without saturating or rounding its token limit.
    format!("{:.0}", number.trunc())
        .parse::<serde_json::Number>()
        .ok()
        .map(Value::Number)
}

fn truth(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_none_or(|value| value != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

// Discovery needs only the classification, not settlement arithmetic. Invalid
// prices must not claim explicit cache rates merely because both keys exist.
fn price_fields(raw: &Value) -> Option<Vec<(bool, bool)>> {
    let normalized = || -> Option<Vec<(bool, bool)>> {
        let price = raw["pricing"].as_object()?;
        let levels = match price.get("tiers") {
            None => vec![&raw["pricing"]],
            Some(Value::Array(tiers)) if (1..=64).contains(&tiers.len()) => tiers.iter().collect(),
            _ => return None,
        };
        let mut previous = 0;
        let mut fields = Vec::new();
        for (index, level) in levels.iter().enumerate() {
            let mut merged = price.clone();
            merged.extend(level.as_object()?.clone());
            for (name, alias) in [
                ("input_per_million", "input"),
                ("output_per_million", "output"),
                ("cache_read_per_million", "cache_read_per_million"),
                ("cache_write_per_million", "cache_write_per_million"),
            ] {
                let value = merged.get(name).or_else(|| merged.get(alias));
                if name.starts_with("cache_") && value.is_none_or(Value::is_null) {
                    continue;
                }
                let number = value?.as_f64()?;
                if !(0.0..=1e12).contains(&number) {
                    return None;
                }
            }
            fields.push((
                merged
                    .get("cache_read_per_million")
                    .is_some_and(|v| !v.is_null()),
                merged
                    .get("cache_write_per_million")
                    .is_some_and(|v| !v.is_null()),
            ));
            if price.contains_key("tiers") {
                let bound = &level["max_prompt_tokens"];
                if index == levels.len() - 1 {
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
            }
        }
        Some(fields)
    };
    let fields = normalized();
    if fields.is_some() || raw["pricing"].get("tiers").is_some() {
        fields
    } else {
        (raw["free"] == true).then(|| vec![(false, false)])
    }
}
fn cache_pricing(provider: &str, raw: &Value, current: Option<&Value>) -> &'static str {
    let current = current
        .filter(|p| p.get("tiers").is_none())
        .and_then(|p| price_fields(&json!({"pricing":p})))
        .and_then(|p| p.first().copied());
    let fields = price_fields(raw)
        .or_else(|| (raw["pricing"].is_null() && current.is_some()).then(|| vec![(false, false)]));
    let overlay = current.unwrap_or((false, false));
    if fields.is_some_and(|fields| {
        fields
            .iter()
            .all(|(read, write)| (*read || overlay.0) && (*write || overlay.1))
    }) {
        "explicit"
    } else if provider == "anthropic" {
        "provider_default"
    } else {
        "conservative"
    }
}

impl Model {
    fn parse(alias: &str, raw: &Value, prices: &Value) -> Option<Self> {
        if !(1..=128).contains(&alias.len())
            || !alias.as_bytes()[0].is_ascii_alphanumeric()
            || !alias
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._:/-".contains(&b))
        {
            return None;
        }
        let derived = match raw.get("kind").map_or(Some("remote"), Value::as_str)? {
            "remote" => None,
            "remote/openrouter" => Some("openrouter"),
            "remote/anthropic" => Some("anthropic"),
            "remote/openai" => Some("openai"),
            _ => return None,
        };
        let provider = match raw.get("provider").filter(|value| truth(value)) {
            Some(value) => value.as_str(),
            None => derived,
        }?;
        if !PROVIDERS.contains(&provider) {
            return None;
        }
        let base = match raw.get("base_url") {
            None if provider == "openai" => "https://api.openai.com/v1",
            Some(Value::String(base)) => base.as_str(),
            _ => return None,
        };
        let url = Url::parse(base).ok()?;
        if !["http", "https"].contains(&url.scheme())
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some_and(|s| !s.is_empty())
            || url.fragment().is_some_and(|s| !s.is_empty())
        {
            return None;
        }
        let upstream = raw
            .get("upstream_model")
            .filter(|value| truth(value))
            .or_else(|| raw.get("upstream"))?
            .as_str()?;
        if upstream.trim().is_empty() {
            return None;
        }
        let context = positive(&raw["context_window"])?;
        let output = positive(&raw["max_output"]);
        if provider == "openai" && output.is_none() {
            return None;
        }
        Some(Self {
            alias: alias.into(),
            provider: provider.into(),
            name: raw["name"].as_str().unwrap_or(alias).into(),
            context,
            output: output.unwrap_or(Value::Null),
            cache_pricing: cache_pricing(
                provider,
                raw,
                (provider == "openrouter")
                    .then(|| prices.get(upstream.trim()))
                    .flatten(),
            ),
        })
    }
    fn capabilities(&self) -> Value {
        json!({"cache_pricing":self.cache_pricing,"remote":true,"remote_provider":self.provider,"echo":false,
            "cache_hints":false,"cancel":self.provider!="openai","prefill":false,
            "preserve_thinking":false,"context_window":self.context})
    }
    fn row(&self) -> Value {
        json!({"id":self.alias,"object":"model","created":0,"owned_by":self.provider,
            "name":self.name,"remote":true,"provider":self.provider,
            "context_length":self.context,"context_window":self.context,
            "max_output_tokens":self.output,"capabilities":self.capabilities()})
    }
    fn chooser_row(&self) -> Value {
        json!({"id":self.alias,"key":"cloud","name":self.name,"engine":"remote",
            "engine_status":"remote","qualification":null,"memory_gib":null,
            "battery_start_percent":null,"prefix":"off","selected":false,"default":false,
            "state":"unloaded","remote":true,"provider":self.provider,
            "context_window":self.context,"max_output":self.output,
            "capabilities":self.capabilities()})
    }
}

/// Metadata for a cloud-only controller, selected before any local engine setup.
/// HTTP authentication, forwarding, secrets and accounting belong to later slices.
#[derive(Clone, Debug)]
pub struct Catalog {
    models: Vec<Model>,
    selected: Result<Value, &'static str>,
}
impl Catalog {
    /// Returns `None` for a local-engine configuration. Cloud is off by default.
    /// `prices` is a trusted snapshot of validated per-million public rates,
    /// keyed by upstream ID. The caller owns bounded, safe cache-file loading.
    pub fn from_config(config: &Value, offline: bool, prices: &Value) -> Option<Self> {
        if config["cloud_only"] != true {
            return None;
        }
        let mut models = Vec::new();
        if config["cloud"]["enabled"] == true && !offline {
            if let Some(entries) = config["cloud"]["models"].as_object() {
                models.extend(
                    entries
                        .iter()
                        .filter_map(|(alias, raw)| Model::parse(alias, raw, prices)),
                );
            }
        }
        models.sort_by(|a, b| a.alias.cmp(&b.alias));
        let base = config
            .get("model_alias")
            .cloned()
            .unwrap_or_else(|| json!("qwen-local"));
        let selected = config
            .get("selected_model")
            .cloned()
            .unwrap_or_else(|| base.clone());
        let profiles = config["profiles"].as_object();
        let selected = if selected == base
            || selected
                .as_str()
                .is_some_and(|alias| profiles.is_some_and(|p| p.contains_key(alias)))
        {
            Ok(selected)
        } else if profiles.is_none_or(|profiles| profiles.is_empty()) {
            Ok(base)
        } else {
            Err("Unknown local model. Choose one listed in /v1/models.")
        };
        Some(Self { models, selected })
    }
    /// The public `/v1/models` body, without credential paths or upstream URLs.
    pub fn models(&self) -> Value {
        json!({"object":"list","data":self.models.iter().map(Model::row).collect::<Vec<_>>()})
    }
    pub fn model(&self, alias: &str) -> Option<Value> {
        self.models
            .iter()
            .find(|model| model.alias == alias)
            .map(Model::row)
    }
    /// The cloud-only `/_local/models` chooser body. Stored chooser selection is
    /// descriptive metadata; it does not resolve or qualify a local profile.
    pub fn chooser(&self) -> Result<Value, &'static str> {
        let selected = self.selected.as_ref().map_err(|message| *message)?;
        Ok(
            json!({"object":"list","data":self.models.iter().map(Model::chooser_row).collect::<Vec<_>>(),
            "default_model":"ornith-35b-a3b-local","selected_model":selected}),
        )
    }
    /// Merge this fragment into the controller's status response after HTTP gates.
    pub fn status_fragment(&self) -> Value {
        let limits: Map<_,_> = self.models.iter().map(|model|(model.alias.clone(),
            json!({"mode":null,"modes":[],"cap":model.context,"context_window":model.context,
                "max_output":model.output,"capabilities":model.capabilities()}))).collect();
        json!({"engine_mode":"none","model_limits":limits})
    }
    pub fn health(&self) -> Value {
        json!({"status":"ok","sleeping":true})
    }
    pub fn startup_line(&self) -> String {
        format!(
            "oxc: cloud-only mode, no local engine, {} remote models",
            self.models.len()
        )
    }
}
