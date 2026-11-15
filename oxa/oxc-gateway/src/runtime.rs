//! Anthropic-only in-process admission and settlement. Provider I/O and the
//! async/synchronous bridge are injected by the caller; no listener or engine.
use crate::{
    accounting::{Hold, Model, Outcome, Usage},
    catalog::Catalog,
    ledger::{Context, Entry, Ledger},
    policy::{self, Refusal},
    pricing::Price,
    redaction,
    secrets::{self, Key, KeyFile},
};
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::{Arc, OnceLock},
};

#[derive(Clone, Copy)]
pub struct Options {
    pub offline: bool,
    pub ledger_enabled: bool,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            offline: false,
            ledger_enabled: true,
        }
    }
}
pub struct Discovery {
    pub models: Value,
    pub status: Value,
}
pub struct Runtime {
    config: Value,
    catalog: Catalog,
    key_file: Result<KeyFile, secrets::Error>,
    ledger_dir: PathBuf,
    ledger: OnceLock<Result<Ledger, Refusal>>,
    options: Options,
}
/// Keep this guard alive across provider work. Dropping a forwarded guard
/// records a conservative failed charge, including when an async future aborts.
pub struct Call {
    model: Value,
    payload: Value,
    key: Arc<Key>,
    usage: Usage,
    hold: Option<Hold>,
    entry: Option<Entry>,
    forwarded: bool,
}
fn invalid(message: &'static str, param: &'static str) -> Refusal {
    Refusal {
        status: 400,
        message,
        code: "invalid_value",
        param,
    }
}
fn credentials(error: secrets::Error) -> Refusal {
    // All reasons are static; never include an IO error, path or key.
    let message = match error {
        secrets::Error::Unavailable => "Cloud credentials are not configured.",
        secrets::Error::InsideRepository => "Cloud credentials are not configured: the configured path is inside a source repository.",
        secrets::Error::SymbolicLink => "Cloud credentials are not configured: the configured path is a symbolic link.",
        secrets::Error::Unreadable => "Cloud credentials are not configured: the file cannot be read.",
        secrets::Error::NotRegular => "Cloud credentials are not configured: the configured path is not a regular file.",
        secrets::Error::WrongOwner => "Cloud credentials are not configured: the file is owned by another user.",
        secrets::Error::Permissions => "Cloud credentials are not configured: the file is not mode 0600 (group/other permissions must be off).",
        secrets::Error::TooLarge => "Cloud credentials are not configured: the file is larger than 65536 bytes.",
    };
    Refusal {
        message,
        ..Refusal::credentials()
    }
}
impl Runtime {
    /// Paths are resolved by trusted user configuration, never request data.
    /// Construction reads no credential bytes, opens no ledger and contacts no
    /// provider. Retain one Runtime and share it across all sessions/requests.
    pub fn new(
        config: Value,
        key_path: PathBuf,
        ledger_dir: PathBuf,
        options: Options,
    ) -> Result<Self, Refusal> {
        let catalog = Catalog::from_config(&config, false, &json!({})).ok_or(Refusal {
            status: 503,
            message: "The in-process gateway requires cloud-only mode.",
            code: "cloud_only_required",
            param: "model",
        })?;
        Ok(Self {
            config,
            catalog,
            key_file: KeyFile::prepare(&key_path),
            ledger_dir,
            ledger: OnceLock::new(),
            options,
        })
    }
    pub fn discovery(&self) -> Discovery {
        let catalog = Catalog::from_config(&self.config, self.options.offline, &json!({}))
            .expect("validated cloud-only config");
        let mut models = catalog.models();
        models["data"]
            .as_array_mut()
            .expect("catalog list")
            .retain(|model| model["provider"] == "anthropic");
        let mut status = catalog.status_fragment();
        status["model_limits"]
            .as_object_mut()
            .expect("catalog limits")
            .retain(|_, limit| limit["capabilities"]["remote_provider"] == "anthropic");
        status["loaded"] = json!(false);
        status["sleeping"] = json!(true);
        Discovery { models, status }
    }
    /// Call after the client has validated its pure provider translation, and
    /// before any network access. The payload's bounded max_tokens is the
    /// authoritative limit the client must use when translating/sending.
    pub fn begin(&self, mut payload: Value, context: Context<'_>) -> Result<Call, Refusal> {
        if self.config["cloud"]["enabled"] != true {
            return Err(Refusal {
                status: 403,
                message: "Cloud models are disabled.",
                code: "cloud_disabled",
                param: "model",
            });
        }
        let alias = payload["model"]
            .as_str()
            .ok_or_else(|| invalid("Choose a cloud model listed in /v1/models.", "model"))?
            .to_owned();
        let public = self.catalog.model(&alias).ok_or_else(|| {
            invalid(
                "Unknown cloud model. Choose one listed in /v1/models.",
                "model",
            )
        })?;
        if public["provider"] != "anthropic" {
            return Err(Refusal {
                status: 503,
                message: "This in-process gateway currently supports Anthropic models.",
                code: "cloud_provider_unavailable",
                param: "model",
            });
        }
        let raw = &self.config["cloud"]["models"][&alias];
        let price = Price::from_model(raw);
        policy::before_credentials(&payload, false, self.options.offline, price.is_some())?;
        let key = self
            .key_file
            .as_ref()
            .map_err(|error| credentials(*error))?
            .read("anthropic")
            .map_err(credentials)?;
        let maximum = public["max_output_tokens"]
            .as_u64()
            .filter(|n| *n > 0)
            .unwrap_or(4096);
        let mut output = maximum;
        for field in ["max_tokens", "max_completion_tokens", "n_predict"] {
            if let Some(value) = payload.get(field) {
                // Preserve arbitrary-size positive integers while clamping to
                // the catalog maximum. Booleans/floats/strings are refusals.
                let text = value.as_number().map(ToString::to_string);
                let digits = text.as_ref().filter(|value| {
                    !value.is_empty()
                        && value.bytes().all(|byte| byte.is_ascii_digit())
                        && value.bytes().any(|byte| byte != b'0')
                });
                if digits.is_none() {
                    return Err(invalid(
                        "The output token limit must be a positive integer.",
                        field,
                    ));
                }
                output = value.as_u64().unwrap_or(u64::MAX).min(maximum);
                break;
            }
        }
        payload["max_tokens"] = json!(output);
        let upstream = raw
            .get("upstream_model")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .or_else(|| raw["upstream"].as_str())
            .expect("validated catalog upstream")
            .trim();
        let model = Model {
            provider: "anthropic".into(),
            upstream: upstream.into(),
            context: public["context_window"]
                .as_u64()
                .ok_or_else(|| invalid("Invalid cloud context window.", "model"))?,
            max_output: maximum,
            price: price.expect("policy validated price"),
        };
        let cap = &self.config["cloud"]["daily_budget_usd"];
        policy::daily_budget(cap, self.options.ledger_enabled, Some(0.0), 0.0, 0.0)?;
        let ledger = self
            .ledger
            .get_or_init(|| Ledger::open(&self.ledger_dir).map_err(|_| Refusal::ledger()))
            .as_ref()
            .map_err(Clone::clone)?;
        let daily = ledger.today().map_err(|_| Refusal::ledger())?;
        let mut entry = daily
            .entry(&alias, &payload, context)
            .map_err(|_| Refusal::ledger())?;
        let hold = daily.budget().reserve(cap, true, &model, output)?;
        entry.admitted();
        let item = json!({"id":alias,"provider":"anthropic","base_url":raw["base_url"],
            "upstream_model":upstream,"context_window":model.context,"max_output":output,
            "pricing":model.price.normalized()});
        Ok(Call {
            model: item,
            payload,
            key: Arc::new(key),
            usage: Usage::default(),
            hold: Some(hold),
            entry: Some(entry),
            forwarded: false,
        })
    }
}
impl Call {
    pub fn model(&self) -> &Value {
        &self.model
    }
    pub fn payload(&self) -> &Value {
        &self.payload
    }
    pub fn key(&self) -> &Key {
        &self.key
    }
    /// A fresh per-request key shared with provider I/O and redactors. This
    /// clones only the handle, so a Sending/Chunk callback can mutate Call
    /// while the client retains credentials, without copying secret bytes.
    pub fn key_handle(&self) -> Arc<Key> {
        Arc::clone(&self.key)
    }
    pub fn mark_forwarded(&mut self) {
        self.forwarded = true;
        self.hold.as_mut().expect("unsettled call").mark_forwarded();
    }
    /// Observe a canonical normalized event. Provider strings are redacted
    /// before they reach accounting. Streaming content redaction belongs to the
    /// native client (one independent Stream per translator field).
    pub fn observe(&mut self, value: &Value) {
        let mut safe = value.clone();
        redaction::value(&mut safe, &self.key);
        self.usage.observe(&safe);
        self.entry.as_mut().expect("unsettled call").observe();
    }
    pub fn finish(
        mut self,
        outcome: Outcome,
        status: u16,
        error: Option<&'static str>,
    ) -> Result<Value, Refusal> {
        self.settle(outcome, status, error)
    }
    fn settle(
        &mut self,
        outcome: Outcome,
        status: u16,
        error: Option<&'static str>,
    ) -> Result<Value, Refusal> {
        let hold = self.hold.take().expect("unsettled call");
        let entry = self.entry.take().expect("unsettled call");
        let key = &self.key;
        let mut saved = Value::Null;
        // A translation failure before any paid bytes must cost zero. Keep the
        // hold through this append so a failed write still closes admission.
        let zero = Usage {
            prompt: Some(0),
            output: Some(0),
            ..Usage::default()
        };
        let usage = if self.forwarded { &self.usage } else { &zero };
        let outcome = if self.forwarded {
            outcome
        } else {
            Outcome::Complete
        };
        hold.finish(usage, outcome, |fields| {
            let mut fields = fields.clone();
            if !self.forwarded {
                fields["settled"] = json!("not_forwarded");
            }
            saved = entry.append(&fields, status, error, Some(key))?;
            Ok(())
        })?;
        Ok(saved)
    }
}
impl Drop for Call {
    fn drop(&mut self) {
        if self.hold.is_some() {
            let _ = self.settle(Outcome::Failed, 503, Some("cloud_interrupted"));
        }
    }
}
