//! Session-local goal controls; no worker, timer thread or external scheduler.
use oxa_proto::{Access, Exposure, Mode, Source, Tool, ToolClass, ToolCtx, ToolOutput, ToolSpec, Value};
use oxa_session::{Goal, GoalBudget, GoalStatus, SessionRef};
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Wake, Waker};
use std::time::{Duration, Instant};

/// Lifecycle requests from an already-authorized session frontend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GoalControl {
    Pause,
    Resume,
    Cancel,
}

/// Goal status and revision, bound to the owner's full session reference.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GoalObservation<'a> {
    /// Frontend-private identity. Exclude it from shared summaries.
    pub session: &'a SessionRef,
    pub revision: u64,
    pub status: Option<GoalStatus>,
}

pub use oxa_session::SteeringLease;

/// Identity captured for a steering request at submission.
#[derive(Debug)]
pub struct SteeringBinding {
    pub session: SessionRef,
    pub revision: u64,
    pub lease: Option<SteeringLease>,
}

impl SteeringBinding {
    /// Revalidate just before appending at the native steering boundary. The
    /// frontend supplies current lease evidence from its existing authority.
    pub fn validate(
        &self,
        owner: &crate::SessionState,
        current_lease: Option<&SteeringLease>,
    ) -> Result<(), oxa_proto::OxaError> {
        if self.session != owner.sref
            || self.lease.as_ref() != current_lease
            || !Goal::allows_steering(owner.tree.entries(), self.revision)
        {
            return Err(stale_steering());
        }
        Ok(())
    }
}

const STEER_STALE: &str = "goal_steer_stale";

fn stale_steering() -> oxa_proto::OxaError {
    oxa_proto::OxaError::new(
        STEER_STALE,
        "The goal or its assignment changed. Read the current binding before resending.",
    )
}

/// Invoke the supplied operation while the frontend's existing assignment
/// guard holds, supplying the current lease identity. If verification is
/// unavailable or fails, never invoke the operation.
pub type SteeringAuthority = dyn Fn(&SessionRef, &mut dyn FnMut(Option<&SteeringLease>)) + Send + Sync;

mod lead;
pub use lead::checkpoint_idle_lead_goal;
pub(crate) use lead::{deliver_saved, reanchor_resumed, BoundLeadAuthority};

/// One typed steering input. Its callback reports the result of appending.
pub struct GoalSteering {
    pub(crate) binding: SteeringBinding,
    text: String,
    authority: Arc<SteeringAuthority>,
    receipt: Box<dyn FnOnce(Result<(), &'static str>) + Send>,
}

impl std::fmt::Debug for GoalSteering {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("GoalSteering").finish_non_exhaustive()
    }
}

impl GoalSteering {
    pub fn new(
        binding: SteeringBinding,
        text: String,
        authority: Arc<SteeringAuthority>,
        receipt: impl FnOnce(Result<(), &'static str>) + Send + 'static,
    ) -> Self {
        Self {
            binding,
            text,
            authority,
            receipt: Box::new(receipt),
        }
    }

    pub(crate) fn reject(self) {
        (self.receipt)(Err(STEER_STALE));
    }

    pub(crate) fn deliver<T>(
        self,
        owner: &mut crate::SessionState,
        append: impl FnOnce(&mut crate::SessionState, String) -> Result<T, oxa_proto::OxaError>,
    ) -> Result<Option<T>, oxa_proto::OxaError> {
        let Self {
            binding,
            text,
            authority,
            receipt,
        } = self;
        let mut delivery = Some((append, text));
        let mut result = Err(stale_steering());
        authority(&binding.session, &mut |current| {
            if let Some((append, text)) = delivery.take() {
                result = binding
                    .validate(owner, current)
                    .and_then(|()| append(owner, text));
            }
        });
        receipt(result.as_ref().map(|_| ()).map_err(|error| error.code));
        match result {
            Ok(entry) => Ok(Some(entry)),
            Err(error) if error.code == STEER_STALE => Ok(None),
            Err(error) => Err(error),
        }
    }
}

impl crate::Session {
    /// The saved goal revision, scoped to the owner's current session reference.
    pub fn goal_revision(&self) -> u64 {
        Goal::revision(self.inner().tree.entries())
    }

    /// Read status and revision together without copying objective or evidence text.
    /// Use the full reference when comparing observations or requesting a control.
    pub fn goal_observation(&self) -> GoalObservation<'_> {
        GoalObservation {
            session: &self.inner().sref,
            revision: self.goal_revision(),
            status: self.inner().goal.as_ref().map(|goal| goal.status()),
        }
    }

    /// Apply a lifecycle request on the idle session owner. The frontend
    /// authenticates the sender and keeps the full session reference it bound
    /// at admission private. Two files can share an ID and revision.
    ///
    /// Success means the store sync succeeded. If the goal revision advanced
    /// before a later step failed, the error distinguishes an unconfirmed sync
    /// from a saved transition that still needs a prefix refresh.
    pub fn control_goal(
        &mut self,
        bound_session: &SessionRef,
        revision: u64,
        control: GoalControl,
        sink: &mut dyn oxa_proto::EventSink,
    ) -> Result<u64, oxa_proto::OxaError> {
        use oxa_proto::{codes, OxaError};
        if &self.inner().sref != bound_session || self.goal_revision() != revision {
            return Err(OxaError::new(
                "goal_revision_conflict",
                "The session or goal revision changed.",
            ));
        }
        let mut goal = self
            .goal()
            .ok_or_else(|| OxaError::new(codes::NOT_FOUND, "The session has no goal."))?;
        match control {
            GoalControl::Pause if goal.status == GoalStatus::Pursuing => goal.pause(),
            GoalControl::Resume if matches!(goal.status, GoalStatus::Paused | GoalStatus::Blocked) => {
                goal.resume()?;
            }
            GoalControl::Cancel => goal.cancel()?,
            _ => {
                return Err(OxaError::new(
                    codes::INVALID,
                    "The goal cannot make that transition.",
                ))
            }
        }
        let result = self.replace_goal(Some(goal), sink);
        let applied_revision = self.goal_revision();
        if applied_revision == revision {
            return result.map(|()| applied_revision);
        }
        self.sync_goal_transition()?;
        publish_status(self.inner());
        crate::progress::notify(
            self.inner(),
            crate::Progress::GoalControlCommitted {
                revision: applied_revision,
            },
        );
        result.map_err(|error| {
            OxaError::new("goal_applied_prefix_failed", error.message)
                .with_hint("The goal transition was saved. Refresh the session prefix before running again.")
        })?;
        Ok(applied_revision)
    }

    /// Make the goal revision advanced by the last transition durable. Every goal
    /// transition goes through here before it is acknowledged, so a failed sync is
    /// never reported as success.
    fn sync_goal_transition(&self) -> Result<(), oxa_proto::OxaError> {
        let state = self.inner();
        state.store.sync(&state.sref).map_err(|error| {
            oxa_proto::OxaError::new("goal_sync_failed", error.message).with_hint(
                "The goal revision advanced. Confirm durability before acknowledging the transition.",
            )
        })
    }

    /// Apply a `/goal` set or clear and wait for the store sync before it is reported.
    fn commit_goal(
        &mut self,
        goal: Option<Goal>,
        sink: &mut dyn oxa_proto::EventSink,
    ) -> Result<(), oxa_proto::OxaError> {
        let before = self.goal_revision();
        let result = self.replace_goal(goal, sink);
        if result.is_err() && self.goal_revision() == before {
            return result;
        }
        self.sync_goal_transition()?;
        if result.is_ok() {
            publish_status(self.inner());
        }
        result
    }
}

pub struct GoalRuntime(Mutex<Inner>);
struct Inner {
    goal: Goal,
    clock: Option<Instant>,
    engaged: bool,
    last_tokens: u64,
}
impl GoalRuntime {
    pub fn new(goal: Goal) -> Self {
        Self(Mutex::new(Inner {
            goal,
            clock: None,
            engaged: false,
            last_tokens: 0,
        }))
    }
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
    pub fn snapshot(&self) -> Goal {
        self.lock().goal.clone()
    }
    fn replace_snapshot(&self, goal: Goal) {
        self.lock().goal = goal;
    }

    pub(crate) fn has_pending_steer(&self) -> bool {
        self.lock()
            .goal
            .lead
            .as_ref()
            .is_some_and(|lead| lead.pending_steer.is_some())
    }

    pub(crate) fn reserves_entry_id(&self, id: &str) -> bool {
        self.lock()
            .goal
            .lead
            .as_ref()
            .and_then(|lead| lead.pending_steer.as_ref())
            .is_some_and(|steer| steer.entry_id == id)
    }

    pub fn pursuing(&self) -> bool {
        self.status() == GoalStatus::Pursuing
    }
    fn status(&self) -> GoalStatus {
        self.lock().goal.status
    }
    pub(crate) fn begin(&self, tokens: u64) {
        let mut state = self.lock();
        state.last_tokens = tokens;
        state.engaged = state.goal.status == GoalStatus::Pursuing;
        state.clock = state.engaged.then(Instant::now);
    }
    pub(crate) fn engaged(&self) -> bool {
        self.lock().engaged
    }
    /// Read-only admission for the delayed prefill helper. The session thread
    /// remains the sole owner of charging/checkpointing known usage and time.
    pub(crate) fn allows_request(&self, tokens: u64) -> bool {
        let state = self.lock();
        state.engaged
            && state.goal.status == GoalStatus::Pursuing
            && !state.goal.exhausted()
            && state.goal.budget.max_tokens.is_none_or(|cap| {
                tokens
                    .checked_sub(state.last_tokens)
                    .and_then(|pending| state.goal.tokens_used.checked_add(pending))
                    .is_some_and(|used| used < cap)
            })
            && state.goal.budget.max_time_ms.is_none_or(|cap| {
                u128::from(state.goal.elapsed_ms) + state.clock.map_or(0, |clock| clock.elapsed().as_millis())
                    < u128::from(cap)
            })
    }
    /// Whether a wait of `delay` ends inside the owner's time budget (no time budget: always).
    pub(crate) fn time_allows_wait(&self, delay: Duration) -> bool {
        let state = self.lock();
        state.goal.budget.max_time_ms.is_none_or(|cap| {
            u128::from(state.goal.elapsed_ms)
                + state.clock.map_or(0, |clock| clock.elapsed().as_millis())
                + delay.as_millis()
                < u128::from(cap)
        })
    }
    pub(crate) fn charge(&self, tokens: u64, turn: bool) -> Result<Goal, oxa_proto::OxaError> {
        let mut state = self.lock();
        let now = Instant::now();
        let elapsed = state.clock.map_or(0, |t| now.duration_since(t).as_millis());
        let delta = tokens.checked_sub(state.last_tokens).ok_or_else(|| {
            oxa_proto::OxaError::new(oxa_proto::codes::INVARIANT, "Goal token counter moved backwards.")
        })?;
        state
            .goal
            .charge(Some(delta), u64::try_from(elapsed).unwrap_or(u64::MAX), turn)?;
        state.clock = Some(now);
        state.last_tokens = tokens;
        Ok(state.goal.clone())
    }
    /// The assistant stopped without a tool call. `pause_idle` is the owner's
    /// `oxa.goal.idlePause`; the bool is true when this reply made the second idle one and
    /// pursuit goes on (a notice, not a stop).
    pub(crate) fn stopped(&self, pause_idle: bool) -> (Goal, bool) {
        let mut state = self.lock();
        let before = state.goal.idle_stops;
        // A lead goal is stepped by the lead loop, which relies on the idle pause.
        let pause_idle = pause_idle || state.goal.lead.is_some();
        state.goal.stopped_with(false, pause_idle);
        let noted = !pause_idle
            && before < 2
            && state.goal.idle_stops == 2
            && state.goal.status == GoalStatus::Pursuing;
        (state.goal.clone(), noted)
    }
    /// Pause a pursuing goal whose controls the tool selection removed; returns the saved goal.
    pub(crate) fn pause_because(&self, reason: &str) -> Goal {
        let mut state = self.lock();
        state.goal.pause_because(reason);
        state.goal.clone()
    }
    pub(crate) fn note_tools(&self) {
        self.lock().goal.idle_stops = 0;
    }
    pub(crate) fn reject_completion(&self) {
        let mut state = self.lock();
        if state.goal.status == GoalStatus::Done {
            state.goal.status = GoalStatus::Blocked;
            state.goal.evidence = Some("Project done checks failed; completion refused.".into());
        }
    }
    pub(crate) fn defer_completion(&self) -> Option<String> {
        let mut state = self.lock();
        if state.goal.status != GoalStatus::Done {
            return None;
        }
        state.goal.status = GoalStatus::Pursuing;
        state.goal.evidence.take()
    }
    pub(crate) fn finish_completion(&self, evidence: String) -> Result<(), oxa_proto::OxaError> {
        self.lock().goal.finish(GoalStatus::Done, evidence)
    }
    /// Record a request whose spend is unknown. It never ends the goal: the count is kept and
    /// shown. Returns true for the first one under an owner token cap, so the caller warns once.
    pub(crate) fn uncertain_request(&self) -> Result<bool, oxa_proto::OxaError> {
        if !self.engaged() {
            return Ok(false);
        }
        let mut state = self.lock();
        state.goal.charge(None, 0, false)?;
        Ok(state.goal.unknown_requests == 1 && state.goal.budget.max_tokens.is_some())
    }
    pub(crate) fn enforce_budget(&self) {
        let mut state = self.lock();
        if state.goal.status == GoalStatus::Pursuing && state.goal.exhausted() {
            state.goal.stopped_with(true, true);
        }
    }
    pub(crate) fn tools(
        self: &Arc<Self>,
        shared: &Arc<crate::queue::Shared>,
        hooks: &[Arc<dyn oxa_proto::Hooks>],
        wait: WaitPolicy,
        jobs: &Arc<oxa_tools::JobRegistry>,
    ) -> Vec<Arc<dyn Tool>> {
        let interrupt = shared.cancel_retry.clone();
        [
            (
                "goal_done",
                crate::prompt::GOAL_DONE_DESCRIPTION,
                crate::prompt::GOAL_DONE_SCHEMA,
            ),
            (
                "wait",
                crate::prompt::GOAL_WAIT_DESCRIPTION,
                crate::prompt::GOAL_WAIT_SCHEMA,
            ),
        ]
        .into_iter()
        .map(|(name, description, schema)| {
            Arc::new(Control {
                goal: Some(self.clone()),
                interrupt: interrupt.clone(),
                shared: shared.clone(),
                hooks: hooks.to_vec(),
                jobs: jobs.clone(),
                wait,
                spec: ToolSpec {
                    name: name.into(),
                    description: description.into(),
                    schema_json: schema.into(),
                    class: ToolClass::Write,
                    exposure: Exposure::Direct,
                    source: Source::Builtin,
                },
            }) as Arc<dyn Tool>
        })
        .collect()
    }
}
/// The `wait` tool of a session with no goal pursued: the same spec as the goal's `wait`
/// (so the cached prefix does not move when a goal starts), bounded by `oxa.wait.maxSeconds`,
/// cancellation, input from the owner and the run's own limits instead of a goal budget.
pub(crate) fn wait_tool(
    shared: &Arc<crate::queue::Shared>,
    hooks: &[Arc<dyn oxa_proto::Hooks>],
    wait: WaitPolicy,
    jobs: &Arc<oxa_tools::JobRegistry>,
) -> Arc<dyn Tool> {
    Arc::new(Control {
        goal: None,
        interrupt: shared.cancel_retry.clone(),
        shared: shared.clone(),
        hooks: hooks.to_vec(),
        jobs: jobs.clone(),
        wait,
        spec: ToolSpec {
            name: "wait".into(),
            description: crate::prompt::GOAL_WAIT_DESCRIPTION.into(),
            schema_json: crate::prompt::GOAL_WAIT_SCHEMA.into(),
            class: ToolClass::Write,
            exposure: Exposure::Direct,
            source: Source::Builtin,
        },
    })
}
/// The `oxa.wait.*` settings, read when the goal tools are built (each epoch). Missing or
/// mistyped values fall back to the defaults.
#[derive(Clone, Copy, Debug)]
pub(crate) struct WaitPolicy {
    /// How often a wait with `until` asks the hook sets for the condition's state.
    poll: Duration,
    /// Cap on one wait's `seconds`.
    max_secs: u64,
}
impl WaitPolicy {
    pub(crate) fn from_settings(settings: &oxa_config::Settings) -> WaitPolicy {
        WaitPolicy {
            poll: Duration::from_secs(u64::from(settings.u32("oxa.wait.pollSeconds", 30)).max(1)),
            max_secs: u64::from(settings.u32("oxa.wait.maxSeconds", 86400)).clamp(1, 86400),
        }
    }
}
struct Control {
    /// `None` for the `wait` of a session with no goal pursued.
    goal: Option<Arc<GoalRuntime>>,
    interrupt: oxa_proto::CancelToken,
    /// Its `wait_wake` ends a wait early when input for the active session arrives.
    shared: Arc<crate::queue::Shared>,
    /// Hook sets consulted for a wait's `until` condition (`Hooks::poll_event`).
    hooks: Vec<Arc<dyn oxa_proto::Hooks>>,
    /// The session's background jobs, for `until` `job_done`.
    jobs: Arc<oxa_tools::JobRegistry>,
    wait: WaitPolicy,
    spec: ToolSpec,
}
impl Tool for Control {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    /// `wait` is in the default selection of every session; `-wait` in `defaultTools` removes it.
    fn default_selected(&self) -> bool {
        self.spec.name == "wait"
    }
    fn access(&self, args: &Value, cx: &ToolCtx) -> Result<Vec<Access>, String> {
        validate(&self.spec.name, args)?;
        // A wait on a GitHub probe is network access (#1919): plan denies it and ask asks once
        // per host; workspace, auto and yolo are unchanged (the same convention as web search).
        let mut out: Vec<Access> = Vec::new();
        if self.spec.name == "wait" && matches!(cx.mode, Mode::Ask | Mode::Plan) {
            if let Some(until) = args.get("until") {
                for hook in &self.hooks {
                    for access in hook.poll_access(until) {
                        if !out.contains(&access) {
                            out.push(access);
                        }
                    }
                }
            }
        }
        Ok(out)
    }
    fn run(&self, args: &Value, cx: &ToolCtx, live: &mut dyn FnMut(&str)) -> ToolOutput {
        if let Err(e) = validate(&self.spec.name, args) {
            return ToolOutput::error(e);
        }
        if cx.cancel.is_cancelled() {
            return ToolOutput::error("Goal control cancelled.");
        }
        if self.spec.name == "goal_done" {
            let status = if args["status"] == "complete" {
                GoalStatus::Done
            } else {
                GoalStatus::Blocked
            };
            let Some(goal) = self.goal.as_ref() else {
                return ToolOutput::error("No goal is being pursued.");
            };
            return match goal
                .lock()
                .goal
                .finish(status, args["evidence"].as_str().unwrap().into())
            {
                Ok(()) => ToolOutput::ok("Goal terminal status requested."),
                Err(e) => ToolOutput::error(e.to_string()),
            };
        }
        let duration = match self.goal.as_ref() {
            Some(goal) => {
                let state = goal.lock();
                if state.goal.status != GoalStatus::Pursuing || state.goal.exhausted() {
                    return ToolOutput::error("Goal is not pursuing or its budget is exhausted.");
                }
                let asked = Duration::from_secs(args["seconds"].as_u64().unwrap().min(self.wait.max_secs));
                let spent = state.clock.map_or(0, |t| t.elapsed().as_millis());
                let remaining = state.goal.budget.max_time_ms.map(|cap| {
                    Duration::from_millis(
                        cap.saturating_sub(state.goal.elapsed_ms)
                            .saturating_sub(u64::try_from(spent).unwrap_or(u64::MAX)),
                    )
                });
                remaining.map_or(asked, |remaining| asked.min(remaining))
            }
            // No goal: the run's own `--max-time` cancels `cx.cancel`; typed input wakes the wait.
            None => Duration::from_secs(args["seconds"].as_u64().unwrap().min(self.wait.max_secs)),
        };
        self.shared.arm_wait_wake();
        live("Waiting; cancellation remains available.");
        if let Some(until) = args.get("until") {
            return self.wait_until(until, duration, cx);
        }
        wait_cancelled(cx.cancel, &self.interrupt, &self.shared.wait_wake, duration);
        if cx.cancel.is_cancelled() {
            ToolOutput::error("Wait cancelled.")
        } else if self.shared.wait_wake.is_cancelled() {
            ToolOutput::ok("Wait ended: input arrived.")
        } else {
            ToolOutput::ok("Wait ended.")
        }
    }
}
impl Control {
    /// A wait with an `until` condition: poll the hook sets, sleeping `oxa.wait.pollSeconds`
    /// between polls, until one answers. No model request is made meanwhile.
    fn wait_until(&self, until: &Value, duration: Duration, cx: &ToolCtx) -> ToolOutput {
        let deadline = Instant::now() + duration;
        loop {
            // The first hook set that answers `Some` decides this poll. A probe is told what
            // the wait has left so its own work cannot outlive the wait.
            let remaining = deadline.saturating_duration_since(Instant::now());
            let kind = until["kind"].as_str().unwrap_or("");
            if kind == "job_done" {
                match self.job_done(until) {
                    Err(e) => return ToolOutput::error(e),
                    Ok(Some(evidence)) => return ToolOutput::ok(evidence),
                    Ok(None) => {}
                }
            } else {
                match self
                    .hooks
                    .iter()
                    .find_map(|h| h.poll_event(kind, until, remaining))
                {
                    None => return ToolOutput::error(format!("Unknown wait condition: {kind}.")),
                    Some(Err(e)) => return ToolOutput::error(e),
                    Some(Ok(Some(evidence))) => return ToolOutput::ok(evidence),
                    Some(Ok(None)) => {}
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                // The probe contract carries no text for a pending state, so the last state is none.
                return ToolOutput::ok("Wait ended: deadline; last state: none");
            }
            wait_cancelled(
                cx.cancel,
                &self.interrupt,
                &self.shared.wait_wake,
                remaining.min(if until["kind"] == "job_done" {
                    Duration::from_millis(250).min(self.wait.poll)
                } else {
                    self.wait.poll
                }),
            );
            if cx.cancel.is_cancelled() {
                return ToolOutput::error("Wait cancelled.");
            }
            if self.shared.wait_wake.is_cancelled() {
                // A job's own finish wakes the session too: report the job, not the wake.
                if until["kind"] == "job_done" {
                    if let Ok(Some(evidence)) = self.job_done(until) {
                        return ToolOutput::ok(evidence);
                    }
                }
                return ToolOutput::ok("Wait ended: input arrived.");
            }
            if self.interrupt.is_cancelled() {
                return ToolOutput::ok("Wait ended.");
            }
        }
    }
    /// `until {kind:"job_done", job:<id>}`: the background bash job has exited or was killed.
    /// Reading the listing does not consume the job's output.
    fn job_done(&self, until: &Value) -> Result<Option<String>, String> {
        let id = until["job"]
            .as_str()
            .ok_or("wait until job_done needs `job` (the job id).")?;
        let info = self
            .jobs
            .list()
            .into_iter()
            .find(|j| j.id == id)
            .ok_or_else(|| format!("Unknown job: {id}"))?;
        Ok(match info.status {
            oxa_tools::JobStatus::Running => None,
            oxa_tools::JobStatus::Exited(code) => Some(format!("Job {id} finished: exit {code}.")),
            _ => Some(format!("Job {id} finished: killed.")),
        })
    }
}
fn validate(name: &str, args: &Value) -> Result<(), String> {
    let object = args.as_object().ok_or("Expected a goal-control object.")?;
    let valid = if name == "goal_done" {
        object.len() == 2
            && matches!(args["status"].as_str(), Some("complete" | "blocked"))
            && args["evidence"]
                .as_str()
                .is_some_and(|s| !s.trim().is_empty() && s.len() <= 8192)
    } else {
        // `until` is optional; its kind is answered by the hook sets, so only its shape is checked.
        let until_ok = match object.get("until") {
            None => true,
            Some(until) => {
                until.is_object()
                    && until["kind"]
                        .as_str()
                        .is_some_and(|k| !k.trim().is_empty() && k.len() <= 64)
            }
        };
        (object.len() == 2 || (object.len() == 3 && object.contains_key("until")))
            && until_ok
            && args["seconds"].as_u64().is_some_and(|n| (1..=86400).contains(&n))
            && args["reason"]
                .as_str()
                .is_some_and(|s| !s.trim().is_empty() && s.len() <= 1024)
    };
    if valid {
        Ok(())
    } else {
        Err("Invalid goal-control arguments.".into())
    }
}
struct Unpark(std::thread::Thread);
impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}
fn wait_cancelled(
    cancel: &oxa_proto::CancelToken,
    interrupt: &oxa_proto::CancelToken,
    wake: &oxa_proto::CancelToken,
    duration: Duration,
) {
    let deadline = Instant::now() + duration;
    let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cancelled = std::pin::pin!(cancel.cancelled());
    let mut interrupted = std::pin::pin!(interrupt.cancelled());
    let mut woken = std::pin::pin!(wake.cancelled());
    let mut context = Context::from_waker(&waker);
    loop {
        if cancelled.as_mut().poll(&mut context).is_ready()
            || interrupted.as_mut().poll(&mut context).is_ready()
            || woken.as_mut().poll(&mut context).is_ready()
        {
            return;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return;
        }
        std::thread::park_timeout(remaining);
    }
}

/// Engage before any request, including turn-start compaction.
pub(crate) fn begin(st: &crate::SessionState) {
    if let Some(goal) = &st.goal {
        goal.begin(
            *st.shared
                .gate
                .run_tokens_used
                .lock()
                .unwrap_or_else(|p| p.into_inner()),
        );
    }
}
pub(crate) fn ensure_begin(st: &crate::SessionState) {
    if st.goal.as_ref().is_some_and(|goal| !goal.engaged()) {
        begin(st);
    }
}
/// Account before choosing a terminal path; publish only after project checks.
pub(crate) fn account(st: &mut crate::SessionState, turn: bool) -> Result<(), oxa_proto::OxaError> {
    if let Some(goal) = st.goal.clone().filter(|g| g.engaged()) {
        let tokens = *st
            .shared
            .gate
            .run_tokens_used
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        goal.charge(tokens, turn)?;
        goal.enforce_budget();
    }
    Ok(())
}
pub(crate) fn checkpoint(
    st: &mut crate::SessionState,
    turn: bool,
    sink: &mut dyn oxa_proto::EventSink,
) -> Result<(), oxa_proto::OxaError> {
    account(st, turn)?;
    if let Some(goal) = st.goal.clone().filter(|g| g.engaged()) {
        save_checkpoint(st, &goal.snapshot(), sink)?;
    }
    Ok(())
}
/// The pause reason when a pursuing goal has no `goal_done` in the tool selection.
pub(crate) const CONTROLS_MISSING: &str =
    "goal_done is not in this session's tool selection; add goal_done to --tools or defaultTools";
pub(crate) fn save_checkpoint(
    st: &mut crate::SessionState,
    goal: &Goal,
    sink: &mut dyn oxa_proto::EventSink,
) -> Result<(), oxa_proto::OxaError> {
    let body = goal.checkpoint_body(st.tree.entries());
    crate::agent_loop::persist(st, body, sink)?;
    publish_status(st);
    Ok(())
}
/// The visible half of an idle pause. The checkpoint publishes the saved status.
pub(crate) fn announce_pause(reason: &str, sink: &mut dyn oxa_proto::EventSink) {
    sink.emit(&oxa_proto::AgentEvent::Notice {
        level: oxa_proto::Level::Warning,
        text: format!("Goal paused: {reason}; /goal resume"),
    });
}

/// The warning for a request with unknown token usage under an owner token budget.
pub(crate) fn announce_unknown_usage(sink: &mut dyn oxa_proto::EventSink) {
    sink.emit(&oxa_proto::AgentEvent::Notice {
        level: oxa_proto::Level::Warning,
        text: "Goal: a request reported no token usage; its spend is unknown and recorded as uncertain. The token budget keeps counting known usage and does not stop the goal for it".into(),
    });
}

/// The notice for a second reply without an action when the owner did not ask for a pause.
pub(crate) fn announce_idle(sink: &mut dyn oxa_proto::EventSink) {
    sink.emit(&oxa_proto::AgentEvent::Notice {
        level: oxa_proto::Level::Warning,
        text: "Goal: two replies without an action; still pursuing (oxa.goal.idlePause pauses here, /goal pause pauses now)".into(),
    });
}

/// Publish the goal currently mounted in the session, including a saved pause reason.
pub(crate) fn publish_status(st: &crate::SessionState) {
    let snapshot = st.goal.as_ref().map(|goal| goal.snapshot());
    crate::progress::notify(
        st,
        crate::Progress::GoalStatus {
            status: snapshot.as_ref().map_or("none", |goal| goal.status.as_str()),
            reason: snapshot.as_ref().and_then(|goal| goal.pause_reason.as_deref()),
        },
    );
}
pub(crate) fn terminal(st: &crate::SessionState) -> bool {
    st.goal.as_ref().is_some_and(|g| g.engaged() && !g.pursuing())
}

/// `oxa.goal.failureBackoffSeconds` default: the first wait after a transient request failure.
const FAILURE_BACKOFF_SECS: u32 = 60;
/// The longest wait between goal retries unless the owner set a longer first wait.
const FAILURE_BACKOFF_CAP_SECS: u64 = 900;

/// How a wait between goal retries ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BackoffEnd {
    Elapsed,
    /// The run was aborted: the goal stops.
    Aborted,
    /// `abort_retry`: the owner stops the retries, so the goal stops with the failure.
    RetryStopped,
}

/// Whether a failed request is transient, so a pursuing goal may wait and send it again: a stream
/// cut, engine down or busy, a timeout, a rate limit or a 5xx. Auth errors, refusals, the
/// repetition stop, aborts, budgets and every other class stop the run.
pub(crate) fn transient_failure(error: &oxa_proto::OxaError) -> bool {
    use oxa_proto::codes;
    if error.message.starts_with(oxa_proto::CUT_BY_REFUSAL)
        || error.message.starts_with(crate::agent_loop::REPEAT_STOP)
    {
        return false;
    }
    match error.code {
        codes::ENGINE_DOWN
        | codes::ENGINE_BUSY
        | codes::STREAM_BROKEN
        | codes::TIMEOUT
        | "cloud_rate_limited" => true,
        codes::ENGINE_HTTP => {
            matches!(crate::retry::http_status(&error.message), Some(s) if s >= 500 || s == 429)
        }
        _ => false,
    }
}

/// The wait before the next attempt of a pursuing goal after `error`, or `None` when the run stops
/// with it. `failures` counts the consecutive transient failures before this one. The wait is
/// `oxa.goal.failureBackoffSeconds` (`0` opts out), doubled per failure and capped at 900 s unless
/// the owner set a longer first wait. A pending abort or wrap-up stops the run, and so does a time
/// budget that would run out during the wait.
pub(crate) fn failure_backoff(
    st: &crate::SessionState,
    error: &oxa_proto::OxaError,
    failures: u32,
) -> Option<Duration> {
    let goal = st
        .goal
        .as_ref()
        .filter(|goal| goal.engaged() && goal.pursuing())?;
    if !transient_failure(error)
        || st.shared.cancel.is_cancelled()
        || st.shared.gate.wrap_up.load(std::sync::atomic::Ordering::SeqCst) != 0
    {
        return None;
    }
    let base = u64::from(
        st.settings
            .user_u32("oxa.goal.failureBackoffSeconds", FAILURE_BACKOFF_SECS),
    );
    if base == 0 {
        return None;
    }
    let cap = FAILURE_BACKOFF_CAP_SECS.max(base);
    let delay = Duration::from_secs(base.saturating_mul(1u64 << failures.min(32)).min(cap));
    goal.time_allows_wait(delay).then_some(delay)
}

/// Wait `delay` without a model request. An abort ends the wait as `Aborted`; `abort_retry` ends
/// it as `RetryStopped` and resets that token, as a retry wait does.
pub(crate) fn wait_failure_backoff(st: &crate::SessionState, delay: Duration) -> BackoffEnd {
    wait_cancelled(
        &st.shared.cancel,
        &st.shared.cancel_retry,
        &st.shared.wait_wake,
        delay,
    );
    if st.shared.cancel.is_cancelled() {
        BackoffEnd::Aborted
    } else if st.shared.cancel_retry.is_cancelled() {
        st.shared.cancel_retry.reset();
        BackoffEnd::RetryStopped
    } else {
        BackoffEnd::Elapsed
    }
}

/// Without terminal usage, a failed attempted generation has no exact spend.
/// Only stable pre-admission classes prove no generation, unless output was
/// already observed. Transport failures remain uncertain even before output.
pub(crate) fn uncertain(error: &oxa_proto::OxaError, output: bool, pre_cancelled: bool) -> bool {
    use oxa_proto::codes;
    if output {
        return true;
    }
    if pre_cancelled && error.code == codes::ABORTED {
        return false;
    }
    // A generic ledger error may be a failed settlement after forwarding.
    // Without authoritative pre-admission evidence its usage remains unknown.
    !matches!(
        error.code,
        codes::OFFLINE
            | codes::BUDGET_EXHAUSTED
            | codes::ENGINE_DOWN
            | codes::ENGINE_BUSY
            | codes::CONTEXT_OVERFLOW
            | codes::DENIED
            | codes::INVALID
            | codes::USAGE
            | codes::UNSUPPORTED_IN_OXA
            | "cost_unknown_history"
            | "cloud_credentials_unavailable"
            | "cloud_sdk_unavailable"
    )
}

/// [`uncertain`] for a request on `remote` or local engine. A remote upstream's 503 is
/// normalized to `ENGINE_BUSY`, which only proves a local engine refused before generating;
/// a forwarded cloud call may have spent tokens, so it stays uncertain. Goal recovery retries
/// that class, and the owner's token budget must fail closed instead of being bypassed.
pub(crate) fn uncertain_on(
    remote: bool,
    error: &oxa_proto::OxaError,
    output: bool,
    pre_cancelled: bool,
) -> bool {
    uncertain(error, output, pre_cancelled)
        || (remote && error.code == oxa_proto::codes::ENGINE_BUSY && !pre_cancelled)
}

#[cfg(test)]
mod uncertain_tests {
    use super::{uncertain, uncertain_on};

    #[test]
    fn a_remote_busy_reply_may_have_spent_tokens_but_a_local_one_has_not() {
        let busy = oxa_proto::OxaError::new(oxa_proto::codes::ENGINE_BUSY, "upstream 503");

        assert!(uncertain_on(true, &busy, false, false));
        assert!(!uncertain_on(false, &busy, false, false));
    }

    #[test]
    fn unknown_cost_history_is_known_pre_admission_usage() {
        let error = oxa_proto::OxaError::new("cost_unknown_history", "prior usage has no known cost");

        assert!(!uncertain(&error, false, false));
        assert!(uncertain(&error, true, false));
    }
}

/// A parsed `/goal` command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GoalCommand {
    Show,
    Set(String),
    Clear,
    Pause,
    Resume,
}

/// Empty text shows the goal, a bare control word controls it, and any other
/// text sets the objective.
pub fn parse_goal_command(args: &str) -> GoalCommand {
    match args.trim() {
        "" => GoalCommand::Show,
        "clear" => GoalCommand::Clear,
        "pause" => GoalCommand::Pause,
        "resume" => GoalCommand::Resume,
        objective => GoalCommand::Set(objective.to_string()),
    }
}

fn show_lines(goal: Option<&Goal>) -> Vec<String> {
    let Some(goal) = goal else {
        return vec!["No goal.".into()];
    };
    vec![
        format!("Objective: {}", goal.objective),
        format!("Status: {}", goal.status.as_str()),
        format!("Tokens: {}", goal.tokens_used),
        format!("Elapsed: {} ms", goal.elapsed_ms),
        format!("Turns: {}", goal.turns),
    ]
}

impl crate::Session {
    /// Apply a `/goal` command on the idle session. Returns the display lines and
    /// the prompt to run next, if any. A set or replace is refused with `busy`
    /// during a run.
    pub fn apply_goal_command(
        &mut self,
        cmd: GoalCommand,
        now_ms: u64,
        sink: &mut dyn oxa_proto::EventSink,
    ) -> Result<(Vec<String>, Option<String>), oxa_proto::OxaError> {
        match cmd {
            GoalCommand::Show => Ok((show_lines(self.goal().as_ref()), None)),
            GoalCommand::Set(objective) => {
                let goal = Goal::new(objective, GoalBudget::default(), now_ms)?;
                let line = format!("Goal set: {}", goal.objective);
                let prompt = crate::prompt::goal_start(&goal.objective);
                self.commit_goal(Some(goal), sink)?;
                Ok((vec![line], Some(prompt)))
            }
            GoalCommand::Clear => {
                self.commit_goal(None, sink)?;
                Ok((vec!["Goal cleared.".into()], None))
            }
            GoalCommand::Pause => {
                let bound = self.inner().sref.clone();
                let revision = self.goal_revision();
                self.control_goal(&bound, revision, GoalControl::Pause, sink)?;
                Ok((vec!["Goal paused.".into()], None))
            }
            GoalCommand::Resume => {
                // A pursuing goal (announced on open, not run) continues without a transition.
                if self.goal().is_some_and(|g| g.status == GoalStatus::Pursuing) {
                    return Ok((
                        vec!["Goal resumed.".into()],
                        Some(crate::prompt::GOAL_CONTINUE.into()),
                    ));
                }
                let bound = self.inner().sref.clone();
                let revision = self.goal_revision();
                self.control_goal(&bound, revision, GoalControl::Resume, sink)?;
                Ok((
                    vec!["Goal resumed.".into()],
                    Some(crate::prompt::GOAL_CONTINUE.into()),
                ))
            }
        }
    }
}
