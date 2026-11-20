//! C2: auto-retry and overflow recovery.
// ported from pi packages/coding-agent/src/core/agent-session.ts (auto retry, overflow recovery)
use crate::agent_loop::persist;
use crate::compaction::compact_now;
use crate::exec::{request, RequestOpts};
use crate::SessionState;
use oxa_proto::{
    codes, shown_why, AgentEvent, AssistantTurn, CancelToken, CompactReason, Engine, EngineInfo, EntryBody,
    EventSink, Finish, OxaError, StreamEvent, Ui,
};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    pub enabled: bool,
    pub max_retries: u32,
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
    /// #2050: the whole retry wait of a failure on this machine's own engine. Waiting cannot
    /// make a local engine that is down answer, so the budget is short; `0` gives up at once.
    pub local_max_total_ms: u64,
    /// #2050: retries of the same plain 5xx body (status and message) before giving up. An
    /// identical answer will not change; overload replies (503, 529, a named wait) keep the
    /// long backoff.
    pub identical_5xx_retries: u32,
}

/// pi's backoff: `base * 2^(attempt-1)` capped at `max_delay_ms`; `attempt` starts at 1.
pub fn backoff_ms(attempt: u32, p: &RetryPolicy) -> u64 {
    // Attempt 0 is treated like attempt 1 (pi: `Math.max(0, attempt - 1)`). The shift is capped
    // so the multiplication saturates instead of overflowing.
    let shift = attempt.saturating_sub(1).min(63);
    let delay = p.base_delay_ms.saturating_mul(1u64 << shift);
    delay.min(p.max_delay_ms)
}

/// `request` with pi's retry behaviour, adapted (plan sec. 1 "Queue, retry"):
/// * Retryable: `engine_down`, `engine_busy` (after its `retry after <n> s` when given, capped
///   at `max_delay_ms`), `cloud_rate_limited`, `engine_http` 5xx / 429, `stream_broken` **before any data byte**.
///   A cloud rate limit with a wait beyond the configured cap is returned to the caller.
///   The same idempotency key is reused. Emits `AutoRetryStart` / `AutoRetryEnd`; the wait is
///   interruptible by `Shared::cancel_retry` and `cancel`.
/// * #2050: this machine's own engine gets a short whole budget (`retry.localMaxTotalMs`)
///   instead of the cloud backoff: an engine that is down does not come back by waiting. Such
///   a give-up names the endpoint and says to restart the engine.
/// * #2050: the same plain 5xx body (status and message) answering again stops after
///   `retry.identical5xxRetries` retries. 503, 529, a reply that names a wait and overload
///   markers keep the long backoff: a cloud provider that is briefly overloaded recovers.
/// * `stream_broken` after output: not replayed here; returned as the error. The agent loop
///   stores the partial as `Shown { why: cut }` and resends a transport cut itself (#456).
/// * `context_overflow`: once per prompt, `compact_now(.., Overflow)` then one retry; a second
///   overflow is returned.
/// * `aborted` is never retried.
/// * F6: a `Length` reply with `prompt + completion >= n_ctx - 1` was cut because the CONTEXT
///   is full, not by `max_tokens`: it is a `context_overflow`. The cut reply is never pushed
///   (W2); with `events` it is stored as `Shown { why: cut }` (which closes the open message
///   for front ends) and a new pending `MessageStart` opens the retry's message. Then the
///   overflow path above: compact (foreground) and one retry; a second one is returned.
/// * F4: a request that succeeds after an `engine_busy` / `engine_down` / 5xx retry (a model
///   load or an engine restart happened) sets `Gate::reread_info`; the loop re-reads the
///   engine metadata once.
pub fn chat_with_retry(
    st: &mut SessionState,
    opts: &RequestOpts,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) -> Result<AssistantTurn, OxaError> {
    chat_with_retry_keyed(st, &mut opts.clone(), sink, ui)
}

/// As `chat_with_retry`, but the effective idempotency key (fresh after an overflow
/// compaction) is left in `opts`, so a later replay of the same request reuses it.
pub(crate) fn chat_with_retry_keyed(
    st: &mut SessionState,
    opts: &mut RequestOpts,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) -> Result<AssistantTurn, OxaError> {
    let policy = retry_policy(st);
    // A `abort_retry` only means something during a wait of this call; a stale one from an
    // earlier call must not cancel this call's wait.
    st.shared.cancel_retry.reset();
    st.shared.gate.retry_stopped.store(false, Ordering::SeqCst);

    if opts.idem_key.is_empty() {
        opts.idem_key = fresh_idem_key(st);
    }
    let mut retries: u32 = 0;
    // H (SET-054): `retry.provider.*`, the HTTP-level attempts inside each agent-level attempt
    // (one HTTP layer in oxa). Same replay rule and visible notices as agent retries.
    let provider = st.settings.provider_retry();
    let mut provider_retries: u32 = 0;
    // #2050: this machine's own engine gets a short whole budget instead of the cloud backoff,
    // and the same 5xx body answering again is not retried like a transient overload.
    let own_engine = local_engine(st);
    let mut local_waited_ms: u64 = 0;
    let mut repeated = Repeated::default();
    let mut overflowed = false;
    let mut reloaded = false;
    let mut loading_noticed = false; // R2: one "model loading" notice per logical request
    loop {
        let (res, got_data) = probed_request(st, opts, sink, ui);
        let err = match res {
            Ok(turn) if opts.max_tokens != Some(0) && context_full(st, &turn) => {
                // The reply is dropped, but the request ran: its tokens count in the stats.
                crate::agent_loop::account_discarded(st, &turn);
                let err = OxaError::new(
                    codes::CONTEXT_OVERFLOW,
                    format!(
                        "the reply was cut because the context is full ({} prompt + {} reply tokens of {})",
                        turn.usage.prompt, turn.usage.completion, st.info.n_ctx
                    ),
                );
                if !overflowed && opts.events {
                    let text = turn
                        .msg
                        .content
                        .clone()
                        .filter(|t| !t.is_empty())
                        .unwrap_or_else(|| err.message.clone());
                    persist(
                        st,
                        EntryBody::Shown {
                            text,
                            why: shown_why::CUT,
                        },
                        sink,
                    )?;
                    sink.emit(&AgentEvent::MessageStart {
                        message: crate::agent_loop::pending_message(),
                    });
                }
                err
            }
            Ok(turn) => {
                if reloaded {
                    st.shared.gate.reread_info.store(true, Ordering::SeqCst);
                }
                if retries > 0 {
                    sink.emit(&AgentEvent::AutoRetryEnd {
                        success: true,
                        attempt: retries,
                        final_error: None,
                    });
                }
                return Ok(turn);
            }
            Err(e) => e,
        };

        if err.code == codes::ABORTED {
            finish_failed(sink, retries, &err);
            return Err(err);
        }

        if st.shared.gate.wrap_up.load(Ordering::SeqCst) != 0 {
            finish_failed(sink, retries, &err);
            return Err(err);
        }

        if err.code == codes::CONTEXT_OVERFLOW {
            if !overflowed {
                overflowed = true;
                let before = st.stats.compactions;
                // Retry only when a compaction really rebased the wire: a skipped one (nothing
                // before the current prompt to summarise, or a hook cancel) leaves the same
                // bytes, and sending them again would only overflow a second time.
                if compact_now(st, None, CompactReason::Overflow, sink, ui).is_ok()
                    && st.stats.compactions > before
                {
                    if st.shared.cancel.is_cancelled() {
                        let aborted = OxaError::new(codes::ABORTED, "aborted");
                        finish_failed(sink, retries, &aborted);
                        return Err(aborted);
                    }
                    if st.shared.gate.wrap_up.load(Ordering::SeqCst) != 0 {
                        finish_failed(sink, retries, &err);
                        return Err(err);
                    }
                    // The wire changed under the compaction: this is a new logical request.
                    opts.idem_key = fresh_idem_key(st);
                    continue;
                }
            }
            finish_failed(sink, retries, &err);
            return Err(err);
        }

        // #2050: a local engine that is not answering and a 5xx body that repeats byte for
        // byte are not transient; the long backoff is for a cloud provider briefly overloaded.
        let local_down = own_engine && local_failure(&err);
        let identical = repeated.record(&err, policy.identical_5xx_retries);

        // #514: the provider layer is a retry too: `set_auto_retry(false)` (the user switch) and
        // `retry.enabled false` stop it as well.
        if policy.enabled
            && !local_down
            && !identical
            && provider_retries < provider.max_retries
            && retryable(&err, got_data)
        {
            // A server-requested wait above `maxRetryDelayMs` (0 = no limit) is not waited for
            // here; the agent-level retry decides.
            let delay_ms = match retry_after_ms(&err.message) {
                Some(ms) if provider.max_retry_delay_ms == 0 || ms <= provider.max_retry_delay_ms => Some(ms),
                Some(_) => None,
                None => Some(backoff_ms(provider_retries + 1, &policy)),
            };
            if let Some(delay_ms) = delay_ms {
                provider_retries += 1;
                retry_notice(sink, provider_retries, delay_ms, &err);
                reloaded |= matches!(
                    err.code,
                    codes::ENGINE_BUSY | codes::ENGINE_DOWN | codes::ENGINE_HTTP
                );
                match wait(st, delay_ms) {
                    Wait::Elapsed => continue,
                    Wait::RetryCancelled => {
                        st.shared.cancel_retry.reset();
                        st.shared.gate.retry_stopped.store(true, Ordering::SeqCst);
                        finish_failed(sink, retries, &retry_cancelled());
                        return Err(err);
                    }
                    Wait::Aborted => {
                        let aborted = OxaError::new(codes::ABORTED, "aborted");
                        finish_failed(sink, retries, &retry_cancelled());
                        return Err(aborted);
                    }
                }
            }
        }

        // #2050: the local budget is spent, or the endpoint repeats the same 5xx body: stop and
        // say what to do. `local_down` and `identical` are decided before the provider layer.
        let local_spent_out = local_down && local_waited_ms >= policy.local_max_total_ms;
        if !policy.enabled
            || retries >= policy.max_retries
            || !retryable(&err, got_data)
            || local_spent_out
            || identical
        {
            let err = stopped_error(&err, st, local_down, identical);
            finish_failed(sink, retries, &err);
            return Err(err);
        }

        let Some(mut delay_ms) = agent_retry_delay(&err, retries + 1, &policy) else {
            let err = stopped_error(&err, st, local_down, identical);
            finish_failed(sink, retries, &err);
            return Err(err);
        };
        if local_down {
            // The whole local wait is bounded by the owner's budget; the last wait is the rest.
            delay_ms = delay_ms.min(policy.local_max_total_ms - local_waited_ms);
            local_waited_ms += delay_ms;
        }
        retries += 1;
        provider_retries = 0;
        crate::reload::loading_notice(&err, &mut loading_noticed, sink);
        reloaded |= matches!(
            err.code,
            codes::ENGINE_BUSY | codes::ENGINE_DOWN | codes::ENGINE_HTTP
        );
        sink.emit(&AgentEvent::AutoRetryStart {
            attempt: retries,
            max_attempts: policy.max_retries,
            delay_ms,
            error_message: err.to_string(),
        });
        retry_notice(sink, retries, delay_ms, &err);
        match wait(st, delay_ms) {
            Wait::Elapsed => {}
            Wait::RetryCancelled => {
                // Consume the cancel so it does not reach a later wait.
                st.shared.cancel_retry.reset();
                st.shared.gate.retry_stopped.store(true, Ordering::SeqCst);
                finish_failed(sink, retries, &retry_cancelled());
                return Err(err);
            }
            Wait::Aborted => {
                let aborted = OxaError::new(codes::ABORTED, "aborted");
                finish_failed(sink, retries, &retry_cancelled());
                return Err(aborted);
            }
        }
    }
}

/// Which summary a retried summarization request produces (pi's `source`, with the
/// compaction `reason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SummarySource {
    Compaction(CompactReason),
}

/// A summary request (compaction or branch summary) with pi's summarization retry
/// (`retryAssistantCall` with `settings.retry` and the summarization callbacks of
/// agent-session.ts): a transient failure (the [`retryable`] errors of [`chat_with_retry`]) is
/// retried up to `retry.maxRetries` times with the same backoff, each retry announced by
/// `SummarizationRetryScheduled` before the wait and `SummarizationRetryAttemptStart` after it;
/// once a retry was scheduled, `SummarizationRetryFinished` closes the sequence whatever the
/// outcome. The same idempotency key is reused, and nothing is appended (W5: the session only
/// changes when the summary has arrived). The wait ends early on an abort, and for a background
/// summary also on the user's input (`Gate::bg_cancel`, ENG-002); the summary is then
/// `aborted`. #2050 gives a local engine its short budget and stops on a repeated 5xx body
/// here too. Context overflow is not handled here: the caller's compaction is the recovery.
pub(crate) fn summary_with_retry(
    st: &mut SessionState,
    opts: &RequestOpts,
    source: SummarySource,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) -> Result<AssistantTurn, OxaError> {
    let policy = retry_policy(st);
    let mut opts = opts.clone();
    if opts.idem_key.is_empty() {
        opts.idem_key = fresh_idem_key(st);
    }
    let mut retries: u32 = 0;
    // #2050: the same short local budget and repeated-5xx rule as a chat request.
    let own_engine = local_engine(st);
    let mut local_waited_ms: u64 = 0;
    let mut repeated = Repeated::default();
    loop {
        let (res, got_data) = probed_request(st, &opts, sink, ui);
        let err = match res {
            Ok(turn) => {
                if retries > 0 {
                    sink.emit(&AgentEvent::SummarizationRetryFinished);
                }
                return Ok(turn);
            }
            Err(e) => e,
        };
        let local_down = own_engine && local_failure(&err);
        let identical = repeated.record(&err, policy.identical_5xx_retries);
        let final_ = err.code == codes::ABORTED
            || !policy.enabled
            || retries >= policy.max_retries
            || !retryable(&err, got_data)
            || identical
            || (local_down && local_waited_ms >= policy.local_max_total_ms);
        if final_ {
            if retries > 0 {
                sink.emit(&AgentEvent::SummarizationRetryFinished);
            }
            return Err(stopped_error(&err, st, local_down, identical));
        }
        let Some(mut delay_ms) = agent_retry_delay(&err, retries + 1, &policy) else {
            if retries > 0 {
                sink.emit(&AgentEvent::SummarizationRetryFinished);
            }
            return Err(stopped_error(&err, st, local_down, identical));
        };
        if local_down {
            delay_ms = delay_ms.min(policy.local_max_total_ms - local_waited_ms);
            local_waited_ms += delay_ms;
        }
        retries += 1;
        sink.emit(&AgentEvent::SummarizationRetryScheduled {
            attempt: retries,
            max_attempts: policy.max_retries,
            delay_ms,
            error_message: err.to_string(),
        });
        if !summary_wait(st, delay_ms, opts.background) {
            sink.emit(&AgentEvent::SummarizationRetryFinished);
            return Err(OxaError::new(codes::ABORTED, "aborted"));
        }
        let (source, reason) = match source {
            SummarySource::Compaction(r) => ("compaction", Some(r)),
        };
        sink.emit(&AgentEvent::SummarizationRetryAttemptStart { source, reason });
    }
}

/// The backoff of a summarization retry: `false` when it was cut short by an abort (or, for a
/// background summary, by the user's input).
fn summary_wait(st: &SessionState, ms: u64, background: bool) -> bool {
    let end = Instant::now() + Duration::from_millis(ms);
    loop {
        if st.shared.cancel.is_cancelled() || (background && st.shared.gate.bg_cancel.is_cancelled()) {
            return false;
        }
        let now = Instant::now();
        if now >= end {
            return true;
        }
        std::thread::sleep((end - now).min(Duration::from_millis(10)));
    }
}

/// F6: a `Length` stop at the end of the context (`prompt + completion >= n_ctx - 1`).
fn context_full(st: &SessionState, turn: &AssistantTurn) -> bool {
    turn.finish == Finish::Length
        && st.info.n_ctx > 0
        && u64::from(turn.usage.prompt) + u64::from(turn.usage.completion) >= u64::from(st.info.n_ctx - 1)
}

fn retry_notice(sink: &mut dyn EventSink, attempt: u32, delay_ms: u64, error: &OxaError) {
    // #2050: the reason the user is waiting, not only the class: the first line of the error,
    // bounded so a provider's body cannot fill the transcript.
    let why: String = error
        .message
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .chars()
        .take(160)
        .collect();
    let text = if why.is_empty() {
        format!(
            "retrying request in {delay_ms} ms (attempt {attempt}, {})",
            error.code
        )
    } else {
        format!(
            "retrying request in {delay_ms} ms (attempt {attempt}, {}): {why}",
            error.code
        )
    };
    sink.emit(&AgentEvent::Notice {
        level: oxa_proto::Level::Info,
        text,
    });
}

/// Retry transient refusals and transport failures only before output was observed.
fn retryable(e: &OxaError, got_data: bool) -> bool {
    if got_data {
        return false;
    }
    match e.code {
        codes::ENGINE_DOWN
        | codes::ENGINE_BUSY
        | codes::STREAM_BROKEN
        | codes::TIMEOUT
        | "cloud_rate_limited"
        | "cloud_credentials_unavailable"
        | "cloud_ledger_unavailable" => true,
        codes::ENGINE_HTTP => matches!(http_status(&e.message), Some(s) if s >= 500 || s == 429),
        _ => false,
    }
}

/// #2050: the session's endpoint is this machine's own engine (an in-process engine or a
/// loopback host serving a local model). A cloud model through the local gateway
/// (`caps.remote`) is not local: its provider's overload is worth the long wait.
fn local_engine(st: &SessionState) -> bool {
    !st.info.caps.remote && crate::session::engine_on_this_machine(&st.info.base_url)
}

/// #2050: a failure that waiting cannot fix on this machine: the engine is not reachable, or
/// it answered a plain 5xx that names no wait.
fn local_failure(e: &OxaError) -> bool {
    match e.code {
        codes::ENGINE_DOWN | codes::TIMEOUT => true,
        codes::ENGINE_HTTP => {
            http_status(&e.message).is_some_and(|s| (500..=599).contains(&s))
                && retry_after_ms(&e.message).is_none()
                && !overload_marker(&e.message)
        }
        _ => false,
    }
}

/// #2050: the give-up of a request that stopped retrying early, with what to do about it.
fn stopped_error(e: &OxaError, st: &SessionState, local_down: bool, identical: bool) -> OxaError {
    if local_down {
        let at = if st.info.base_url.contains("://") {
            format!("at {}", st.info.base_url)
        } else {
            "on this machine".to_string()
        };
        return e.clone().with_hint(format!(
            "the local engine {at} is not answering; oxa starts oxe-server when a request needs it, so check its launch path or restart oxa (with OXA_ENGINE_URL, start the engine it names), then send the request again"
        ));
    }
    if identical {
        return e.clone().with_hint(
            "the endpoint answered the same error again; oxa stopped retrying, so check the engine or provider and send the request again",
        );
    }
    e.clone()
}

/// #2050: consecutive identical plain 5xx answers of one logical request.
#[derive(Default)]
struct Repeated {
    last: Option<String>,
    seen: u32,
}

impl Repeated {
    /// Record `e`; `true` when it repeats the previous plain 5xx body for the `limit`-th time
    /// after the first (limit 0 = no retry of an identical body).
    fn record(&mut self, e: &OxaError, limit: u32) -> bool {
        if !plain_5xx(e) || overload_marker(&e.message) {
            self.last = None;
            self.seen = 0;
            return false;
        }
        if self.last.as_deref() == Some(e.message.as_str()) {
            self.seen += 1;
        } else {
            self.last = Some(e.message.clone());
            self.seen = 1;
        }
        self.seen > limit
    }
}

/// #2050: a plain `engine_http` 5xx. 503 (queue full or overload) and 529 (overload), and any
/// reply that names a wait, keep the long backoff.
fn plain_5xx(e: &OxaError) -> bool {
    e.code == codes::ENGINE_HTTP
        && http_status(&e.message).is_some_and(|s| (500..=599).contains(&s) && s != 503 && s != 529)
        && retry_after_ms(&e.message).is_none()
}

/// #2050: the words a provider uses for a temporary overload. Such a reply keeps the long
/// backoff even when its body repeats.
fn overload_marker(msg: &str) -> bool {
    let msg = msg.to_ascii_lowercase();
    [
        "overload",
        "try again",
        "temporarily",
        "busy",
        "rate limit",
        "too many requests",
    ]
    .iter()
    .any(|needle| msg.contains(needle))
}

/// The HTTP status in an `engine_http` message ("... HTTP 503: ...").
pub(crate) fn http_status(msg: &str) -> Option<u16> {
    let at = msg.find("HTTP ")? + "HTTP ".len();
    let digits: String = msg[at..].chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.len() != 3 {
        return None;
    }
    digits.parse().ok()
}

/// The wait an `engine_busy` message names (`... retry after 30 s ...`), in milliseconds.
fn retry_after_ms(msg: &str) -> Option<u64> {
    let at = msg.find("retry after ")? + "retry after ".len();
    let rest = &msg[at..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() || !rest[digits.len()..].starts_with(" s") {
        return None;
    }
    digits.parse::<u64>().ok().map(|s| s.saturating_mul(1000))
}

fn agent_retry_delay(error: &OxaError, attempt: u32, policy: &RetryPolicy) -> Option<u64> {
    match retry_after_ms(&error.message) {
        Some(ms) if error.code == "cloud_rate_limited" && ms > policy.max_delay_ms => None,
        Some(ms) => Some(ms.min(policy.max_delay_ms)),
        None => Some(backoff_ms(attempt, policy)),
    }
}

fn retry_policy(st: &SessionState) -> RetryPolicy {
    let r = st.settings.retry();
    RetryPolicy {
        enabled: st.auto_retry && r.enabled,
        max_retries: r.max_retries,
        base_delay_ms: r.base_delay_ms,
        max_delay_ms: r.max_delay_ms,
        local_max_total_ms: r.local_max_total_ms,
        identical_5xx_retries: r.identical_5xx_retries,
    }
}

enum Wait {
    Elapsed,
    RetryCancelled,
    Aborted,
}

/// Sleep for `ms`, returning early when the retry wait or the whole run is cancelled.
fn wait(st: &SessionState, ms: u64) -> Wait {
    let end = Instant::now() + Duration::from_millis(ms);
    loop {
        if st.shared.cancel.is_cancelled() {
            return Wait::Aborted;
        }
        if st.shared.cancel_retry.is_cancelled() {
            return Wait::RetryCancelled;
        }
        let now = Instant::now();
        if now >= end {
            return Wait::Elapsed;
        }
        std::thread::sleep((end - now).min(Duration::from_millis(10)));
    }
}

fn retry_cancelled() -> OxaError {
    OxaError::new(codes::ABORTED, "Retry cancelled")
}

/// Close an open retry sequence with a failed `AutoRetryEnd` (nothing when no retry started).
fn finish_failed(sink: &mut dyn EventSink, retries: u32, e: &OxaError) {
    if retries > 0 {
        sink.emit(&AgentEvent::AutoRetryEnd {
            success: false,
            attempt: retries,
            final_error: Some(e.to_string()),
        });
    }
}

/// Runs [`request`] with a probe that records whether any reply data (text, reasoning or a
/// tool-call delta) arrived. `request` reports `Prefill` events only as progress, so the probe
/// is the only place that sees data when `RequestOpts::events` is false.
fn probed_request(
    st: &mut SessionState,
    opts: &RequestOpts,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) -> (Result<AssistantTurn, OxaError>, bool) {
    let data = Arc::new(AtomicBool::new(false));
    let real = st.engine.clone();
    st.engine = Arc::new(ProbeEngine {
        inner: real.clone(),
        data: Arc::clone(&data),
    });
    let res = request(st, opts, sink, ui);
    st.engine = real;
    (res, data.load(Ordering::SeqCst))
}

/// An engine that forwards everything and marks the first data increment.
struct ProbeEngine {
    inner: Arc<dyn Engine>,
    data: Arc<AtomicBool>,
}

impl Engine for ProbeEngine {
    fn info(&self, target: Option<&str>) -> Result<EngineInfo, OxaError> {
        self.inner.info(target)
    }

    fn count_tokens(&self, text: &str) -> Result<u32, OxaError> {
        self.inner.count_tokens(text)
    }

    fn chat(
        &self,
        r: &oxa_proto::ChatRequest,
        cancel: &CancelToken,
        on: &mut dyn FnMut(StreamEvent),
    ) -> Result<AssistantTurn, OxaError> {
        let data = Arc::clone(&self.data);
        let mut probe = |ev: StreamEvent| {
            if !matches!(
                ev,
                StreamEvent::Prefill { .. } | StreamEvent::Queued { .. } | StreamEvent::Warning(_)
            ) {
                data.store(true, Ordering::SeqCst);
            }
            on(ev);
        };
        self.inner.chat(r, cancel, &mut probe)
    }
}

/// A fresh idempotency key (same construction as `exec`'s, which is private to B3).
pub(crate) fn fresh_idem_key(st: &SessionState) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut h = Sha256::new();
    h.update(st.sref.id.as_bytes());
    h.update(nanos.to_le_bytes());
    h.update(n.to_le_bytes());
    let digest: [u8; 32] = h.finalize().into();
    digest[..16].iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod fit_tests {
    use super::*;

    #[test]
    fn transient_failures_after_output_are_never_replayed() {
        for (class, message) in [
            (codes::ENGINE_BUSY, "HTTP 503: busy"),
            (codes::ENGINE_DOWN, "connection reset"),
            (codes::ENGINE_HTTP, "HTTP 429: rate limited"),
            ("cloud_rate_limited", "HTTP 429: rate limited"),
            (codes::TIMEOUT, "timed out"),
            ("cloud_credentials_unavailable", "credentials unavailable"),
            ("cloud_ledger_unavailable", "ledger unavailable"),
        ] {
            assert!(!retryable(&OxaError::new(class, message), true), "{class}");
        }
    }

    /// F3 / S2: `engine_busy` is retryable (also after nothing arrived), 4xx is final, and the
    /// wait named by `retry after <n> s` is read in milliseconds.
    #[test]
    fn engine_busy_is_retryable_and_names_its_wait() {
        let busy = OxaError::new(
            codes::ENGINE_BUSY,
            "the engine answered HTTP 503: queue full (retry after 30 s)",
        );
        assert!(retryable(&busy, false));
        let rate = OxaError::new("cloud_rate_limited", "HTTP 429: retry after 7 s");
        assert!(retryable(&rate, false));
        assert_eq!(retry_after_ms(&rate.message), Some(7000));
        let bad = OxaError::new(codes::ENGINE_HTTP, "the engine answered HTTP 400: bad");
        assert!(!retryable(&bad, false));
        let gone = OxaError::new(codes::ENGINE_HTTP, "the engine answered HTTP 500: oops");
        assert!(retryable(&gone, false));
        assert_eq!(retry_after_ms(&busy.message), Some(30_000));
        assert_eq!(retry_after_ms("the engine answered HTTP 503: busy"), None);
        assert_eq!(retry_after_ms("retry after soon"), None);
    }
}
