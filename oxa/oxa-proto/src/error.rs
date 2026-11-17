use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Poll, Waker};

/// The one error type of the workspace.
///
/// `code` is a stable, machine-readable snake_case identifier (see [`codes`]); `message` is for
/// people; `hint` says what to do next. User-facing layers print `message` and, on its own line,
/// `hint`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OxaError {
    pub code: &'static str,
    pub message: String,
    pub hint: Option<String>,
}

impl OxaError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        OxaError {
            code,
            message: message.into(),
            hint: None,
        }
    }
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
    /// The fixed error for a pi feature that oxa omits (verdict O in PARITY.md).
    pub fn unsupported(what: &str) -> Self {
        OxaError::new(
            codes::UNSUPPORTED_IN_OXA,
            format!("{what} is not supported in oxa"),
        )
    }
    /// Wrap an I/O error with the path or action it concerned.
    pub fn io(context: &str, e: &std::io::Error) -> Self {
        OxaError::new(codes::IO, format!("{context}: {e}"))
    }
}

impl fmt::Display for OxaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)?;
        if let Some(h) = &self.hint {
            write!(f, "\n{h}")?;
        }
        Ok(())
    }
}

impl std::error::Error for OxaError {}

/// Process exit code for a spent daily cloud budget (`codes::BUDGET_EXHAUSTED`).
pub const EXIT_BUDGET: i32 = 3;

/// Message prefix of a `stream_broken` that wraps a refusal arriving after reply data (budget,
/// settlement, malformed chunk): never replayed, unlike a plain transport cut (#456).
pub const CUT_BY_REFUSAL: &str = "the reply was cut: ";

/// Stable error codes. A new code is added here by the architect; tasks pick from this list.
pub mod codes {
    /// The engine endpoint refused the connection or is not reachable. Always carries a hint.
    pub const ENGINE_DOWN: &str = "engine_down";
    /// The engine answered with a non-2xx status that is not an overflow.
    pub const ENGINE_HTTP: &str = "engine_http";
    /// The engine answered 400 `exceed_context_size_error`.
    pub const CONTEXT_OVERFLOW: &str = "context_overflow";
    /// The stream ended, stalled past the silence limit, or was malformed.
    pub const STREAM_BROKEN: &str = "stream_broken";
    /// The operation was cancelled through a [`super::CancelToken`].
    pub const ABORTED: &str = "aborted";
    /// A pi feature omitted from oxa (verdict O).
    pub const UNSUPPORTED_IN_OXA: &str = "unsupported_in_oxa";
    /// The interactive terminal disappeared (process exit 129).
    pub const TERMINAL_CLOSED: &str = "terminal_closed";
    pub const IO: &str = "io";
    /// A file, session, model, tool, command or entry was not found.
    pub const NOT_FOUND: &str = "not_found";
    /// Malformed input: JSON, settings value, argument, frontmatter, session line.
    pub const INVALID: &str = "invalid";
    /// Command line usage error (exit code 2).
    pub const USAGE: &str = "usage";
    /// Refused by the permission policy or by the user.
    pub const DENIED: &str = "denied";
    /// `bwrap` is missing or unusable; bash refuses to run.
    pub const NO_SANDBOX: &str = "no_sandbox";
    /// A W-guard refusal: a hook returned a wire that does not extend the current one.
    pub const GUARD_REFUSED: &str = "guard_refused";
    /// An invariant of the session (W1-W5) would be broken; a bug, never a user error.
    pub const INVARIANT: &str = "invariant";
    /// A session path failed the safety checks (symlink, wrong owner or mode).
    pub const UNSAFE_PATH: &str = "unsafe_path";
    /// The agent is busy with a run and the operation needs an idle session.
    pub const BUSY: &str = "busy";
    /// The engine's queue is full or the slot is taken (503, also as an SSE `error` event):
    /// retried, after the `Retry-After` the message carries (`retry after <n> s`) when given.
    pub const ENGINE_BUSY: &str = "engine_busy";
    /// The cloud gateway refused with 429 `budget_exceeded` (daily budget spent). Not retried;
    /// `-p` / `--mode json` exit with [`EXIT_BUDGET`](super::EXIT_BUDGET).
    pub const BUDGET_EXHAUSTED: &str = "budget_exhausted";
    /// A script (extension, codemode) threw, ran out of memory or passed its deadline.
    pub const SCRIPT: &str = "script";
    /// An MCP server failed or answered with a protocol error.
    pub const MCP: &str = "mcp";
    /// An MCP server needs an OAuth sign-in (`oxa mcp login <server>`).
    pub const MCP_NEEDS_AUTH: &str = "mcp_needs_auth";
    pub const TIMEOUT: &str = "timeout";
    /// The run reached `oxa.maxTurns` model replies (#507).
    pub const TURN_LIMIT: &str = "turn_limit";
    /// Final answer still violates the requested JSON schema after one repair turn.
    pub const SCHEMA_MISMATCH: &str = "schema_mismatch";
    /// The project is not trusted and the resource is trust-gated.
    pub const UNTRUSTED: &str = "untrusted";
    /// A bug: an internal assumption did not hold.
    pub const INTERNAL: &str = "internal";
    /// P4: refused because offline mode is on (`OXA_OFFLINE`, `--offline`, `oxa.offline`).
    pub const OFFLINE: &str = "offline";
    /// P3: the engine host is not loopback and `oxa.engine.allowRemote` is off.
    pub const REMOTE_ENGINE: &str = "remote_engine";
}

/// A cooperative cancellation flag shared between the UI thread and workers.
///
/// Cloning shares the flag. Long operations poll [`CancelToken::is_cancelled`] at least every
/// 100 ms; blocking reads use a read timeout so they can poll.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<CancelState>);

#[derive(Debug, Default)]
struct CancelState {
    flag: AtomicBool,
    waiters: Mutex<Vec<Weak<Mutex<Option<Waker>>>>>,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.flag.store(true, Ordering::SeqCst);
        let waiters = std::mem::take(&mut *self.0.waiters.lock().unwrap_or_else(|e| e.into_inner()));
        for waiter in waiters.into_iter().filter_map(|waiter| waiter.upgrade()) {
            let waker = waiter.lock().unwrap_or_else(|e| e.into_inner()).take();
            if let Some(waker) = waker {
                waker.wake();
            }
        }
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.flag.load(Ordering::SeqCst)
    }
    /// Clear the flag for the next run. Only the owner of the run calls this.
    pub fn reset(&self) {
        self.0.flag.store(false, Ordering::SeqCst);
    }

    /// Wake an async operation once cancelled, without a timer or worker thread.
    /// As with the flag, the run's owner resets only after its operations finish.
    pub fn cancelled(&self) -> impl std::future::Future<Output = ()> + Send + 'static {
        let token = self.clone();
        async move {
            if token.is_cancelled() {
                return;
            }
            let waiter = Arc::new(Mutex::new(None::<Waker>));
            {
                let mut waiters = token.0.waiters.lock().unwrap_or_else(|e| e.into_inner());
                waiters.retain(|waiter| waiter.strong_count() > 0);
                waiters.push(Arc::downgrade(&waiter));
            }
            std::future::poll_fn(move |context| {
                let mut registered = waiter.lock().unwrap_or_else(|e| e.into_inner());
                if !registered
                    .as_ref()
                    .is_some_and(|waker| waker.will_wake(context.waker()))
                {
                    *registered = Some(context.waker().clone());
                }
                // Recheck after registration: cancellation may race the first poll.
                if token.is_cancelled() {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            })
            .await;
        }
    }
}

#[cfg(test)]
mod cancellation_tests {
    use super::*;
    use std::{
        future::Future,
        sync::atomic::AtomicUsize,
        task::{Context, Wake},
    };
    struct Count(AtomicUsize);
    impl Wake for Count {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    #[test]
    fn cancellation_wakes_registered_operations_once_and_reset_starts_a_new_run() {
        let token = CancelToken::new();
        let counts = [
            Arc::new(Count(AtomicUsize::new(0))),
            Arc::new(Count(AtomicUsize::new(0))),
        ];
        let mut futures = [Box::pin(token.cancelled()), Box::pin(token.cancelled())];
        for (future, count) in futures.iter_mut().zip(&counts) {
            assert!(future
                .as_mut()
                .poll(&mut Context::from_waker(&Waker::from(count.clone())))
                .is_pending());
        }
        let other = token.clone();
        std::thread::spawn(move || other.cancel()).join().unwrap();
        token.cancel();
        for (future, count) in futures.iter_mut().zip(&counts) {
            assert_eq!(count.0.load(Ordering::SeqCst), 1);
            assert!(future
                .as_mut()
                .poll(&mut Context::from_waker(&Waker::from(count.clone())))
                .is_ready());
        }
        token.reset();
        let mut next = Box::pin(token.cancelled());
        let waker = Waker::from(counts[0].clone());
        assert!(next.as_mut().poll(&mut Context::from_waker(&waker)).is_pending());
        token.cancel();
        assert!(next.as_mut().poll(&mut Context::from_waker(&waker)).is_ready());
    }
    #[test]
    fn pre_cancelled_and_dropped_waiters_need_no_background_work() {
        let token = CancelToken::new();
        let count = Arc::new(Count(AtomicUsize::new(0)));
        let waker = Waker::from(count.clone());
        let mut dropped = Box::pin(token.cancelled());
        assert!(dropped
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending());
        drop(dropped);
        token.cancel();
        assert_eq!(count.0.load(Ordering::SeqCst), 0);
        assert!(Box::pin(token.cancelled())
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_ready());
    }
}
