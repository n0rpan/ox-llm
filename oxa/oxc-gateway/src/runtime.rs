//! Admission and consuming settlement, with one process-shared private ledger.
use crate::{
    accounting::{self, Hold, Outcome, Usage},
    catalog::{self, Access, Catalog, Model, ModelInfo, Provider},
    ledger::{Context, Daily, Entry, Ledger},
    policy::{self, Refusal},
    secrets::{Key, KeyFile},
};
mod auth;
pub use crate::billing::Billing;
pub use crate::history::HistorySeal;
use auth::Credential;
pub use auth::{OpenAiAuthHandle, Subscription};
use serde_json::{json, Value};
#[cfg(feature = "upstream-openai")]
use std::path::Path;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, OnceLock},
};

pub struct Options {
    pub access: Access,
    pub daily_budget_usd: Option<f64>,
    pub ledger_enabled: bool,
    pub subscription_limits: Option<crate::billing::SubscriptionLimits>,
    pub subscription: Option<Subscription>,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            access: Access::Disabled,
            daily_budget_usd: None,
            ledger_enabled: true,
            subscription_limits: None,
            subscription: None,
        }
    }
}
pub struct Runtime {
    diagnostics: Option<crate::diagnostics::Diagnostics>,
    catalog: Catalog,
    billings: BTreeMap<String, Billing>,
    go_models: BTreeMap<String, crate::opencode::Model>,
    /// Bounded stale-aware Go catalog metadata, beside the ledger in the same
    /// private agent directory. Never a key, header or transcript.
    go_cache: crate::opencode::cache::Cache,
    key_file: Result<KeyFile, crate::secrets::Error>,
    ledger_dir: PathBuf,
    ledger: OnceLock<Result<Ledger, Refusal>>,
    cap: Value,
    ledger_enabled: bool,
    subscription_limits: Option<crate::billing::SubscriptionLimits>,
    #[cfg(feature = "upstream-openai")]
    subscription: Option<Subscription>,
    #[cfg(feature = "upstream-openai")]
    token_path: PathBuf,
}
/// An admitted call owns its reservation until a durable row is committed.
pub struct Call<'a> {
    model: &'a Model,
    output: u64,
    credential: Credential,
    #[cfg(feature = "upstream-openai")]
    subscription: Option<&'a Subscription>,
    #[cfg(feature = "upstream-openai")]
    token_path: &'a Path,
    usage: Usage,
    hold: Option<Hold>,
    entry: Option<Entry>,
    forwarded: bool,
    go_usage: Option<Value>,
    diagnostics: Option<crate::diagnostics::Diagnostics>,
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
fn supported(provider: Provider) -> bool {
    matches!(
        provider,
        Provider::Anthropic | Provider::OpenRouter | Provider::OpenCode
    ) || (cfg!(feature = "upstream-openai") && matches!(provider, Provider::OpenAi | Provider::ChatGpt))
}
fn invalid_billing() -> Refusal {
    Refusal {
        status: 400,
        code: "invalid_billing",
        param: "model",
        message: "The provider and billing class do not match.",
    }
}
fn unpaid(model: &Model, billing: Option<&Billing>) -> Value {
    let mut fields = json!({"remote":true,"remote_provider":model.info().provider.as_str(),"remote_model":model.upstream(),
        "cost_usd":0.0,"settled":"not_forwarded"});
    if matches!(billing, Some(Billing::Subscription)) {
        fields["billing"] = json!("subscription");
        fields["subscription_requests"] = json!(0);
        fields["subscription_tokens"] = json!(0);
    } else {
        fields["billing"] = json!("metered");
    }
    fields
}
impl Runtime {
    /// Install only for a trusted terminal caller, before discovery/requests.
    pub fn with_diagnostics(mut self, diagnostics: crate::diagnostics::Diagnostics) -> Self {
        self.diagnostics = Some(diagnostics);
        self
    }
    pub(crate) fn diagnostics(&self) -> Option<&crate::diagnostics::Diagnostics> {
        self.diagnostics.as_ref()
    }
    fn credential_error(&self) -> Refusal {
        if let Some(diagnostics) = &self.diagnostics {
            diagnostics.local();
        }
        Refusal::credentials()
    }
    /// Trusted, typed configuration. Preparing a path reads no key bytes;
    /// construction/discovery open no ledger and start no provider/runtime.
    pub fn new(
        models: impl IntoIterator<Item = (Model, Option<Billing>)>,
        key_path: PathBuf,
        ledger_dir: PathBuf,
        options: Options,
    ) -> Result<Self, Refusal> {
        if let Some(config) = &options.subscription {
            if config.client_id.is_empty()
                || config.client_id.len() > 128
                || config.client_id.chars().any(char::is_control)
                || config.timeout.is_zero()
                || std::time::Instant::now().checked_add(config.timeout).is_none()
            {
                return Err(invalid("subscription"));
            }
            Model::new(
                "subscription-refresh",
                Provider::OpenAi,
                "token",
                &config.refresh_endpoint,
                1,
                1,
            )
            .map_err(|_| invalid("subscription"))?;
        }
        let mut billings = BTreeMap::new();
        let mut routes = Vec::new();
        for (model, billing) in models {
            if matches!(model.info().provider, Provider::ChatGpt | Provider::OpenCode)
                != matches!(billing, Some(Billing::Subscription))
            {
                return Err(invalid_billing());
            }
            if let Some(billing) = billing {
                billings.insert(model.info().id.to_owned(), billing);
            }
            routes.push(model);
        }
        let catalog = Catalog::new(options.access, routes).map_err(catalog_error)?;
        let go_cache =
            crate::opencode::cache::Cache::new(ledger_dir.parent().unwrap_or(ledger_dir.as_path()));
        #[cfg(feature = "upstream-openai")]
        let token_path = key_path
            .parent()
            .ok_or_else(Refusal::credentials)?
            .join("chatgpt-auth.json");
        Ok(Self {
            diagnostics: None,
            catalog,
            billings,
            go_models: BTreeMap::new(),
            go_cache,
            key_file: KeyFile::prepare(&key_path),
            ledger_dir,
            ledger: OnceLock::new(),
            cap: json!(options.daily_budget_usd),
            ledger_enabled: options.ledger_enabled,
            subscription_limits: options.subscription_limits,
            #[cfg(feature = "upstream-openai")]
            subscription: options.subscription,
            #[cfg(feature = "upstream-openai")]
            token_path,
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
    pub(crate) fn go_model(&self, alias: &str) -> Option<&crate::opencode::Model> {
        self.go_models.get(alias)
    }
    /// Consume the bounded stale-aware cache before any network call, then fall
    /// back to live discovery exactly as before. A cache hit is re-validated by
    /// the same strict parser as the wire, so it can never widen routing,
    /// limits or admission.
    pub(crate) async fn discover_go(
        &mut self,
        client: &reqwest::Client,
        base: &str,
        timeout: std::time::Duration,
    ) -> Result<crate::opencode::cache::State, crate::opencode::client::Error> {
        use crate::opencode::{cache::State, client::Error};
        if let Some(diagnostics) = &self.diagnostics {
            diagnostics.clear();
        }
        if let Err(catalog::Error::Disabled | catalog::Error::Offline) = self.catalog.resolve(None) {
            return Err(Error::Request);
        }
        if self.subscription_limits.is_none() {
            return Err(Error::Request);
        }
        // The credential store is still read before the cache, so a missing or
        // unsafe store keeps failing at startup with the same error class and
        // still precedes any HTTP, exactly as before.
        let key = self.go_key()?;
        if let Some((state, models)) = self.go_cache.load(base, std::time::SystemTime::now()) {
            self.apply_go(models).map_err(|_| Error::Protocol)?;
            return Ok(state);
        }
        let discovered = crate::opencode::client::models_observed(
            client,
            base,
            &key,
            timeout,
            &oxa_proto::CancelToken::new(),
            self.diagnostics.as_ref(),
        )
        .await?;
        self.apply_go(discovered.clone()).map_err(|_| Error::Protocol)?;
        // A cache write failure only costs the next start its fast path.
        let _ = self
            .go_cache
            .store(base, &discovered, std::time::SystemTime::now());
        Ok(State::Fresh)
    }
    /// One bounded background refresh: it shares the gateway's existing HTTP
    /// pool, never mutates the live catalog and never touches the foreground
    /// diagnostics sink, so a catalog failure stays non-fatal to every other
    /// local or configured route.
    pub(crate) async fn refresh_go(
        &self,
        client: &reqwest::Client,
        base: &str,
        timeout: std::time::Duration,
    ) -> Result<(), crate::opencode::client::Error> {
        use crate::opencode::client::Error;
        if let Err(catalog::Error::Disabled | catalog::Error::Offline) = self.catalog.resolve(None) {
            return Err(Error::Request);
        }
        if self.subscription_limits.is_none() {
            return Err(Error::Request);
        }
        let key = self.go_key()?;
        let discovered = crate::opencode::client::models_observed(
            client,
            base,
            &key,
            timeout,
            &oxa_proto::CancelToken::new(),
            None,
        )
        .await?;
        let _ = self
            .go_cache
            .store(base, &discovered, std::time::SystemTime::now());
        Ok(())
    }
    fn go_key(&self) -> Result<Key, crate::opencode::client::Error> {
        use crate::opencode::client::Error;
        self.key_file
            .as_ref()
            .map_err(|_| {
                self.credential_error();
                Error::Refused(crate::opencode::Refusal::Unauthenticated)
            })?
            .read("opencode")
            .map_err(|_| {
                self.credential_error();
                Error::Refused(crate::opencode::Refusal::Unauthenticated)
            })
    }
    fn apply_go(
        &mut self,
        discovered: Vec<crate::opencode::Model>,
    ) -> Result<(), crate::opencode::client::Error> {
        use crate::opencode::client::Error;
        let mut models: Vec<_> = self
            .catalog
            .models()
            .filter_map(|m| self.catalog.configured(m.id).cloned())
            .collect();
        let mut additions = BTreeMap::new();
        for model in discovered {
            if !model.api.contains_key("chat-completions") {
                continue;
            }
            let alias = format!("opencode/{}", model.id);
            let endpoint = &model.api["chat-completions"].base_url;
            models.push(
                Model::new(
                    &alias,
                    Provider::OpenCode,
                    &model.id,
                    endpoint,
                    model.context_window,
                    model.max_output,
                )
                .map_err(|_| Error::Protocol)?,
            );
            additions.insert(alias, model);
        }
        let catalog = Catalog::new(Access::Enabled, models).map_err(|_| Error::Protocol)?;
        for alias in additions.keys() {
            self.billings.insert(alias.clone(), Billing::Subscription);
        }
        self.go_models = additions;
        self.catalog = catalog;
        Ok(())
    }
    pub fn begin(&self, payload: &Value, context: Context<'_>) -> Result<Call<'_>, Refusal> {
        if let Some(diagnostics) = &self.diagnostics {
            diagnostics.clear();
        }
        let owner_origin = matches!(context.origin, crate::ledger::Origin::Oxa);
        let shared = context.guest_key_name.is_some();
        let alias = payload["model"].as_str().ok_or_else(|| invalid("model"))?;
        if matches!(self.catalog.resolve(Some(alias)), Err(catalog::Error::Disabled)) {
            return Err(catalog_error(catalog::Error::Disabled));
        }
        let known = self
            .catalog
            .configured(alias)
            .filter(|model| supported(model.info().provider));
        let mut audit = known.map(|_| self.audit(alias, context));
        let mut credential: Option<Credential> = None;
        let result = (|| {
            let model = self.catalog.resolve(Some(alias)).map_err(catalog_error)?;
            if !supported(model.info().provider) {
                return Err(Refusal {
                    status: 503,
                    code: "cloud_provider_unavailable",
                    param: "model",
                    message: "This cloud provider is not available in this build.",
                });
            }
            let billing = self.billings.get(alias);
            policy::before_credentials(payload, shared, false, billing.is_some())?;
            if model.info().provider == Provider::OpenCode && !owner_origin {
                return Err(Refusal {
                    status: 403,
                    code: "cloud_provider_forbidden",
                    param: "model",
                    message: "This subscription provider is available only to the owner agent.",
                });
            }
            let billing = billing.expect("policy validated billing");
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
            if matches!(billing, Billing::Metered(_)) {
                policy::daily_budget(&self.cap, self.ledger_enabled, Some(0.0), 0.0, 0.0)?;
            } else if self.subscription_limits.is_none() {
                return Err(crate::billing::SubscriptionLimits::missing());
            } else if !self.ledger_enabled {
                return Err(Refusal::ledger());
            }
            credential = Some(match model.info().provider {
                Provider::Anthropic | Provider::OpenRouter | Provider::OpenAi | Provider::OpenCode => {
                    Credential::ApiKey(Arc::new(
                        self.key_file
                            .as_ref()
                            .map_err(|_| self.credential_error())?
                            .read(model.info().provider.as_str())
                            .map_err(|_| self.credential_error())?,
                    ))
                }
                #[cfg(feature = "upstream-openai")]
                Provider::ChatGpt => {
                    self.key_file.as_ref().map_err(|_| Refusal::credentials())?;
                    Credential::Subscription(
                        crate::openai::chatgpt_tokens::read(&self.token_path)
                            .map_err(|_| Refusal::credentials())?,
                    )
                }
                #[cfg(not(feature = "upstream-openai"))]
                Provider::ChatGpt => return Err(Refusal::credentials()),
            });
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
                billing: billing.clone(),
            };
            let hold = match &billing.billing {
                Billing::Metered(_) => daily.budget().reserve(&self.cap, true, &billing, output)?,
                Billing::Subscription => {
                    let cap = (model.info().provider == Provider::OpenCode && !self.cap.is_null())
                        .then_some(&self.cap);
                    daily.budget().reserve_subscription_with_budget(
                        self.subscription_limits,
                        true,
                        &billing,
                        output,
                        cap,
                    )?
                }
            };
            let (_, mut entry) = audit.take().expect("known model")?;
            entry.admitted();
            Ok(Call {
                model,
                output,
                credential: credential.take().expect("fresh credentials"),
                #[cfg(feature = "upstream-openai")]
                token_path: &self.token_path,
                #[cfg(feature = "upstream-openai")]
                subscription: self.subscription.as_ref(),
                usage: Usage::default(),
                hold: Some(hold),
                entry: Some(entry),
                forwarded: false,
                go_usage: None,
                diagnostics: self.diagnostics.clone(),
            })
        })();
        if let Err(error) = &result {
            if let (Some(model), Some(Ok((daily, entry)))) = (known, audit) {
                let fields = unpaid(model, self.billings.get(alias));
                let _ = daily.budget().write_unpaid(|| {
                    entry
                        .append_redacting(&fields, error.status, Some(error.code), &|value| {
                            if let Some(credential) = &credential {
                                credential.redact(value);
                            }
                        })
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
                let fields = unpaid(model, self.billings.get(alias));
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
    pub fn key_handle(&self) -> Result<Arc<Key>, Refusal> {
        self.credential.key()
    }
    pub fn provider_account_scope(&self) -> [u8; 32] {
        self.credential.account_scope()
    }
    /// Provider-bound conversation hint, distinct from the private ledger's
    /// session identifier. Neither the raw session id nor the salt leaves here.
    pub fn provider_session_hint(&self) -> Result<String, Refusal> {
        self.entry
            .as_ref()
            .and_then(|entry| entry.provider_session_hint(self.model.info().provider.as_str()))
            .ok_or_else(|| invalid("session"))
    }
    pub fn openai_auth_handle(&self) -> Result<OpenAiAuthHandle, Refusal> {
        match (self.model.info().provider, &self.credential) {
            (Provider::OpenAi, Credential::ApiKey(key)) => Ok(OpenAiAuthHandle::ApiKey(Arc::clone(key))),
            #[cfg(feature = "upstream-openai")]
            (Provider::ChatGpt, Credential::Subscription(record)) => {
                let config = self.subscription.ok_or_else(Refusal::credentials)?;
                Ok(OpenAiAuthHandle::Subscription {
                    token_path: self.token_path.into(),
                    expected_account_id: record.account_id().into(),
                    refresh_endpoint: config.refresh_endpoint.clone(),
                    client_id: config.client_id.clone(),
                    timeout: config.timeout,
                })
            }
            _ => Err(invalid_billing()),
        }
    }
    pub fn metered_usage(&mut self, usage: Usage) {
        self.usage = usage;
    }
    pub fn prepare_history(
        &self,
        request: &oxa_proto::ChatRequest<'_>,
        items: Arc<[Value]>,
    ) -> Result<HistorySeal, Refusal> {
        if items.iter().any(|item| self.credential.contains_secret(item)) {
            return Err(crate::history::invalid_output());
        }
        HistorySeal::prepare(
            self.entry.as_ref().ok_or_else(Refusal::ledger)?.ledger(),
            self.model,
            &self.provider_account_scope(),
            request.prefix,
            request.messages,
            items,
        )
    }
    pub fn verify_history(&self, request: &oxa_proto::ChatRequest<'_>) -> Result<(), Refusal> {
        self.entry
            .as_ref()
            .ok_or_else(Refusal::ledger)?
            .ledger()
            .verify_provider_history(
                self.model,
                &self.provider_account_scope(),
                request.prefix,
                request.messages,
            )
    }
    pub fn mark_forwarded(&mut self) {
        self.forwarded = true;
        self.hold.as_mut().expect("unsettled call").mark_forwarded();
    }
    pub fn observe(&mut self, chunk: &Value) {
        if self.model.info().provider == Provider::OpenCode {
            if let Ok(usage) = crate::opencode::Usage::parse(&chunk["usage"]) {
                self.metered_go(Some(&usage));
            }
        }
        self.usage.observe(chunk);
        self.entry.as_mut().expect("unsettled call").observe();
    }
    pub(crate) fn metered_go(&mut self, usage: Option<&crate::opencode::Usage>) {
        self.usage.reported_cost = usage.map(|usage| usage.cost);
        self.go_usage = usage.map(|usage| {
            self.usage.prompt = Some(usage.prompt);
            self.usage.output = Some(usage.completion);
            self.usage.cache_read = usage.cache_read;
            self.usage.cache_write = usage.cache_write;
            json!({"billing_source":usage.billing_source.as_str(),"allowance":usage.allowance,"list_cost":usage.list_cost,"tokens":usage.tokens})
        });
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
            if let Some(usage) = &self.go_usage {
                fields["opencode"] = usage.clone();
            }
            if !self.forwarded {
                fields["settled"] = json!("not_forwarded");
            }
            saved = entry.append_redacting(&fields, status, error, &|value| self.credential.redact(value))?;
            Ok(())
        })?;
        if saved["settled"] == "usage" {
            if let (Some(diagnostics), Some(cost), Some(plan)) = (
                self.diagnostics.as_ref(),
                saved["cost_usd"].as_f64(),
                saved["opencode"].get("billing_source"),
            ) {
                diagnostics.settled_billing(crate::opencode::PlanName::from_value(plan), cost);
            }
        }
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
