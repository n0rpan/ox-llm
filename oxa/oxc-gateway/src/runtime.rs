//! Admission and consuming settlement, with one process-shared private ledger.
use crate::{
    accounting::{self, Hold, Outcome, Usage},
    catalog::{self, Access, Catalog, Model, ModelInfo, Provider},
    catalog_discovery::{Offer, Source},
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

/// Window and output assumed for a discovered model no catalog gives limits for: small enough to
/// be safe on any current model, so an unknown model works without a promise it cannot keep.
const UNKNOWN_CONTEXT: u32 = 128_000;
const UNKNOWN_OUTPUT: u32 = 8_192;
#[cfg(feature = "upstream-openai")]
use std::path::Path;
use std::{
    collections::{BTreeMap, BTreeSet},
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
    configured_models: BTreeSet<String>,
    recommended: BTreeSet<String>,
    unpriced_discovered: BTreeMap<String, crate::pricing::Price>,
    #[cfg(feature = "share")]
    guest_models: BTreeSet<String>,
    go_models: BTreeMap<String, crate::opencode::Model>,
    model_rules: crate::model_rules::Rules,
    /// Bounded stale-aware Go catalog metadata, beside the ledger in the same
    /// private agent directory. Never a key, header or transcript.
    go_cache: Option<crate::opencode::cache::Cache>,
    key_file: Result<KeyFile, crate::secrets::Error>,
    ledger_dir: PathBuf,
    ledger: OnceLock<Result<Ledger, Refusal>>,
    cap: Value,
    /// Owner-chosen OpenRouter `provider` routing preferences. `None` sends nothing.
    openrouter_provider: Option<Value>,
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
    let mut fields = json!({"remote":true,"remote_provider":model.provider_name(),"remote_model":model.upstream(),
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
        let mut configured_models = BTreeSet::new();
        for (model, billing) in models {
            if model.configured_route().is_none()
                && matches!(model.info().provider, Provider::ChatGpt | Provider::OpenCode)
                    != matches!(billing, Some(Billing::Subscription))
            {
                return Err(invalid_billing());
            }
            if let Some(billing) = billing {
                billings.insert(model.info().id.to_owned(), billing);
            }
            configured_models.insert(model.info().id.to_owned());
            routes.push(model);
        }
        let catalog = Catalog::new(options.access, routes).map_err(catalog_error)?;
        #[cfg(feature = "upstream-openai")]
        let token_path = key_path
            .parent()
            .ok_or_else(Refusal::credentials)?
            .join("chatgpt-auth.json");
        Ok(Self {
            diagnostics: None,
            catalog,
            billings,
            configured_models,
            recommended: BTreeSet::new(),
            unpriced_discovered: BTreeMap::new(),
            #[cfg(feature = "share")]
            guest_models: BTreeSet::new(),
            go_models: BTreeMap::new(),
            model_rules: crate::model_rules::Rules::default(),
            go_cache: None,
            key_file: KeyFile::prepare(&key_path),
            ledger_dir,
            ledger: OnceLock::new(),
            cap: json!(options.daily_budget_usd),
            openrouter_provider: None,
            ledger_enabled: options.ledger_enabled,
            subscription_limits: options.subscription_limits,
            #[cfg(feature = "upstream-openai")]
            subscription: options.subscription,
            #[cfg(feature = "upstream-openai")]
            token_path,
        })
    }

    pub fn discovery(&self) -> impl Iterator<Item = ModelInfo<'_>> {
        let models = || self.catalog.models().filter(|model| supported(model.provider));
        models()
            .filter(move |model| !self.recommended.contains(model.id))
            .chain(models().filter(move |model| self.recommended.contains(model.id)))
    }
    pub(crate) fn recommended_models(&self) -> &BTreeSet<String> {
        &self.recommended
    }
    /// Trusted user settings, keyed by the exact discovered public alias.
    pub fn with_unpriced_discovered_pricing(mut self, raw: &Value) -> Result<Self, Refusal> {
        let bad = || invalid("unpriced_discovered_pricing");
        for (alias, value) in raw.as_object().ok_or_else(bad)? {
            let price = crate::pricing::Price::conservative(value).ok_or_else(bad)?;
            self.unpriced_discovered.insert(alias.clone(), price);
        }
        Ok(self)
    }
    /// Owner overrides of the model rule table (`oxa.cloud.model_rules`, user settings only).
    pub fn with_model_rules(mut self, raw: &Value) -> Result<Self, Refusal> {
        self.model_rules = crate::model_rules::Rules::from_overrides(raw).map_err(|e| invalid(e.0))?;
        Ok(self)
    }
    pub(crate) fn model_rules(&self) -> &crate::model_rules::Rules {
        &self.model_rules
    }

    /// Trusted user settings: OpenRouter provider routing preferences, sent
    /// exactly as configured. Only `zdr` (bool) and `data_collection`
    /// (`allow` or `deny`) are accepted; anything else is refused. Never forced.
    pub fn with_openrouter_provider(mut self, raw: &Value) -> Result<Self, Refusal> {
        let bad = || invalid("openrouter_provider");
        let map = raw.as_object().ok_or_else(bad)?;
        let mut out = serde_json::Map::new();
        for (key, value) in map {
            match (key.as_str(), value) {
                ("zdr", Value::Bool(_)) => {}
                ("data_collection", Value::String(s)) if matches!(s.as_str(), "allow" | "deny") => {}
                _ => return Err(bad()),
            }
            out.insert(key.clone(), value.clone());
        }
        self.openrouter_provider = (!out.is_empty()).then_some(Value::Object(out));
        Ok(self)
    }
    pub(crate) fn openrouter_provider(&self) -> Option<&Value> {
        self.openrouter_provider.as_ref()
    }
    /// Enable catalog writes only for the owner-selected discovery setting.
    pub fn with_catalog_cache(mut self) -> Self {
        self.go_cache = Some(crate::opencode::cache::Cache::new(self.catalog_cache_dir()));
        self
    }
    #[cfg(feature = "share")]
    pub(crate) fn set_metered_guest_models(&mut self, aliases: BTreeSet<String>) -> Result<(), Refusal> {
        if aliases.len() > 64
            || aliases.iter().any(|alias| {
                !matches!(self.billings.get(alias), Some(Billing::Metered(_)))
                    || !self
                        .catalog
                        .resolve(Some(alias))
                        .is_ok_and(|model| supported(model.info().provider))
            })
        {
            return Err(invalid("share.remoteModels"));
        }
        self.guest_models = aliases;
        Ok(())
    }

    #[cfg(feature = "share")]
    pub(crate) fn metered_guest_models(&self) -> impl Iterator<Item = ModelInfo<'_>> {
        self.discovery()
            .filter(|model| self.guest_models.contains(model.id))
    }

    #[cfg(feature = "share")]
    pub(crate) fn begin_authenticated_guest(
        &self,
        payload: &Value,
        guest: &crate::share::listener::Guest<'_>,
    ) -> Result<Call<'_>, Refusal> {
        let context = Context {
            origin: crate::ledger::Origin::Api,
            guest_key_name: Some(guest.key_name()),
            ..Context::default()
        };
        let allowed = payload["model"]
            .as_str()
            .is_some_and(|alias| self.guest_models.contains(alias));
        self.begin_with_context(payload, context, allowed)
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

    pub(crate) fn catalog_cache_dir(&self) -> &std::path::Path {
        self.ledger_dir.parent().unwrap_or(self.ledger_dir.as_path())
    }

    pub(crate) fn catalog_key(&self, provider: Provider) -> Result<Option<Key>, Refusal> {
        self.catalog_key_named(provider, provider.as_str())
    }

    pub(crate) fn catalog_key_named(&self, provider: Provider, key: &str) -> Result<Option<Key>, Refusal> {
        if let Err(error @ (catalog::Error::Disabled | catalog::Error::Offline)) = self.catalog.resolve(None)
        {
            return Err(catalog_error(error));
        }
        if provider == Provider::ChatGpt {
            #[cfg(not(feature = "upstream-openai"))]
            return Ok(None);
            #[cfg(feature = "upstream-openai")]
            // The plan has no API key: a usable sign-in and refresh settings are the credential.
            return Ok(self.subscription.as_ref().and_then(|_| {
                crate::openai::chatgpt_tokens::read(&self.token_path)
                    .ok()
                    .map(|record| Key::for_account(record.account_id()))
            }));
        }
        Ok(self.key_file.as_ref().ok().and_then(|file| file.read(key).ok()))
    }

    /// The token file and refresh settings a ChatGPT-plan model list needs.
    #[cfg(feature = "upstream-openai")]
    pub(crate) fn plan(&self) -> Option<crate::catalog_discovery::Plan<'_>> {
        let subscription = self.subscription.as_ref()?;
        Some(crate::catalog_discovery::Plan {
            token_path: &self.token_path,
            refresh_endpoint: &subscription.refresh_endpoint,
            client_id: &subscription.client_id,
            timeout: subscription.timeout,
        })
    }

    pub(crate) fn add_recommended_models(
        &mut self,
        models: impl IntoIterator<Item = (Model, crate::pricing::Price)>,
    ) -> Result<(), Refusal> {
        let candidates: Vec<_> = models
            .into_iter()
            .filter(|(model, _)| {
                let info = model.info();
                model.configured_route().is_none()
                    && matches!(
                        info.provider,
                        Provider::Anthropic | Provider::OpenRouter | Provider::OpenAi
                    )
                    && supported(info.provider)
                    && self.catalog.configured(info.id).is_none()
            })
            .collect();
        let mut available = Vec::new();
        for provider in [Provider::Anthropic, Provider::OpenRouter, Provider::OpenAi] {
            if candidates
                .iter()
                .any(|(model, _)| model.info().provider == provider)
                && self.catalog_key(provider)?.is_some()
            {
                available.push(provider);
            }
        }
        let (models, billings): (Vec<_>, Vec<_>) = candidates
            .into_iter()
            .filter(|(model, _)| available.contains(&model.info().provider))
            .map(|(model, price)| {
                let billing = (model.info().id.to_owned(), Billing::Metered(price));
                (model, billing)
            })
            .unzip();
        if !models.is_empty() {
            self.catalog = Catalog::new(
                Access::Enabled,
                self.catalog
                    .models()
                    .filter_map(|info| self.catalog.configured(info.id).cloned())
                    .chain(models),
            )
            .map_err(catalog_error)?;
            self.recommended
                .extend(billings.iter().map(|(alias, _)| alias.clone()));
            self.billings.extend(billings);
        }
        Ok(())
    }

    pub(crate) fn apply_provider_catalog(
        &mut self,
        source: &Source,
        offers: Vec<Offer>,
    ) -> Result<(usize, usize, usize), Refusal> {
        let same_source = |model: &Model| {
            (source.is_named() || model.configured_route().is_none())
                && model.provider_name() == source.name()
                && model.credential_name() == source.key_name()
                && model.info().provider == source.provider()
                && model.endpoint().as_str().trim_end_matches('/') == source.base().trim_end_matches('/')
        };
        let (configured, mut routes): (Vec<_>, Vec<_>) = self
            .catalog
            .models()
            .filter_map(|info| self.catalog.configured(info.id).cloned())
            .partition(same_source);
        let mut billings = self.billings.clone();
        for model in &configured {
            billings.remove(model.info().id);
        }
        let mut missing_price = 0;
        let mut missing_limits = 0;
        let mut estimated_price = 0;
        for offer in offers {
            let explicit: Vec<_> = configured
                .iter()
                .filter(|model| model.upstream() == offer.id)
                .collect();
            let generated;
            let ids: Vec<&str> = if explicit.is_empty() {
                generated = format!("{}/{}", source.name(), offer.id);
                vec![generated.as_str()]
            } else {
                explicit.iter().map(|model| model.info().id).collect()
            };
            for alias in ids {
                let existing = self.catalog.configured(alias);
                let fallback = existing.filter(|model| same_source(model) && model.upstream() == offer.id);
                if fallback.is_none() && existing.is_some() {
                    // Discovery never shadows another registered route.
                    continue;
                }
                let fallback_info = fallback.map(Model::info);
                // Discovery may raise the owner's configured reservation bounds.
                let context = offer.context.max(fallback_info.map(|info| info.context_window));
                let max_output = offer.max_output.max(fallback_info.map(|info| info.max_output));
                // What nobody can tell is not refused: a conservative generic window and, for a price
                // nobody lists, an unpriced route whose usage is recorded with unknown cost.
                let limits = match (context, max_output) {
                    (Some(context), Some(output)) if output <= context => Some((context, output)),
                    (Some(context), None) => Some((context, UNKNOWN_OUTPUT.min(context))),
                    (None, Some(output)) => Some((UNKNOWN_CONTEXT.max(output), output)),
                    _ => None,
                };
                let (context, output) = limits.unwrap_or((UNKNOWN_CONTEXT, UNKNOWN_OUTPUT));
                let Ok(model) = source.model(alias, &offer.id, context, output) else {
                    continue;
                };
                missing_limits += usize::from(limits.is_none());
                if source.is_subscription() {
                    billings.insert(alias.into(), Billing::Subscription);
                    routes.push(model);
                    continue;
                }
                let price = match (self.billings.get(alias), offer.price.as_ref()) {
                    (Some(Billing::Metered(configured)), live) if configured.is_estimated() => live.cloned(),
                    (Some(Billing::Metered(configured)), Some(live)) => {
                        Some(configured.clone().overlay_discovered(live))
                    }
                    (Some(Billing::Metered(configured)), None) => Some(configured.clone()),
                    (_, live) => live.cloned(),
                }
                .or_else(|| self.unpriced_discovered.get(alias).cloned());
                let price = price.filter(|price| {
                    price
                        .reservation(source.name(), &offer.id, u64::from(context), u64::from(output))
                        .is_some_and(f64::is_finite)
                });
                missing_price += usize::from(price.is_none());
                let price = price.unwrap_or_else(crate::pricing::Price::unpriced);
                estimated_price += usize::from(price.is_estimated());
                billings.insert(alias.into(), Billing::Metered(price));
                routes.push(model);
            }
        }
        if source.provider() == Provider::ChatGpt || source.is_named() {
            // The picker list is not authority to delete an owner's route: owner entries win.
            for model in configured {
                if self.configured_models.contains(model.info().id)
                    && routes.iter().all(|route| route.info().id != model.info().id)
                {
                    if let Some(billing) = self.billings.get(model.info().id) {
                        billings.insert(model.info().id.into(), billing.clone());
                    }
                    routes.push(model);
                }
            }
        }
        let catalog = Catalog::new(Access::Enabled, routes).map_err(catalog_error)?;
        self.catalog = catalog;
        self.billings = billings;
        Ok((missing_price, missing_limits, estimated_price))
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
        // The credential store is still read before the cache, so a missing or
        // unsafe store keeps failing at startup with the same error class and
        // still precedes any HTTP, exactly as before.
        let key = self.go_key()?;
        if let Some((state, models)) = self
            .go_cache
            .as_ref()
            .and_then(|cache| cache.load(base, &key, std::time::SystemTime::now()))
        {
            self.apply_go(&models).map_err(|_| Error::Protocol)?;
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
        self.apply_go(&discovered).map_err(|_| Error::Protocol)?;
        if let Some(cache) = &self.go_cache {
            let _ = cache.store(base, &key, &discovered, std::time::SystemTime::now());
        }
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
        let cache = self.go_cache.as_ref().ok_or(Error::Request)?;
        if let Err(catalog::Error::Disabled | catalog::Error::Offline) = self.catalog.resolve(None) {
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
        let _ = cache.store(base, &key, &discovered, std::time::SystemTime::now());
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
        discovered: &[crate::opencode::Model],
    ) -> Result<(), crate::opencode::client::Error> {
        use crate::opencode::client::Error;
        let mut models: Vec<_> = self
            .catalog
            .models()
            .filter(|model| !self.go_models.contains_key(model.id))
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
            additions.insert(alias, model.clone());
        }
        let catalog = Catalog::new(Access::Enabled, models).map_err(|_| Error::Protocol)?;
        for alias in self.go_models.keys() {
            self.billings.remove(alias);
        }
        for alias in additions.keys() {
            self.billings.insert(alias.clone(), Billing::Subscription);
        }
        self.go_models = additions;
        self.catalog = catalog;
        Ok(())
    }
    pub fn begin(&self, payload: &Value, context: Context<'_>) -> Result<Call<'_>, Refusal> {
        self.begin_with_context(payload, context, false)
    }

    fn begin_with_context(
        &self,
        payload: &Value,
        context: Context<'_>,
        allow_guest: bool,
    ) -> Result<Call<'_>, Refusal> {
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
            let guest_allowed = allow_guest && matches!(billing, Some(Billing::Metered(_)));
            policy::before_credentials(
                payload,
                shared && !guest_allowed,
                false,
                billing.is_some(),
                !self.cap.is_null(),
            )?;
            if model.info().provider == Provider::OpenCode && !owner_origin {
                return Err(Refusal {
                    status: 403,
                    code: "cloud_provider_forbidden",
                    param: "model",
                    message: "This subscription provider is available only to the owner agent.",
                });
            }
            // An unpriced model reaches here only with no cap (policy above). It is metered with
            // no rates, so admission records its usage as unknown cost.
            let unpriced = Billing::Metered(crate::pricing::Price::unpriced());
            let billing = billing.unwrap_or(&unpriced);
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
            } else if !self.ledger_enabled {
                return Err(Refusal::ledger());
            }
            credential = Some(match model.info().provider {
                Provider::Anthropic | Provider::OpenRouter | Provider::OpenAi | Provider::OpenCode => {
                    Credential::ApiKey(Arc::new(
                        self.key_file
                            .as_ref()
                            .map_err(|_| self.credential_error())?
                            .read(model.credential_name())
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
                provider: model.provider_name().into(),
                upstream: model.upstream().into(),
                context: u64::from(model.info().context_window),
                max_output: u64::from(model.info().max_output),
                billing: billing.clone(),
            };
            let hold = match &billing.billing {
                Billing::Metered(_) => daily.budget().reserve(&self.cap, true, &billing, output)?,
                Billing::Subscription => daily.budget().reserve_subscription_route(
                    self.subscription_limits,
                    true,
                    &billing,
                    output,
                    model.configured_route().is_some(),
                )?,
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
    pub(crate) fn budget_warning(&self) -> Option<&'static str> {
        self.hold.as_ref().and_then(Hold::budget_warning)
    }

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
        let account = self.credential.account_scope();
        let Some(route) = self.model.configured_route() else {
            return account;
        };
        use sha2::{Digest, Sha256};
        let mut scope = Sha256::new();
        scope.update(account);
        scope.update(route.provider().as_bytes());
        scope.update([0]);
        scope.update(self.model.endpoint().as_str().as_bytes());
        scope.finalize().into()
    }
    /// Provider-bound conversation hint, distinct from the private ledger's
    /// session identifier. Neither the raw session id nor the salt leaves here.
    pub fn provider_session_hint(&self) -> Result<String, Refusal> {
        let entry = self.entry.as_ref().ok_or_else(|| invalid("session"))?;
        let hint = match self.model.configured_route() {
            Some(route) => {
                entry.provider_session_hint(&format!("{}\0{}", route.provider(), self.model.endpoint()))
            }
            None => entry.provider_session_hint(self.model.provider_name()),
        };
        hint.ok_or_else(|| invalid("session"))
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
    pub fn mark_forwarded(&mut self) -> Result<(), Refusal> {
        self.hold.as_mut().expect("unsettled call").mark_forwarded()?;
        self.forwarded = true;
        Ok(())
    }
    pub fn observe(&mut self, chunk: &Value) {
        if self.model.info().provider == Provider::OpenCode {
            if let Ok(usage) = crate::opencode::Usage::parse(&chunk["usage"]) {
                self.metered_go(Some(&usage));
            }
        } else {
            self.usage.observe(chunk);
        }
        self.entry.as_mut().expect("unsettled call").observe();
    }
    pub(crate) fn metered_go(&mut self, usage: Option<&crate::opencode::Usage>) {
        self.usage.reported_cost = usage.filter(|usage| !usage.estimated).map(|usage| usage.cost);
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
                if fields
                    .as_object_mut()
                    .and_then(|row| row.remove("estimated_cost_usd"))
                    .is_some()
                {
                    fields["cost_usd"] = json!(0.0);
                }
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

#[cfg(test)]
mod discovery_budget_tests {
    use super::*;
    use crate::pricing::Price;

    #[test]
    fn discovery_preserves_configured_price_floors_and_long_context_tiers() {
        assert_discovered_price_floor(0, 4096, 64);
    }

    #[test]
    fn discovery_cannot_lower_a_configured_price_with_a_positive_live_rate() {
        assert_discovered_price_floor(1, 4096, 64);
    }

    #[test]
    fn discovery_cannot_lower_configured_token_limits() {
        assert_discovered_price_floor(1, 1, 1);
    }

    #[cfg(feature = "upstream-openai")]
    #[test]
    fn a_configured_plan_route_survives_refresh_when_a_discovered_route_does_not() {
        let base = "https://example.invalid/chatgpt";
        let owner = Model::new(
            "chatgpt/owner-pick",
            Provider::ChatGpt,
            "owner-pick",
            base,
            4096,
            64,
        )
        .unwrap();
        let mut runtime = Runtime::new(
            [(owner, Some(Billing::Subscription))],
            PathBuf::from("/synthetic/credentials.json"),
            PathBuf::from("/synthetic/ledger"),
            Options {
                access: Access::Enabled,
                ..Options::default()
            },
        )
        .unwrap();
        let source = Source::new(Provider::ChatGpt, base).unwrap();
        let offer = Offer {
            public: false,
            id: "listed".into(),
            context: Some(1000),
            max_output: Some(10),
            price: None,
        };
        runtime.apply_provider_catalog(&source, vec![offer]).unwrap();
        assert!(runtime.catalog.resolve(Some("chatgpt/owner-pick")).is_ok());
        assert!(runtime.catalog.resolve(Some("chatgpt/listed")).is_ok());
        runtime.apply_provider_catalog(&source, Vec::new()).unwrap();
        assert!(runtime.catalog.resolve(Some("chatgpt/owner-pick")).is_ok());
        assert!(runtime.catalog.resolve(Some("chatgpt/listed")).is_err());
        assert!(matches!(
            runtime.billings.get("chatgpt/owner-pick"),
            Some(Billing::Subscription)
        ));
    }

    fn assert_discovered_price_floor(live_input: u32, context: u32, output: u32) {
        let base = "https://example.invalid/api/v1";
        let provider = Provider::OpenRouter;
        let alias = "synthetic-alias";
        let upstream = "synthetic/model";
        let model = Model::new(alias, provider, upstream, base, 4096, 64).unwrap();
        let configured = Price::from_model(&json!({"pricing": {"tiers": [
            {"max_prompt_tokens": 1024, "input_per_million": 3, "output_per_million": 2},
            {"input_per_million": 12, "output_per_million": 10, "cache_write_per_million": 15}
        ]}}))
        .unwrap();
        let live = Price::from_model(&json!({"pricing": {
            "input_per_million": live_input, "output_per_million": 4, "cache_write_per_million": 8
        }}))
        .unwrap();
        let mut runtime = Runtime::new(
            [(model, Some(Billing::Metered(configured)))],
            PathBuf::from("/synthetic/credentials.json"),
            PathBuf::from("/synthetic/ledger"),
            Options {
                access: Access::Enabled,
                daily_budget_usd: Some(1.0),
                ..Options::default()
            },
        )
        .unwrap()
        .with_unpriced_discovered_pricing(&json!({alias:{
            "input_per_million":100, "output_per_million":100
        }}))
        .unwrap();
        let source = Source::new(provider, base).unwrap();
        let offer = Offer {
            public: false,
            id: upstream.into(),
            context: Some(context),
            max_output: Some(output),
            price: Some(live),
        };
        assert_eq!(
            runtime.apply_provider_catalog(&source, vec![offer]).unwrap(),
            (0, 0, 0)
        );
        let info = runtime.catalog.resolve(Some(alias)).unwrap().info();
        assert_eq!(info.context_window, 4096);
        assert_eq!(info.max_output, 64);
        let Some(Billing::Metered(price)) = runtime.billings.get(alias) else {
            panic!("the discovered model lost its metered route");
        };
        let tiers = price.normalized()["tiers"]
            .as_array()
            .expect("configured tiers survived");
        assert_eq!(tiers.len(), 2);
        assert_eq!(tiers[0]["max_prompt_tokens"].as_u64(), Some(1024));
        assert_eq!(tiers[0]["input_per_million"].as_f64(), Some(3.0));
        assert_eq!(tiers[0]["output_per_million"].as_f64(), Some(4.0));
        assert_eq!(tiers[0]["cache_write_per_million"].as_f64(), Some(8.0));
        assert_eq!(tiers[1]["input_per_million"].as_f64(), Some(12.0));
        assert_eq!(tiers[1]["output_per_million"].as_f64(), Some(10.0));
        assert_eq!(tiers[1]["cache_write_per_million"].as_f64(), Some(15.0));
    }
}
