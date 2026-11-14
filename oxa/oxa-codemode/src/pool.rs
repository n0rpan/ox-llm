//! E6: the worker pool (at most N script VMs alive per process). Owner: task E6. Signatures below are frozen; bodies are yours.
// ported from pi packages/codemode/src/runtime/*
use crate::api::{install_globals, ScriptSink};
use crate::tool::{output_limit_error, ScriptOptions, ScriptOutcome};
use crate::{Shared, WORKER_WAIT_S};
use oxa_proto::{CancelToken, Value};
use oxa_script::{InterruptHandle, Limits, Runtime};
use std::collections::BTreeSet;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

const WORKER_WAIT_TEXT: &str = "no codemode worker became free within 60 seconds";
const DEADLINE_WAIT_TEXT: &str = "the script's timeout_ms passed before a codemode worker became free";
const SHUT_DOWN_TEXT: &str = "codemode is shut down";

/// Mutable pool state, guarded by `WorkerPool::state`.
#[derive(Default)]
pub(crate) struct PoolState {
    /// Workers alive (a slot is held until the worker thread has exited).
    pub(crate) alive: usize,
    /// Ticket allowed to take the next slot (FIFO in arrival order).
    pub(crate) serve: u64,
    /// Next ticket to hand out.
    pub(crate) next: u64,
    /// Tickets whose waiter gave up; skipped when they reach the head.
    pub(crate) abandoned: BTreeSet<u64>,
    /// Set by `shutdown`; no new script starts.
    pub(crate) closed: bool,
    /// Interrupt handles of the scripts running now, by ticket.
    pub(crate) running: Vec<(u64, InterruptHandle)>,
}

/// Limits how many script VMs are alive at once. A cancelled worker occupies its slot until its
/// thread has exited. At capacity, scripts wait for a free worker in arrival order.
pub struct WorkerPool {
    pub(crate) max: usize,
    pub(crate) state: Mutex<PoolState>,
    pub(crate) freed: Condvar,
}

/// A held worker slot; released when dropped.
struct Slot {
    shared: Arc<Shared>,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let pool = &self.shared.pool;
        {
            let mut st = lock(&pool.state);
            st.alive = st.alive.saturating_sub(1);
        }
        pool.freed.notify_all();
    }
}

fn lock(m: &Mutex<PoolState>) -> MutexGuard<'_, PoolState> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Failed outcome with no output.
fn failed(message: impl Into<String>) -> ScriptOutcome {
    ScriptOutcome {
        ok: false,
        error: Some(message.into()),
        ..ScriptOutcome::default()
    }
}

impl WorkerPool {
    pub fn new(max_workers: usize) -> WorkerPool {
        WorkerPool {
            max: max_workers.max(1),
            state: Mutex::new(PoolState::default()),
            freed: Condvar::new(),
        }
    }

    pub fn max_workers(&self) -> usize {
        self.max
    }

    /// Run one script to its end on a fresh worker thread with a fresh `oxa_script::Runtime`
    /// (`Limits::CODEMODE`, deadline from `opts.timeout_ms`) and block until it is done.
    ///
    /// * Waits for a free worker for at most `WORKER_WAIT_S` seconds (less if `timeout_ms` is
    ///   shorter: the deadline includes the wait); no worker = a failed outcome with the fixed
    ///   error text `no codemode worker became free within 60 seconds`.
    /// * `cancel` interrupts the script; the outcome is failed with `Script aborted`.
    /// * A worker panic is caught and reported as a failed outcome.
    pub fn run(
        &self,
        shared: &Arc<Shared>,
        source: &str,
        opts: &ScriptOptions,
        cancel: &CancelToken,
    ) -> ScriptOutcome {
        let started = Instant::now();
        let wait_limit = Duration::from_secs(WORKER_WAIT_S);
        let timeout = opts.timeout_ms.map(Duration::from_millis);
        let (limit, limit_text) = match timeout {
            Some(t) if t < wait_limit => (t, DEADLINE_WAIT_TEXT),
            _ => (wait_limit, WORKER_WAIT_TEXT),
        };
        let (slot, ticket) = match self.acquire(shared, limit) {
            Ok(v) => v,
            Err(()) => {
                return failed(if self.is_closed() {
                    SHUT_DOWN_TEXT
                } else {
                    limit_text
                });
            }
        };
        // Time already spent waiting counts against the script's own deadline.
        let remaining_ms = timeout.map(|t| {
            let left = t.saturating_sub(started.elapsed()).as_millis();
            left.max(1) as u64
        });
        let source = source.to_string();
        let sh = shared.clone();
        let (handle_tx, handle_rx) = mpsc::channel::<InterruptHandle>();
        let (outcome_tx, outcome_rx) = mpsc::channel::<ScriptOutcome>();
        let spawned = thread::Builder::new()
            .name("oxa-codemode-worker".to_string())
            .spawn(move || {
                let _slot = slot;
                let outcome =
                    oxa_proto::panic::contain(|| execute(&sh, &source, ticket, remaining_ms, handle_tx))
                        .unwrap_or_else(|error| failed(format!("the script worker panicked: {error}")));
                let _ = outcome_tx.send(outcome);
            });
        let join = match spawned {
            Ok(j) => j,
            Err(e) => return failed(format!("could not start a codemode worker: {e}")),
        };

        let mut handle: Option<InterruptHandle> = None;
        let outcome = loop {
            if handle.is_none() {
                handle = handle_rx.try_recv().ok();
            }
            if cancel.is_cancelled() {
                if let Some(h) = &handle {
                    h.interrupt();
                }
            }
            match outcome_rx.recv_timeout(Duration::from_millis(10)) {
                Ok(outcome) => break outcome,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break failed("the script worker stopped"),
            }
        };
        let _ = join.join();

        let mut outcome = outcome;
        if cancel.is_cancelled() && !outcome.ok {
            outcome.error = Some("Script aborted".to_string());
        } else if !outcome.ok && self.is_closed() && outcome.error.is_none() {
            outcome.error = Some(SHUT_DOWN_TEXT.to_string());
        }
        outcome
    }

    /// Workers alive right now.
    pub fn alive(&self) -> usize {
        lock(&self.state).alive
    }

    fn is_closed(&self) -> bool {
        lock(&self.state).closed
    }

    /// Wait for a free slot, in ticket order. `Err(())` when the wait passed `limit` or the pool
    /// is shut down.
    fn acquire(&self, shared: &Arc<Shared>, limit: Duration) -> Result<(Slot, u64), ()> {
        let started = Instant::now();
        let mut st = lock(&self.state);
        if st.closed {
            return Err(());
        }
        let ticket = st.next;
        st.next += 1;
        loop {
            skip_abandoned(&mut st);
            if st.closed {
                st.abandoned.insert(ticket);
                skip_abandoned(&mut st);
                self.freed.notify_all();
                return Err(());
            }
            if st.serve == ticket && st.alive < self.max {
                st.serve += 1;
                st.alive += 1;
                skip_abandoned(&mut st);
                drop(st);
                self.freed.notify_all();
                return Ok((
                    Slot {
                        shared: shared.clone(),
                    },
                    ticket,
                ));
            }
            let elapsed = started.elapsed();
            if elapsed >= limit {
                st.abandoned.insert(ticket);
                skip_abandoned(&mut st);
                drop(st);
                self.freed.notify_all();
                return Err(());
            }
            let (guard, _) = match self.freed.wait_timeout(st, limit - elapsed) {
                Ok(pair) => pair,
                Err(poisoned) => poisoned.into_inner(),
            };
            st = guard;
        }
    }

    /// Record a running script's interrupt handle. `false` when the pool is shut down.
    pub(crate) fn enter_run(&self, ticket: u64, handle: InterruptHandle) -> bool {
        let mut st = lock(&self.state);
        if st.closed {
            return false;
        }
        st.running.push((ticket, handle));
        true
    }

    pub(crate) fn leave_run(&self, ticket: u64) {
        lock(&self.state).running.retain(|(t, _)| *t != ticket);
    }

    /// Interrupt every running script and refuse new ones. Idempotent.
    pub fn shutdown(&self) {
        let handles: Vec<InterruptHandle> = {
            let mut st = lock(&self.state);
            st.closed = true;
            st.running.iter().map(|(_, h)| h.clone()).collect()
        };
        for h in handles {
            h.interrupt();
        }
        self.freed.notify_all();
    }
}

/// Drop abandoned tickets from the head of the queue.
fn skip_abandoned(st: &mut PoolState) {
    while st.abandoned.remove(&st.serve) {
        st.serve += 1;
    }
}

/// Body of a worker thread: a fresh runtime with the codemode globals, the script, and the
/// outcome built from the sink. The runtime is dropped before this returns.
fn execute(
    shared: &Arc<Shared>,
    source: &str,
    ticket: u64,
    deadline_ms: Option<u64>,
    handle_tx: mpsc::Sender<InterruptHandle>,
) -> ScriptOutcome {
    let mut rt = match Runtime::new(Limits::CODEMODE) {
        Ok(rt) => rt,
        Err(e) => return failed(e.message),
    };
    rt.set_deadline(deadline_ms);
    let handle = rt.interrupt_handle();
    let _ = handle_tx.send(handle.clone());
    if !shared.pool.enter_run(ticket, handle) {
        return failed(SHUT_DOWN_TEXT);
    }
    let sink = Arc::new(Mutex::new(ScriptSink::default()));
    if let Err(e) = install_globals(&mut rt, shared, &sink) {
        return failed(e.message);
    }
    let result = rt.eval_async_body("codemode", source);
    shared.pool.leave_run(ticket);

    let (texts, console, store_writes, exited, limit) = {
        let s = sink.lock().unwrap_or_else(|p| p.into_inner());
        (
            s.texts.clone(),
            s.console.clone(),
            s.store_writes.clone(),
            s.exited,
            output_limit_error(&s),
        )
    };
    let mut texts = texts;
    let (ok, error, returned) = match (limit, result) {
        (Some(message), _) => (false, Some(message), None),
        (None, _) if exited => (true, None, None),
        (None, Ok(value)) => (true, None, Some(value)),
        (None, Err(e)) => {
            let mut message = e.message;
            if let Some(hint) = e.hint.filter(|h| !h.is_empty()) {
                message.push('\n');
                message.push_str(&hint);
            }
            (false, Some(message), None)
        }
    };
    if let Some(value) = returned {
        if !value.is_null() {
            texts.push(match value {
                Value::String(s) => s,
                other => other.to_string(),
            });
        }
    }
    ScriptOutcome {
        ok,
        texts,
        console,
        error,
        store_writes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abandoned_tickets_are_skipped_at_the_head() {
        let mut st = PoolState::default();
        st.abandoned.insert(0);
        st.abandoned.insert(1);
        skip_abandoned(&mut st);
        assert_eq!(st.serve, 2);
    }
}
