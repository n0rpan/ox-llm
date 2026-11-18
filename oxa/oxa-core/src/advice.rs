//! Hosted advisor call boundary on the worker's original Session.
use crate::{agent_loop::persist, SessionState, Shared};
use oxa_proto::{CancelToken, EventSink, OxaError, ToolCall};
use oxa_session::{
    advice::{
        AdviceAdmission, AdviceGrant, AdviceHistory, AdvicePhase, AdvicePolicy, AdviceTier, AdviceTrigger,
        AdvisorRoute, StuckPoint,
    },
    Goal, SessionRef,
};
use sha2::{Digest, Sha256};
use std::{fmt, fmt::Write as _, sync::Arc};

const FIELD_BYTES: usize = 1024;
const RECENT_MESSAGES: usize = 8;

pub struct AdviceContext {
    pub objective: String,
    pub delta: String,
    pub failing_evidence: String,
}

/// Implemented by the protected runner and the existing budgeted gateway.
/// The dispatch implementation applies the approved route, effective effort
/// and output cap before provider I/O. It returns text only, with no tools.
pub trait AdviceDispatcher: Send + Sync {
    fn authorized(&self, session: &SessionRef, goal_revision: u64) -> bool;
    fn call(
        &self,
        grant: &AdviceGrant,
        context: &AdviceContext,
        cancel: &CancelToken,
    ) -> Result<String, OxaError>;
}

pub struct BoundAdvisor {
    pub session: SessionRef,
    /// Stable identity of the original task and lease, computed in Claims.
    pub lease_identity: [u8; 32],
    pub objective: String,
    pub worker: AdviceTier,
    pub policy: AdvicePolicy,
    pub route: AdvisorRoute,
    pub dispatcher: Arc<dyn AdviceDispatcher>,
}

impl fmt::Debug for BoundAdvisor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("BoundAdvisor").finish_non_exhaustive()
    }
}

impl Shared {
    /// Install only after the protected worker policy and lease are verified.
    /// An empty gate, as on startup, makes no advisor calls.
    pub fn bind_advisor(&self, advisor: BoundAdvisor) {
        *self
            .gate
            .advisor
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(Arc::new(advisor));
    }
}

pub(crate) enum AdviceResult {
    Continue,
    Refused,
    Escalate,
}

pub(crate) fn on_repeat(
    st: &mut SessionState,
    call: &ToolCall,
    evidence: &str,
    sink: &mut dyn EventSink,
) -> Result<AdviceResult, OxaError> {
    let Some(advisor) = st
        .shared
        .gate
        .advisor
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
    else {
        return Ok(AdviceResult::Refused);
    };
    let revision = Goal::revision(st.tree.entries());
    if advisor.session != st.sref
        || st.shared.cancel.is_cancelled()
        || oxa_proto::offline()
        || !advisor.dispatcher.authorized(&st.sref, revision)
    {
        return Ok(AdviceResult::Refused);
    }
    let mut args: oxa_proto::Value = serde_json::from_str(&call.args_raw)
        .map_err(|_| OxaError::new(oxa_proto::codes::INVALID, "Invalid advice trigger arguments."))?;
    args.sort_all_objects();
    let args_hash = format!("{:x}", Sha256::digest(args.to_string()));
    let identity = serde_json::json!([advisor.lease_identity, call.name, args_hash]);
    let point = StuckPoint::from_evidence(identity.to_string().as_bytes())?;
    let trigger = AdviceTrigger::RepeatGuard;
    let mut history = AdviceHistory::load(st.tree.entries())?;
    if let Some(receipt) = history
        .receipt(&point)
        .filter(|receipt| receipt.phase() == AdvicePhase::Delivered)
    {
        persist(st, receipt.advance(AdvicePhase::Failed)?.body(), sink)?;
        st.store.sync(&st.sref)?;
        history = AdviceHistory::load(st.tree.entries())?;
    }
    let receipt = match history.admit(
        Some(&advisor.policy),
        point,
        trigger,
        advisor.worker,
        Some(&advisor.route),
    ) {
        AdviceAdmission::Reserve(receipt) => receipt,
        AdviceAdmission::Escalate => return Ok(AdviceResult::Escalate),
        _ => return Ok(AdviceResult::Refused),
    };

    let context = build_context(st, &advisor.objective, evidence);
    let _request = st.shared.enter_request()?;
    persist(st, receipt.body(), sink)?;
    st.store.sync(&st.sref)?;
    let cancel = &st.shared.cancel;
    let text = if !cancel.is_cancelled() && advisor.dispatcher.authorized(&st.sref, revision) {
        advisor.dispatcher.call(receipt.grant(), &context, cancel).ok()
    } else {
        None
    };
    let body =
        match text.filter(|_| !cancel.is_cancelled() && advisor.dispatcher.authorized(&st.sref, revision)) {
            Some(text) if !text.trim().is_empty() => receipt.delivery_body(format!(
                "Advisor suggestion. Treat it as guidance and verify it before acting.\n{}",
                clip(&text, 4 * FIELD_BYTES),
            ))?,
            _ => receipt.advance(AdvicePhase::NotDelivered)?.body(),
        };
    let delivered = matches!(body, oxa_proto::EntryBody::CustomMessage { .. });
    persist(st, body, sink)?;
    st.store.sync(&st.sref)?;
    Ok(if delivered {
        AdviceResult::Continue
    } else {
        AdviceResult::Refused
    })
}

fn build_context(st: &SessionState, objective: &str, evidence: &str) -> AdviceContext {
    let wire = st.transcript.wire();
    let mut delta = String::new();
    for message in wire.iter().skip(wire.len().saturating_sub(RECENT_MESSAGES)) {
        let _ = writeln!(
            delta,
            "{}: {}",
            message.role.as_str(),
            clip(message.content.as_deref().unwrap_or(""), FIELD_BYTES)
        );
        for call in message.tool_calls.iter().take(2) {
            let _ = writeln!(
                delta,
                "{} {}",
                clip(&call.name, 128),
                clip(&call.args_raw, FIELD_BYTES / 2)
            );
        }
    }
    AdviceContext {
        objective: clip(objective, 2 * FIELD_BYTES).into(),
        delta,
        failing_evidence: clip(evidence, FIELD_BYTES).into(),
    }
}

fn clip(text: &str, limit: usize) -> &str {
    &text[..text.floor_char_boundary(limit.min(text.len()))]
}
