//! Session-owned goal lifecycle for the project orchestrator (#94).
//! State and budgets survive turn boundaries; a reply is not goal completion.
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::time::Duration;

const MAX_EVENTS: usize = 128;
const MAX_TEXT: usize = 16 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    pub wall_ms: u64,
    pub tokens: Option<u64>,
    pub turns: u32,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Active,
    Pausing,
    Paused,
    Done,
    Blocked,
    BudgetExhausted,
    Canceled,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum Decision {
    Continue { summary: String },
    Done { summary: String, evidence: Vec<String> },
    Blocked { reason: String },
}
#[derive(Clone, Debug, Serialize)]
pub struct Snapshot {
    pub id: String,
    pub goal: String,
    pub state: State,
    pub revision: u64,
    pub budget: Budget,
    pub turns_started: u32,
    /// Unknown during a turn or after any turn with unavailable usage.
    pub reported_tokens: Option<u64>,
    pub turn_active: bool,
    pub elapsed_ms: u64,
    pub reason: Option<String>,
    /// Agent-reported evidence, never approval to execute or merge results.
    pub evidence: Vec<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct Event {
    pub sequence: u64,
    pub revision: u64,
    pub kind: String,
    pub state: State,
    pub turn: u32,
}
#[derive(Clone, Debug, Serialize)]
pub struct Events {
    pub oldest_sequence: u64,
    /// Inclusive cursor for the next page, immediately after its last event.
    pub next_sequence: u64,
    /// Exclusive high-water mark of the ledger, which may be beyond this page.
    pub latest_sequence: u64,
    pub gap: bool,
    pub events: Vec<Event>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    None,
    AbortTurn,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Invalid,
    Conflict,
    NotActive,
    TurnActive,
    NoTurn,
    AccountingUnavailable,
    ClockUnavailable,
}

/// One authoritative goal, intended to live inside the same session owner used
/// by a TUI and external control clients. No writer identity or exclusive UI lock.
pub struct Goal {
    snapshot: Snapshot,
    started_ns: u64,
    finished_elapsed: Option<Duration>,
    known_tokens: u64,
    unknown_turns: u32,
    events: VecDeque<Event>,
    next_sequence: u64,
}
impl Goal {
    pub(crate) fn state(&self) -> State {
        self.snapshot.state
    }
    pub(crate) fn revision(&self) -> u64 {
        self.snapshot.revision
    }
    pub(crate) fn runtime_blocked(&mut self, reason: &str) {
        self.snapshot.state = State::Blocked;
        self.snapshot.reason = Some(reason.into());
        self.snapshot.evidence.clear();
        self.event("goal_blocked");
    }
    pub(crate) fn runtime_budget(&mut self, reason: &str) {
        self.exhaust(reason);
    }
    pub(crate) fn steered(&mut self) {
        self.event("steer_queued");
    }
    pub fn new(id: String, goal: String, budget: Budget) -> Result<Self, Error> {
        if id.is_empty()
            || id.len() > 64
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
            || !text(&goal, MAX_TEXT)
            || budget.wall_ms == 0
            || budget.wall_ms > 86_400_000
            || budget.tokens == Some(0)
            || budget.turns == 0
            || budget.turns > 10_000
        {
            return Err(Error::Invalid);
        }
        let mut value = Self {
            snapshot: Snapshot {
                id,
                goal,
                state: State::Active,
                revision: 0,
                budget,
                turns_started: 0,
                reported_tokens: Some(0),
                turn_active: false,
                elapsed_ms: 0,
                reason: None,
                evidence: vec![],
            },
            started_ns: crate::resume::boot_time_ns().map_err(|_| Error::ClockUnavailable)?,
            finished_elapsed: None,
            known_tokens: 0,
            unknown_turns: 0,
            events: VecDeque::new(),
            next_sequence: 0,
        };
        value.event("goal_set");
        Ok(value)
    }
    pub fn snapshot(&self) -> Snapshot {
        let mut snapshot = self.snapshot.clone();
        snapshot.elapsed_ms = self.elapsed().as_millis().min(u64::MAX as u128) as u64;
        snapshot
    }
    /// Read from the inclusive `after` cursor. Continue with the returned
    /// `next_sequence`; `latest_sequence` describes the ledger, not the page.
    pub fn events(&self, after: u64, limit: usize) -> Events {
        let oldest = self
            .events
            .front()
            .map_or(self.next_sequence, |event| event.sequence);
        let events: Vec<_> = self
            .events
            .iter()
            .filter(|event| event.sequence >= after)
            .take(limit.min(64))
            .cloned()
            .collect();
        Events {
            oldest_sequence: oldest,
            next_sequence: events.last().map_or(after, |event| event.sequence + 1),
            latest_sequence: self.next_sequence,
            gap: after < oldest,
            events,
        }
    }
    pub fn begin_turn(&mut self) -> Result<(), Error> {
        self.tick();
        if self.snapshot.state != State::Active {
            return Err(Error::NotActive);
        }
        if self.snapshot.turn_active {
            return Err(Error::TurnActive);
        }
        if self.snapshot.turns_started == self.snapshot.budget.turns {
            self.exhaust("turn_budget");
            return Err(Error::NotActive);
        }
        if self
            .snapshot
            .budget
            .tokens
            .is_some_and(|cap| self.known_tokens >= cap)
        {
            self.exhaust("token_budget");
            return Err(Error::NotActive);
        }
        self.snapshot.turns_started += 1;
        self.snapshot.turn_active = true;
        self.snapshot.reported_tokens = None;
        self.event("turn_started");
        Ok(())
    }
    /// Account a settled turn once. None is unknown usage, never zero. A token-
    /// bounded goal cannot continue or claim DONE without complete accounting.
    pub fn finish_turn(&mut self, usage: Option<u64>, decision: Decision) -> Result<(), Error> {
        if !self.snapshot.turn_active {
            return Err(Error::NoTurn);
        }
        validate_decision(&decision)?;
        let total = match usage {
            Some(tokens) => Some(self.known_tokens.checked_add(tokens).ok_or(Error::Invalid)?),
            None => None,
        };
        if let Some(total) = total {
            self.known_tokens = total;
        } else {
            self.unknown_turns += 1;
        }
        self.snapshot.turn_active = false;
        self.snapshot.reported_tokens = (self.unknown_turns == 0).then_some(self.known_tokens);
        self.event("turn_finished");
        self.tick();
        if matches!(self.snapshot.state, State::BudgetExhausted | State::Canceled) {
            return Ok(());
        }
        if self
            .snapshot
            .budget
            .tokens
            .is_some_and(|cap| self.known_tokens > cap)
        {
            self.exhaust("token_budget");
            return Ok(());
        }
        if self.snapshot.budget.tokens.is_some() && self.unknown_turns != 0 {
            self.snapshot.state = State::Blocked;
            self.snapshot.reason = Some("token_accounting_unavailable".into());
            self.event("goal_blocked");
            return Ok(());
        }
        if self.snapshot.state == State::Pausing {
            self.snapshot.state = State::Paused;
            self.event("goal_paused");
            return Ok(());
        }
        match decision {
            Decision::Continue { .. } => {
                if self.snapshot.turns_started == self.snapshot.budget.turns {
                    self.exhaust("turn_budget");
                } else if self
                    .snapshot
                    .budget
                    .tokens
                    .is_some_and(|cap| self.known_tokens == cap)
                {
                    self.exhaust("token_budget");
                }
            }
            Decision::Done { evidence, .. } => {
                self.snapshot.state = State::Done;
                self.snapshot.evidence = evidence;
                self.event("goal_done");
            }
            Decision::Blocked { reason } => {
                self.snapshot.state = State::Blocked;
                self.snapshot.reason = Some(reason);
                self.event("goal_blocked");
            }
        }
        Ok(())
    }
    /// Pause is visible as pausing until the runtime confirms the turn settled.
    /// Callers must execute AbortTurn and still supervise owned cleanup.
    pub fn pause(&mut self, revision: u64) -> Result<Effect, Error> {
        self.check(revision)?;
        if !matches!(self.snapshot.state, State::Active | State::Blocked) {
            return Err(Error::NotActive);
        }
        self.snapshot.state = if self.snapshot.turn_active {
            State::Pausing
        } else {
            State::Paused
        };
        self.event("pause_requested");
        Ok(if self.snapshot.turn_active {
            Effect::AbortTurn
        } else {
            Effect::None
        })
    }
    pub fn resume(&mut self, revision: u64) -> Result<(), Error> {
        self.check(revision)?;
        self.tick();
        if !matches!(self.snapshot.state, State::Paused | State::Blocked) {
            return Err(Error::NotActive);
        }
        if self.snapshot.budget.tokens.is_some() && self.unknown_turns != 0 {
            return Err(Error::AccountingUnavailable);
        }
        self.snapshot.state = State::Active;
        self.snapshot.reason = None;
        self.event("goal_resumed");
        Ok(())
    }
    /// Redirect settled work without resetting any budget or accounting. The
    /// operator must explicitly resume after changing the goal.
    pub fn set_goal(&mut self, revision: u64, goal: String) -> Result<(), Error> {
        self.check(revision)?;
        self.tick();
        if self.snapshot.turn_active || !matches!(self.snapshot.state, State::Paused | State::Blocked) {
            return Err(Error::NotActive);
        }
        if !text(&goal, MAX_TEXT) {
            return Err(Error::Invalid);
        }
        self.snapshot.goal = goal;
        self.snapshot.evidence.clear();
        self.snapshot.state = State::Paused;
        self.snapshot.reason = None;
        self.event("goal_updated");
        Ok(())
    }
    pub fn cancel(&mut self, revision: u64) -> Result<Effect, Error> {
        self.check(revision)?;
        if matches!(
            self.snapshot.state,
            State::Done | State::Canceled | State::BudgetExhausted
        ) {
            return Err(Error::NotActive);
        }
        self.snapshot.state = State::Canceled;
        self.snapshot.reason = Some("operator_canceled".into());
        self.event("goal_canceled");
        Ok(if self.snapshot.turn_active {
            Effect::AbortTurn
        } else {
            Effect::None
        })
    }
    /// Time includes pauses. Terminal budget state does not falsely assert that
    /// an in-flight request has already stopped; turn_active remains explicit.
    pub fn tick(&mut self) -> Effect {
        if matches!(
            self.snapshot.state,
            State::Done | State::Canceled | State::BudgetExhausted
        ) {
            return if self.snapshot.turn_active {
                Effect::AbortTurn
            } else {
                Effect::None
            };
        }
        match self.read_elapsed() {
            Ok(elapsed) => self.tick_at(elapsed),
            Err(_) => {
                self.exhaust("clock_unavailable");
                if self.snapshot.turn_active {
                    Effect::AbortTurn
                } else {
                    Effect::None
                }
            }
        }
    }
    fn tick_at(&mut self, elapsed: Duration) -> Effect {
        self.snapshot.elapsed_ms = elapsed.as_millis().min(u64::MAX as u128) as u64;
        if !matches!(
            self.snapshot.state,
            State::Done | State::Canceled | State::BudgetExhausted
        ) && elapsed >= Duration::from_millis(self.snapshot.budget.wall_ms)
        {
            self.exhaust("time_budget");
        }
        if self.snapshot.state == State::BudgetExhausted && self.snapshot.turn_active {
            Effect::AbortTurn
        } else {
            Effect::None
        }
    }
    fn elapsed(&self) -> Duration {
        self.read_elapsed()
            .unwrap_or(Duration::from_millis(self.snapshot.elapsed_ms))
    }
    fn read_elapsed(&self) -> Result<Duration, Error> {
        if let Some(elapsed) = self.finished_elapsed {
            return Ok(elapsed);
        }
        let now = crate::resume::boot_time_ns().map_err(|_| Error::ClockUnavailable)?;
        Ok(Duration::from_nanos(
            now.checked_sub(self.started_ns).ok_or(Error::ClockUnavailable)?,
        ))
    }
    fn check(&self, revision: u64) -> Result<(), Error> {
        if revision == self.snapshot.revision {
            Ok(())
        } else {
            Err(Error::Conflict)
        }
    }
    fn exhaust(&mut self, reason: &str) {
        self.snapshot.state = State::BudgetExhausted;
        self.snapshot.reason = Some(reason.into());
        self.event("goal_budget_exhausted");
    }
    fn event(&mut self, kind: &str) {
        if !self.snapshot.turn_active
            && matches!(
                self.snapshot.state,
                State::Done | State::Canceled | State::BudgetExhausted
            )
            && self.finished_elapsed.is_none()
        {
            self.finished_elapsed = Some(
                self.elapsed()
                    .max(Duration::from_millis(self.snapshot.elapsed_ms)),
            );
        }
        self.snapshot.revision += 1;
        if self.events.len() == MAX_EVENTS {
            self.events.pop_front();
        }
        self.events.push_back(Event {
            sequence: self.next_sequence,
            revision: self.snapshot.revision,
            kind: kind.into(),
            state: self.snapshot.state,
            turn: self.snapshot.turns_started,
        });
        self.next_sequence += 1;
    }
}
fn text(value: &str, max: usize) -> bool {
    !value.trim().is_empty() && value.len() <= max
}
fn validate_decision(decision: &Decision) -> Result<(), Error> {
    let valid = match decision {
        Decision::Continue { summary } => text(summary, 4096),
        Decision::Done { summary, evidence } => {
            text(summary, 4096)
                && !evidence.is_empty()
                && evidence.len() <= 16
                && evidence.iter().all(|item| text(item, 4096))
        }
        Decision::Blocked { reason } => text(reason, 4096),
    };
    if valid {
        Ok(())
    } else {
        Err(Error::Invalid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn goal(tokens: Option<u64>) -> Goal {
        Goal::new(
            "project".into(),
            "Review and fix the selected project lane".into(),
            Budget {
                wall_ms: 60_000,
                tokens,
                turns: 4,
            },
        )
        .unwrap()
    }
    fn more() -> Decision {
        Decision::Continue {
            summary: "One useful step finished; more remains".into(),
        }
    }
    #[test]
    fn event_pages_preserve_unread_history_and_expose_the_high_water_mark() {
        let mut goal = goal(None);
        for _ in 0..90 {
            goal.steered();
        }
        let first = goal.events(0, 64);
        assert_eq!(first.events.len(), 64);
        assert_eq!(first.next_sequence, 64);
        assert_eq!(first.latest_sequence, 91);
        let second = goal.events(first.next_sequence, 64);
        assert_eq!(second.events.len(), 27);
        assert_eq!(second.events.first().unwrap().sequence, 64);
        assert_eq!(second.next_sequence, 91);
        let empty = goal.events(second.next_sequence, 64);
        assert!(empty.events.is_empty());
        assert_eq!(empty.next_sequence, second.next_sequence);
        let zero = goal.events(0, 0);
        assert!(zero.events.is_empty());
        assert_eq!(zero.next_sequence, 0);
        assert_eq!(zero.latest_sequence, 91);

        for _ in 0..90 {
            goal.steered();
        }
        let retained = goal.events(0, 64);
        assert!(retained.gap);
        assert_eq!(retained.events.len(), 64);
        assert_eq!(retained.events[0].sequence, retained.oldest_sequence);
        let tail = goal.events(retained.next_sequence, 64);
        assert!(!tail.gap);
        assert_eq!(tail.events.len(), 64);
        assert_eq!(tail.next_sequence, tail.latest_sequence);
    }

    #[test]
    fn a_waiting_goal_can_be_paused_without_starting_or_refunding_a_turn() {
        let mut goal = goal(Some(100));
        goal.begin_turn().unwrap();
        goal.finish_turn(
            Some(20),
            Decision::Blocked {
                reason: "waiting_background_tasks".into(),
            },
        )
        .unwrap();
        assert_eq!(goal.begin_turn(), Err(Error::NotActive));
        let before = goal.snapshot();
        assert_eq!(goal.pause(before.revision), Ok(Effect::None));
        let paused = goal.snapshot();
        assert_eq!(paused.state, State::Paused);
        assert_eq!(paused.reported_tokens, Some(20));
        assert_eq!(paused.turns_started, 1);
        assert_eq!(paused.budget.tokens, Some(100));
        goal.resume(paused.revision).unwrap();
        goal.begin_turn().unwrap();
    }
    #[test]
    fn redirect_requires_settlement_and_preserves_cumulative_allowances() {
        let mut goal = goal(Some(100));
        goal.begin_turn().unwrap();
        assert_eq!(
            goal.set_goal(goal.revision(), "New goal".into()),
            Err(Error::NotActive)
        );
        goal.pause(goal.revision()).unwrap();
        assert_eq!(
            goal.set_goal(goal.revision(), "New goal".into()),
            Err(Error::NotActive)
        );
        goal.finish_turn(Some(20), more()).unwrap();
        let before = goal.snapshot();
        assert!(goal.set_goal(before.revision + 1, "New goal".into()).is_err());
        goal.set_goal(before.revision, "New goal".into()).unwrap();
        let after = goal.snapshot();
        assert_eq!(after.goal, "New goal");
        assert_eq!(after.state, State::Paused);
        assert_eq!(after.turns_started, before.turns_started);
        assert_eq!(after.reported_tokens, Some(20));
        assert_eq!(after.budget.tokens, Some(100));
        assert!(after.elapsed_ms >= before.elapsed_ms);
        goal.resume(after.revision).unwrap();
        goal.begin_turn().unwrap();
        goal.finish_turn(Some(80), more()).unwrap();
        assert_eq!(goal.snapshot().state, State::BudgetExhausted);
    }
    #[test]
    fn one_goal_survives_multiple_turns_with_cumulative_budget_and_explicit_done() {
        let mut goal = goal(Some(100));
        for tokens in [20, 30] {
            goal.begin_turn().unwrap();
            assert!(goal.snapshot().reported_tokens.is_none());
            goal.finish_turn(Some(tokens), more()).unwrap();
        }
        assert_eq!(goal.snapshot().state, State::Active);
        assert_eq!(goal.snapshot().reported_tokens, Some(50));
        goal.begin_turn().unwrap();
        goal.finish_turn(
            Some(10),
            Decision::Done {
                summary: "Requested checks passed".into(),
                evidence: vec!["Owned regression suite passed".into()],
            },
        )
        .unwrap();
        assert_eq!(goal.snapshot().state, State::Done);
        assert_eq!(goal.snapshot().turns_started, 3);
        assert_eq!(goal.snapshot().reported_tokens, Some(60));
        let elapsed = goal.snapshot().elapsed_ms;
        goal.started_ns = goal.started_ns.saturating_sub(10_000_000_000);
        assert_eq!(
            goal.snapshot().elapsed_ms,
            elapsed,
            "terminal goal time must not keep growing while the session remains open"
        );
        assert_eq!(goal.begin_turn(), Err(Error::NotActive));
    }
    #[test]
    fn pause_waits_for_settlement_and_two_control_clients_use_revisions_not_writer_locks() {
        let mut goal = goal(Some(100));
        goal.begin_turn().unwrap();
        let tui_revision = goal.snapshot().revision;
        assert_eq!(goal.pause(tui_revision).unwrap(), Effect::AbortTurn);
        assert_eq!(goal.snapshot().state, State::Pausing);
        assert!(goal.snapshot().turn_active);
        assert_eq!(goal.resume(tui_revision), Err(Error::Conflict));
        goal.finish_turn(Some(7), more()).unwrap();
        assert_eq!(goal.snapshot().state, State::Paused);
        let external_revision = goal.snapshot().revision;
        goal.resume(external_revision).unwrap();
        goal.begin_turn().unwrap();
        goal.finish_turn(
            Some(5),
            Decision::Blocked {
                reason: "Need the operator's review input".into(),
            },
        )
        .unwrap();
        assert_eq!(goal.snapshot().state, State::Blocked);
        goal.resume(goal.snapshot().revision).unwrap();
        assert_eq!(goal.snapshot().reported_tokens, Some(12));
    }
    #[test]
    fn missing_usage_and_budget_exhaustion_never_become_done_or_a_fresh_allowance() {
        let mut unknown = goal(Some(100));
        unknown.begin_turn().unwrap();
        unknown.finish_turn(None, more()).unwrap();
        assert_eq!(unknown.snapshot().state, State::Blocked);
        assert!(unknown.snapshot().reported_tokens.is_none());
        assert_eq!(
            unknown.resume(unknown.snapshot().revision),
            Err(Error::AccountingUnavailable)
        );
        let mut capped = goal(Some(5));
        capped.begin_turn().unwrap();
        capped.finish_turn(Some(5), more()).unwrap();
        assert_eq!(capped.snapshot().state, State::BudgetExhausted);
        assert_eq!(capped.resume(capped.snapshot().revision), Err(Error::NotActive));
        let mut timed = goal(None);
        timed.begin_turn().unwrap();
        timed.pause(timed.snapshot().revision).unwrap();
        assert_eq!(timed.tick_at(Duration::from_secs(60)), Effect::AbortTurn);
        assert_eq!(timed.snapshot().state, State::BudgetExhausted);
        assert!(timed.snapshot().turn_active);
        timed.finish_turn(None, more()).unwrap();
        assert!(!timed.snapshot().turn_active);
    }
    #[test]
    fn invalid_completion_is_not_done_and_event_history_reports_gaps() {
        let mut goal = goal(None);
        goal.begin_turn().unwrap();
        assert_eq!(
            goal.finish_turn(
                Some(1),
                Decision::Done {
                    summary: "claimed done".into(),
                    evidence: vec![]
                }
            ),
            Err(Error::Invalid)
        );
        assert!(goal.snapshot().turn_active);
        assert_eq!(goal.snapshot().state, State::Active);
        goal.finish_turn(Some(1), more()).unwrap();
        for _ in 0..80 {
            goal.pause(goal.snapshot().revision).unwrap();
            goal.resume(goal.snapshot().revision).unwrap();
        }
        let events = goal.events(0, 999);
        assert!(events.gap);
        assert_eq!(events.events.len(), 64);
        assert!(events.oldest_sequence > 0);
    }
}
