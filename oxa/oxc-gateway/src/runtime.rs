//! Admission and consuming settlement, with one process-shared private ledger.
use crate::{
    accounting::{self, Hold, Outcome, Usage},
    catalog::{self, Access, Catalog, Model, ModelInfo, Provider},
    ledger::{Context, Daily, Entry, Ledger},
    policy::{self, Refusal},
    pricing::Price,
    secrets::{Key, KeyFile},
};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, OnceLock},
};

fn supported(provider: Provider) -> bool {
    matches!(provider, Provider::Anthropic | Provider::OpenRouter)
}

pub struct Options {
    pub access: Access,
    pub daily_budget_usd: Option<f64>,
    pub ledger_enabled: bool,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            access: Access::Disabled,
            daily_budget_usd: None,
            ledger_enabled: true,
        }
    }
}
pub struct Runtime {
    catalog: Catalog,
    prices: BTreeMap<String, Price>,
    key_file: Result<KeyFile, crate::secrets::Error>,
    ledger_dir: PathBuf,
    ledger: OnceLock<Result<Ledger, Refusal>>,
    cap: Value,
    ledger_enabled: bool,
}
/// An admitted call owns its reservation until a durable row is committed.
pub struct Call<'a> {
    model: &'a Model,
    output: u64,
    key: Arc<Key>,
    usage: Usage,
    hold: Option<Hold>,
    entry: Option<Entry>,
    forwarded: bool,
}
fn invalid(param: &'static str) -> Refusal {
    Refusal {
        status: 400,
        message: "Invalid cloud request field.",
        code: "invalid_value",
        param,
    }
}
fn catalog_error(error: catalog::Error) -> Refusal {
    let (status, code, message) = match error {
        catalog::Error::Disabled => (403, "cloud_disabled", "Cloud models are disabled."),
        catalog::Error::Offline => (403, "offline", "Cloud models are unavailable in offline mode."),
        catalog::Error::UnknownModel => (400, "unknown_model", "Choose a configured cloud model."),
        _ => (400, "invalid_value", "Invalid cloud catalog."),
    };
    Refusal {
        status,
        code,
        message,
        param: "model",
    }
}
impl Runtime {
    /// Trusted, typed configuration. Preparing a path reads no key bytes;
    /// construction/discovery open no ledger and start no provider/runtime.
    pub fn new(
        models: impl IntoIterator<Item = (Model, Option<Price>)>,
        key_path: PathBuf,
        ledger_dir: PathBuf,
        options: Options,
    ) -> Result<Self, Refusal> {
        let mut prices = BTreeMap::new();
        let catalog = Catalog::new(
            options.access,
            models.into_iter().map(|(model, price)| {
                if let Some(price) = price {
                    prices.insert(model.info().id.to_owned(), price);
                }
                model
            }),
        )
        .map_err(catalog_error)?;
        Ok(Self {
            catalog,
            prices,
            key_file: KeyFile::prepare(&key_path),
            ledger_dir,
            ledger: OnceLock::new(),
            cap: json!(options.daily_budget_usd),
            ledger_enabled: options.ledger_enabled,
        })
    }
    pub fn discovery(&self) -> impl Iterator<Item = ModelInfo<'_>> {
        self.catalog.models().filter(|model| supported(model.provider))
    }
    fn audit(&self, model: &str, context: Context<'_>) -> Result<(Daily, Entry), Refusal> {
        if !self.ledger_enabled {
            return Err(Refusal::ledger());
        }
        let ledger = self
            .ledger
            .get_or_init(|| Ledger::open(&self.ledger_dir).map_err(|_| Refusal::ledger()))
            .as_ref()
            .map_err(Clone::clone)?;
        let daily = ledger.today().map_err(|_| Refusal::ledger())?;
        let entry = daily.entry(model, context).map_err(|_| Refusal::ledger())?;
        Ok((daily, entry))
    }
    pub fn begin(&self, payload: &Value, context: Context<'_>) -> Result<Call<'_>, Refusal> {
        let alias = payload["model"].as_str().ok_or_else(|| invalid("model"))?;
        if matches!(self.catalog.resolve(Some(alias)), Err(catalog::Error::Disabled)) {
            return Err(catalog_error(catalog::Error::Disabled));
        }
        let known = self
            .catalog
            .configured(alias)
            .filter(|model| supported(model.info().provider));
        let mut audit = known.map(|_| self.audit(alias, context));
        let mut key = None;
        let result = (|| {
            let model = self.catalog.resolve(Some(alias)).map_err(catalog_error)?;
            if !supported(model.info().provider) {
                return Err(Refusal {
                    status: 503,
                    code: "cloud_provider_unavailable",
                    param: "model",
                    message: "This gateway currently supports Anthropic and OpenRouter models.",
                });
            }
            let price = self.prices.get(alias);
            policy::before_credentials(payload, false, false, price.is_some())?;
            let mut output = u64::from(model.info().max_output);
            for field in ["max_tokens", "max_completion_tokens", "n_predict"] {
                if let Some(value) = payload.get(field) {
                    output = value
                        .as_u64()
                        .filter(|value| *value > 0)
                        .ok_or_else(|| invalid(field))?
                        .min(output);
                    break;
                }
            }
            policy::daily_budget(&self.cap, self.ledger_enabled, Some(0.0), 0.0, 0.0)?;
            key = Some(
                self.key_file
                    .as_ref()
                    .map_err(|_| Refusal::credentials())?
                    .read(model.info().provider.as_str())
                    .map_err(|_| Refusal::credentials())?,
            );
            let (daily, _) = audit
                .as_ref()
                .ok_or_else(Refusal::ledger)?
                .as_ref()
                .map_err(Clone::clone)?;
            let billing = accounting::Model {
                provider: model.info().provider.as_str().into(),
                upstream: model.upstream().into(),
                context: u64::from(model.info().context_window),
                max_output: u64::from(model.info().max_output),
                price: price.expect("policy validated price").clone(),
            };
            let hold = daily.budget().reserve(&self.cap, true, &billing, output)?;
            let (_, mut entry) = audit.take().expect("known model")?;
            entry.admitted();
            Ok(Call {
                model,
                output,
                key: Arc::new(key.take().expect("fresh key")),
                usage: Usage::default(),
                hold: Some(hold),
                entry: Some(entry),
                forwarded: false,
            })
        })();
        if let Err(error) = &result {
            if let (Some(model), Some(Ok((daily, entry)))) = (known, audit) {
                let fields = json!({"remote":true,"remote_provider":model.info().provider.as_str(),"remote_model":model.upstream(),
                    "cost_usd":0.0,"settled":"not_forwarded"});
                let _ = daily.budget().write_unpaid(|| {
                    entry
                        .append(&fields, error.status, Some(error.code), key.as_ref())
                        .map(|_| ())
                });
            }
        }
        result
    }
    pub(crate) fn refused(&self, alias: &str, context: Context<'_>, error: &Refusal) {
        if matches!(self.catalog.resolve(Some(alias)), Err(catalog::Error::Disabled)) {
            return;
        }
        if let Some(model) = self
            .catalog
            .configured(alias)
            .filter(|model| supported(model.info().provider))
        {
            if let Ok((daily, entry)) = self.audit(alias, context) {
                let fields = json!({"remote":true,"remote_provider":model.info().provider.as_str(),"remote_model":model.upstream(),
                    "cost_usd":0.0,"settled":"not_forwarded"});
                let _ = daily.budget().write_unpaid(|| {
                    entry
                        .append(&fields, error.status, Some(error.code), None)
                        .map(|_| ())
                });
            }
        }
    }
}
impl<'a> Call<'a> {
    pub fn model(&self) -> &'a Model {
        self.model
    }
    pub fn max_output(&self) -> u64 {
        self.output
    }
    pub fn key_handle(&self) -> Arc<Key> {
        Arc::clone(&self.key)
    }
    pub fn mark_forwarded(&mut self) {
        self.forwarded = true;
        self.hold.as_mut().expect("unsettled call").mark_forwarded();
    }
    pub fn observe(&mut self, chunk: &Value) {
        self.usage.observe(chunk);
        self.entry.as_mut().expect("unsettled call").observe();
    }
    pub fn metered(&mut self, usage: crate::anthropic_stream::Usage) {
        self.usage.prompt =
            Some(usage.input_tokens + usage.cache_read_input_tokens + usage.cache_creation_input_tokens);
        self.usage.output = Some(usage.output_tokens);
        self.usage.cache_read = Some(usage.cache_read_input_tokens);
        self.usage.cache_write = Some(usage.cache_creation_input_tokens);
    }
    /// Private terminal metering only, never caller-supplied request fields.
    pub(crate) fn metered_cost(&mut self, cost: Option<f64>) {
        self.usage.reported_cost = cost.filter(|cost| cost.is_finite() && *cost >= 0.0);
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
        let mut saved = Value::Null;
        hold.finish(usage, outcome, |fields| {
            let mut fields = fields.clone();
            if !self.forwarded {
                fields["settled"] = json!("not_forwarded");
            }
            saved = entry.append(&fields, status, error, Some(&self.key))?;
            Ok(())
        })?;
        Ok(saved)
    }
}
impl Drop for Call<'_> {
    fn drop(&mut self) {
        if self.hold.is_some() {
            let _ = self.settle(Outcome::Failed, 503, Some("cloud_interrupted"));
        }
    }
}
