//! User-only selection and production composition of the native cloud gateway.
use oxa_config::{Dirs, Settings};
use oxa_engine::EmbeddedEngine;
use oxa_proto::{codes, Engine, OxaError};
use oxc_gateway::{
    billing::SubscriptionLimits,
    catalog::{Access, Model, Provider},
    catalog_discovery::{Source, State},
    gateway::Gateway,
    pricing::Price,
    runtime::{Billing, Options, Runtime, Subscription},
};
use serde_json::Value;
use std::{sync::Arc, time::Duration};

mod routing;

pub(crate) const SETTING: &str = "oxa.engine.inProcessCloud";
pub(crate) const GO_SETTING: &str = "oxa.cloud.opencode.enabled";
type Selected = (Arc<dyn Engine>, Vec<String>);

pub(crate) struct ModelSelection<'a> {
    pub pattern: Option<&'a str>,
    pub scope: &'a [String],
    pub endpoint: &'a str,
    pub owner_selected: bool,
    /// The model came from a command-line argument or a restored session, and not from a
    /// launcher's task file: a nested child may use it, a delegated run may not.
    pub model_argument: bool,
}

enum SelectPurpose<'a> {
    Model(ModelSelection<'a>),
    Inventory,
}

pub(crate) fn credential_path(dirs: &Dirs) -> std::path::PathBuf {
    dirs.home.join(".config/oxc/llm/cloud-secrets.json")
}

/// Consume private facts only at a trusted terminal failure boundary.
pub(crate) fn report_diagnostics(
    diagnostics: &oxc_gateway::diagnostics::Diagnostics,
    dirs: &Dirs,
    stderr: &mut dyn std::io::Write,
) {
    use oxc_gateway::diagnostics::Stage;
    let Some(snapshot) = diagnostics.take() else {
        return;
    };
    if snapshot.stage == Stage::LocalCredentialRead {
        let _ = writeln!(stderr,
            "Local cloud credentials unavailable; attempted protected store: {:?}. Check the provider entry and file protections.",
            credential_path(dirs));
        return;
    }
    let stage = match snapshot.stage {
        Stage::DiscoveryHttp => "discovery_http",
        Stage::ChatHttp => "chat_http",
        Stage::LocalCredentialRead => unreachable!(),
    };
    let _ = write!(stderr, "oxa native diagnostic: stage={stage}");
    if let Some(status) = snapshot.status {
        let _ = write!(stderr, " status={status}");
    }
    if let Some(code) = snapshot.code {
        let _ = write!(stderr, " code={code}");
    }
    for (name, value) in ["retry_after", "rate_limit", "rate_remaining", "rate_reset"]
        .into_iter()
        .zip(snapshot.rate)
    {
        if let Some(value) = value {
            let _ = write!(stderr, " {name}={value}");
        }
    }
    if let Some(plan) = snapshot.billing_source {
        let _ = write!(stderr, " billing_source={}", plan.as_str());
    }
    if let Some(cause) = snapshot.cause {
        let _ = write!(stderr, " cause={}", cause.as_str());
    }
    if snapshot.cause == Some(oxc_gateway::diagnostics::StreamCause::TerminalUsage) {
        if let Some(check) = snapshot.terminal_check {
            let _ = write!(stderr, " terminal_check={}", check.as_str());
        }
    }
    let _ = writeln!(stderr);
}

fn invalid() -> OxaError {
    OxaError::new(codes::INVALID, "Invalid user cloud configuration in oxa.cloud.")
}
fn text<'a>(value: &'a Value, field: &str) -> Result<&'a str, OxaError> {
    value[field]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(invalid)
}
fn limit(value: &Value, field: &str) -> Result<u32, OxaError> {
    value[field]
        .as_u64()
        .and_then(|v| u32::try_from(v).ok())
        .ok_or_else(invalid)
}

pub(crate) fn select(
    user: &Settings,
    effective: &Settings,
    dirs: &Dirs,
    requested: Option<&str>,
    http: impl FnOnce() -> Result<Arc<dyn Engine>, OxaError>,
) -> Result<Arc<dyn Engine>, OxaError> {
    select_observed(
        user,
        effective,
        dirs,
        ModelSelection {
            pattern: requested,
            scope: &[],
            endpoint: "",
            owner_selected: requested.is_some(),
            model_argument: requested.is_some(),
        },
        None,
        SharedEngines::default(),
        http,
    )
    .map(|(engine, _)| engine)
}

/// Native launch configuration is user-only. Stream policy follows the effective session.
pub(crate) fn local_engine_settings<'a>(
    user: &'a Settings,
    effective: &Settings,
) -> (Option<&'a Value>, oxa_engine::StreamOpts) {
    let stream = oxa_engine::StreamOpts {
        silence_limit_ms: crate::dispatch::stream_silence_timeout(effective)
            .unwrap_or(365 * 24 * 3600 * 1000),
        ..oxa_engine::StreamOpts::default()
    };
    (user.get("oxa.local"), stream)
}

#[test]
fn shared_local_engine_uses_effective_stream_timeout() {
    let user_values = serde_json::json!({"httpIdleTimeoutMs": 1700});
    let user = Settings::from_values(user_values.clone(), serde_json::json!({}));
    let effective = Settings::from_values(user_values, serde_json::json!({"httpIdleTimeoutMs": 2400}));
    let (_, stream) = local_engine_settings(&user, &effective);
    assert_eq!(stream.silence_limit_ms, 2400);
}

#[derive(Clone)]
pub(crate) struct OpenedGateway {
    pub gateway: Arc<Gateway>,
    notices: Vec<String>,
    go_failure: Option<OxaError>,
}

#[derive(Clone, Default)]
pub(crate) struct SharedEngines {
    pub local: Option<Arc<oxc_local::native::LocalEngine>>,
    pub cloud: Option<OpenedGateway>,
}

#[derive(Clone)]
pub(crate) enum ProviderState {
    On(&'static str),
    Off(OxaError),
    Broken(OxaError),
}

impl ProviderState {
    fn refusal(&self) -> Option<&OxaError> {
        match self {
            Self::On(_) => None,
            Self::Off(error) | Self::Broken(error) => Some(error),
        }
    }

    fn notice(&self, name: &str, setting: &str) -> String {
        match self {
            Self::On(reason) => format!("{name}: on ({reason}; user setting: {setting})"),
            Self::Off(error) | Self::Broken(error) => error.to_string(),
        }
    }
}

fn go_state(user: &Settings, dirs: &Dirs) -> ProviderState {
    use oxc_gateway::{policy::Refusal, secrets};

    let error = |state: &str, code, reason: &str| {
        OxaError::new(code, format!("OpenCode Go: {state} ({reason})"))
            .with_hint(format!("User setting: {GO_SETTING}"))
    };
    let off = |code, reason| ProviderState::Off(error("off", code, reason));
    if oxa_proto::offline() {
        return off(codes::OFFLINE, "offline mode");
    }
    if user.get(SETTING).is_some_and(|value| value != &Value::Bool(true)) {
        return off(codes::INVALID, "oxa.engine.inProcessCloud is not enabled");
    }
    match user.get(GO_SETTING) {
        // Keep the request-time reader and its protected-store diagnostics for
        // the legacy explicit opt-in. The absent setting infers enablement.
        Some(Value::Bool(true)) => return ProviderState::On("enabled by user settings"),
        Some(Value::Bool(false)) => return off(codes::INVALID, "disabled by user settings"),
        Some(_) => {
            return ProviderState::Broken(error("broken", codes::INVALID, "enabled must be a boolean"));
        }
        None => {}
    }
    match secrets::read(&credential_path(dirs), Provider::OpenCode.as_str()) {
        Ok(_) => ProviderState::On("key found in the protected cloud key file"),
        Err(secrets::Error::Unavailable) => off(
            Refusal::credentials().code,
            "no key; add the opencode entry in the protected cloud key file",
        ),
        Err(cause) => ProviderState::Broken(error("broken", Refusal::credentials().code, &cause.to_string())),
    }
}

pub(crate) fn select_catalog(
    user: &Settings,
    dirs: &Dirs,
    http: impl FnOnce() -> Result<Arc<dyn Engine>, OxaError>,
) -> Result<Selected, OxaError> {
    select_for(
        user,
        user,
        dirs,
        SelectPurpose::Inventory,
        None,
        SharedEngines::default(),
        http,
    )
}

pub(crate) fn select_observed(
    user: &Settings,
    effective: &Settings,
    dirs: &Dirs,
    requested: ModelSelection<'_>,
    diagnostics: Option<oxc_gateway::diagnostics::Diagnostics>,
    shared: SharedEngines,
    http: impl FnOnce() -> Result<Arc<dyn Engine>, OxaError>,
) -> Result<Selected, OxaError> {
    select_for(
        user,
        effective,
        dirs,
        SelectPurpose::Model(requested),
        diagnostics,
        shared,
        http,
    )
}

pub(crate) fn restores_go_model(user: &Settings, model: &str) -> bool {
    routing::is_go_model(model, "")
        && !(user.bool(oxc_local::LOCAL_SETTING, false)
            && user
                .get("oxa.local.models")
                .and_then(Value::as_object)
                .is_some_and(|models| routing::catalog_owns(model, "", models.keys().cloned())))
}

fn select_for(
    user: &Settings,
    effective: &Settings,
    dirs: &Dirs,
    purpose: SelectPurpose<'_>,
    diagnostics: Option<oxc_gateway::diagnostics::Diagnostics>,
    shared: SharedEngines,
    http: impl FnOnce() -> Result<Arc<dyn Engine>, OxaError>,
) -> Result<Selected, OxaError> {
    let (requested, scope, endpoint, owner_selected, model_argument, show_status) = match purpose {
        SelectPurpose::Model(selection) => (
            selection.pattern,
            selection.scope,
            selection.endpoint,
            selection.owner_selected,
            selection.model_argument,
            false,
        ),
        SelectPurpose::Inventory => (None, &[][..], "", false, false, true),
    };
    let local_engine = || match shared.local {
        Some(engine) => Ok(engine as Arc<dyn Engine>),
        None => {
            let (launch, stream) = local_engine_settings(user, effective);
            oxc_local::configured_engine(launch, dirs.agent_dir.join("local-engine"), stream)
        }
    };
    let go = go_state(user, dirs);
    let go_error = go.refusal();
    let go_enabled = go_error.is_none();
    let native_local = user.bool(oxc_local::LOCAL_SETTING, false);
    let explicit_cloud = user.bool(SETTING, false);
    let explicit_go = user.bool(GO_SETTING, false);
    let aliases = |path| {
        user.get(path)
            .and_then(Value::as_object)
            .map(|models| models.keys().cloned().collect::<std::collections::BTreeSet<_>>())
            .unwrap_or_default()
    };
    let local_models = aliases("oxa.local.models");
    let cloud_models = aliases("oxa.cloud.models");
    // A configured local alias stays local whatever shape its name has, in a single `--model`
    // and in every `--models` pattern of the scope.
    // Only an exact local owner excludes the Go model of the same name: a fuzzy match on
    // another name (`opencode/x-local` for `opencode/x`) must not shadow it.
    let local_alias =
        |model: &str| native_local && routing::catalog_owns(model, endpoint, local_models.iter().cloned());
    let requested_local = requested.is_some_and(local_alias);
    let requests_go = |pattern: &str| {
        routing::is_go_model(pattern, endpoint)
            && !(explicit_cloud
                && routing::resolve_in_catalog(pattern, endpoint, cloud_models.iter().cloned()).is_some())
            && !local_alias(pattern)
    };
    let go_requested = requested.map_or_else(
        || !scope.is_empty() && scope.iter().all(|pattern| requests_go(pattern)),
        requests_go,
    );
    // A keyed catalog is available for inventory and owner model selection.
    // A project default alone cannot choose a new cloud transport, and a delegated
    // model (`oxa work`'s task file) is not the owner's selection: a key (with no
    // user `false`) enables Go for the owner's own model choice only, and never for
    // an alias the local catalog already resolves unless the owner enabled Go.
    let native_cloud = explicit_cloud
        || (go_enabled
            && (show_status
                || owner_selected && (!requested_local || explicit_go)
                || model_argument && go_requested && (!requested_local || explicit_go)
                || explicit_go && (requested.is_none() || go_requested)));
    let availability_notice = || {
        if show_status {
            vec![go.notice("OpenCode Go", GO_SETTING)]
        } else {
            Vec::new()
        }
    };
    let cloud_engine = || {
        if oxa_proto::offline() {
            return Err(oxa_proto::offline_error("the in-process cloud gateway"));
        }
        if let Some(error) = go_error {
            let no_other_cloud = user
                .get("oxa.cloud.models")
                .is_none_or(|models| models.is_null() || models.as_object().is_some_and(|m| m.is_empty()))
                && !user.bool("oxa.cloud.discover_models", false);
            if no_other_cloud && user.get("oxa.cloud.opencode").is_some() {
                return Err(error.clone());
            }
        }
        let OpenedGateway {
            gateway,
            mut notices,
            go_failure,
        } = match shared.cloud {
            Some(opened) => opened,
            None => open_gateway_with_go(user, effective, dirs, diagnostics, Ok, Some(go_enabled))?,
        };
        if show_status {
            let notice = go_failure
                .as_ref()
                .map_or_else(|| go.notice("OpenCode Go", GO_SETTING), ToString::to_string);
            notices.push(notice);
        }
        Ok((
            Arc::new(EmbeddedEngine::from_shared_gateway(gateway)) as Arc<dyn Engine>,
            notices,
            go_failure,
        ))
    };
    if native_local && native_cloud {
        let (cloud, catalog_notices, go_failure) = match cloud_engine() {
            Ok((engine, notices, go_failure)) => (Ok(engine), notices, go_failure),
            Err(error) => {
                let notice = go_error.unwrap_or(&error).to_string();
                let notices = if show_status { vec![notice] } else { Vec::new() };
                (Err(error), notices, None)
            }
        };
        return routing::Routed {
            local: local_engine(),
            cloud,
            local_models: aliases("oxa.local.models"),
            cloud_models,
            go_error: go_error.cloned().or(go_failure),
            endpoint: endpoint.to_owned(),
            default: effective.string("defaultModel"),
        }
        .into_engine(requested)
        .map(|engine| (engine, catalog_notices));
    }
    if let Some(error) = go_error.filter(|_| go_requested && !requested_local) {
        return Err(error.clone());
    }
    if native_local {
        return local_engine().map(|engine| (engine, availability_notice()));
    }
    if !native_cloud {
        return http().map(|engine| (engine, availability_notice()));
    }
    let selected = cloud_engine();
    if !explicit_cloud && !show_status && !go_requested && (requested.is_some() || !scope.is_empty()) {
        let resolves_go = selected
            .as_ref()
            .ok()
            .and_then(|(engine, _, _)| engine.info(None).ok())
            .is_some_and(|info| {
                if let Some(pattern) = requested {
                    routing::resolve_in_catalog(pattern, endpoint, info.models)
                        .is_some_and(|model| routing::is_go_model(&model, ""))
                } else {
                    routing::scope_includes_go(scope, endpoint, info.models)
                }
            });
        if !resolves_go {
            return http().map(|engine| (engine, Vec::new()));
        }
    }
    let (engine, notices, go_failure) = selected?;
    if let Some(error) = go_failure.filter(|_| go_requested) {
        return Err(error);
    }
    Ok((engine, notices))
}

#[cfg(feature = "share")]
pub(crate) fn open_gateway(
    user: &Settings,
    effective: &Settings,
    dirs: &Dirs,
    diagnostics: Option<oxc_gateway::diagnostics::Diagnostics>,
    configure: impl FnOnce(Gateway) -> Result<Gateway, OxaError>,
) -> Result<OpenedGateway, OxaError> {
    open_gateway_with_go(user, effective, dirs, diagnostics, configure, None)
}

fn open_gateway_with_go(
    user: &Settings,
    effective: &Settings,
    dirs: &Dirs,
    diagnostics: Option<oxc_gateway::diagnostics::Diagnostics>,
    configure: impl FnOnce(Gateway) -> Result<Gateway, OxaError>,
    go_enabled: Option<bool>,
) -> Result<OpenedGateway, OxaError> {
    // Shared callers defer key checks until the guest catalog is validated.
    let go_configured = go_enabled == Some(true);
    let empty_cloud = Value::Object(serde_json::Map::new());
    let cloud = match user.get("oxa.cloud") {
        Some(cloud) if cloud.is_object() => cloud,
        None if go_configured => &empty_cloud,
        _ => return Err(invalid()),
    };
    let discover = cloud.get("discover_models") == Some(&Value::Bool(true));
    let empty = serde_json::Map::new();
    let models = match cloud.get("models") {
        None | Some(Value::Null) if go_configured || discover => &empty,
        Some(value) => value.as_object().ok_or_else(invalid)?,
        None => return Err(invalid()),
    };
    if models.is_empty() && !go_configured && !discover {
        return Err(invalid());
    }
    let mut routes = Vec::with_capacity(models.len());
    let mut needs_subscription = false;
    for (alias, value) in models {
        let (provider, default_endpoint) = match text(value, "provider")? {
            "anthropic" => (Provider::Anthropic, "https://api.anthropic.com"),
            "openrouter" => (Provider::OpenRouter, "https://openrouter.ai/api/v1"),
            "openai" => (Provider::OpenAi, oxc_gateway::openai::client::API_BASE),
            "chatgpt" => (Provider::ChatGpt, oxc_gateway::openai::client::SUBSCRIPTION_BASE),
            _ => return Err(OxaError::unsupported("this in-process cloud provider")),
        };
        let endpoint = match value.get("base_url") {
            None => default_endpoint,
            Some(value) => value.as_str().ok_or_else(invalid)?,
        };
        let model = Model::new(
            alias,
            provider,
            text(value, "upstream_model")?,
            endpoint,
            limit(value, "context_window")?,
            limit(value, "max_output")?,
        )
        .map_err(|_| invalid())?;
        let billing = if provider == Provider::ChatGpt {
            needs_subscription = true;
            Some(Billing::Subscription)
        } else {
            Price::from_model(value).map(Billing::Metered)
        };
        routes.push((model, billing));
    }
    let (subscription, subscription_limits) = subscription_options(cloud, needs_subscription)?;
    let cap = match cloud.get("daily_budget_usd") {
        None | Some(Value::Null) => None,
        Some(value) => Some(value.as_f64().ok_or_else(invalid)?),
    };
    let mut sources = Vec::new();
    if discover {
        for (provider, default) in [
            (Provider::OpenRouter, "https://openrouter.ai/api/v1"),
            (Provider::OpenAi, oxc_gateway::openai::client::API_BASE),
            (Provider::Anthropic, "https://api.anthropic.com"),
        ] {
            let mut configured = routes.iter().filter_map(|(model, _)| {
                (model.info().provider == provider).then_some(model.endpoint().as_str())
            });
            let base = match cloud.pointer(&format!("/discovery/{}/base_url", provider.as_str())) {
                Some(base) => base.as_str().ok_or_else(invalid)?,
                None => {
                    let first = configured.next().unwrap_or(default);
                    if configured.any(|other| other != first) {
                        continue;
                    }
                    first
                }
            };
            sources.extend(Source::new(provider, base));
        }
    }
    // These existing private locations are denied to both file tools and bash.
    // Configured-route discovery reads no key or ledger. Explicit Go discovery
    // uses the existing protected key reader and this same gateway HTTP pool.
    let mut runtime = Runtime::new(
        routes,
        credential_path(dirs),
        dirs.agent_dir.join("cloud-ledger"),
        Options {
            access: Access::Enabled,
            daily_budget_usd: cap,
            ledger_enabled: true,
            subscription,
            subscription_limits,
        },
    )
    .map_err(|e| OxaError::new(e.code, e.message))?;
    if discover {
        runtime = runtime.with_catalog_cache();
        if let Some(pricing) = cloud.get("unpriced_discovered_pricing") {
            runtime = runtime
                .with_unpriced_discovered_pricing(pricing)
                .map_err(|e| OxaError::new(e.code, e.message))?;
        }
    }
    if let Some(diagnostics) = diagnostics {
        runtime = runtime.with_diagnostics(diagnostics);
    }
    let timeout = crate::dispatch::stream_silence_timeout(effective).map(Duration::from_millis);
    let gateway = Gateway::with_read_timeout(runtime, timeout).map_err(|e| {
        let refusal = e.refusal();
        OxaError::new(refusal.code, e.to_string())
    })?;
    // Validate guest selection before discovery can read provider credentials.
    let mut gateway = configure(gateway)?;
    let go_enabled = go_enabled.unwrap_or_else(|| go_state(user, dirs).refusal().is_none());
    let mut refresh_go = None;
    let mut go_failure = None;
    let mut refresh_cloud = Vec::new();
    let mut notices = Vec::new();
    let discovery_timeout = Duration::from_secs(15);
    for source in sources {
        let provider = source.provider().as_str();
        match gateway.discover_cloud(&source, discovery_timeout) {
            Ok(Some(status)) => {
                if status.state == State::Stale {
                    notices.push(format!("Using a stale {provider} model catalog."));
                    refresh_cloud.push(source);
                }
                if status.missing_price + status.missing_limits > 0 {
                    notices.push(format!(
                        "{provider} catalog omitted {} offers with unknown pricing and {} with unknown limits.",
                        status.missing_price, status.missing_limits,
                    ));
                }
                if status.estimated_price > 0 {
                    notices.push(format!(
                        "{provider} catalog uses owner-estimated prices for {} models.",
                        status.estimated_price,
                    ));
                }
            }
            Ok(None) => {}
            Err(_) => notices.push(format!(
                "{provider} catalog refresh failed. Using configured models."
            )),
        }
    }
    if go_enabled {
        match discover_go(&mut gateway, cloud) {
            Ok((State::Stale, base)) => {
                refresh_go = Some(base.to_owned());
                notices.push("Using a stale OpenCode model catalog.".into());
            }
            Ok(_) => {}
            Err(error) if gateway.discovery().next().is_some() => go_failure = Some(error),
            Err(error) => return Err(error),
        }
    }
    let gateway = Arc::new(gateway);
    if refresh_go.is_some() || !refresh_cloud.is_empty() {
        let refresh = gateway.clone();
        let _ = std::thread::Builder::new().spawn(move || {
            if let Some(base) = refresh_go {
                let _ = refresh.refresh_opencode(&base, Duration::from_secs(30));
            }
            for source in refresh_cloud {
                let _ = refresh.refresh_cloud(&source, discovery_timeout);
            }
        });
    }
    Ok(OpenedGateway {
        gateway,
        notices,
        go_failure,
    })
}

fn discover_go<'a>(gateway: &mut Gateway, cloud: &'a Value) -> Result<(State, &'a str), OxaError> {
    let broken = |code, reason: &str| {
        OxaError::new(code, format!("OpenCode Go: broken ({reason})"))
            .with_hint(format!("User setting: {GO_SETTING}"))
    };
    let base = cloud
        .pointer("/opencode/base_url")
        .map_or(Some(oxc_gateway::opencode::GO_BASE), Value::as_str)
        .ok_or_else(|| broken(codes::INVALID, "oxa.cloud.opencode.base_url must be a URL string"))?;
    let state = gateway
        .discover_opencode(base, Duration::from_secs(30))
        .map_err(|error| broken(error.refusal().code, &error.to_string()))?;
    Ok((state, base))
}

// One canonical cap object shared with the other subscription routes. Refresh
// identity/timeouts are separate and never supply an implicit/unlimited cap.
fn subscription_options(
    cloud: &Value,
    needed: bool,
) -> Result<(Option<Subscription>, Option<SubscriptionLimits>), OxaError> {
    let parsed = oxc_gateway::openai::subscription_config::parse(cloud, needed).map_err(|_| invalid())?;
    let subscription = parsed.refresh.map(|refresh| Subscription {
        client_id: refresh.client_id.into(),
        refresh_endpoint: refresh.endpoint.into(),
        timeout: refresh.timeout,
    });
    let limits = parsed
        .limits
        .map(|(requests, tokens)| SubscriptionLimits::new(requests, tokens).map_err(|_| invalid()))
        .transpose()?;
    Ok((subscription, limits))
}

#[cfg(test)]
mod subscription_factory_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn canonical_caps_and_public_refresh_defaults_are_separate() {
        let cloud = json!({"subscription_limits":{"daily_requests":3,"daily_tokens":100000}});
        let (refresh, caps) = subscription_options(&cloud, true).unwrap();
        let refresh = refresh.unwrap();
        assert_eq!(refresh.client_id, "app_EMoamEEZ73f0CkXaXp7hrann");
        assert_eq!(refresh.refresh_endpoint, "https://auth.openai.com/oauth/token");
        assert_eq!(refresh.timeout, Duration::from_secs(15));
        let caps = caps.unwrap();
        assert_eq!((caps.requests(), caps.tokens()), (3, 100000));
        let (refresh, caps) = subscription_options(
            &json!({"subscription":{"daily_requests":99,"daily_tokens":999}}),
            true,
        )
        .unwrap();
        assert!(refresh.is_some());
        assert!(caps.is_none(), "refresh object is never a second cap setting");
        let (refresh, caps) = subscription_options(&cloud, false).unwrap();
        assert!(refresh.is_none());
        assert!(
            caps.is_some(),
            "Go shares the same canonical limits without ChatGPT refresh"
        );
    }
    #[test]
    fn malformed_caps_ids_timeouts_and_refresh_redirects_refuse_locally() {
        for value in [
            json!(null),
            json!({}),
            json!({"daily_requests":0,"daily_tokens":1}),
            json!({"daily_requests":1,"daily_tokens":0}),
            json!({"daily_requests":-1,"daily_tokens":1}),
            json!({"daily_requests":1.5,"daily_tokens":1}),
            json!({"daily_requests":1,"daily_tokens":"1"}),
        ] {
            assert!(subscription_options(&json!({"subscription_limits":value}), true).is_err());
        }
        for id in ["", "bad id", "bad/id", "bad\nid", "λ", &"a".repeat(129)] {
            assert!(subscription_options(&json!({"subscription":{"client_id":id}}), true).is_err());
        }
        for id in ["A", "synthetic-client_1.2", &"a".repeat(128)] {
            let (refresh, _) = subscription_options(&json!({"subscription":{"client_id":id}}), true).unwrap();
            assert_eq!(refresh.unwrap().client_id, id);
        }
        for value in [json!(0), json!(-1), json!(1.5), json!("1000")] {
            assert!(
                subscription_options(&json!({"subscription":{"refresh_timeout_ms":value}}), true).is_err()
            );
        }
        for endpoint in [
            "https://example.invalid/token",
            "http://127.0.0.1:9/token",
            "https://auth.openai.com/oauth/token",
        ] {
            assert!(
                subscription_options(&json!({"subscription":{"refresh_endpoint":endpoint}}), true).is_err()
            );
        }
        assert!(subscription_options(&json!({"subscription":false}), true).is_err());
    }
}
