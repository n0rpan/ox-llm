use super::Scope;
use crate::Result;
use serde::{Deserialize, Serialize};

/// Supplied by a trusted frontend; this is not a field in a rule/tool request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Authority {
    Owner,
    Lead,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Goal,
    Priority,
    Freeze,
    Hold,
    Budget,
    Model,
    Placement,
    Gate,
    Eligible,
    Decided,
    Reserve,
    Note,
}

/// A source reference is selected/checked by the frontend, never inferred from tool success.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Source {
    Command { command: String },
    SessionEntry { session_id: String, entry_id: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Value {
    Text(String),
    /// An explicit release is a new entry which supersedes the hold/freeze it releases.
    Constraint {
        active: bool,
        until: Option<Condition>,
    },
    Retired,
    Budget {
        daily_usd_micros: u64,
    },
    Gate {
        tier: ReviewTier,
        evidence: Vec<GateEvidence>,
    },
    Reserve {
        classes: Vec<ReservedClass>,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Condition {
    PrMerged(u64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewTier {
    T1,
    T2,
    T3,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GateEvidence {
    LocalChecks,
    PrBuild,
    Review,
    Queue,
    HardwareMeasurement,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReservedClass {
    Public,
    Paid,
    Credentials,
    Destructive,
    Desktop,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub kind: Kind,
    pub scope: Scope,
    pub value: Value,
    pub said: String,
    pub source: Source,
    pub supersedes: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum EntryState {
    Stored,
    Applied { verified_at: u64, evidence: String },
    Superseded,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub id: String,
    pub kind: Kind,
    pub scope: Scope,
    pub value: Value,
    pub said: String,
    pub at: u64,
    pub said_by: Authority,
    pub source: Source,
    pub supersedes: Option<String>,
    pub state: EntryState,
    pub reminders: u32,
    pub last_reminder: Option<Reminder>,
    // Inherited by a strengthening replacement: a lead cannot launder an owner's live hold
    // into its own hold and then release it. Request source remains attributed separately.
    pub(super) authority: Authority,
}
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Reminder {
    pub at: u64,
    pub source: Source,
}

pub(super) fn text(value: &str, maximum: usize) -> Result<()> {
    if value.trim().is_empty()
        || value.len() > maximum
        || value
            .chars()
            .any(|c| c.is_control() || matches!(c, '\u{2028}' | '\u{2029}'))
    {
        return Err("standing text must be bounded and single-line".into());
    }
    Ok(())
}
pub(super) fn id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.:/".contains(&c))
    {
        return Err("invalid standing identifier".into());
    }
    Ok(())
}
impl Scope {
    pub(super) fn validate(&self) -> Result<()> {
        match self {
            Self::Repository => Ok(()),
            Self::PullRequest(number) if *number > 0 => Ok(()),
            Self::PullRequest(_) => Err("standing PR number must be positive".into()),
            Self::Task(value) | Self::Host(value) => id(value),
            Self::Branch(value) => {
                text(value, 256)?;
                if value.starts_with('-') {
                    return Err("invalid standing branch".into());
                }
                Ok(())
            }
            Self::Path(value) => {
                text(value, 256)?;
                if value.starts_with('/') || value.split('/').any(|part| matches!(part, "" | "." | "..")) {
                    return Err("standing path selector must be repository-relative".into());
                }
                Ok(())
            }
        }
    }
}
impl Source {
    pub(super) fn validate(&self) -> Result<()> {
        match self {
            Self::Command { command } => text(command, 512),
            Self::SessionEntry { session_id, entry_id } => {
                id(session_id)?;
                id(entry_id)
            }
        }
    }
}
impl Rule {
    pub(super) fn validate(&self) -> Result<()> {
        self.scope.validate()?;
        self.source.validate()?;
        text(&self.said, 1024)?;
        if let Some(prior) = &self.supersedes {
            id(prior)?;
        }
        if let Value::Constraint {
            active,
            until: Some(Condition::PrMerged(pr)),
        } = &self.value
        {
            if !active || *pr == 0 {
                return Err("invalid standing release condition".into());
            }
        }
        match (&self.kind, &self.value) {
            (_, Value::Retired) => Ok(()),
            (Kind::Hold | Kind::Freeze, Value::Constraint { .. })
                if !matches!(self.scope, Scope::Path(_)) =>
            {
                Ok(())
            }
            (Kind::Budget, Value::Budget { .. }) => Ok(()),
            (Kind::Gate, Value::Gate { evidence, .. }) => {
                if evidence.len() > 5
                    || evidence
                        .iter()
                        .enumerate()
                        .any(|(i, value)| evidence[..i].contains(value))
                {
                    return Err("invalid gate requirements".into());
                }
                Ok(())
            }
            (Kind::Reserve, Value::Reserve { classes }) => {
                if classes.is_empty()
                    || classes.len() > 5
                    || classes
                        .iter()
                        .enumerate()
                        .any(|(i, value)| classes[..i].contains(value))
                {
                    return Err("invalid reserved classes".into());
                }
                Ok(())
            }
            (
                Kind::Goal
                | Kind::Priority
                | Kind::Model
                | Kind::Placement
                | Kind::Eligible
                | Kind::Decided
                | Kind::Note,
                Value::Text(value),
            ) => text(value, 1024),
            _ => Err("standing kind and value schema disagree".into()),
        }
    }
}
impl Entry {
    pub fn is_live(&self) -> bool {
        self.state != EntryState::Superseded
            && !matches!(
                self.value,
                Value::Retired | Value::Constraint { active: false, .. }
            )
    }
    pub fn needs_conversion_offer(&self) -> bool {
        self.kind == Kind::Note && self.reminders >= 2 && self.state != EntryState::Superseded
    }
    pub(super) fn rule(&self) -> Rule {
        Rule {
            kind: self.kind,
            scope: self.scope.clone(),
            value: self.value.clone(),
            said: self.said.clone(),
            source: self.source.clone(),
            supersedes: self.supersedes.clone(),
        }
    }
    pub(super) fn validate(&self) -> Result<()> {
        id(&self.id)?;
        self.rule().validate()?;
        if (self.reminders == 0) != self.last_reminder.is_none() {
            return Err("standing reminder metadata disagrees".into());
        }
        if let Some(reminder) = &self.last_reminder {
            reminder.source.validate()?;
        }
        if let EntryState::Applied { evidence, .. } = &self.state {
            text(evidence, 256)?;
        }
        Ok(())
    }
}
