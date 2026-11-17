//! One native goal snapshot in the existing Custom session envelope.
//! Metadata never contributes text or tools to the transcript/prefix fold.
use oxa_proto::{codes, Entry, EntryBody, OxaError, Value};
use serde_json::json;

pub const GOAL_ENTRY: &str = "oxa.goal.v1";
const MAX_OBJECTIVE: usize = 64 * 1024;
const MAX_EVIDENCE: usize = 8 * 1024;

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
    pub idle_stops: u8,
    pub unknown_requests: u64,
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
            idle_stops: 0,
            unknown_requests: 0,
        };
        goal.validate()?;
        Ok(goal)
    }
    fn validate(&self) -> Result<(), OxaError> {
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
            || self.idle_stops > 2
            || matches!(self.status, GoalStatus::Done | GoalStatus::Blocked) && self.evidence.is_none()
        {
            return Err(invalid());
        }
        Ok(())
    }
    pub fn exhausted(&self) -> bool {
        self.budget
            .max_tokens
            .is_some_and(|n| self.tokens_used >= n || self.unknown_requests > 0)
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
        if self.idle_stops == 2 {
            self.status = GoalStatus::Paused;
            return false;
        }
        true
    }
    pub fn pause(&mut self) {
        if self.status == GoalStatus::Pursuing {
            self.status = GoalStatus::Paused;
        }
    }
    pub fn resume(&mut self) -> Result<(), OxaError> {
        if !self.can_continue() {
            return Err(invalid());
        }
        self.status = GoalStatus::Pursuing;
        self.idle_stops = 0;
        self.evidence = None;
        Ok(())
    }
    pub fn cancel(&mut self) -> Result<(), OxaError> {
        if !self.can_continue() {
            return Err(invalid());
        }
        self.status = GoalStatus::Canceled;
        Ok(())
    }
    fn can_continue(&self) -> bool {
        !matches!(self.status, GoalStatus::Done | GoalStatus::Canceled) && !self.exhausted()
    }
    pub fn value(&self) -> Value {
        json!({"version":1,"objective":self.objective,"status":self.status.as_str(),"started_ms":self.started_ms,
            "tokens_used":self.tokens_used,"elapsed_ms":self.elapsed_ms,"turns":self.turns,
            "budget":{"max_tokens":self.budget.max_tokens,"max_time_ms":self.budget.max_time_ms,"max_turns":self.budget.max_turns},
            "evidence":self.evidence,"idle_stops":self.idle_stops,"unknown_requests":self.unknown_requests})
    }
    pub fn body(&self) -> EntryBody {
        EntryBody::Custom {
            custom_type: GOAL_ENTRY.into(),
            data: self.value(),
        }
    }
    pub fn clear_body() -> EntryBody {
        EntryBody::Custom {
            custom_type: GOAL_ENTRY.into(),
            data: Value::Null,
        }
    }
    pub fn decode(v: &Value) -> Result<Self, OxaError> {
        if v.as_object().ok_or_else(invalid)?.len() != 11 || v["version"] != 1 {
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
            idle_stops: u8::try_from(number("idle_stops")?).map_err(|_| invalid())?,
            unknown_requests: number("unknown_requests")?,
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
}

fn last_record(entries: &[Entry]) -> Option<(usize, &Value)> {
    entries
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, entry)| match &entry.body {
            EntryBody::Custom { custom_type, data } if custom_type == GOAL_ENTRY => Some((index, data)),
            _ => None,
        })
}
