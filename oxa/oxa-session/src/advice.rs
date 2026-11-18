//! Advice admissions and outcomes belong to the worker's existing Session.
use oxa_proto::{codes, Entry, EntryBody, OxaError, Value};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    num::{NonZeroU16, NonZeroU32},
};

const ENTRY: &str = "oxa.advice.v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum AdviceTier {
    Bulk,
    Main,
    Top,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdviceTrigger {
    WorkerAsk,
    DoneChecks,
    RepeatGuard,
    RepeatedCi,
    SensitiveAcceptance,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdvicePhase {
    Reserved,
    NotDelivered,
    Delivered,
    Resolved,
    Failed,
}

/// Gate choices come from owner policy. No policy means fixed mode.
#[derive(Clone, Debug)]
pub struct AdvicePolicy {
    pub triggers: Vec<AdviceTrigger>,
    pub max_calls: NonZeroU16,
    pub failures_before_escalation: NonZeroU16,
    pub max_output: NonZeroU32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdvisorRoute {
    pub tier: AdviceTier,
    pub model: String,
    pub pool: String,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct StuckPoint(String);

impl StuckPoint {
    /// Hash the canonical, stable failure identity, not the whole prompt or
    /// transcript. Repeated observations of that identity share admission.
    pub fn from_evidence(identity: &[u8]) -> Result<Self, OxaError> {
        if identity.is_empty() || identity.len() > 65_536 {
            return Err(invalid());
        }
        Ok(Self(format!("{:x}", Sha256::digest(identity))))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdviceGrant {
    pub point: StuckPoint,
    pub trigger: AdviceTrigger,
    pub worker: AdviceTier,
    pub advisor: AdvisorRoute,
    pub max_output: NonZeroU32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdviceReceipt {
    grant: AdviceGrant,
    phase: AdvicePhase,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AdviceAdmission {
    Disabled,
    AlreadyRequested,
    Unavailable,
    LimitReached,
    Escalate,
    Reserve(AdviceReceipt),
}

#[derive(Default)]
pub struct AdviceHistory(BTreeMap<StuckPoint, AdviceReceipt>);

impl AdviceHistory {
    /// Load under the Session's existing writer lock. A reservation seen on
    /// reopen consumes its call even if provider delivery is uncertain.
    pub fn load(entries: &[Entry]) -> Result<Self, OxaError> {
        let mut history = Self::default();
        for entry in entries {
            let (data, in_wire) = match &entry.body {
                EntryBody::Custom { custom_type, data } if custom_type == ENTRY => (data, false),
                EntryBody::CustomMessage {
                    custom_type, details, ..
                } if custom_type == ENTRY => (details, true),
                _ => continue,
            };
            let receipt = AdviceReceipt::decode(data)?;
            if in_wire != (receipt.phase == AdvicePhase::Delivered) {
                return Err(invalid());
            }
            let previous = history.0.get(&receipt.grant.point);
            match previous {
                None if receipt.phase == AdvicePhase::Reserved => {}
                Some(previous) if previous.grant == receipt.grant && previous.phase.allows(receipt.phase) => {
                }
                _ => return Err(invalid()),
            }
            history.0.insert(receipt.grant.point.clone(), receipt);
        }
        Ok(history)
    }

    /// The caller must checkpoint Reserve before contacting the gateway, with
    /// the same Session owner and lock used for the read.
    pub fn admit(
        &self,
        policy: Option<&AdvicePolicy>,
        point: StuckPoint,
        trigger: AdviceTrigger,
        worker: AdviceTier,
        advisor: Option<&AdvisorRoute>,
    ) -> AdviceAdmission {
        let Some(policy) = policy.filter(|policy| policy.triggers.contains(&trigger)) else {
            return AdviceAdmission::Disabled;
        };
        let failed = self
            .0
            .values()
            .filter(|receipt| receipt.grant.worker == worker && receipt.phase == AdvicePhase::Failed)
            .count();
        if worker < AdviceTier::Top && failed >= usize::from(policy.failures_before_escalation.get()) {
            return AdviceAdmission::Escalate;
        }
        if self.0.contains_key(&point) {
            return AdviceAdmission::AlreadyRequested;
        }
        if self.0.len() >= usize::from(policy.max_calls.get()) {
            return AdviceAdmission::LimitReached;
        }
        let Some(advisor) = advisor
            .filter(|route| route.tier > worker && valid_label(&route.model) && valid_label(&route.pool))
        else {
            return AdviceAdmission::Unavailable;
        };
        AdviceAdmission::Reserve(AdviceReceipt {
            grant: AdviceGrant {
                point,
                trigger,
                worker,
                advisor: advisor.clone(),
                max_output: policy.max_output,
            },
            phase: AdvicePhase::Reserved,
        })
    }

    pub fn receipt(&self, point: &StuckPoint) -> Option<&AdviceReceipt> {
        self.0.get(point)
    }
}

impl AdviceReceipt {
    pub fn grant(&self) -> &AdviceGrant {
        &self.grant
    }

    pub fn phase(&self) -> AdvicePhase {
        self.phase
    }

    pub fn advance(&self, next: AdvicePhase) -> Result<Self, OxaError> {
        if !self.phase.allows(next) {
            return Err(invalid());
        }
        Ok(Self {
            grant: self.grant.clone(),
            phase: next,
        })
    }

    /// Private metadata only. Provider usage remains in the existing gateway ledger.
    pub fn body(&self) -> EntryBody {
        EntryBody::Custom {
            custom_type: ENTRY.into(),
            data: self.data(),
        }
    }

    /// Persist delivery and its native worker-wire message in a single append.
    pub fn delivery_body(&self, content: String) -> Result<EntryBody, OxaError> {
        let delivered = self.advance(AdvicePhase::Delivered)?;
        if content.trim().is_empty() || content.len() > 8192 {
            return Err(invalid());
        }
        Ok(EntryBody::CustomMessage {
            custom_type: ENTRY.into(),
            content,
            display: false,
            details: delivered.data(),
        })
    }

    fn data(&self) -> Value {
        let grant = &self.grant;
        json!({
            "point": grant.point.0, "trigger": grant.trigger.as_str(), "worker": grant.worker.as_str(),
            "tier": grant.advisor.tier.as_str(), "model": grant.advisor.model, "pool": grant.advisor.pool,
            "max_output": grant.max_output.get(), "phase": self.phase.as_str(),
        })
    }

    fn decode(value: &Value) -> Result<Self, OxaError> {
        if value.as_object().is_none_or(|object| object.len() != 8) {
            return Err(invalid());
        }
        let get = |key: &str| value[key].as_str().ok_or_else(invalid);
        let point = get("point")?;
        let model = get("model")?;
        let pool = get("pool")?;
        let worker = AdviceTier::parse(get("worker")?)?;
        let tier = AdviceTier::parse(get("tier")?)?;
        if point.len() != 64
            || !point
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            || tier <= worker
            || !valid_label(model)
            || !valid_label(pool)
        {
            return Err(invalid());
        }
        let max_output = value["max_output"]
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .and_then(NonZeroU32::new)
            .ok_or_else(invalid)?;
        Ok(Self {
            grant: AdviceGrant {
                point: StuckPoint(point.into()),
                trigger: AdviceTrigger::parse(get("trigger")?)?,
                worker,
                advisor: AdvisorRoute {
                    tier,
                    model: model.into(),
                    pool: pool.into(),
                },
                max_output,
            },
            phase: AdvicePhase::parse(get("phase")?)?,
        })
    }
}

impl AdvicePhase {
    fn allows(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Reserved, Self::NotDelivered | Self::Delivered)
                | (Self::Delivered, Self::Resolved | Self::Failed)
        )
    }
}

fn valid_label(value: &str) -> bool {
    value.len() <= 256 && !value.trim().is_empty() && !value.chars().any(char::is_control)
}

fn invalid() -> OxaError {
    OxaError::new(codes::INVALID, "Invalid durable advice receipt or transition.")
}

macro_rules! names {
    ($type:ident, $($variant:ident => $name:literal),+ $(,)?) => {
        impl $type {
            fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $name),+ }
            }
            fn parse(value: &str) -> Result<Self, OxaError> {
                match value { $($name => Ok(Self::$variant),)+ _ => Err(invalid()) }
            }
        }
    };
}
names!(AdviceTier, Bulk => "bulk", Main => "main", Top => "top");
names!(AdviceTrigger, WorkerAsk => "worker_ask", DoneChecks => "done_checks",
    RepeatGuard => "repeat_guard", RepeatedCi => "repeated_ci", SensitiveAcceptance => "sensitive_acceptance");
names!(AdvicePhase, Reserved => "reserved", NotDelivered => "not_delivered", Delivered => "delivered",
    Resolved => "resolved", Failed => "failed");
