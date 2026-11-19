//! C1: steering / follow-up queues and the cross-thread handle.
// ported from pi packages/agent/src/agent.ts (steer, followUp, queue modes)
use crate::goal::GoalSteering;
use oxa_config::QueueMode;
use oxa_proto::CancelToken;
use oxa_session::SessionRef;
use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard};

/// The two message queues.
#[derive(Debug, Default)]
pub struct Queue {
    pub steering: Vec<String>,
    pub follow_up: Vec<String>,
    session_id: String,
    /// Inbox messages stay with their destination, including across switches and clones.
    session_follow_up: BTreeMap<String, Vec<String>>,
    /// One private steering request per active native run.
    steering_session: Option<SessionRef>,
    goal_steering: Option<GoalSteering>,
}

impl Queue {
    fn follow_ups(&self) -> impl Iterator<Item = &String> {
        self.follow_up
            .iter()
            .chain(self.session_follow_up.get(&self.session_id).into_iter().flatten())
    }

    fn take_follow_up(&mut self, mode: QueueMode) -> Vec<String> {
        let mut taken = take_by_mode(&mut self.follow_up, mode);
        if mode == QueueMode::All || taken.is_empty() {
            if let Some(messages) = self.session_follow_up.get_mut(&self.session_id) {
                taken.extend(take_by_mode(messages, mode));
            }
        }
        taken
    }
}

/// State shared between a `Session` and its `Handle`s.
#[derive(Debug)]
pub struct Shared {
    pub queue: Mutex<Queue>,
    /// Cancels the current run (stream and tools).
    pub cancel: CancelToken,
    /// Cancels a pending auto-retry wait only.
    pub cancel_retry: CancelToken,
    /// Wakes a running `wait` tool when input for the active session arrives (reset per wait).
    pub wait_wake: CancelToken,
    /// `true` while `prompt` is running.
    pub busy: std::sync::atomic::AtomicBool,
    pub modes: Mutex<(QueueMode, QueueMode)>,
    /// Set by every queue change; the loop turns it into one `QueueUpdate` event.
    pub dirty: std::sync::atomic::AtomicBool,
    /// S1 / ENG-001 / ENG-002: the request gate and the background cancel (see [`Gate`]).
    pub gate: Gate,
}

/// What an extension asked for: a new session, or another one (a path or an id).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwapRequest {
    New,
    Switch(String),
}

/// Why an unattended front end requested a final turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum WrapUpReason {
    TimeLimit = 1,
    Signal = 2,
    TokenLimit = 3,
}

impl WrapUpReason {
    pub(crate) fn notice(self) -> &'static str {
        match self {
            Self::TimeLimit => "time limit reached; one final turn to commit or summarize",
            Self::Signal => "SIGTERM received; one final turn to commit or summarize",
            Self::TokenLimit => "token limit reached; one final turn to commit or summarize",
        }
    }
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
    /// L1: the settings files as last seen (`settings_refresh`).
    pub(crate) settings_watch: Mutex<crate::settings_refresh::Watch>,
    live_settings: Mutex<LiveSettings>,
    goal_steering_enabled: std::sync::atomic::AtomicBool,
    pub(crate) lead_steering_authority: Mutex<Option<crate::goal::BoundLeadAuthority>>,
    pub(crate) advisor: Mutex<Option<Arc<crate::advice::BoundAdvisor>>>,
    /// Coalesce frontend inbox wakes while a model run drains follow-ups internally.
    pub inbox_wake_pending: std::sync::atomic::AtomicBool,
    /// Foreground work, excluding settlement after a dropped reply.
    pub running: std::sync::atomic::AtomicBool,
    /// Serializes abort publication with foreground reservation claims and releases.
    pub(crate) foreground_transition: Mutex<()>,
    /// Coalesce worker wakes for ordinary editor queues, independently of bound inbox wakes.
    pub queued_wake_pending: std::sync::atomic::AtomicBool,
    pub progress: Mutex<Option<Arc<dyn crate::ProgressObserver>>>,
    /// The owner confirm broker of this session (`Session::enable_owner_confirm`).
    pub(crate) owner_confirm: Mutex<Option<Arc<crate::owner_confirm::OwnerConfirm>>>,
    pub in_flight: std::sync::atomic::AtomicBool,
    /// Sticky headless interruption: abort now, without a final wrap-up turn.
    pub interrupted: std::sync::atomic::AtomicBool,
    /// Sticky soft termination request: zero, or a `WrapUpReason` discriminant.
    pub wrap_up: std::sync::atomic::AtomicU8,
    /// Remaining uncached prompt + completion tokens for an unattended invocation.
    pub token_budget: Mutex<Option<u64>>,
    literal_turn: Mutex<Option<LiteralTurn>>,
    /// Total charged usage, including non-chat requests and final-turn overspend.
    pub run_tokens_used: Mutex<u64>,
    pub bg_cancel: CancelToken,
    pub reread_info: std::sync::atomic::AtomicBool,
    pub info_rechecked: std::sync::atomic::AtomicBool,
    pub override_warned: Mutex<Option<(String, [u8; 32])>>,
    pub abort_next: std::sync::atomic::AtomicBool,
    pub bg_abort: std::sync::atomic::AtomicBool,
    /// A session swap an extension asked for (`ctx.newSession()` / `ctx.switchSession()`),
    /// held for the front end, which opted in with `swap_enabled`.
    pub swap: Mutex<Option<SwapRequest>>,
    /// `true` once a front end polls `Session::take_swap_request`; without it the two
    /// extension calls answer `unsupported_in_oxa`.
    pub swap_enabled: std::sync::atomic::AtomicBool,
    /// The wake a front end registered for state it cannot poll (see
    /// [`Handle::set_attention_waker`]).
    pub attention: Attention,
}

/// The wake a front end registered for state it cannot see through its own channels: a host
/// request posted while its worker waits ([`CoreHost::post`]) and a wrap-up or interruption
/// request. A front end that would otherwise poll a channel waits with a deadline and is
/// woken here instead. One waker per session; the closure must be cheap and must not block.
#[derive(Default)]
pub struct Attention(Mutex<Option<Arc<dyn Fn() + Send + Sync>>>);

impl std::fmt::Debug for Attention {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The closure is not printable; its presence is enough for a `{:?}` of the state.
        f.write_str("Attention(..)")
    }
}

impl Attention {
    fn set(&self, waker: Arc<dyn Fn() + Send + Sync>) {
        *lock(&self.0) = Some(waker);
    }

    /// Wake the front end, if one registered. Called after the state the front end will look
    /// at is in place (the host request is in its channel, the wrap-up flag is set), so a
    /// woken front end that re-checks sees it. A panic in the waker is contained here.
    pub(crate) fn notify(&self) {
        let waker = lock(&self.0).clone();
        if let Some(waker) = waker {
            let _ = oxa_proto::panic::contain(|| waker());
        }
    }
}

#[derive(Default)]
struct LiveSettings {
    subscribed: bool,
    pending: Option<oxa_config::Settings>,
}

impl std::fmt::Debug for LiveSettings {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fmt.debug_struct("LiveSettings")
            .field("subscribed", &self.subscribed)
            .field("pending", &self.pending.is_some())
            .finish()
    }
}

/// Holds [`Gate::in_flight`] for one request; dropping it opens the gate.
pub(crate) struct GateGuard(Arc<Shared>);

struct LiteralTurn {
    context: Option<oxa_proto::WireMsg>,
    remaining: u32,
}

impl std::fmt::Debug for LiteralTurn {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LiteralTurn")
            .field("remaining", &self.remaining)
            .finish_non_exhaustive()
    }
}

pub(crate) struct LiteralTurnGuard(Arc<Shared>);

impl Drop for LiteralTurnGuard {
    fn drop(&mut self) {
        lock(&self.0.gate.literal_turn).take();
    }
}

pub(crate) struct GoalSteeringGuard(Arc<Shared>);

impl Drop for GoalSteeringGuard {
    fn drop(&mut self) {
        let pending = {
            let mut queue = lock(&self.0.queue);
            queue.steering_session = None;
            queue.goal_steering.take()
        };
        if let Some(request) = pending {
            request.reject();
        }
    }
}

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

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

impl Shared {
    pub(crate) fn publish_live_settings(&self, settings: &oxa_config::Settings) {
        let mut live = lock(&self.gate.live_settings);
        if live.subscribed {
            live.pending = Some(settings.clone());
        }
    }

    pub(crate) fn begin_literal_turn(
        self: &Arc<Self>,
        context: Option<oxa_proto::WireMsg>,
        max_output: std::num::NonZeroU32,
    ) -> Result<LiteralTurnGuard, oxa_proto::OxaError> {
        let mut slot = lock(&self.gate.literal_turn);
        if slot.is_some() {
            return Err(oxa_proto::OxaError::new(
                oxa_proto::codes::BUSY,
                "A literal turn is already active.",
            ));
        }
        *slot = Some(LiteralTurn {
            context,
            remaining: max_output.get(),
        });
        Ok(LiteralTurnGuard(self.clone()))
    }

    /// Reserve before generation. On a failure or missing completion count, the
    /// reservation stays charged, since the provider may already have generated.
    pub(crate) fn reserve_literal_output(
        &self,
        requested: u32,
        kind: oxa_proto::TurnKind,
    ) -> Result<Option<(u32, Option<oxa_proto::WireMsg>)>, oxa_proto::OxaError> {
        let mut slot = lock(&self.gate.literal_turn);
        let Some(turn) = slot.as_mut().filter(|_| requested > 0) else {
            return Ok(None);
        };
        let reserved = requested.min(turn.remaining);
        if reserved == 0 {
            return Err(oxa_proto::OxaError::new(
                oxa_proto::codes::LITERAL_OUTPUT_EXHAUSTED,
                "The literal turn's output limit is exhausted.",
            ));
        }
        turn.remaining -= reserved;
        let context = if kind == oxa_proto::TurnKind::Chat {
            turn.context.clone()
        } else {
            None
        };
        Ok(Some((reserved, context)))
    }

    /// `None` when no bounded literal turn is active, else the output tokens it
    /// can still reserve. The length-recovery path consults this before it
    /// schedules a request that could never be admitted.
    pub(crate) fn literal_output_remaining(&self) -> Option<u32> {
        lock(&self.gate.literal_turn).as_ref().map(|turn| turn.remaining)
    }

    pub(crate) fn settle_literal_output(&self, reserved: u32, completion: u32) {
        if completion > 0 {
            if let Some(turn) = lock(&self.gate.literal_turn).as_mut() {
                turn.remaining += reserved.saturating_sub(completion);
            }
        }
    }

    pub fn new(steering: QueueMode, follow_up: QueueMode) -> Shared {
        Shared {
            queue: Mutex::new(Queue::default()),
            cancel: CancelToken::new(),
            cancel_retry: CancelToken::new(),
            wait_wake: CancelToken::new(),
            busy: std::sync::atomic::AtomicBool::new(false),
            modes: Mutex::new((steering, follow_up)),
            dirty: std::sync::atomic::AtomicBool::new(false),
            gate: Gate::default(),
        }
    }

    pub(crate) fn owner_confirm(&self) -> Option<Arc<crate::owner_confirm::OwnerConfirm>> {
        lock(&self.gate.owner_confirm).clone()
    }

    /// The owner typed `text`: it answers a waiting owner confirmation. Only the handle's
    /// steer and follow-up (the owner's typed messages) call this, never the inbox path.
    fn owner_typed(&self, text: &str) {
        if let Some(broker) = self.owner_confirm() {
            broker.resolve_typed(text);
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

    /// Rebind only the inbox view; ordinary steering and follow-up behavior is unchanged.
    pub(crate) fn bind_session(&self, id: &str) {
        let mut q = lock(&self.queue);
        if q.session_id != id {
            let had_input = q.follow_ups().next().is_some();
            q.session_id = id.to_string();
            if had_input || q.follow_ups().next().is_some() {
                self.dirty.store(true, Ordering::SeqCst);
            }
        }
    }

    /// Arm the wait wakeup: reset it, then re-check the queues. Input is queued before the
    /// handle cancels, so input admitted at any moment either is seen here or wakes the wait.
    pub(crate) fn arm_wait_wake(&self) {
        self.wait_wake.reset();
        if self.has_input() {
            self.wait_wake.cancel();
        }
    }

    /// Steering or follow-up messages for the active session are queued.
    pub fn has_input(&self) -> bool {
        let q = lock(&self.queue);
        !q.steering.is_empty() || q.follow_ups().next().is_some()
    }

    pub(crate) fn has_follow_up(&self) -> bool {
        lock(&self.queue).follow_ups().next().is_some()
    }

    pub(crate) fn update_modes(&self, steering: Option<QueueMode>, follow_up: Option<QueueMode>) {
        let mut modes = lock(&self.modes);
        *modes = (steering.unwrap_or(modes.0), follow_up.unwrap_or(modes.1));
        self.dirty.store(true, Ordering::SeqCst);
    }

    /// ENG-002: arm the background cancel for one background request. Returns `false` (send
    /// nothing) when user input is already queued or the run was aborted. The token is cleared
    /// BEFORE the queue is read, and the handle queues before it cancels, so input that arrives
    /// at any moment either is seen here or cancels the request.
    pub(crate) fn begin_background(&self) -> bool {
        self.gate.bg_cancel.reset();
        if self.gate.wrap_up.load(Ordering::SeqCst) != 0 || self.gate.interrupted.load(Ordering::SeqCst) {
            self.gate.bg_cancel.cancel();
            return false;
        }
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

    pub(crate) fn open_goal_steering(self: &Arc<Self>, session: &SessionRef) -> Option<GoalSteeringGuard> {
        if !self.gate.goal_steering_enabled.load(Ordering::Relaxed) {
            return None;
        }
        lock(&self.queue).steering_session = Some(session.clone());
        Some(GoalSteeringGuard(self.clone()))
    }

    pub(crate) fn take_goal_steering(&self) -> Option<GoalSteering> {
        if !self.gate.goal_steering_enabled.load(Ordering::Relaxed) {
            return None;
        }
        lock(&self.queue).goal_steering.take()
    }

    /// Take follow-ups when the agent would stop; same mode rule.
    pub fn take_follow_up(&self) -> Vec<String> {
        let mode = lock(&self.modes).1;
        let taken = lock(&self.queue).take_follow_up(mode);
        if !taken.is_empty() {
            self.dirty.store(true, Ordering::SeqCst);
        }
        taken
    }

    /// Both queues, cloned (steering, follow-up).
    pub(crate) fn snapshot(&self) -> (Vec<String>, Vec<String>) {
        let q = lock(&self.queue);
        (q.steering.clone(), q.follow_ups().cloned().collect())
    }
}

/// The cross-thread control surface. `Send + Clone`.
#[derive(Debug, Clone)]
pub struct Handle {
    pub(crate) shared: Arc<Shared>,
}

impl Handle {
    /// Subscribe a local frontend to applied settings before it starts a run.
    pub fn watch_live_settings(&self) {
        lock(&self.shared.gate.live_settings).subscribed = true;
    }

    /// Take the latest effective settings without sending values through public events.
    pub fn take_live_settings(&self) -> Option<oxa_config::Settings> {
        lock(&self.shared.gate.live_settings).pending.take()
    }

    /// Enable typed steering for this frontend before it starts the native run.
    pub fn enable_goal_steering(&self) {
        self.shared
            .gate
            .goal_steering_enabled
            .store(true, Ordering::Relaxed);
    }

    /// Submit to the current native run. Success reports admission only. The
    /// owner revalidates the binding when it is ready to append.
    pub fn submit_goal_steering(&self, request: GoalSteering) -> Result<(), &'static str> {
        let mut queue = lock(&self.shared.queue);
        if queue.steering_session.as_ref() != Some(&request.binding.session) {
            return Err("goal_steer_stale");
        }
        if queue.goal_steering.is_some() {
            return Err("control_queue_full");
        }
        queue.goal_steering = Some(request);
        Ok(())
    }

    /// Queue a steering message (delivered after the current tool batch; when idle the caller
    /// should `prompt` instead).
    /// It also cancels this instance's own background request (ENG-002); a foreground request
    /// is not touched.
    pub fn steer(&self, text: String) {
        self.shared.owner_typed(&text);
        lock(&self.shared.queue).steering.push(text);
        self.shared.dirty.store(true, Ordering::SeqCst);
        self.shared.gate.bg_cancel.cancel();
        self.shared.wait_wake.cancel();
    }
    /// Queue a follow-up (delivered when the agent would otherwise stop). Cancels this
    /// instance's own background request like [`Handle::steer`].
    pub fn follow_up(&self, text: String) {
        self.shared.owner_typed(&text);
        lock(&self.shared.queue).follow_up.push(text);
        self.shared.dirty.store(true, Ordering::SeqCst);
        self.shared.gate.bg_cancel.cancel();
        self.shared.wait_wake.cancel();
    }
    /// Whether these handles control the same ordinary input queues. Reopening creates
    /// another instance; a fork within the same instance retains its existing queue semantics.
    pub fn same_instance(&self, other: &Handle) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared)
    }

    /// Reserve one worker wake for admitted editor input, even if the active consumer has
    /// already passed its final queue take. The wake carries this handle across UI rebinds.
    pub fn request_queued_wake(&self) -> bool {
        !self.shared.gate.queued_wake_pending.swap(true, Ordering::SeqCst)
    }

    /// The worker clears the reservation BEFORE checking/taking input, so an admission after
    /// its empty take can reserve another wake. Clear the originating handle, never a rebind.
    pub fn clear_queued_wake(&self) {
        self.shared
            .gate
            .queued_wake_pending
            .store(false, Ordering::SeqCst);
    }

    /// Claim the single pending frontend wake. Queue consumption during a long model
    /// run must not turn every admitted message into another unbounded channel job.
    pub fn request_inbox_wake(&self) -> bool {
        !self.shared.gate.inbox_wake_pending.swap(true, Ordering::SeqCst)
    }

    /// The frontend has taken its wake job, before running the queued input.
    pub fn clear_inbox_wake(&self) {
        self.shared.gate.inbox_wake_pending.store(false, Ordering::SeqCst);
    }

    /// Whether the active session has admitted inbox input (including after switching back).
    pub fn has_inbox_input(&self) -> bool {
        let q = lock(&self.shared.queue);
        q.session_follow_up
            .get(&q.session_id)
            .is_some_and(|messages| !messages.is_empty())
    }

    /// Queue an inbox follow-up bound to its destination, not to whichever session is
    /// active when the watcher runs. Inactive messages remain available on switching back.
    /// Returns false under backpressure; the watcher must retain the file and retry.
    pub fn follow_up_for(&self, session_id: &str, text: String) -> bool {
        let mut q = lock(&self.shared.queue);
        let messages = q.session_follow_up.values().flatten();
        let (count, bytes) = messages.fold((0, 0), |(count, bytes), text| (count + 1, bytes + text.len()));
        if text.len() > oxa_session::INBOX_MESSAGE_BYTES
            || count >= 64
            || text.len() > (1024 * 1024usize).saturating_sub(bytes)
        {
            return false;
        }
        q.session_follow_up
            .entry(session_id.to_string())
            .or_default()
            .push(text);
        if q.session_id == session_id {
            self.shared.dirty.store(true, Ordering::SeqCst);
            self.shared.gate.bg_cancel.cancel();
            self.shared.wait_wake.cancel();
        }
        true
    }

    /// Move all destination-bound inbox messages to a replacement session's handle.
    /// Frontends must stop the old watcher first. Ordinary editor queues stay behind;
    /// inactive inbox destinations must survive even when reopening creates a fresh Shared.
    pub fn transfer_inbox_to(&self, target: &Handle) {
        if Arc::ptr_eq(&self.shared, &target.shared) {
            return;
        }
        let pending = std::mem::take(&mut lock(&self.shared.queue).session_follow_up);
        // These were already admitted. Never discard them if the replacement has
        // input too; new admissions remain blocked until the combined backlog drains.
        {
            let mut q = lock(&target.shared.queue);
            for (id, messages) in pending {
                q.session_follow_up.entry(id).or_default().extend(messages);
            }
        }
        target.shared.dirty.store(true, Ordering::SeqCst);
        target.shared.gate.bg_cancel.cancel();
        self.shared.dirty.store(true, Ordering::SeqCst);
    }

    /// Remove and return active queued messages, steering first (Alt+Up, `clear_queue`).
    pub fn dequeue(&self) -> Vec<String> {
        let (mut steering, follow_up) = {
            let mut q = lock(&self.shared.queue);
            (std::mem::take(&mut q.steering), q.take_follow_up(QueueMode::All))
        };
        let changed = !steering.is_empty() || !follow_up.is_empty();
        steering.extend(follow_up);
        if changed {
            self.shared.dirty.store(true, Ordering::SeqCst);
        }
        steering
    }
    /// Remove the inbox messages bound to sessions other than the active one, grouped by
    /// destination. [`Handle::dequeue`] leaves them, so a headless owner that switched sessions
    /// can return each message to the inbox it came from instead of dropping it at exit.
    pub fn take_inactive_inbox(&self) -> Vec<(String, Vec<String>)> {
        let mut q = lock(&self.shared.queue);
        let active = q.session_id.clone();
        let inactive: Vec<String> = q
            .session_follow_up
            .keys()
            .filter(|id| **id != active)
            .cloned()
            .collect();
        inactive
            .into_iter()
            .filter_map(|id| q.session_follow_up.remove(&id).map(|messages| (id, messages)))
            .collect()
    }

    /// Set an invocation-wide soft budget before accepting prompts (not restored from a session).
    pub fn set_token_budget(&self, tokens: Option<u64>) {
        *lock(&self.shared.gate.token_budget) = tokens;
    }

    pub(crate) fn remaining_tokens(&self) -> Option<u64> {
        *lock(&self.shared.gate.token_budget)
    }

    /// Invocation-wide uncached prompt + completion usage, not capped at the budget.
    /// Includes compaction and tool prefill, which emit no assistant message events.
    pub fn run_tokens_used(&self) -> u64 {
        *lock(&self.shared.gate.run_tokens_used)
    }

    pub(crate) fn account_tokens(&self, tokens: u64) {
        {
            let mut used = lock(&self.shared.gate.run_tokens_used);
            *used = used.saturating_add(tokens);
        }
        let exhausted = {
            let mut budget = lock(&self.shared.gate.token_budget);
            if let Some(left) = budget.as_mut() {
                *left = left.saturating_sub(tokens);
                *left == 0
            } else {
                false
            }
        };
        if exhausted {
            self.request_wrap_up(WrapUpReason::TokenLimit);
        }
    }

    /// Finish the active tool batch, then allow one short final turn. Never cancels tools.
    pub fn request_wrap_up(&self, reason: WrapUpReason) {
        let _ =
            self.shared
                .gate
                .wrap_up
                .compare_exchange(0, reason as u8, Ordering::SeqCst, Ordering::SeqCst);
        self.cancel_background();
        self.shared.cancel_retry.cancel();
        // The flag is set: a front end that waits instead of polling (see
        // [`Handle::set_attention_waker`]) must re-check it now.
        self.notify_attention();
    }

    pub fn wrap_up_reason(&self) -> Option<WrapUpReason> {
        match self.shared.gate.wrap_up.load(Ordering::SeqCst) {
            1 => Some(WrapUpReason::TimeLimit),
            2 => Some(WrapUpReason::Signal),
            3 => Some(WrapUpReason::TokenLimit),
            _ => None,
        }
    }

    pub fn wrap_up_requested(&self) -> bool {
        self.wrap_up_reason().is_some()
    }

    /// End a headless invocation cooperatively. Unlike `abort`, this is sticky across
    /// claims, so SIGINT just before a prompt starts cannot be lost. Unlike wrap-up,
    /// it cancels generation and tools immediately and requests no final model turn.
    pub fn interrupt(&self) {
        self.shared.gate.interrupted.store(true, Ordering::SeqCst);
        self.abort();
        self.abort_retry();
        self.cancel_background();
        // Set before the wake, so a woken front end sees it (see `set_attention_waker`).
        self.notify_attention();
    }

    /// Register the wake this session gives a front end that waits instead of polling: a host
    /// request posted while the front end waits ([`CoreHost::post`]) and a wrap-up or
    /// interruption request ([`Handle::request_wrap_up`], [`Handle::interrupt`]). One waker
    /// per session; the closure must be cheap and must not block.
    pub fn set_attention_waker(&self, waker: Arc<dyn Fn() + Send + Sync>) {
        self.shared.gate.attention.set(waker);
    }

    /// Wake the registered front end, if any (see [`Handle::set_attention_waker`]).
    pub(crate) fn notify_attention(&self) {
        self.shared.gate.attention.notify();
    }

    pub fn interrupted(&self) -> bool {
        self.shared.gate.interrupted.load(Ordering::SeqCst)
    }

    /// Abort the current run. The partial reply is shown, never sent (W2). Queued messages stay
    /// queued; the front end restores them with `dequeue`. A no-op when no run is in progress
    /// (a stale cancel must not abort the next prompt; `Session` also resets the token). A
    /// foreground reservation counts as the current run: preserve an abort until its claim.
    pub fn abort(&self) {
        let _transition = lock(&self.shared.gate.foreground_transition);
        if self.shared.busy.load(Ordering::SeqCst) {
            self.shared.cancel.cancel();
            self.shared.gate.bg_cancel.cancel();
        } else if self.shared.gate.running.load(Ordering::SeqCst) {
            self.shared.gate.abort_next.store(true, Ordering::SeqCst);
            // The reservation may have become a claim after the first busy check. Cancel it
            // directly too; its guard clears the sticky bit if `begin` already passed it.
            if self.shared.busy.load(Ordering::SeqCst) {
                self.shared.cancel.cancel();
                self.shared.gate.bg_cancel.cancel();
            }
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
    /// A foreground operation still accepts input, even before its start event is consumed.
    pub fn is_running(&self) -> bool {
        self.shared.gate.running.load(Ordering::SeqCst)
    }
    pub fn is_busy(&self) -> bool {
        self.shared.busy.load(Ordering::SeqCst)
    }
    /// Whether the run that holds the session is winding down: an abort ([`Handle::abort`]) or
    /// the run's own loop guard set `cancel`, so the run takes no further tool batch and leaves
    /// the queue undrained. A front end must send input typed now as a new prompt, never as a
    /// steering message ([`Handle::steer`]) that nothing would deliver. `false` while the
    /// session is idle: the next claim resets the token.
    pub fn is_aborting(&self) -> bool {
        self.shared.busy.load(Ordering::SeqCst) && self.shared.cancel.is_cancelled()
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
        self.shared.update_modes(Some(steering), Some(follow_up));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn handle(steer: QueueMode, follow: QueueMode) -> Handle {
        Handle {
            shared: Arc::new(Shared::new(steer, follow)),
        }
    }

    #[test]
    fn soft_wrap_up_preserves_tools_and_keeps_background_work_cancelled() {
        let h = handle(QueueMode::All, QueueMode::All);
        let tool_cancel = h.shared.cancel.clone();
        let background_cancel = h.shared.gate.bg_cancel.clone();
        h.follow_up("retained instruction".to_string());
        h.request_wrap_up(WrapUpReason::TimeLimit);
        assert!(!tool_cancel.is_cancelled());
        assert!(background_cancel.is_cancelled());
        assert!(!h.shared.begin_background());
        assert!(
            background_cancel.is_cancelled(),
            "a new prefill must not revoke the wrap-up cancellation"
        );
        assert_eq!(h.queued().1, vec!["retained instruction"]);
        h.request_wrap_up(WrapUpReason::Signal);
        assert_eq!(h.wrap_up_reason(), Some(WrapUpReason::TimeLimit));
    }

    /// O-386: a front end that waits instead of polling is woken when the state it cannot see
    /// through its own channels changes: a wrap-up request and an interruption.
    #[test]
    fn the_attention_waker_fires_on_wrap_up_and_interrupt() {
        let h = handle(QueueMode::All, QueueMode::All);
        let fired = Arc::new(AtomicUsize::new(0));
        let count = fired.clone();
        h.set_attention_waker(Arc::new(move || {
            count.fetch_add(1, Ordering::SeqCst);
        }));
        h.request_wrap_up(WrapUpReason::TimeLimit);
        assert_eq!(fired.load(Ordering::SeqCst), 1, "the wrap-up wakes the front end");
        h.interrupt();
        assert_eq!(
            fired.load(Ordering::SeqCst),
            2,
            "the interruption wakes the front end"
        );
        // The state is set before the wake, so a woken front end that re-checks sees it.
        assert!(h.interrupted());
        assert_eq!(h.wrap_up_reason(), Some(WrapUpReason::TimeLimit));
    }

    /// A waker is front-end code: a panic in it must not unwind into the session that set the
    /// state (the state is already in place and the front end re-checks it).
    #[test]
    fn a_panicking_attention_waker_is_contained() {
        let h = handle(QueueMode::All, QueueMode::All);
        h.set_attention_waker(Arc::new(|| panic!("injected waker panic")));
        h.request_wrap_up(WrapUpReason::Signal);
        assert_eq!(h.wrap_up_reason(), Some(WrapUpReason::Signal));
        h.interrupt();
        assert!(h.interrupted());
    }

    #[test]
    fn inbox_wakes_coalesce_even_when_model_drains_input_between_admissions() {
        let h = handle(QueueMode::All, QueueMode::All);
        h.shared.bind_session("active");
        assert!(h.request_inbox_wake());
        for _ in 0..1000 {
            assert!(h.follow_up_for("active", "message".into()));
            assert_eq!(h.dequeue(), ["message"]);
            assert!(
                !h.request_inbox_wake(),
                "one pending wake, not one per model turn"
            );
        }
        h.clear_inbox_wake();
        assert!(h.request_inbox_wake());
        assert!(!h.request_inbox_wake());
    }

    #[test]
    fn inbox_admission_bounds_count_and_bytes_across_destinations() {
        let h = handle(QueueMode::All, QueueMode::All);
        h.shared.bind_session("active");
        for _ in 0..65 {
            h.follow_up_for("inactive", "message".into());
        }
        h.shared.bind_session("inactive");
        assert_eq!(h.dequeue().len(), 64, "excess admissions must remain on disk");
        for _ in 0..5 {
            h.follow_up_for("active", "x".repeat(256 * 1024));
        }
        h.shared.bind_session("active");
        assert_eq!(h.dequeue().len(), 4, "admitted text is limited to one MiB");
        h.follow_up_for("active", "x".repeat(256 * 1024 + 1));
        assert!(h.dequeue().is_empty(), "individual messages are bounded too");
        h.follow_up_for("active", "capacity recovered".into());
        assert_eq!(h.dequeue(), ["capacity recovered"]);
    }

    #[test]
    fn inbox_transfer_preserves_destinations_and_leaves_editor_queues_behind() {
        let old = handle(QueueMode::All, QueueMode::All);
        old.shared.bind_session("clone");
        old.steer("editor steering".into());
        old.follow_up("editor follow-up".into());
        old.follow_up_for("original", "first".into());
        old.follow_up_for("original", "second".into());
        old.follow_up_for("clone", "clone only".into());
        let reopened = handle(QueueMode::All, QueueMode::All);
        reopened.shared.bind_session("unrelated");
        old.transfer_inbox_to(&reopened);
        assert_eq!(old.dequeue(), ["editor steering", "editor follow-up"]);
        assert!(reopened.queued().1.is_empty());
        reopened.shared.bind_session("original");
        assert_eq!(reopened.dequeue(), ["first", "second"]);
        reopened.shared.bind_session("clone");
        // Rebinding a handle to itself must neither deadlock nor discard messages.
        reopened.transfer_inbox_to(&reopened);
        assert_eq!(reopened.dequeue(), ["clone only"]);
        old.shared.bind_session("original");
        assert!(old.dequeue().is_empty());
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

    /// `is_aborting` is the run the front end must not steer: a claimed session whose run was
    /// cancelled. Idle, or busy with a clear token, is a run that still takes input.
    #[test]
    fn is_aborting_needs_a_claimed_cancelled_run() {
        let h = handle(QueueMode::All, QueueMode::All);
        assert!(!h.is_aborting(), "idle");
        h.shared.busy.store(true, Ordering::SeqCst);
        assert!(!h.is_aborting(), "a run that was not cancelled takes input");
        h.abort();
        assert!(h.is_aborting());
        // The run ends and the next claim resets the token (`Session::begin`).
        h.shared.busy.store(false, Ordering::SeqCst);
        assert!(!h.is_aborting(), "a stale abort is not the next run");
        h.shared.busy.store(true, Ordering::SeqCst);
        h.shared.cancel.reset();
        assert!(!h.is_aborting());
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
    fn inbox_follow_ups_remain_bound_across_session_changes() {
        let h = handle(QueueMode::All, QueueMode::OneAtATime);
        h.shared.bind_session("old");
        h.follow_up_for("old", "already admitted".into());
        assert!(h.shared.has_input());
        assert_eq!(h.queued().1, ["already admitted"]);
        h.shared.bind_session("new");
        h.shared.gate.bg_cancel.reset();
        h.follow_up_for("old", "racing callback".into());
        assert!(!h.shared.has_input());
        assert!(!h.shared.gate.bg_cancel.is_cancelled());
        assert!(h.shared.take_follow_up().is_empty());
        assert!(
            h.dequeue().is_empty(),
            "clearing the new queue must leave the old inbox alone"
        );
        h.follow_up("ordinary follow-up".into());
        h.follow_up_for("new", "new inbox".into());
        assert_eq!(h.shared.take_follow_up(), ["ordinary follow-up"]);
        assert_eq!(h.shared.take_follow_up(), ["new inbox"]);
        h.shared.bind_session("old");
        assert_eq!(h.shared.take_follow_up(), ["already admitted"]);
        assert_eq!(h.dequeue(), ["racing callback"]);
        assert!(!h.shared.has_input());
    }

    #[test]
    fn inactive_inbox_messages_are_taken_for_their_own_session() {
        let h = handle(QueueMode::All, QueueMode::OneAtATime);
        h.shared.bind_session("old");
        h.follow_up_for("old", "admitted before switch".into());
        h.shared.bind_session("new");
        h.follow_up_for("new", "new inbox".into());
        assert_eq!(
            h.take_inactive_inbox(),
            vec![("old".to_string(), vec!["admitted before switch".to_string()])]
        );
        assert!(h.take_inactive_inbox().is_empty());
        assert_eq!(h.dequeue(), ["new inbox"]);
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
