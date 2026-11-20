//! One native goal snapshot in the existing Custom session envelope.
//! Metadata never contributes text or tools to the transcript/prefix fold.
use oxa_proto::{codes, Entry, EntryBody, OxaError, Value};
use serde_json::json;

mod lead;
pub use lead::plan::{CheckResult, GoalCheck, GoalPlan, Subgoal};
pub use lead::{HeldGoalLease, LeadGoalState, OpenGoalAsk, PendingGoalSteer, SteeringLease};

pub const GOAL_ENTRY: &str = "oxa.goal.v1";
const MAX_OBJECTIVE: usize = 64 * 1024;
const MAX_EVIDENCE: usize = 8 * 1024;
const MAX_PAUSE_REASON: usize = 1024;
const IDLE_PAUSE_REASON: &str = "two replies without an action";
const CONTROL_FIELDS: [&str; 6] = ["objective", "status", "started_ms", "budget", "evidence", "plan"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalStatus {
    Pursuing,
    Done,
    Blocked,
    Paused,
    Canceled,
}
impl GoalStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pursuing => "pursuing",
            Self::Done => "done",
            Self::Blocked => "blocked",
            Self::Paused => "paused",
            Self::Canceled => "canceled",
        }
    }
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GoalBudget {
    pub max_tokens: Option<u64>,
    pub max_time_ms: Option<u64>,
    pub max_turns: Option<u64>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Goal {
    pub objective: String,
    pub status: GoalStatus,
    pub started_ms: u64,
    pub tokens_used: u64,
    pub elapsed_ms: u64,
    pub turns: u64,
    pub budget: GoalBudget,
    pub evidence: Option<String>,
    /// Why the goal is paused, when the pause was made by the idle stop rule.
    pub pause_reason: Option<String>,
    pub idle_stops: u8,
    pub unknown_requests: u64,
    pub lead: Option<LeadGoalState>,
    pub plan: Option<GoalPlan>,
}
fn invalid() -> OxaError {
    OxaError::new(codes::INVALID, "invalid stored goal or goal arguments")
}
impl Goal {
    pub fn new(objective: String, budget: GoalBudget, started_ms: u64) -> Result<Self, OxaError> {
        let goal = Self {
            objective,
            status: GoalStatus::Pursuing,
            started_ms,
            tokens_used: 0,
            elapsed_ms: 0,
            turns: 0,
            budget,
            evidence: None,
            pause_reason: None,
            idle_stops: 0,
            unknown_requests: 0,
            lead: None,
            plan: None,
        };
        goal.validate()?;
        Ok(goal)
    }
    pub fn validate(&self) -> Result<(), OxaError> {
        if let Some(lead) = &self.lead {
            lead.validate()?;
        }
        if let Some(plan) = &self.plan {
            plan.validate()?;
            if self.status == GoalStatus::Done && !plan.verified() {
                return Err(invalid());
            }
        }
        if self.objective.trim().is_empty()
            || self.objective.len() > MAX_OBJECTIVE
            || self
                .evidence
                .as_ref()
                .is_some_and(|s| s.trim().is_empty() || s.len() > MAX_EVIDENCE)
            || [
                self.budget.max_tokens,
                self.budget.max_time_ms,
                self.budget.max_turns,
            ]
            .contains(&Some(0))
            || self
                .pause_reason
                .as_ref()
                .is_some_and(|s| s.trim().is_empty() || s.len() > MAX_PAUSE_REASON)
            || self.idle_stops > 2
            || matches!(self.status, GoalStatus::Done | GoalStatus::Blocked) && self.evidence.is_none()
        {
            return Err(invalid());
        }
        Ok(())
    }
    pub fn exhausted(&self) -> bool {
        self.budget.max_tokens.is_some_and(|n| self.tokens_used >= n)
            || self.budget.max_time_ms.is_some_and(|n| self.elapsed_ms >= n)
            || self.budget.max_turns.is_some_and(|n| self.turns >= n)
    }
    pub fn charge(&mut self, tokens: Option<u64>, elapsed_ms: u64, turn: bool) -> Result<(), OxaError> {
        // Compute first: an overflow must not leave half-updated accounting.
        let used = self
            .tokens_used
            .checked_add(tokens.unwrap_or(0))
            .ok_or_else(invalid)?;
        let unknown = self
            .unknown_requests
            .checked_add(u64::from(tokens.is_none()))
            .ok_or_else(invalid)?;
        let elapsed = self.elapsed_ms.checked_add(elapsed_ms).ok_or_else(invalid)?;
        let turns = self.turns.checked_add(u64::from(turn)).ok_or_else(invalid)?;
        self.tokens_used = used;
        self.unknown_requests = unknown;
        self.elapsed_ms = elapsed;
        self.turns = turns;
        Ok(())
    }
    pub fn finish(&mut self, status: GoalStatus, evidence: String) -> Result<(), OxaError> {
        if self.status != GoalStatus::Pursuing
            || !matches!(status, GoalStatus::Done | GoalStatus::Blocked)
            || status == GoalStatus::Done && self.plan.as_ref().is_some_and(|plan| !plan.verified())
            || evidence.trim().is_empty()
            || evidence.len() > MAX_EVIDENCE
        {
            return Err(invalid());
        }
        self.status = status;
        self.evidence = Some(evidence);
        Ok(())
    }
    /// Called once when the assistant stops normally; a wait is a real tool call.
    /// Returns true only when another fixed-message continuation is permitted.
    pub fn stopped(&mut self, had_tools: bool) -> bool {
        self.stopped_with(had_tools, true)
    }
    /// [`Goal::stopped`] with the owner's idle rule: with `pause_idle` two replies without a
    /// tool call pause the goal; without it (the default) they are only counted (`idle_stops`
    /// reaches 2) and pursuit goes on; the caller then nudges once and waits between requests.
    pub fn stopped_with(&mut self, had_tools: bool, pause_idle: bool) -> bool {
        if self.status != GoalStatus::Pursuing {
            return false;
        }
        if self.exhausted() {
            self.status = GoalStatus::Blocked;
            self.evidence = Some("Goal budget exhausted.".into());
            return false;
        }
        self.idle_stops = if had_tools {
            0
        } else {
            (self.idle_stops + 1).min(2)
        };
        if self.idle_stops == 2 && pause_idle {
            self.status = GoalStatus::Paused;
            self.pause_reason = Some(IDLE_PAUSE_REASON.into());
            return false;
        }
        true
    }
    pub fn pause(&mut self) {
        if self.status == GoalStatus::Pursuing {
            self.status = GoalStatus::Paused;
            self.pause_reason = None;
        }
    }
    /// Pause with a reason the owner sees (the goal cannot go on until they act).
    pub fn pause_because(&mut self, reason: &str) {
        if self.status == GoalStatus::Pursuing {
            self.status = GoalStatus::Paused;
            self.pause_reason = Some(reason.into());
        }
    }
    pub fn resume(&mut self) -> Result<(), OxaError> {
        if self.is_terminal() || self.exhausted() {
            return Err(invalid());
        }
        self.status = GoalStatus::Pursuing;
        self.idle_stops = 0;
        self.evidence = None;
        self.pause_reason = None;
        Ok(())
    }
    pub fn cancel(&mut self) -> Result<(), OxaError> {
        if self.is_terminal() {
            return Err(invalid());
        }
        self.status = GoalStatus::Canceled;
        self.pause_reason = None;
        Ok(())
    }
    fn is_terminal(&self) -> bool {
        matches!(self.status, GoalStatus::Done | GoalStatus::Canceled)
    }
    pub fn value(&self) -> Value {
        let mut data = json!({"version":1,"objective":self.objective,"status":self.status.as_str(),"started_ms":self.started_ms,
            "tokens_used":self.tokens_used,"elapsed_ms":self.elapsed_ms,"turns":self.turns,
            "budget":{"max_tokens":self.budget.max_tokens,"max_time_ms":self.budget.max_time_ms,"max_turns":self.budget.max_turns},
            "evidence":self.evidence,"idle_stops":self.idle_stops,"unknown_requests":self.unknown_requests});
        if let Some(lead) = &self.lead {
            data["lead"] = lead.value();
        }
        if let Some(plan) = &self.plan {
            data["plan"] = plan.value();
        }
        if let Some(reason) = &self.pause_reason {
            data["pause_reason"] = reason.as_str().into();
        }
        data
    }
    pub fn body(&self) -> EntryBody {
        Self::record(self.value(), "control")
    }
    /// Charge and settlement checkpoints can also change goal control state.
    pub fn checkpoint_body(&self, entries: &[Entry]) -> EntryBody {
        let data = self.value();
        let accounting = last_record(entries).is_some_and(|(_, previous)| {
            CONTROL_FIELDS
                .iter()
                .all(|&key| previous.get(key) == data.get(key))
        });
        Self::record(data, if accounting { "accounting" } else { "control" })
    }
    fn record(mut data: Value, change: &str) -> EntryBody {
        data["change"] = change.into();
        EntryBody::Custom {
            custom_type: GOAL_ENTRY.into(),
            data,
        }
    }
    pub fn clear_body() -> EntryBody {
        EntryBody::Custom {
            custom_type: GOAL_ENTRY.into(),
            data: Value::Null,
        }
    }
    pub fn decode(v: &Value) -> Result<Self, OxaError> {
        let fields = v.as_object().ok_or_else(invalid)?;
        let field_count = match fields.get("change").map(Value::as_str) {
            None => 11,
            Some(Some("control" | "accounting")) => 12,
            _ => return Err(invalid()),
        };
        let optional = usize::from(fields.contains_key("lead"))
            + usize::from(fields.contains_key("plan"))
            + usize::from(fields.contains_key("pause_reason"));
        if fields.len() != field_count + optional || v["version"] != 1 {
            return Err(invalid());
        }
        let number = |key| v[key].as_u64().ok_or_else(invalid);
        let b = v["budget"].as_object().ok_or_else(invalid)?;
        if b.len() != 3 {
            return Err(invalid());
        }
        let limit = |key| match b.get(key) {
            Some(Value::Null) => Ok(None),
            Some(v) => v.as_u64().map(Some).ok_or_else(invalid),
            None => Err(invalid()),
        };
        let status = match v["status"].as_str() {
            Some("pursuing") => GoalStatus::Pursuing,
            Some("done") => GoalStatus::Done,
            Some("blocked") => GoalStatus::Blocked,
            Some("paused") => GoalStatus::Paused,
            Some("canceled") => GoalStatus::Canceled,
            _ => return Err(invalid()),
        };
        let evidence = match &v["evidence"] {
            Value::Null => None,
            Value::String(s) => Some(s.clone()),
            _ => return Err(invalid()),
        };
        let goal = Self {
            objective: v["objective"].as_str().ok_or_else(invalid)?.into(),
            status,
            started_ms: number("started_ms")?,
            tokens_used: number("tokens_used")?,
            elapsed_ms: number("elapsed_ms")?,
            turns: number("turns")?,
            budget: GoalBudget {
                max_tokens: limit("max_tokens")?,
                max_time_ms: limit("max_time_ms")?,
                max_turns: limit("max_turns")?,
            },
            evidence,
            pause_reason: match fields.get("pause_reason") {
                None => None,
                Some(Value::String(s)) => Some(s.clone()),
                Some(_) => return Err(invalid()),
            },
            idle_stops: u8::try_from(number("idle_stops")?).map_err(|_| invalid())?,
            unknown_requests: number("unknown_requests")?,
            lead: fields.get("lead").map(LeadGoalState::decode).transpose()?,
            plan: fields.get("plan").map(GoalPlan::decode).transpose()?,
        };
        goal.validate()?;
        Ok(goal)
    }
    /// A goal belongs to the session, not a branch: latest native snapshot wins
    /// in file order, like its display name. A clear is an explicit tombstone.
    pub fn latest(entries: &[Entry]) -> Result<Option<Self>, OxaError> {
        last_record(entries)
            .filter(|(_, data)| !data.is_null())
            .map(|(_, data)| Self::decode(data))
            .transpose()
    }

    /// Session-scoped revision of the saved goal, including a clear. Zero means
    /// there has been no goal record. File order keeps it stable on reopen and
    /// advances it on replacement even when the new goal has identical contents.
    pub fn revision(entries: &[Entry]) -> u64 {
        last_record(entries).map_or(0, |(index, _)| index as u64 + 1)
    }

    /// The requested revision must name a saved, nonterminal goal. Later goal
    /// records must be explicitly tagged as accounting-only. Legacy records
    /// remain readable and can anchor new requests.
    pub fn allows_steering(entries: &[Entry], revision: u64) -> bool {
        let Some(index) = revision.checked_sub(1).and_then(|i| usize::try_from(i).ok()) else {
            return false;
        };
        let Some(anchor) = entries.get(index).and_then(record_data) else {
            return false;
        };
        let steerable =
            |data: &Value| matches!(data["status"].as_str(), Some("pursuing" | "paused" | "blocked"));
        steerable(anchor)
            && entries[index + 1..].iter().filter_map(record_data).all(|data| {
                steerable(data)
                    && data["change"] == "accounting"
                    && CONTROL_FIELDS.iter().all(|&key| data.get(key) == anchor.get(key))
            })
    }
}

fn record_data(entry: &Entry) -> Option<&Value> {
    match &entry.body {
        EntryBody::Custom { custom_type, data } if custom_type == GOAL_ENTRY => Some(data),
        _ => None,
    }
}

fn last_record(entries: &[Entry]) -> Option<(usize, &Value)> {
    entries
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, entry)| record_data(entry).map(|data| (index, data)))
}
