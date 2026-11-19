//! One admission/stream/settlement path, with Done only after a durable row.
#[cfg(test)]
#[path = "../tests/common/openrouter_cases.rs"]
mod openrouter_tests;
#[cfg(test)]
#[path = "../tests/common/gateway_cases.rs"]
mod tests;
use crate::{
    accounting::{Outcome, Usage},
    anthropic_client::{self, Cancellation, Chat, Client, Control},
    catalog::Provider,
    catalog_discovery::{Source, Status},
    ledger::Context,
    policy::Refusal,
    runtime::{Call, HistorySeal, Runtime},
};
mod completion;
#[cfg(feature = "upstream-openai")]
mod openai;
use serde_json::{json, Value};
use std::{
    fmt,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub enum Event {
    Chunk(Value),
    Warning(&'static str),
    Done,
}

pub struct Completed {
    pub assistant: Value,
    pub usage: Option<Usage>,
    pub output_items: Arc<[Value]>,
    pub replay_items: Arc<[Value]>,
    pub finish_reason: &'static str,
    /// Bind only to the engine's actual assembled assistant, after settlement.
    pub history: Option<HistorySeal>,
}

#[derive(Debug)]
pub enum Error {
    Refused(Refusal),
    Upstream(anthropic_client::Error),
    OpenCode(crate::opencode::client::Error),
    #[cfg(feature = "upstream-openai")]
    OpenAi(crate::openai::client::Error),
}
impl Error {
    pub fn refusal(&self) -> Refusal {
        let error = match self {
            Self::Refused(refusal) => return refusal.clone(),
            Self::OpenCode(error) => return go_refusal(*error),
            Self::Upstream(error) => error,
            #[cfg(feature = "upstream-openai")]
            Self::OpenAi(error) => return completion::openai_refusal(error),
        };
        let (status, code, param) = match error {
            anthropic_client::Error::Request(param) => (400, "invalid_value", *param),
            anthropic_client::Error::Cancelled => (499, "request_cancelled", ""),
            anthropic_client::Error::Setup => (503, "cloud_transport_unavailable", ""),
            anthropic_client::Error::Endpoint => (503, "cloud_upstream_unavailable", "model"),
            anthropic_client::Error::Http(status) => (
                if (400..600).contains(status) { *status } else { 502 },
                "cloud_upstream_error",
                "",
            ),
            anthropic_client::Error::Protocol => (502, "cloud_upstream_stream_broken", ""),
            anthropic_client::Error::TooLarge => (502, "cloud_upstream_too_large", ""),
            anthropic_client::Error::Transport => (502, "cloud_upstream_unavailable", ""),
        };
        Refusal {
            status,
            code,
            param,
            message: match error {
                anthropic_client::Error::Cancelled => "Request cancelled.",
                anthropic_client::Error::Request(_) => "Invalid cloud request.",
                _ => "The cloud upstream request failed.",
            },
        }
    }
    pub fn retry_after(&self) -> Option<u32> {
        match self {
            Self::OpenCode(crate::opencode::client::Error::Refused(
                crate::opencode::Refusal::RateLimited { retry_after },
            )) => *retry_after,
            _ => None,
        }
    }
    pub fn body(&self) -> Value {
        self.refusal().body()
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.refusal().message)?;
        if let Some(seconds) = self.retry_after() {
            write!(f, " (retry after {seconds} s)")?;
        }
        Ok(())
    }
}
impl std::error::Error for Error {}

pub struct Gateway {
    runtime: Runtime,
    client: Client,
    go_base: Option<String>,
    go_usage: tokio::sync::Mutex<Option<CachedGoUsage>>,
    #[cfg(feature = "upstream-openai")]
    openai: crate::openai::client::Client,
}

struct CachedGoUsage {
    account: [u8; 32],
    fetched_at: std::time::Instant,
    observation: crate::opencode::PlanUsageObservation,
}

impl CachedGoUsage {
    fn fresh(&self, account: [u8; 32], max_age: Duration) -> bool {
        let Ok(now) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) else {
            return false;
        };
        let usage = &self.observation.usage;
        self.account == account
            && self.fetched_at.elapsed() < max_age
            && [&usage.rolling, &usage.weekly, &usage.monthly]
                .into_iter()
                .flatten()
                .all(|window| window.resets_at.is_none_or(|reset| reset > now.as_secs()))
    }
}

impl Gateway {
    pub fn new(runtime: Runtime) -> Result<Self, Error> {
        Self::with_read_timeout(runtime, Some(Duration::from_secs(1800)))
    }
    pub fn with_read_timeout(runtime: Runtime, timeout: Option<Duration>) -> Result<Self, Error> {
        let client = Client::with_read_timeout(timeout).map_err(Error::Upstream)?;
        #[cfg(feature = "upstream-openai")]
        let openai = crate::openai::client::Client::from_messages_transport(&client);
        Ok(Self {
            runtime,
            client,
            go_base: None,
            go_usage: tokio::sync::Mutex::new(None),
            #[cfg(feature = "upstream-openai")]
            openai,
        })
    }
    /// Startup-only bounded live discovery. Reuses this gateway's HTTP pool;
    /// the temporary current-thread driver creates no second queue or daemon.
    /// A bounded stale-aware cache entry is consumed before any network call.
    pub fn discover_opencode(
        &mut self,
        base: &str,
        timeout: Duration,
    ) -> Result<crate::opencode::cache::State, Error> {
        if tokio::runtime::Handle::try_current().is_ok() {
            return Err(Error::OpenCode(crate::opencode::client::Error::Request));
        }
        let driver = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| Error::OpenCode(crate::opencode::client::Error::Transport))?;
        let result = driver.block_on(self.runtime.discover_go(self.client.transport(), base, timeout));
        driver.shutdown_timeout(Duration::from_secs(1));
        if result.is_ok() {
            if self.go_base.as_deref() != Some(base) {
                *self.go_usage.get_mut() = None;
            }
            self.go_base = Some(base.to_owned());
            if let Some(diagnostics) = self.runtime.diagnostics() {
                diagnostics.clear();
            }
        }
        result.map_err(Error::OpenCode)
    }
    /// Read Go Usage independently of chat, with the default one-minute freshness.
    pub async fn opencode_plan_usage(
        &self,
        timeout: Duration,
        cancel: &Cancellation,
    ) -> Result<Option<crate::opencode::PlanUsage>, Error> {
        self.opencode_plan_observation(timeout, Duration::from_secs(60), cancel)
            .await
            .map(|observation| observation.map(|observation| observation.usage))
    }

    /// Read a current Go Usage observation with owner-selected freshness.
    /// A zero maximum age explicitly refreshes. Only successful typed snapshots
    /// are reused, scoped to the current protected credential and discovery base.
    pub async fn opencode_plan_observation(
        &self,
        timeout: Duration,
        max_age: Duration,
        cancel: &Cancellation,
    ) -> Result<Option<crate::opencode::PlanUsageObservation>, Error> {
        use crate::opencode::{client, PlanUsageObservation};
        let Some(base) = &self.go_base else {
            return Ok(None);
        };
        let operation = async {
            let mut cache = self.go_usage.lock().await;
            let Some(key) = self
                .runtime
                .catalog_key(crate::catalog::Provider::OpenCode)
                .map_err(Error::Refused)?
            else {
                *cache = None;
                return Ok(None);
            };
            let account = key.account_scope();
            if let Some(hit) = cache.as_ref().filter(|hit| hit.fresh(account, max_age)) {
                return Ok(Some(hit.observation.clone()));
            }
            *cache = None;
            let observation = client::plan_usage(self.client.transport(), base, &key, timeout, cancel)
                .await
                .map_err(Error::OpenCode)?
                .map(|usage| PlanUsageObservation {
                    usage,
                    observed_at: std::time::SystemTime::now(),
                });
            if let Some(observation) = &observation {
                *cache = Some(CachedGoUsage {
                    account,
                    fetched_at: std::time::Instant::now(),
                    observation: observation.clone(),
                });
            }
            Ok(observation)
        };
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(Error::OpenCode(client::Error::Cancelled)),
            result = tokio::time::timeout(timeout, operation) =>
                result.map_err(|_| Error::OpenCode(client::Error::Transport))?,
        }
    }
    /// One bounded background refresh of the Go catalog cache, sharing this
    /// gateway's existing HTTP pool. It never mutates the live catalog, so a
    /// failure cannot make any other local or configured route fatal.
    pub fn refresh_opencode(&self, base: &str, timeout: Duration) -> Result<(), Error> {
        if tokio::runtime::Handle::try_current().is_ok() {
            return Err(Error::OpenCode(crate::opencode::client::Error::Request));
        }
        let driver = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| Error::OpenCode(crate::opencode::client::Error::Transport))?;
        let result = driver.block_on(self.runtime.refresh_go(self.client.transport(), base, timeout));
        driver.shutdown_timeout(Duration::from_secs(1));
        result.map_err(Error::OpenCode)
    }
    /// Owner-requested catalog discovery, using the established provider pool.
    /// Missing credentials leave the configured fallback catalog in place.
    pub fn discover_cloud(&mut self, source: &Source, timeout: Duration) -> Result<Option<Status>, Error> {
        let Some(key) = self
            .runtime
            .catalog_key(source.provider())
            .map_err(Error::Refused)?
        else {
            return Ok(None);
        };
        let directory = self.runtime.catalog_cache_dir();
        use crate::catalog_cache::State;
        let (state, offers) = match source.load(directory, &key) {
            // OpenRouter discovery can supply the rates used to reserve spend.
            Some((State::Stale, _)) if source.provider() == Provider::OpenRouter => {
                (State::Fresh, self.fetch_catalog(source, &key, timeout)?)
            }
            Some(cached) => cached,
            None => (State::Fresh, self.fetch_catalog(source, &key, timeout)?),
        };
        let (missing_price, missing_limits, estimated_price) = self
            .runtime
            .apply_provider_catalog(source, offers)
            .map_err(Error::Refused)?;
        Ok(Some(Status {
            state,
            missing_price,
            missing_limits,
            estimated_price,
        }))
    }
    /// Replace only the private cache on a stale refresh.
    pub fn refresh_cloud(&self, source: &Source, timeout: Duration) -> Result<(), Error> {
        if let Some(key) = self
            .runtime
            .catalog_key(source.provider())
            .map_err(Error::Refused)?
        {
            self.fetch_catalog(source, &key, timeout)?;
        }
        Ok(())
    }
    fn fetch_catalog(
        &self,
        source: &Source,
        key: &crate::secrets::Key,
        timeout: Duration,
    ) -> Result<Vec<crate::catalog_discovery::Offer>, Error> {
        if tokio::runtime::Handle::try_current().is_ok() {
            return Err(Error::Upstream(anthropic_client::Error::Request("discovery")));
        }
        let driver = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| Error::Upstream(anthropic_client::Error::Setup))?;
        let result =
            driver.block_on(source.fetch(&self.client, key, timeout, self.runtime.catalog_cache_dir()));
        driver.shutdown_timeout(Duration::from_secs(1));
        result.map_err(Error::Upstream)
    }
    pub fn discovery(&self) -> impl Iterator<Item = crate::catalog::ModelInfo<'_>> {
        self.runtime.discovery()
    }

    #[cfg(feature = "share")]
    pub fn with_metered_guest_models(
        mut self,
        aliases: std::collections::BTreeSet<String>,
    ) -> Result<Self, Error> {
        self.runtime
            .set_metered_guest_models(aliases)
            .map_err(Error::Refused)?;
        Ok(self)
    }

    #[cfg(feature = "share")]
    pub fn metered_guest_models(&self) -> impl Iterator<Item = crate::catalog::ModelInfo<'_>> {
        self.runtime.metered_guest_models()
    }

    #[cfg(feature = "share")]
    pub(crate) async fn chat_authenticated_guest<F>(
        &self,
        request: &oxa_proto::ChatRequest<'_>,
        guest: &crate::share::listener::Guest<'_>,
        temperature: Option<f64>,
        top_p: Option<f64>,
        cancel: &Cancellation,
        on: &mut F,
    ) -> Result<Completed, Error>
    where
        F: FnMut(Event) -> Control + ?Sized,
    {
        if request.messages.iter().any(|message| {
            !matches!(
                message.role,
                oxa_proto::Role::User | oxa_proto::Role::Assistant | oxa_proto::Role::System
            ) || message.tool_call_id.is_some()
                || !message.tool_calls.is_empty()
                || message.reasoning.is_some()
        }) {
            return Err(Error::Refused(guest_history_refusal()));
        }
        let mut payload = payload(request).map_err(Error::Refused)?;
        for (field, value) in [("temperature", temperature), ("top_p", top_p)] {
            if let Some(value) = value {
                payload[field] = json!(value);
            }
        }
        if !payload["tools"].as_array().is_some_and(Vec::is_empty) {
            return Err(Error::Refused(guest_history_refusal()));
        }
        if cancel.is_cancelled() {
            return Err(Error::Upstream(anthropic_client::Error::Cancelled));
        }
        let call = self
            .runtime
            .begin_authenticated_guest(&payload, guest)
            .map_err(Error::Refused)?;
        self.forward(call, &payload, None, cancel, on).await
    }

    /// Admit, read the current key, reserve, forward, meter, and settle exactly
    /// once. Done is sent only after the private ledger writer has succeeded.
    pub async fn chat<F>(
        &self,
        request: &oxa_proto::ChatRequest<'_>,
        context: Context<'_>,
        cancel: &Cancellation,
        on: &mut F,
    ) -> Result<Completed, Error>
    where
        F: FnMut(Event) -> Control + ?Sized,
    {
        if cancel.is_cancelled() {
            return Err(Error::Upstream(anthropic_client::Error::Cancelled));
        }
        let payload = match payload(request) {
            Ok(payload) => payload,
            Err(error) => {
                self.runtime.refused(request.model, context, &error);
                return Err(Error::Refused(error));
            }
        };
        self.chat_payload_with_history(&payload, Some(request), context, cancel, on)
            .await
    }
    #[cfg(test)]
    async fn chat_payload<F>(
        &self,
        payload: &Value,
        context: Context<'_>,
        cancel: &Cancellation,
        on: &mut F,
    ) -> Result<Completed, Error>
    where
        F: FnMut(Event) -> Control + ?Sized,
    {
        self.chat_payload_with_history(payload, None, context, cancel, on)
            .await
    }
    async fn chat_payload_with_history<F>(
        &self,
        payload: &Value,
        history: Option<&oxa_proto::ChatRequest<'_>>,
        context: Context<'_>,
        cancel: &Cancellation,
        on: &mut F,
    ) -> Result<Completed, Error>
    where
        F: FnMut(Event) -> Control + ?Sized,
    {
        if cancel.is_cancelled() {
            return Err(Error::Upstream(anthropic_client::Error::Cancelled));
        }
        let call = self.runtime.begin(payload, context).map_err(Error::Refused)?;
        if let Some(request) = history {
            if let Err(error) = call.verify_history(request) {
                call.finish(Outcome::Complete, error.status, Some(error.code))
                    .map_err(Error::Refused)?;
                return Err(Error::Refused(error));
            }
        }
        if let Some(warning) = call.budget_warning() {
            if on(Event::Warning(warning)) == Control::Stop {
                return self.finish(
                    call,
                    Err(Error::Upstream(anthropic_client::Error::Cancelled)),
                    None,
                    false,
                    on,
                );
            }
        }
        self.forward(call, payload, history, cancel, on).await
    }

    async fn forward<F>(
        &self,
        mut call: Call<'_>,
        payload: &Value,
        history: Option<&oxa_proto::ChatRequest<'_>>,
        cancel: &Cancellation,
        on: &mut F,
    ) -> Result<Completed, Error>
    where
        F: FnMut(Event) -> Control + ?Sized,
    {
        let model = call.model();
        #[cfg(feature = "upstream-openai")]
        if matches!(model.info().provider, Provider::OpenAi | Provider::ChatGpt) {
            return self.openai_chat(call, payload, history, cancel, on).await;
        }
        if model.info().provider == Provider::OpenCode {
            return self.chat_go(call, payload, cancel, on).await;
        }
        // Transport dispatch is added with the provider client; an admitted
        // foreign route must never be sent using the Anthropic protocol.
        if !matches!(model.info().provider, Provider::Anthropic | Provider::OpenRouter) {
            let error = Refusal {
                status: 503,
                code: "cloud_provider_unavailable",
                param: "model",
                message: "This provider transport is not available in this build.",
            };
            call.finish(Outcome::Complete, error.status, Some(error.code))
                .map_err(Error::Refused)?;
            return Err(Error::Refused(error));
        }
        let key = call.key_handle().map_err(Error::Refused)?;
        let mut terminal_usage = None;
        let mut terminal_reported_cost = None;
        let request = Chat {
            base_url: model.endpoint().as_str(),
            upstream_model: model.upstream(),
            alias: model.info().id,
            max_output: Some(call.max_output()),
            payload,
            created: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |t| t.as_secs()),
            provider: self.runtime.openrouter_provider(),
        };
        let mut forwarding_error = None;
        let mut deliver = |event| match event {
            anthropic_client::Event::Sending => {
                if cancel.is_cancelled() {
                    return Control::Stop;
                }
                if let Err(error) = call.mark_forwarded() {
                    forwarding_error = Some(error);
                    return Control::Stop;
                }
                Control::Continue
            }
            anthropic_client::Event::Chunk(chunk) => {
                call.observe(&chunk);
                on(Event::Chunk(chunk))
            }
        };
        let result = if model.info().provider == Provider::OpenRouter {
            self.client
                .stream_openrouter(
                    request,
                    &key,
                    cancel,
                    &mut deliver,
                    &mut terminal_usage,
                    &mut terminal_reported_cost,
                )
                .await
        } else {
            self.client
                .stream_with_blocks(
                    request,
                    &key,
                    cancel,
                    &mut deliver,
                    |index| {
                        history?
                            .messages
                            .get(index.checked_sub(1)?)?
                            .provider_history
                            .as_ref()
                            .map(|saved| saved.items().to_vec())
                    },
                    &mut terminal_usage,
                )
                .await
        };
        if let Some(error) = forwarding_error {
            return self.finish(call, Err(Error::Refused(error)), history, false, on);
        }
        // This record survives cancellation during final chunk delivery and
        // public usage opt-out; both must settle the actual cache categories.
        if let Some(usage) = terminal_usage {
            call.metered(usage);
            if model.info().provider == Provider::OpenRouter {
                call.metered_cost(terminal_reported_cost);
            }
        }
        self.finish(
            call,
            result.map(completion::anthropic).map_err(Error::Upstream),
            history,
            terminal_usage.is_some(),
            on,
        )
    }
    fn finish<F>(
        &self,
        call: Call<'_>,
        result: Result<Completed, Error>,
        history: Option<&oxa_proto::ChatRequest<'_>>,
        usage_final: bool,
        on: &mut F,
    ) -> Result<Completed, Error>
    where
        F: FnMut(Event) -> Control + ?Sized,
    {
        let (outcome, status, code) = match &result {
            Ok(_) => (Outcome::Complete, 200, None),
            Err(error) => {
                let refusal = error.refusal();
                let outcome = if completion::cancelled(error) && usage_final {
                    Outcome::Cancelled {
                        request_bytes: 0,
                        streamed_bytes: 0,
                        usage_final: true,
                        elapsed: Duration::ZERO,
                    }
                } else if usage_final {
                    // A locally refused complete reply (for example unsafe
                    // private replay) still has trusted provider usage. Charge
                    // those facts without granting reply/replay authority.
                    Outcome::Complete
                } else if completion::clean_refusal(error)
                    || call.model().info().provider == Provider::OpenRouter
                        && matches!(error, Error::Upstream(anthropic_client::Error::Http(402)))
                {
                    Outcome::Refused
                } else {
                    Outcome::Failed
                };
                // The ledger's clean-refusal guard requires the actual 4xx.
                // Go auth is intentionally public503, but upstream401 is known
                // before generation; do not poison accounting with public503.
                let status = if matches!(
                    error,
                    Error::OpenCode(crate::opencode::client::Error::Refused(
                        crate::opencode::Refusal::Unauthenticated
                    ))
                ) {
                    401
                } else {
                    refusal.status
                };
                (outcome, status, Some(refusal.code))
            }
        };
        let pending = match (&result, history) {
            (Ok(completed), Some(request)) if !completed.replay_items.is_empty() => {
                match call.prepare_history(request, Arc::clone(&completed.replay_items)) {
                    Ok(seal) => Some(seal),
                    Err(error) => {
                        call.finish(Outcome::Complete, error.status, Some(error.code))
                            .map_err(Error::Refused)?;
                        return Err(Error::Refused(error));
                    }
                }
            }
            _ => None,
        };
        call.finish(outcome, status, code).map_err(Error::Refused)?;
        let mut completed = result?;
        completed.history = pending;
        // Settlement committed success; a late cancel cannot retract a paid turn.
        let _ = on(Event::Done);
        Ok(completed)
    }
    async fn chat_go<F>(
        &self,
        mut call: crate::runtime::Call<'_>,
        payload: &Value,
        cancel: &Cancellation,
        on: &mut F,
    ) -> Result<Completed, Error>
    where
        F: FnMut(Event) -> Control + ?Sized,
    {
        use crate::opencode::client::{self, Error as GoError};
        let alias = call.model().info().id;
        let model = self
            .runtime
            .go_model(alias)
            .ok_or(Error::OpenCode(GoError::Endpoint))?;
        let hint = crate::opencode::SessionHint::from_provider_hint(
            call.provider_session_hint().map_err(Error::Refused)?,
        )
        .map_err(|_| Error::OpenCode(GoError::Request))?;
        let key = call.key_handle().map_err(Error::Refused)?;
        let mut payload = payload.clone();
        payload["max_tokens"] = call.max_output().into();
        let mut usage = None;
        let mut forwarding_error = None;
        let result = client::stream_observed(
            self.client.transport(),
            client::Chat {
                model,
                alias,
                payload: &payload,
                key: &key,
                hint: &hint,
            },
            cancel,
            &mut |event| match event {
                client::Event::Sending => {
                    if cancel.is_cancelled() {
                        return Control::Stop;
                    }
                    if let Err(error) = call.mark_forwarded() {
                        forwarding_error = Some(error);
                        return Control::Stop;
                    }
                    Control::Continue
                }
                client::Event::Chunk(chunk) => {
                    call.observe(&chunk);
                    on(Event::Chunk(chunk))
                }
            },
            &mut usage,
            self.runtime.diagnostics(),
        )
        .await;
        if let Some(error) = forwarding_error {
            return self.finish(call, Err(Error::Refused(error)), None, false, on);
        }
        let result = self.finish_go(call, result, usage, on);
        if result.is_ok() {
            if let Some(diagnostics) = self.runtime.diagnostics() {
                diagnostics.clear();
            }
        }
        result
    }
    fn finish_go<F>(
        &self,
        mut call: crate::runtime::Call<'_>,
        result: Result<crate::anthropic_stream::Completed, crate::opencode::client::Error>,
        usage: Option<crate::opencode::Usage>,
        on: &mut F,
    ) -> Result<Completed, Error>
    where
        F: FnMut(Event) -> Control + ?Sized,
    {
        use crate::opencode::client::Error as GoError;
        // A validated terminal report survives cancellation before public delivery.
        // Provisional early usage can reach here only after a clean [DONE].
        call.metered_go(usage.as_ref());
        // Go chat completions have no native replay items. The common request
        // path already verifies incoming history before dispatch; the common
        // finish path owns durable settlement and the final Done transition.
        let result = result
            .map(|completed| Completed {
                assistant: completed.assistant,
                usage: None,
                output_items: Arc::from([]),
                replay_items: Arc::from([]),
                finish_reason: completed.finish_reason,
                history: None,
            })
            .map_err(|error| {
                if error == GoError::Cancelled {
                    Error::Upstream(anthropic_client::Error::Cancelled)
                } else {
                    Error::OpenCode(error)
                }
            });
        self.finish(call, result, None, usage.is_some(), on)
    }
}

/// Provider conversion consumes the same borrowed request the core passed to
/// Engine. No request body or stream chunk is encoded/reparsed across that boundary.
#[cfg(feature = "share")]
fn guest_history_refusal() -> Refusal {
    Refusal {
        status: 403,
        code: "guest_chat_only",
        param: "messages",
        message: "Guest cloud sharing supports text chat only.",
    }
}

fn payload(request: &oxa_proto::ChatRequest<'_>) -> Result<Value, Refusal> {
    let mut messages = Vec::with_capacity(request.messages.len() + 1);
    messages.push(json!({"role":"system","content":request.prefix.system_text()}));
    for message in request.messages {
        let mut value = json!({"role":message.role.as_str(),"content":message.content});
        if let Some(id) = &message.tool_call_id {
            value["tool_call_id"] = json!(id);
        }
        if let Some(reasoning) = &message.reasoning {
            value["reasoning_content"] = json!(reasoning);
        }
        if !message.tool_calls.is_empty() {
            value["tool_calls"] = message
                .tool_calls
                .iter()
                .map(|call| {
                    json!({"id":call.id,
                "type":"function","function":{"name":call.name,"arguments":call.args_raw}})
                })
                .collect();
        }
        messages.push(value);
    }
    let effort = match request.effective_level() {
        oxa_proto::ThinkingLevel::Off => "none",
        oxa_proto::ThinkingLevel::Minimal => "minimal",
        oxa_proto::ThinkingLevel::Low => "low",
        oxa_proto::ThinkingLevel::Medium => "medium",
        _ => "high",
    };
    // Tools are already stored as canonical JSON in FrozenPrefix. Only that
    // schema fragment needs decoding; message history stays typed at the boundary.
    let tools = serde_json::from_str::<Value>(&request.prefix.tools_json)
        .ok()
        .filter(Value::is_array)
        .ok_or(Refusal {
            status: 400,
            code: "invalid_value",
            param: "tools",
            message: "Invalid tool definitions.",
        })?;
    Ok(json!({"model":request.model,"messages":messages,"tools":tools,
        "max_tokens":request.max_tokens,"reasoning_effort":effort,
        "stream":true,"stream_options":{"include_usage":true}}))
}

fn go_refusal(error: crate::opencode::client::Error) -> Refusal {
    use crate::opencode::{client::Error as E, Refusal as R};
    let (status, code, message) = match error {
        E::Refused(R::RegionBlocked) => (
            403,
            "region_blocked",
            "This Go model is blocked by the workspace region policy.",
        ),
        E::Refused(R::RateLimited { .. }) => (
            429,
            "cloud_rate_limited",
            "The Go model is rate limited; the request can be retried.",
        ),
        E::Refused(R::Unauthenticated) => (
            503,
            "cloud_credentials_unavailable",
            "OpenCode Go credentials are unavailable.",
        ),
        E::Refused(R::Forbidden) => (403, "cloud_forbidden", "OpenCode Go refused this request."),
        E::Refused(R::Other(status)) => (
            if (400..600).contains(&status) { status } else { 502 },
            "cloud_upstream_error",
            "The Go upstream request failed.",
        ),
        E::Discovery(problem) => (502, "cloud_discovery_invalid", problem.message()),
        E::Cancelled => (499, "request_cancelled", "Request cancelled."),
        E::Request => (400, "invalid_value", "Invalid Go configuration or request."),
        E::Endpoint => (
            503,
            "cloud_provider_unavailable",
            "This Go model has no supported chat route.",
        ),
        E::Protocol | E::TooLarge => (
            502,
            "cloud_upstream_stream_broken",
            "Invalid or incomplete Go response.",
        ),
        E::Transport => (502, "cloud_upstream_unavailable", "The Go connection failed."),
    };
    Refusal {
        status,
        code,
        message,
        param: "model",
    }
}
