//! C1: steering / follow-up queues and the cross-thread handle. Owner: task C1. Signatures below are frozen; bodies are yours.
// ported from pi packages/agent/src/agent.ts (steer, followUp, queue modes)
use oxa_config::QueueMode;
use oxa_proto::CancelToken;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard};

/// The two message queues.
#[derive(Debug, Default)]
pub struct Queue {
    pub steering: Vec<String>,
    pub follow_up: Vec<String>,
}

/// State shared between a `Session` and its `Handle`s.
#[derive(Debug)]
pub struct Shared {
    pub queue: Mutex<Queue>,
    /// Cancels the current run (stream and tools).
    pub cancel: CancelToken,
    /// Cancels a pending auto-retry wait only.
    pub cancel_retry: CancelToken,
    /// `true` while `prompt` is running.
    pub busy: std::sync::atomic::AtomicBool,
    pub modes: Mutex<(QueueMode, QueueMode)>,
    /// Set by every queue change; the loop turns it into one `QueueUpdate` event.
    pub dirty: std::sync::atomic::AtomicBool,
    /// S1 / ENG-001 / ENG-002: the request gate and the background cancel (see [`Gate`]).
    pub gate: Gate,
}

/// The engine-use state of one instance (S1, F4, S4). `Default` = idle.
///
/// * `in_flight`: a model request of this instance is running (`exec::request` holds it); a
///   second request is refused with `busy` (ENG-001: one oxa instance never has two requests
///   in flight).
/// * `bg_cancel`: cancels this instance's own BACKGROUND request (threshold summary, idle
///   prefill). [`Handle::steer`], [`Handle::follow_up`] and [`Handle::abort`] set it; it is
///   cleared when a background request starts ([`Shared::begin_background`]) and when the
///   session is claimed. Foreground requests (the run, `/compact`, overflow recovery, the
///   pre-switch compaction, a branch summary, a park) use `cancel` only (ENG-002).
/// * `reread_info`: a request recovered from `engine_busy` / 5xx / `engine_down` (a model load
///   or a restart happened): the loop re-reads the engine metadata once after the reply (F4).
/// * `info_rechecked`: the one re-read after the first successful reply while `prefill_only`
///   was still false has happened (F4).
/// * `override_warned`: (session id, prefix sha) of the epoch in which the
///   `compaction.modelOverrides` warning was shown (S4: once per session and epoch).
/// * `abort_next`: an abort aimed at a run a front end accepted that has not claimed the
///   session yet ([`Handle::abort_next`]); the next claim takes it and starts cancelled.
/// * `bg_abort`: [`Handle::cancel_background`] was called since the session was last claimed:
///   no background request starts before the next claim (a background request that claimed
///   the session and has not armed `bg_cancel` yet would otherwise miss the cancel).
#[derive(Debug, Default)]
pub struct Gate {
    pub in_flight: std::sync::atomic::AtomicBool,
    pub bg_cancel: CancelToken,
    pub reread_info: std::sync::atomic::AtomicBool,
    pub info_rechecked: std::sync::atomic::AtomicBool,
    pub override_warned: Mutex<Option<(String, [u8; 32])>>,
    pub abort_next: std::sync::atomic::AtomicBool,
    pub bg_abort: std::sync::atomic::AtomicBool,
}

/// Holds [`Gate::in_flight`] for one request; dropping it opens the gate.
pub(crate) struct GateGuard(Arc<Shared>);

impl Drop for GateGuard {
    fn drop(&mut self) {
        self.0.gate.in_flight.store(false, Ordering::SeqCst);
    }
}

/// Take from one queue by mode: everything in `All`, else the oldest message.
fn take_by_mode(list: &mut Vec<String>, mode: QueueMode) -> Vec<String> {
    match mode {
        QueueMode::All => std::mem::take(list),
        QueueMode::OneAtATime => {
            if list.is_empty() {
                Vec::new()
            } else {
                vec![list.remove(0)]
            }
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

impl Shared {
    pub fn new(steering: QueueMode, follow_up: QueueMode) -> Shared {
        Shared {
            queue: Mutex::new(Queue::default()),
            cancel: CancelToken::new(),
            cancel_retry: CancelToken::new(),
            busy: std::sync::atomic::AtomicBool::new(false),
            modes: Mutex::new((steering, follow_up)),
            dirty: std::sync::atomic::AtomicBool::new(false),
            gate: Gate::default(),
        }
    }

    /// ENG-001: claim the request gate of this instance, or `busy` when a request is already in
    /// flight (only reachable through re-entrancy: a host request served during a request's
    /// hooks that would itself call the engine).
    pub(crate) fn enter_request(self: &Arc<Shared>) -> Result<GateGuard, oxa_proto::OxaError> {
        if self
            .gate
            .in_flight
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(oxa_proto::OxaError::new(
                oxa_proto::codes::BUSY,
                "a model request of this session is already in flight (one request at a time)",
            ));
        }
        Ok(GateGuard(self.clone()))
    }

    /// Steering or follow-up messages are queued.
    pub fn has_input(&self) -> bool {
        let q = lock(&self.queue);
        !q.steering.is_empty() || !q.follow_up.is_empty()
    }

    /// ENG-002: arm the background cancel for one background request. Returns `false` (send
    /// nothing) when user input is already queued or the run was aborted. The token is cleared
    /// BEFORE the queue is read, and the handle queues before it cancels, so input that arrives
    /// at any moment either is seen here or cancels the request.
    pub(crate) fn begin_background(&self) -> bool {
        self.gate.bg_cancel.reset();
        !(self.has_input() || self.cancel.is_cancelled() || self.gate.bg_abort.load(Ordering::SeqCst))
    }

    /// Take steering messages for delivery after a tool batch: all of them (`All`) or the
    /// oldest one (`OneAtATime`).
    pub fn take_steering(&self) -> Vec<String> {
        let mode = lock(&self.modes).0;
        let taken = take_by_mode(&mut lock(&self.queue).steering, mode);
        if !taken.is_empty() {
            self.dirty.store(true, Ordering::SeqCst);
        }
        taken
    }

    /// Take follow-ups when the agent would stop; same mode rule.
    pub fn take_follow_up(&self) -> Vec<String> {
        let mode = lock(&self.modes).1;
        let taken = take_by_mode(&mut lock(&self.queue).follow_up, mode);
        if !taken.is_empty() {
            self.dirty.store(true, Ordering::SeqCst);
        }
        taken
    }

    /// Both queues, cloned (steering, follow-up).
    pub(crate) fn snapshot(&self) -> (Vec<String>, Vec<String>) {
        let q = lock(&self.queue);
        (q.steering.clone(), q.follow_up.clone())
    }
}

/// The cross-thread control surface. `Send + Clone`.
#[derive(Debug, Clone)]
pub struct Handle {
    pub(crate) shared: Arc<Shared>,
}

impl Handle {
    /// Queue a steering message (delivered after the current tool batch; when idle the caller
    /// should `prompt` instead).
    /// It also cancels this instance's own background request (ENG-002); a foreground request
    /// is not touched.
    pub fn steer(&self, text: String) {
        lock(&self.shared.queue).steering.push(text);
        self.shared.dirty.store(true, Ordering::SeqCst);
        self.shared.gate.bg_cancel.cancel();
    }
    /// Queue a follow-up (delivered when the agent would otherwise stop). Cancels this
    /// instance's own background request like [`Handle::steer`].
    pub fn follow_up(&self, text: String) {
        lock(&self.shared.queue).follow_up.push(text);
        self.shared.dirty.store(true, Ordering::SeqCst);
        self.shared.gate.bg_cancel.cancel();
    }
    /// Remove and return all queued messages, steering first (Alt+Up, `clear_queue`).
    pub fn dequeue(&self) -> Vec<String> {
        let (mut steering, follow_up) = {
            let mut q = lock(&self.shared.queue);
            (std::mem::take(&mut q.steering), std::mem::take(&mut q.follow_up))
        };
        let changed = !steering.is_empty() || !follow_up.is_empty();
        steering.extend(follow_up);
        if changed {
            self.shared.dirty.store(true, Ordering::SeqCst);
        }
        steering
    }
    /// Abort the current run. The partial reply is shown, never sent (W2). Queued messages stay
    /// queued; the front end restores them with `dequeue`. A no-op when no run is in progress
    /// (a stale cancel must not abort the next prompt; `Session` also resets the token).
    pub fn abort(&self) {
        if self.shared.busy.load(Ordering::SeqCst) {
            self.shared.cancel.cancel();
            self.shared.gate.bg_cancel.cancel();
        }
    }
    /// Cancel only this instance's BACKGROUND work (a boot preload, a draft prefill, an idle
    /// prefill): `bg_cancel`, and no background request starts until the session is claimed
    /// again (`Gate::bg_abort`). Unlike [`Handle::abort`], `cancel` (the user's run) is never
    /// touched, so a front end may call it whenever it is unsure whether a background request
    /// or the user's next run holds the session: the run is not cancelled, and a run claimed
    /// after this call starts with no trace of it.
    pub fn cancel_background(&self) {
        self.shared.gate.bg_abort.store(true, Ordering::SeqCst);
        self.shared.gate.bg_cancel.cancel();
    }
    /// Abort the run a front end has accepted but not started yet (the RPC server answered a
    /// prompt `started` and the session thread has not reached it): the next claim of the
    /// session starts cancelled. [`Handle::clear_abort_next`] withdraws it.
    pub fn abort_next(&self) {
        self.shared.gate.abort_next.store(true, Ordering::SeqCst);
    }
    /// Withdraw an [`Handle::abort_next`] that no claim has taken.
    pub fn clear_abort_next(&self) {
        self.shared.gate.abort_next.store(false, Ordering::SeqCst);
    }
    /// Abort a pending retry wait without aborting anything else.
    pub fn abort_retry(&self) {
        self.shared.cancel_retry.cancel();
    }
    pub fn is_busy(&self) -> bool {
        self.shared.busy.load(Ordering::SeqCst)
    }
    /// The queues (steering, follow-up) when they changed since the last report, and clear the
    /// change mark. A front end that changed the queue from another thread reports the change
    /// itself this way, at once (pi emits `queue_update` when a message is queued); the session
    /// then does not report the same change again.
    pub fn take_queue_update(&self) -> Option<(Vec<String>, Vec<String>)> {
        self.shared
            .dirty
            .swap(false, Ordering::SeqCst)
            .then(|| self.shared.snapshot())
    }
    pub fn queued(&self) -> (Vec<String>, Vec<String>) {
        self.shared.snapshot()
    }
    pub fn set_modes(&self, steering: QueueMode, follow_up: QueueMode) {
        *lock(&self.shared.modes) = (steering, follow_up);
        self.shared.dirty.store(true, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handle(steer: QueueMode, follow: QueueMode) -> Handle {
        Handle {
            shared: Arc::new(Shared::new(steer, follow)),
        }
    }

    #[test]
    fn dequeue_is_steering_first_then_follow_up() {
        let h = handle(QueueMode::All, QueueMode::All);
        h.follow_up("f1".to_string());
        h.steer("s1".to_string());
        h.steer("s2".to_string());
        assert_eq!(h.dequeue(), vec!["s1", "s2", "f1"]);
        assert_eq!(h.queued(), (Vec::new(), Vec::new()));
    }

    #[test]
    fn abort_when_idle_is_a_no_op_and_keeps_queue() {
        let h = handle(QueueMode::All, QueueMode::All);
        h.steer("s1".to_string());
        h.abort();
        assert!(!h.shared.cancel.is_cancelled());
        assert_eq!(h.queued().0, vec!["s1"]);
    }

    #[test]
    fn abort_while_busy_cancels_and_keeps_queue() {
        let h = handle(QueueMode::All, QueueMode::All);
        h.shared.busy.store(true, Ordering::SeqCst);
        h.steer("s1".to_string());
        h.abort();
        assert!(h.shared.cancel.is_cancelled());
        assert!(!h.shared.cancel_retry.is_cancelled());
        assert_eq!(h.dequeue(), vec!["s1"]);
    }

    /// A background cancel never reaches the user's run: `cancel` stays clear, and a run
    /// claimed after it (`Session::begin` resets `bg_cancel` and `bg_abort`) starts clean.
    /// A background request that has claimed but not armed yet sees it.
    #[test]
    fn cancel_background_spares_the_run() {
        let h = handle(QueueMode::All, QueueMode::All);
        // A background request holds the session (boot preload, draft prefill).
        h.shared.busy.store(true, Ordering::SeqCst);
        h.cancel_background();
        assert!(h.shared.gate.bg_cancel.is_cancelled());
        assert!(!h.shared.cancel.is_cancelled(), "the run's token is not touched");
        assert!(
            !h.shared.begin_background(),
            "a background request about to arm is skipped"
        );
        // The background request ends; the user's prompt claims (what `Session::begin` does).
        h.shared.busy.store(false, Ordering::SeqCst);
        h.shared.busy.store(true, Ordering::SeqCst);
        h.shared.cancel.reset();
        h.shared.gate.bg_cancel.reset();
        h.shared.gate.bg_abort.store(false, Ordering::SeqCst);
        assert!(!h.shared.cancel.is_cancelled());
        assert!(
            h.shared.begin_background(),
            "the run's own background work is allowed"
        );
        // `abort` of a real run is unchanged: both tokens.
        h.abort();
        assert!(h.shared.cancel.is_cancelled());
        assert!(h.shared.gate.bg_cancel.is_cancelled());
    }

    #[test]
    fn abort_retry_cancels_only_the_wait() {
        let h = handle(QueueMode::All, QueueMode::All);
        h.abort_retry();
        assert!(h.shared.cancel_retry.is_cancelled());
        assert!(!h.shared.cancel.is_cancelled());
    }

    #[test]
    fn take_follow_up_respects_mode_and_marks_dirty() {
        let sh = Shared::new(QueueMode::All, QueueMode::OneAtATime);
        sh.queue.lock().unwrap().follow_up = vec!["a".into(), "b".into()];
        assert!(!sh.dirty.load(Ordering::SeqCst));
        assert_eq!(sh.take_follow_up(), vec!["a"]);
        assert!(sh.dirty.load(Ordering::SeqCst));
        assert_eq!(sh.take_follow_up(), vec!["b"]);
        assert!(sh.take_follow_up().is_empty());
    }

    #[test]
    fn set_modes_applies_to_later_takes() {
        let h = handle(QueueMode::OneAtATime, QueueMode::All);
        h.steer("s1".to_string());
        h.steer("s2".to_string());
        assert_eq!(h.shared.take_steering(), vec!["s1"]);
        h.set_modes(QueueMode::All, QueueMode::All);
        assert_eq!(h.shared.take_steering(), vec!["s2"]);
    }
}
