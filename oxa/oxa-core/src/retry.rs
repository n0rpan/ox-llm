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
///   The same idempotency key is reused. Emits `AutoRetryStart` / `AutoRetryEnd`; the wait is
///   interruptible by `Shared::cancel_retry` and `cancel`.
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

    if opts.idem_key.is_empty() {
        opts.idem_key = fresh_idem_key(st);
    }
    let mut retries: u32 = 0;
    // H (SET-054): `retry.provider.*`, the HTTP-level attempts inside each agent-level attempt
    // (one HTTP layer in oxa). Same replay rule and visible notices as agent retries.
    let provider = st.settings.provider_retry();
    let mut provider_retries: u32 = 0;
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

        // #514: the provider layer is a retry too: `set_auto_retry(false)` (the user switch) and
        // `retry.enabled false` stop it as well.
        if policy.enabled && provider_retries < provider.max_retries && retryable(&err, got_data) {
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

        if !policy.enabled || retries >= policy.max_retries || !retryable(&err, got_data) {
            finish_failed(sink, retries, &err);
            return Err(err);
        }

        retries += 1;
        provider_retries = 0;
        crate::reload::loading_notice(&err, &mut loading_noticed, sink);
        reloaded |= matches!(
            err.code,
            codes::ENGINE_BUSY | codes::ENGINE_DOWN | codes::ENGINE_HTTP
        );
        // F3 / S2: an engine that names its wait (`Retry-After`, carried as `retry after <n> s`
        // in the `engine_busy` message) is honoured, capped by the policy's longest delay.
        let delay_ms = match retry_after_ms(&err.message) {
            Some(ms) => ms.min(policy.max_delay_ms),
            None => backoff_ms(retries, &policy),
        };
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
/// `aborted`. Context overflow is not handled here: the caller's compaction is the recovery.
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
        let final_ = err.code == codes::ABORTED
            || !policy.enabled
            || retries >= policy.max_retries
            || !retryable(&err, got_data);
        if final_ {
            if retries > 0 {
                sink.emit(&AgentEvent::SummarizationRetryFinished);
            }
            return Err(err);
        }
        retries += 1;
        let delay_ms = match retry_after_ms(&err.message) {
            Some(ms) => ms.min(policy.max_delay_ms),
            None => backoff_ms(retries, &policy),
        };
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
    sink.emit(&AgentEvent::Notice {
        level: oxa_proto::Level::Info,
        text: format!(
            "retrying request in {delay_ms} ms (attempt {attempt}, {})",
            error.code
        ),
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

/// The HTTP status in an `engine_http` message ("... HTTP 503: ...").
fn http_status(msg: &str) -> Option<u16> {
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

fn retry_policy(st: &SessionState) -> RetryPolicy {
    let r = st.settings.retry();
    RetryPolicy {
        enabled: st.auto_retry && r.enabled,
        max_retries: r.max_retries,
        base_delay_ms: r.base_delay_ms,
        max_delay_ms: r.max_delay_ms,
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
            if !matches!(ev, StreamEvent::Prefill { .. } | StreamEvent::Queued { .. }) {
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
