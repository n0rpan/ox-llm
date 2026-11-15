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

impl Model {
    fn parse(alias: &str, raw: &Value) -> Option<Self> {
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
        })
    }
    fn capabilities(&self) -> Value {
        json!({"remote":true,"remote_provider":self.provider,"echo":false,
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
    selected: Value,
}
impl Catalog {
    /// Returns `None` for a local-engine configuration. Cloud is off by default.
    pub fn from_config(config: &Value, offline: bool) -> Option<Self> {
        if config["cloud_only"] != true {
            return None;
        }
        let mut models = Vec::new();
        if config["cloud"]["enabled"] == true && !offline {
            if let Some(entries) = config["cloud"]["models"].as_object() {
                models.extend(
                    entries
                        .iter()
                        .filter_map(|(alias, raw)| Model::parse(alias, raw)),
                );
            }
        }
        models.sort_by(|a, b| a.alias.cmp(&b.alias));
        Some(Self {
            models,
            selected: config
                .get("selected_model")
                .or_else(|| config.get("model_alias"))
                .cloned()
                .unwrap_or_else(|| json!("qwen-local")),
        })
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
    /// The cloud-only `/_local/models` chooser body. Legacy chooser selection is
    /// descriptive metadata; it does not resolve or qualify a local profile.
    pub fn chooser(&self) -> Value {
        json!({"object":"list","data":self.models.iter().map(Model::chooser_row).collect::<Vec<_>>(),
            "default_model":"ornith-35b-a3b-local","selected_model":self.selected})
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
