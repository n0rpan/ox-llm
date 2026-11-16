//! Session-local goal controls; no worker, timer thread or external scheduler.
use oxa_proto::{Access, Exposure, Source, Tool, ToolClass, ToolCtx, ToolOutput, ToolSpec, Value};
use oxa_session::{Goal, GoalStatus};
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Wake, Waker};
use std::time::{Duration, Instant};

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
    pub fn pursuing(&self) -> bool {
        self.lock().goal.status == GoalStatus::Pursuing
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
    pub(crate) fn stopped(&self) -> Goal {
        let mut state = self.lock();
        state.goal.stopped(false);
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
    pub(crate) fn uncertain_request(&self) -> Result<(), oxa_proto::OxaError> {
        if self.engaged() {
            self.lock().goal.charge(None, 0, false)?;
            self.enforce_budget();
        }
        Ok(())
    }
    pub(crate) fn enforce_budget(&self) {
        let mut state = self.lock();
        if state.goal.status == GoalStatus::Pursuing && state.goal.exhausted() {
            state.goal.stopped(true);
        }
    }
    pub(crate) fn tools(self: &Arc<Self>, interrupt: oxa_proto::CancelToken) -> Vec<Arc<dyn Tool>> {
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
                goal: self.clone(),
                interrupt: interrupt.clone(),
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
struct Control {
    goal: Arc<GoalRuntime>,
    interrupt: oxa_proto::CancelToken,
    spec: ToolSpec,
}
impl Tool for Control {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn access(&self, args: &Value, _: &ToolCtx) -> Result<Vec<Access>, String> {
        validate(&self.spec.name, args)?;
        Ok(vec![])
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
            return match self
                .goal
                .lock()
                .goal
                .finish(status, args["evidence"].as_str().unwrap().into())
            {
                Ok(()) => ToolOutput::ok("Goal terminal status requested."),
                Err(e) => ToolOutput::error(e.to_string()),
            };
        }
        let duration = {
            let state = self.goal.lock();
            if state.goal.status != GoalStatus::Pursuing || state.goal.exhausted() {
                return ToolOutput::error("Goal is not pursuing or its budget is exhausted.");
            }
            let asked = Duration::from_secs(args["seconds"].as_u64().unwrap());
            let spent = state.clock.map_or(0, |t| t.elapsed().as_millis());
            let remaining = state.goal.budget.max_time_ms.map(|cap| {
                Duration::from_millis(
                    cap.saturating_sub(state.goal.elapsed_ms)
                        .saturating_sub(u64::try_from(spent).unwrap_or(u64::MAX)),
                )
            });
            remaining.map_or(asked, |remaining| asked.min(remaining))
        };
        live("Waiting; cancellation remains available.");
        wait_cancelled(cx.cancel, &self.interrupt, duration);
        if cx.cancel.is_cancelled() {
            ToolOutput::error("Wait cancelled.")
        } else {
            ToolOutput::ok("Wait ended.")
        }
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
        object.len() == 2
            && args["seconds"].as_u64().is_some_and(|n| (1..=3600).contains(&n))
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
fn wait_cancelled(cancel: &oxa_proto::CancelToken, interrupt: &oxa_proto::CancelToken, duration: Duration) {
    let deadline = Instant::now() + duration;
    let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cancelled = std::pin::pin!(cancel.cancelled());
    let mut interrupted = std::pin::pin!(interrupt.cancelled());
    let mut context = Context::from_waker(&waker);
    loop {
        if cancelled.as_mut().poll(&mut context).is_ready()
            || interrupted.as_mut().poll(&mut context).is_ready()
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
        crate::agent_loop::persist(st, goal.snapshot().body(), sink)?;
    }
    Ok(())
}
pub(crate) fn terminal(st: &crate::SessionState) -> bool {
    st.goal.as_ref().is_some_and(|g| g.engaged() && !g.pursuing())
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
            | "cloud_credentials_unavailable"
            | "cloud_sdk_unavailable"
    )
}
