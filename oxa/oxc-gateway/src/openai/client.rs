//! Responses transport only. The caller owns routing, admission and settlement.
//! No ambient credential discovery, sign-in, continuation cache or transport Done.
use super::{chatgpt_request, chatgpt_tokens, request, stream};
use crate::{accounting::Usage, rate_limits, secrets::Key, sse::Framer};
use futures_util::StreamExt;
use reqwest::{
    header::{HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE},
    Url,
};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fmt,
    path::Path,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const MAX_BODY: usize = 64 * 1024 * 1024;
const MAX_ERROR: usize = 64 * 1024;
const REFRESH_TIMEOUT: Duration = Duration::from_secs(30);
pub const API_BASE: &str = "https://api.openai.com/v1";
pub const SUBSCRIPTION_BASE: &str = "https://chatgpt.com/backend-api/codex";

pub use crate::anthropic_client::Cancellation;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    Continue,
    Stop,
}
pub enum Event {
    Sending,
    Chunk(Value),
}

/// Refresh configuration is supplied by the trusted owner, never model data.
/// A firm 30-second upper bound applies to the complete HTTP exchange under lock.
#[derive(Clone)]
pub struct Refresh<'a> {
    pub endpoint: &'a str,
    pub client_id: &'a str,
    pub timeout: Duration,
}
#[derive(Clone)]
pub enum Auth<'a> {
    ApiKey(&'a Key),
    Subscription {
        token_path: &'a Path,
        /// Stable account selected and scoped by the caller before admission.
        expected_account_id: &'a str,
        refresh: Refresh<'a>,
    },
}
impl fmt::Debug for Auth<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Auth([redacted])")
    }
}
impl fmt::Debug for Refresh<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Refresh([redacted])")
    }
}
#[derive(Clone, Copy)]
pub struct Chat<'a> {
    pub base_url: &'a str,
    pub upstream_model: &'a str,
    pub alias: &'a str,
    pub max_output: Option<u64>,
    pub payload: &'a Value,
    /// Explicit per-assistant-turn replay from the caller's persisted history.
    pub replay: &'a [Option<request::ReplayItems>],
    pub created: u64,
}
pub struct Completed {
    pub assistant: Value,
    /// All ordered provider output, including unexecuted/truncated items.
    pub output_items: Arc<[Value]>,
    /// Replay authority; empty for an unexecuted truncated tool call. The caller
    /// then starts a fresh user turn from visible text, without tool results.
    /// Shares output_items' allocation when replay is authorized.
    pub replay_items: Arc<[Value]>,
    pub usage: Option<Usage>,
    pub finish_reason: &'static str,
}
#[derive(Clone, PartialEq, Eq)]
pub enum Refusal {
    Api(rate_limits::Refusal),
    Subscription(rate_limits::chatgpt::Refusal),
}
impl fmt::Debug for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Api(refusal) => f.debug_tuple("Api").field(refusal).finish(),
            Self::Subscription(rate_limits::chatgpt::Refusal::UsageNotIncluded) => {
                f.write_str("Subscription(UsageNotIncluded)")
            }
            Self::Subscription(rate_limits::chatgpt::Refusal::UsageLimitReached {
                resets_at,
                retry_after,
                ..
            }) => f
                .debug_struct("SubscriptionUsageLimitReached")
                .field("plan_type", &"[redacted]")
                .field("resets_at", resets_at)
                .field("retry_after", retry_after)
                .finish(),
        }
    }
}
// Keep all diagnostic fields typed/fixed; never retain upstream text or URLs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    Setup,
    Endpoint,
    Request(&'static str),
    Credentials,
    /// Subscription refresh/recovery failed; status surfaces must show signed out.
    SignedOut,
    Refresh,
    Http {
        status: u16,
        refusal: Option<Refusal>,
    },
    Transport,
    Protocol,
    TooLarge,
    Cancelled,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Setup => f.write_str("OpenAI transport unavailable"),
            Self::Endpoint => f.write_str("Invalid OpenAI endpoint"),
            Self::Request(param) => write!(f, "Invalid OpenAI request field: {param}"),
            Self::Credentials => f.write_str("OpenAI credentials unavailable"),
            Self::SignedOut => f.write_str("ChatGPT signed out; sign in again"),
            Self::Refresh => f.write_str("OpenAI token refresh failed"),
            Self::Http { status, .. } => write!(f, "OpenAI upstream answered HTTP {status}"),
            Self::Transport => f.write_str("OpenAI connection failed"),
            Self::Protocol => f.write_str("Invalid or incomplete OpenAI stream"),
            Self::TooLarge => f.write_str("OpenAI body exceeds size limit"),
            Self::Cancelled => f.write_str("OpenAI request cancelled"),
        }
    }
}
impl std::error::Error for Error {}
impl Error {
    pub fn subscription_state(&self) -> Option<crate::status::State> {
        matches!(self, Self::SignedOut).then_some(crate::status::State::SignedOut)
    }
}
// Secret-bearing recovery state is local to one logical request, never an error
// or Debug value. No continuation/session state is kept on Client.
#[derive(Default)]
struct Recovery {
    rejected: Option<String>,
    retired: Vec<String>,
    expired: bool,
}
#[derive(Clone)]
pub struct Client(reqwest::Client);
impl Client {
    pub fn new() -> Result<Self, Error> {
        Self::with_read_timeout(Some(Duration::from_secs(1800)))
    }
    /// Use the caller's selected silence timeout for each upstream read, never
    /// a total-generation deadline. None disables the read timeout; connection
    /// setup and the complete credential-refresh exchange remain bounded.
    pub fn with_read_timeout(read_timeout: Option<Duration>) -> Result<Self, Error> {
        let mut builder = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(30));
        if let Some(timeout) = read_timeout {
            if std::time::Instant::now().checked_add(timeout).is_none() {
                return Err(Error::Setup);
            }
            builder = builder.read_timeout(timeout);
        }
        builder.build().map(Self).map_err(|_| Error::Setup)
    }
    pub async fn stream<F>(
        &self,
        chat: Chat<'_>,
        auth: Auth<'_>,
        cancel: &Cancellation,
        on: &mut F,
    ) -> Result<Completed, Error>
    where
        F: FnMut(Event) -> Control + ?Sized,
    {
        let mut usage = None;
        self.stream_with_usage(chat, auth, cancel, on, &mut usage).await
    }
    /// Private terminal usage is published before a delivery callback may cancel.
    /// It does not imply the caller has durably settled the request.
    pub async fn stream_with_usage<F>(
        &self,
        chat: Chat<'_>,
        auth: Auth<'_>,
        cancel: &Cancellation,
        on: &mut F,
        terminal_usage: &mut Option<Usage>,
    ) -> Result<Completed, Error>
    where
        F: FnMut(Event) -> Control + ?Sized,
    {
        let mut recovery = Recovery::default();
        for attempt in 0..2 {
            recovery.expired = false;
            let result = self
                .stream_attempt(chat, auth.clone(), cancel, on, terminal_usage, &mut recovery)
                .await;
            if recovery.expired && attempt == 0 {
                continue;
            }
            return result;
        }
        unreachable!("bounded recovery always returns")
    }
    async fn stream_attempt<F: FnMut(Event) -> Control + ?Sized>(
        &self,
        chat: Chat<'_>,
        auth: Auth<'_>,
        cancel: &Cancellation,
        on: &mut F,
        terminal_usage: &mut Option<Usage>,
        recovery: &mut Recovery,
    ) -> Result<Completed, Error> {
        *terminal_usage = None;
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let subscription = matches!(auth, Auth::Subscription { .. });
        let endpoint = responses_endpoint(chat.base_url, subscription)?;
        // Refuse invalid/missing replay before any token-file access or refresh.
        let mode = if subscription {
            request::Mode::Subscription
        } else {
            request::Mode::Api
        };
        let mut body = request::translate(
            chat.payload,
            chat.upstream_model,
            chat.max_output,
            mode,
            chat.replay,
        )
        .map_err(|e| Error::Request(e.param))?;
        if subscription {
            body = chatgpt_request::adapt(body);
        }
        body["stream"] = Value::Bool(true);
        let body = serde_json::to_vec(&body).map_err(|_| Error::Protocol)?;
        if body.len() > MAX_BODY {
            return Err(Error::TooLarge);
        }
        let record;
        let mut sensitive = Vec::new();
        let (access, account) = match auth {
            Auth::ApiKey(key) => {
                sensitive.push(key.expose());
                (key.expose(), None)
            }
            Auth::Subscription {
                token_path,
                expected_account_id,
                refresh,
            } => {
                let credentials = self
                    .credentials(
                        token_path,
                        expected_account_id,
                        refresh,
                        cancel,
                        recovery.rejected.as_deref(),
                    )
                    .await?;
                record = credentials.0;
                recovery.retired.extend(credentials.1);
                recovery.retired.extend(
                    [record.access_token(), record.refresh_token(), record.account_id()].map(str::to_owned),
                );
                sensitive.extend(recovery.retired.iter().map(String::as_str));
                sensitive.extend([record.access_token(), record.refresh_token(), record.account_id()]);
                (record.access_token(), Some(record.account_id()))
            }
        };
        let mut bearer =
            HeaderValue::from_str(&format!("Bearer {access}")).map_err(|_| Error::Credentials)?;
        bearer.set_sensitive(true);
        let mut request = self
            .0
            .post(endpoint)
            .header(AUTHORIZATION, bearer)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "text/event-stream");
        if let Some(account) = account {
            for (name, value) in chatgpt_request::headers(account) {
                let mut value = HeaderValue::from_str(value).map_err(|_| Error::Credentials)?;
                value.set_sensitive(true);
                request = request.header(name, value);
            }
        }
        let request = request.body(body).build().map_err(|_| Error::Endpoint)?;
        if cancel.is_cancelled() || on(Event::Sending) == Control::Stop {
            return Err(Error::Cancelled);
        }
        let response = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(Error::Cancelled),
            response = self.0.execute(request) => response.map_err(|_|Error::Transport)?,
        };
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let headers = response.headers().clone();
            let bytes = bounded(response, MAX_ERROR, cancel).await?;
            if subscription && status == 401 {
                if serde_json::from_slice::<Value>(&bytes).ok().is_some_and(|v| {
                    v["error"]["code"] == "token_expired" || v["error"]["type"] == "token_expired"
                }) {
                    recovery.rejected = Some(access.to_owned());
                    recovery.expired = true;
                }
                return Err(Error::SignedOut);
            }
            let refusal = if subscription {
                rate_limits::chatgpt::classify_chatgpt_refusal(status, &bytes, &headers)
                    .map(Refusal::Subscription)
            } else {
                rate_limits::classify_openai_refusal(status, &bytes, &headers).map(Refusal::Api)
            };
            // Classifier metadata may contain provider-owned strings (plan_type).
            // Scrub those too before constructing a Debug-visible refusal.
            let refusal = scrub_refusal(refusal, &sensitive);
            return Err(Error::Http { status, refusal });
        }
        if !response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| {
                v.split(';')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .eq_ignore_ascii_case("text/event-stream")
            })
        {
            return Err(Error::Protocol);
        }
        let include_usage = chat.payload.pointer("/stream_options/include_usage") == Some(&Value::Bool(true));
        let mut translator = stream::Stream::new(chat.alias, chat.created, include_usage);
        let mut source = response.bytes_stream();
        let mut decoder = Framer::default();
        let mut received = 0usize;
        let mut redactor = Fields::new(sensitive);
        // No executable call escapes until terminal replay authority is known.
        let mut pending_tools = Vec::new();
        loop {
            let bytes = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(Error::Cancelled),
                next = source.next() => next.ok_or(Error::Protocol)?.map_err(|_|Error::Transport)?,
            };
            for byte in bytes {
                received += 1;
                if received > MAX_BODY {
                    return Err(Error::TooLarge);
                }
                let Some(data) = decoder.push(byte) else {
                    continue;
                };
                let event: Value = serde_json::from_slice(&data).map_err(|_| Error::Protocol)?;
                drop(data);
                let terminal = matches!(
                    event["type"].as_str(),
                    Some("response.completed" | "response.incomplete" | "response.failed" | "error")
                );
                let chunks = translator.event(&event);
                // A normalized error is the translator's rejection signal.
                // Do not wait for a terminal event/EOF from a peer held open.
                if chunks.iter().any(|chunk| chunk.get("error").is_some()) {
                    return Err(if cancel.is_cancelled() {
                        Error::Cancelled
                    } else {
                        Error::Protocol
                    });
                }
                if terminal {
                    let native = translator.finish();
                    if native.failed {
                        return Err(Error::Protocol);
                    }
                    let withheld = native.native_blocks.iter().any(|item| {
                        item["type"] == "function_call"
                            && !native.assistant["tool_calls"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .any(|call| {
                                    item["call_id"]
                                        .as_str()
                                        .is_some_and(|id| call["id"].as_str() == Some(id))
                                })
                    });
                    let usage = native.usage.map(|u| Usage {
                        prompt: Some(u.prompt_tokens),
                        output: Some(u.output_tokens),
                        cache_read: Some(u.cached_tokens),
                        cache_write: Some(0),
                        finish: Some(native.finish_reason.into()),
                        ..Usage::default()
                    });
                    *terminal_usage = usage.clone();
                    // Identity must agree with the untouched private replay. Never
                    // execute a renamed/reidentified call to hide a credential.
                    if !withheld
                        && native.assistant["tool_calls"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .flat_map(|call| [call["id"].as_str(), call["function"]["name"].as_str()])
                            .flatten()
                            .any(|identity| redactor.secrets.iter().any(|secret| identity.contains(secret)))
                    {
                        return Err(Error::Protocol);
                    }
                    for chunk in chunks {
                        if chunk.pointer("/choices/0/delta/tool_calls").is_some() {
                            pending_tools.push(chunk);
                            continue;
                        }
                        if chunk
                            .pointer("/choices/0/finish_reason")
                            .is_some_and(|v| !v.is_null())
                        {
                            for tool in pending_tools.drain(..) {
                                if !withheld {
                                    redactor.deliver(tool, cancel, on)?;
                                }
                            }
                            redactor.flush(cancel, on)?;
                        }
                        redactor.deliver(chunk, cancel, on)?;
                    }
                    redactor.flush(cancel, on)?;
                    let mut assistant = native.assistant;
                    if withheld {
                        assistant
                            .as_object_mut()
                            .ok_or(Error::Protocol)?
                            .remove("tool_calls");
                    }
                    let output_items: Arc<[Value]> = native.native_blocks.into();
                    let replay_items = if withheld {
                        Arc::from([])
                    } else {
                        Arc::clone(&output_items)
                    };
                    redact_value(&mut assistant, &redactor.secrets);
                    if cancel.is_cancelled() {
                        return Err(Error::Cancelled);
                    }
                    return Ok(Completed {
                        assistant,
                        output_items,
                        replay_items,
                        usage,
                        finish_reason: native.finish_reason,
                    });
                }
                for chunk in chunks {
                    if chunk.pointer("/choices/0/delta/tool_calls").is_some() {
                        pending_tools.push(chunk);
                    } else {
                        redactor.deliver(chunk, cancel, on)?;
                    }
                }
            }
        }
    }
    async fn credentials(
        &self,
        path: &Path,
        expected_account_id: &str,
        refresh: Refresh<'_>,
        cancel: &Cancellation,
        rejected_access: Option<&str>,
    ) -> Result<(chatgpt_tokens::TokenRecord, Vec<String>), Error> {
        if expected_account_id.is_empty() {
            return Err(Error::Credentials);
        }
        let expected_account_id = expected_account_id.to_owned();
        let rejected_access = rejected_access.map(str::to_owned);
        let endpoint = validate_endpoint(refresh.endpoint)?;
        let timeout = refresh.timeout.min(REFRESH_TIMEOUT);
        if timeout.is_zero() || refresh.client_id.is_empty() || refresh.client_id.len() > 128 {
            return Err(Error::Setup);
        }
        let path = path.to_owned();
        let client_id = refresh.client_id.to_owned();
        let client = self.clone();
        let cancel_task = Cancellation::default();
        // Dropping this request also stops an in-progress refresh, or prevents
        // a waiter from starting one after it eventually acquires the lock.
        let _refresh_guard = CancelOnDrop(cancel_task.clone());
        let handle = tokio::runtime::Handle::current();
        let deadline = tokio::time::Instant::now() + timeout;
        let refreshing = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let acquire = async {
            loop {
                let (path, client_id, client, cancel_task, handle, endpoint, expected_account_id) = (
                    path.clone(),
                    client_id.clone(),
                    client.clone(),
                    cancel_task.clone(),
                    handle.clone(),
                    endpoint.clone(),
                    expected_account_id.clone(),
                );
                let rejected_access = rejected_access.clone();
                let refreshing = Arc::clone(&refreshing);
                let attempt = tokio::task::spawn_blocking(move || {
                    let mut retired = Vec::new();
                    let mut blocked = None;
                    let mut refreshed = false;
                    let refresher = |old: &chatgpt_tokens::TokenRecord| {
                        retired.extend([
                            old.access_token().to_owned(),
                            old.refresh_token().to_owned(),
                            old.account_id().to_owned(),
                        ]);
                        if old.account_id() != expected_account_id {
                            blocked = Some(Error::Credentials);
                            return Err(Error::Credentials);
                        }
                        if cancel_task.is_cancelled() {
                            blocked = Some(Error::Cancelled);
                            return Err(Error::Cancelled);
                        }
                        refreshed = true;
                        refreshing.store(true, std::sync::atomic::Ordering::Release);
                        // Both lock acquisition and the complete exchange share
                        // one firm deadline; the HTTP timer runs under the lock.
                        handle.block_on(async {
                            tokio::select! {
                                biased;
                                _ = cancel_task.cancelled() => Err(Error::Cancelled),
                                result = tokio::time::timeout_at(deadline, client.refresh(endpoint, &client_id, old, &cancel_task)) => result.map_err(|_|Error::Refresh)?,
                            }
                        })
                    };
                    let record = match rejected_access.as_deref() {
                        Some(rejected) => {
                            chatgpt_tokens::try_credentials_after_rejection(&path, rejected, refresher)
                        }
                        None => chatgpt_tokens::try_credentials(&path, refresher),
                    }
                    .map_err(|_| {
                        blocked.unwrap_or(if refreshed {
                            Error::SignedOut
                        } else {
                            Error::Credentials
                        })
                    })?;
                    if record
                        .as_ref()
                        .is_some_and(|record| record.account_id() != expected_account_id)
                    {
                        return Err(Error::Credentials);
                    }
                    Ok::<_, Error>(record.map(|record| (record, retired)))
                });
                let result = attempt.await.map_err(|_| Error::Credentials)??;
                if let Some(result) = result {
                    return Ok(result);
                }
                // Retry only the helper's explicit WouldBlock result, never an
                // invalid file/record, refresh error or upstream refusal.
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(Error::Cancelled),
            result = tokio::time::timeout_at(deadline, acquire) => result.map_err(|_| {
                if refreshing.load(std::sync::atomic::Ordering::Acquire) { Error::SignedOut } else { Error::Credentials }
            })?,
        }
    }

    async fn refresh(
        &self,
        endpoint: Url,
        client_id: &str,
        old: &chatgpt_tokens::TokenRecord,
        cancel: &Cancellation,
    ) -> Result<chatgpt_tokens::TokenRecord, Error> {
        let response = self
            .0
            .post(endpoint)
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", old.refresh_token()),
                ("client_id", client_id),
            ])
            .send()
            .await
            .map_err(|_| Error::Refresh)?;
        if !response.status().is_success() {
            return Err(Error::Refresh);
        }
        let bytes = bounded(response, MAX_ERROR, cancel).await?;
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| Error::Refresh)?;
        let access = value["access_token"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or(Error::Refresh)?;
        let refresh = value
            .get("refresh_token")
            .map_or(Some(old.refresh_token()), Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or(Error::Refresh)?;
        let expires = match value.get("expires_in") {
            Some(value) => {
                let lifetime = value.as_u64().filter(|n| *n > 120).ok_or(Error::Refresh)?;
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(|_| Error::Refresh)?
                    .as_secs();
                now.checked_add(lifetime).ok_or(Error::Refresh)? as f64
            }
            None => chatgpt_tokens::access_expiry(access).ok_or(Error::Refresh)?,
        };
        chatgpt_tokens::TokenRecord::new(access.into(), refresh.into(), old.account_id().into(), expires)
            .map_err(|_| Error::Refresh)
    }
}
struct CancelOnDrop(Cancellation);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

fn scrub_refusal(mut refusal: Option<Refusal>, secrets: &[&str]) -> Option<Refusal> {
    if let Some(Refusal::Subscription(rate_limits::chatgpt::Refusal::UsageLimitReached {
        plan_type: Some(plan),
        ..
    })) = &mut refusal
    {
        *plan = redact_fragment(plan, secrets, true).0;
    }
    refusal
}
async fn bounded(
    mut response: reqwest::Response,
    limit: usize,
    cancel: &Cancellation,
) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::new();
    loop {
        let next = tokio::select! { biased; _=cancel.cancelled()=>return Err(Error::Cancelled), next=response.chunk()=>next.map_err(|_|Error::Transport)?, };
        let Some(chunk) = next else { return Ok(bytes) };
        if chunk.len() > limit.saturating_sub(bytes.len()) {
            return Err(Error::TooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }
}
fn validate_endpoint(base: &str) -> Result<Url, Error> {
    let url = Url::parse(base).map_err(|_| Error::Endpoint)?;
    let local = match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => {
            ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|ip| ip.is_loopback())
        }
        Some(url::Host::Domain(host)) => host == "localhost",
        None => false,
    };
    if !(url.scheme() == "https" || url.scheme() == "http" && local)
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::Endpoint);
    }
    Ok(url)
}
fn responses_endpoint(base: &str, subscription: bool) -> Result<Url, Error> {
    let mut url = validate_endpoint(base)?;
    let path = url.path().trim_end_matches('/');
    let path = if path.ends_with("/responses") {
        path.to_owned()
    } else if subscription || path.ends_with("/v1") {
        format!("{path}/responses")
    } else {
        format!("{path}/v1/responses")
    };
    url.set_path(&path);
    Ok(url)
}
// Field-local withholding prevents credentials split across decoded delta chunks
// from escaping; transport JSON/framing is never fed to the field redactor.
struct Fields<'a> {
    secrets: Vec<&'a str>,
    pending: BTreeMap<String, (Value, String)>,
}
impl<'a> Fields<'a> {
    fn new(secrets: Vec<&'a str>) -> Self {
        Self {
            secrets,
            pending: BTreeMap::new(),
        }
    }
    fn deliver<F: FnMut(Event) -> Control + ?Sized>(
        &mut self,
        mut chunk: Value,
        cancel: &Cancellation,
        on: &mut F,
    ) -> Result<(), Error> {
        if chunk["choices"].as_array().is_some_and(|v| v.is_empty()) {
            redact_value(&mut chunk, &self.secrets);
            return emit(chunk, cancel, on);
        }
        let delta = &chunk["choices"][0]["delta"];
        let mut fields = Vec::new();
        if let Some(object) = delta.as_object() {
            for (key, value) in object {
                if value.is_string() {
                    fields.push((format!("/choices/0/delta/{key}"), key.clone()));
                }
            }
        }
        if let Some(calls) = delta["tool_calls"].as_array() {
            for (i, call) in calls.iter().enumerate() {
                // The translator emits whole IDs and names. The real consumer
                // assigns IDs and treats repeated names idempotently; suffix
                // flushing would corrupt both. Only arguments are fragments.
                let path = format!("/choices/0/delta/tool_calls/{i}/function/arguments");
                if chunk.pointer(&path).is_some_and(Value::is_string) {
                    fields.push((path, format!("tool:{}:arguments", call["index"])));
                }
            }
        }
        for (path, id) in fields {
            let text = chunk.pointer(&path).and_then(Value::as_str).unwrap().to_owned();
            let (_, mut pending) = self.pending.remove(&id).unwrap_or((Value::Null, String::new()));
            pending.push_str(&text);
            let (pending, suffix) = redact_fragment(&pending, &self.secrets, false);
            *chunk.pointer_mut(&path).unwrap() = Value::String(pending);
            if !suffix.is_empty() {
                let mut template = chunk.clone();
                *template.pointer_mut(&path).unwrap() = Value::String(String::new());
                self.pending.insert(id, (template, suffix));
            }
        }
        redact_value(&mut chunk, &self.secrets);
        emit(chunk, cancel, on)
    }
    fn flush<F: FnMut(Event) -> Control + ?Sized>(
        &mut self,
        cancel: &Cancellation,
        on: &mut F,
    ) -> Result<(), Error> {
        for (id, (mut template, text)) in std::mem::take(&mut self.pending) {
            let text = redact_fragment(&text, &self.secrets, true).0;
            // Flush only this independent field, never repeat other chunk data.
            let mut delta = serde_json::json!({});
            if let Some(rest) = id.strip_prefix("tool:") {
                let (index, field) = rest.rsplit_once(':').ok_or(Error::Protocol)?;
                let index: usize = index.parse().map_err(|_| Error::Protocol)?;
                let mut call = serde_json::json!({"index":index});
                call["function"][field] = Value::String(text);
                delta["tool_calls"] = serde_json::json!([call]);
            } else {
                delta[&id] = Value::String(text)
            }
            template["choices"][0]["delta"] = delta;
            template["choices"][0]["finish_reason"] = Value::Null;
            emit(template, cancel, on)?;
        }
        Ok(())
    }
}
fn emit<F: FnMut(Event) -> Control + ?Sized>(
    chunk: Value,
    cancel: &Cancellation,
    on: &mut F,
) -> Result<(), Error> {
    if cancel.is_cancelled() || on(Event::Chunk(chunk)) == Control::Stop {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}
// Leftmost-longest matching, withholding a possible longer rotated credential
// before consuming a shorter complete prefix. Offsets always remain UTF-8 boundaries.
fn redact_fragment(text: &str, secrets: &[&str], final_fragment: bool) -> (String, String) {
    let mut safe = String::with_capacity(text.len());
    let mut offset = 0;
    while offset < text.len() {
        let rest = &text[offset..];
        if !final_fragment
            && secrets
                .iter()
                .any(|key| key.len() > rest.len() && key.starts_with(rest))
        {
            return (safe, rest.to_owned());
        }
        if let Some(key) = secrets
            .iter()
            .filter(|key| !key.is_empty() && rest.starts_with(**key))
            .max_by_key(|key| key.len())
        {
            safe.push_str("[redacted]");
            offset += key.len();
        } else {
            let ch = rest.chars().next().expect("nonempty suffix");
            safe.push(ch);
            offset += ch.len_utf8();
        }
    }
    (safe, String::new())
}

fn redact_value(value: &mut Value, secrets: &[&str]) {
    match value {
        Value::String(s) => *s = redact_fragment(s, secrets, true).0,
        Value::Array(v) => {
            for item in v {
                redact_value(item, secrets)
            }
        }
        Value::Object(v) => {
            let old = std::mem::take(v);
            for (mut key, mut item) in old {
                key = redact_fragment(&key, secrets, true).0;
                redact_value(&mut item, secrets);
                v.insert(key, item);
            }
        }
        _ => {}
    }
}

impl fmt::Debug for Completed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Completed")
            .field("assistant", &"[redacted]")
            .field("output_items", &"[redacted]")
            .field("replay_items", &"[redacted]")
            .field("usage", &self.usage)
            .field("finish_reason", &self.finish_reason)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn http_error_debug_hides_provider_plan_text_even_if_it_is_not_a_known_key() {
        let error = Error::Http {
            status: 429,
            refusal: Some(Refusal::Subscription(
                rate_limits::chatgpt::Refusal::UsageLimitReached {
                    plan_type: Some("synthetic-provider-private-plan-text".into()),
                    resets_at: Some(123),
                    retry_after: None,
                },
            )),
        };
        let debug = format!("{error:?} {error}");
        assert!(!debug.contains("synthetic-provider-private-plan-text"));
        assert!(debug.contains("123"));
    }
    #[test]
    fn endpoints_refuse_plain_remote_credentials_queries_and_fragments() {
        assert_eq!(
            responses_endpoint(API_BASE, false).unwrap().as_str(),
            "https://api.openai.com/v1/responses"
        );
        assert_eq!(
            responses_endpoint(SUBSCRIPTION_BASE, true).unwrap().as_str(),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        for base in [
            "http://example.invalid",
            "https://user:synthetic@example.invalid",
            "https://example.invalid?synthetic",
            "https://example.invalid#synthetic",
            "file:///synthetic",
        ] {
            assert_eq!(validate_endpoint(base), Err(Error::Endpoint));
        }
        for base in [
            "http://127.0.0.1",
            "http://[::1]",
            "http://[::ffff:127.0.0.1]",
            "http://localhost",
        ] {
            assert!(validate_endpoint(base).is_ok());
        }
    }
    #[test]
    fn sse_handles_crlf_multiline_unicode_and_comments() {
        let wire = b": ping\r\nevent: synthetic\r\ndata: {\"type\":\r\ndata: \"ping\",\"text\":\"\xce\xbb\"}\r\n\r\n";
        let mut decoder = Framer::default();
        let events: Vec<Value> = wire
            .iter()
            .filter_map(|b| decoder.push(*b))
            .map(|data| serde_json::from_slice(&data).unwrap())
            .collect();
        assert_eq!(events, vec![serde_json::json!({"type":"ping","text":"λ"})]);
    }
    #[test]
    fn redaction_withholds_overlapping_rotated_credentials_until_disambiguated() {
        let mut fields = Fields::new(vec!["old-access", "old-access-rotated"]);
        let mut values = vec![];
        let cancel = Cancellation::default();
        let mut on = |e| {
            if let Event::Chunk(value) = e {
                values.push(value)
            }
            Control::Continue
        };
        for text in ["Before old-access", "-rotated after"] {
            fields.deliver(serde_json::json!({"choices":[{"index":0,"delta":{"content":text},"finish_reason":null}]}),&cancel,&mut on).unwrap();
        }
        fields.flush(&cancel, &mut on).unwrap();
        let text: String = values
            .iter()
            .filter_map(|v| v.pointer("/choices/0/delta/content")?.as_str())
            .collect();
        assert_eq!(text, "Before [redacted] after");
    }
    #[test]
    fn decoded_redaction_handles_unicode_and_split_secrets_without_panicking() {
        let mut fields = Fields::new(vec!["λcredential"]);
        let mut values = vec![];
        let cancel = Cancellation::default();
        let mut on = |e| {
            if let Event::Chunk(value) = e {
                values.push(value)
            }
            Control::Continue
        };
        for text in ["Before λcre", "dential after"] {
            fields.deliver(serde_json::json!({"choices":[{"index":0,"delta":{"content":text},"finish_reason":null}]}),&cancel,&mut on).unwrap();
        }
        fields.flush(&cancel, &mut on).unwrap();
        let text: String = values
            .iter()
            .filter_map(|v| v.pointer("/choices/0/delta/content")?.as_str())
            .collect();
        assert_eq!(text, "Before [redacted] after");
    }
}
