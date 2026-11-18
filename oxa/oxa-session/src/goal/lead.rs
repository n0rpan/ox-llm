//! Private continuation data carried by the native Session goal record.
use super::invalid;
use oxa_proto::{OxaError, Value};
use serde_json::json;
use std::collections::BTreeSet;

const MAX_ITEMS: usize = 32;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LeadGoalState {
    pub open_asks: Vec<OpenGoalAsk>,
    pub standing_refs: Vec<String>,
    pub held_leases: Vec<HeldGoalLease>,
    pub pending_steer: Option<PendingGoalSteer>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenGoalAsk {
    pub id: String,
    pub revision: u64,
    pub question: String,
}

/// Identity from the assignment owner, never an authorization to use a lease.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SteeringLease {
    pub holder: String,
    pub generation: (u64, u32),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeldGoalLease {
    pub task: String,
    pub lease: SteeringLease,
    pub expires_at: u64,
}

/// The reserved user entry ID is the recovery witness across an interrupted append.
#[derive(Clone, PartialEq, Eq)]
pub struct PendingGoalSteer {
    pub entry_id: String,
    pub revision: u64,
    pub text: String,
    pub lease: Option<SteeringLease>,
}

impl std::fmt::Debug for PendingGoalSteer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("PendingGoalSteer").finish_non_exhaustive()
    }
}

impl LeadGoalState {
    pub fn validate(&self) -> Result<(), OxaError> {
        let unique = |keys: Vec<&str>| {
            keys.len() <= MAX_ITEMS
                && keys.iter().all(|key| atom(key))
                && keys.iter().collect::<BTreeSet<_>>().len() == keys.len()
        };
        if !unique(self.open_asks.iter().map(|ask| ask.id.as_str()).collect())
            || !unique(self.standing_refs.iter().map(String::as_str).collect())
            || !unique(self.held_leases.iter().map(|lease| lease.task.as_str()).collect())
            || self.open_asks.iter().any(|ask| {
                ask.revision == 0 || !text(&ask.question, 512) || ask.question.chars().any(char::is_control)
            })
            || self
                .held_leases
                .iter()
                .any(|held| !held.lease.valid() || held.expires_at == 0)
            || self.pending_steer.as_ref().is_some_and(|pending| {
                pending.revision == 0
                    || pending.entry_id.len() != 8
                    || !pending.entry_id.bytes().all(|byte| byte.is_ascii_hexdigit())
                    || !text(&pending.text, 16 * 1024)
                    || pending.lease.as_ref().is_some_and(|lease| !lease.valid())
            })
        {
            return Err(invalid());
        }
        Ok(())
    }

    pub(super) fn value(&self) -> Value {
        let asks: Vec<_> = self
            .open_asks
            .iter()
            .map(|a| json!({"id":a.id,"revision":a.revision,"question":a.question}))
            .collect();
        let leases: Vec<_> = self.held_leases.iter().map(|h| {
            json!({"task":h.task,"holder":h.lease.holder,"generation":h.lease.generation,"expires_at":h.expires_at})
        }).collect();
        let pending = self.pending_steer.as_ref().map(|p| {
            let lease = p
                .lease
                .as_ref()
                .map(|l| json!({"holder":l.holder,"generation":l.generation}));
            json!({"entry_id":p.entry_id,"revision":p.revision,"text":p.text,"lease":lease})
        });
        json!({"open_asks":asks,"standing_refs":self.standing_refs,"held_leases":leases,"pending_steer":pending})
    }

    pub(super) fn decode(v: &Value) -> Result<Self, OxaError> {
        fields(v, &["open_asks", "standing_refs", "held_leases", "pending_steer"])?;
        let pending = &v["pending_steer"];
        let state = Self {
            open_asks: list(&v["open_asks"], |a| {
                fields(a, &["id", "revision", "question"])?;
                Ok(OpenGoalAsk {
                    id: string(&a["id"])?,
                    revision: number(&a["revision"])?,
                    question: string(&a["question"])?,
                })
            })?,
            standing_refs: list(&v["standing_refs"], string)?,
            held_leases: list(&v["held_leases"], |h| {
                fields(h, &["task", "holder", "generation", "expires_at"])?;
                Ok(HeldGoalLease {
                    task: string(&h["task"])?,
                    lease: SteeringLease::decode(h)?,
                    expires_at: number(&h["expires_at"])?,
                })
            })?,
            pending_steer: if pending.is_null() {
                None
            } else {
                fields(pending, &["entry_id", "revision", "text", "lease"])?;
                let lease = &pending["lease"];
                let lease = if lease.is_null() {
                    None
                } else {
                    fields(lease, &["holder", "generation"])?;
                    Some(SteeringLease::decode(lease)?)
                };
                Some(PendingGoalSteer {
                    entry_id: string(&pending["entry_id"])?,
                    revision: number(&pending["revision"])?,
                    text: string(&pending["text"])?,
                    lease,
                })
            },
        };
        state.validate()?;
        Ok(state)
    }
}

impl SteeringLease {
    fn valid(&self) -> bool {
        text(&self.holder, 128) && !self.holder.chars().any(char::is_control) && self.generation.1 > 0
    }

    fn decode(value: &Value) -> Result<Self, OxaError> {
        let generation = value["generation"]
            .as_array()
            .filter(|g| g.len() == 2)
            .ok_or_else(invalid)?;
        Ok(Self {
            holder: string(&value["holder"])?,
            generation: (
                number(&generation[0])?,
                u32::try_from(number(&generation[1])?).map_err(|_| invalid())?,
            ),
        })
    }
}

fn fields(value: &Value, keys: &[&str]) -> Result<(), OxaError> {
    value
        .as_object()
        .filter(|object| object.len() == keys.len() && keys.iter().all(|key| object.contains_key(*key)))
        .map(|_| ())
        .ok_or_else(invalid)
}

fn list<T>(value: &Value, decode: impl Fn(&Value) -> Result<T, OxaError>) -> Result<Vec<T>, OxaError> {
    value
        .as_array()
        .filter(|items| items.len() <= MAX_ITEMS)
        .ok_or_else(invalid)?
        .iter()
        .map(decode)
        .collect()
}

fn string(value: &Value) -> Result<String, OxaError> {
    value.as_str().map(str::to_owned).ok_or_else(invalid)
}

fn number(value: &Value) -> Result<u64, OxaError> {
    value.as_u64().ok_or_else(invalid)
}

fn atom(value: &str) -> bool {
    text(value, 128)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:/".contains(&byte))
}

fn text(value: &str, max: usize) -> bool {
    !value.trim().is_empty() && value.len() <= max && !value.contains('\0')
}
