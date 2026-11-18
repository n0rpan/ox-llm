//! Recovery and writes for lead continuation, on the existing Session owner.
use super::{stale_steering, SteeringAuthority, SteeringBinding, SteeringLease};
use crate::{Session, SessionState};
use oxa_proto::{codes, AgentEvent, EntryBody, EventSink, OxaError, Part, WireMsg};
use oxa_session::{Goal, GoalStatus, LeadGoalState, PendingGoalSteer, SessionRef};
use std::sync::{atomic::Ordering, Arc};

pub(crate) struct BoundLeadAuthority {
    pub(crate) session: SessionRef,
    pub(crate) authority: Arc<SteeringAuthority>,
}

impl std::fmt::Debug for BoundLeadAuthority {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BoundLeadAuthority")
            .finish_non_exhaustive()
    }
}

/// Explicitly resuming the exact paused or blocked goal preserves a steer
/// admitted at its current, still-valid revision.
pub(crate) fn reanchor_resumed(st: &SessionState, next: &mut Goal) {
    let Some(mut previous) = st.goal.as_ref().map(|goal| goal.snapshot()) else {
        return;
    };
    if !matches!(previous.status, GoalStatus::Paused | GoalStatus::Blocked)
        || previous.resume().is_err()
        || previous != *next
    {
        return;
    }
    if let Some(pending) = next.lead.as_mut().and_then(|lead| lead.pending_steer.as_mut()) {
        if Goal::allows_steering(st.tree.entries(), pending.revision) {
            pending.revision = st.tree.entries().len() as u64 + 1;
        }
    }
}

impl Session {
    /// Save verified facts from the lead without replacing an already admitted steer.
    pub fn update_lead_goal(
        &mut self,
        session: &SessionRef,
        revision: u64,
        mut lead: LeadGoalState,
        sink: &mut dyn EventSink,
    ) -> Result<u64, OxaError> {
        let mut goal = idle_goal(self, session, revision)?;
        if lead.pending_steer.is_some() {
            return Err(OxaError::new(
                codes::INVALID,
                "Use the steering admission boundary.",
            ));
        }
        lead.pending_steer = goal.lead.take().and_then(|saved| saved.pending_steer);
        goal.lead = Some(lead);
        save(&mut self.st, goal, sink)
    }

    /// Durably admit confirmed input for the next owner turn. The caller authenticates
    /// the request; current assignment is checked again when the entry is appended.
    pub fn stage_lead_steer(
        &mut self,
        session: &SessionRef,
        revision: u64,
        text: String,
        lease: Option<SteeringLease>,
        sink: &mut dyn EventSink,
    ) -> Result<u64, OxaError> {
        let mut goal = idle_goal(self, session, revision)?;
        if !Goal::allows_steering(self.st.tree.entries(), revision) {
            return Err(stale_steering());
        }
        let lead = goal.lead.get_or_insert_with(LeadGoalState::default);
        if lead.pending_steer.is_some() {
            return Err(OxaError::new(codes::BUSY, "A durable steer is already pending."));
        }
        lead.pending_steer = Some(PendingGoalSteer {
            entry_id: oxa_session::new_entry_id(&|id| self.st.tree.get(id).is_some()),
            revision,
            text,
            lease,
        });
        save(&mut self.st, goal, sink)
    }
}

fn idle_goal(session: &Session, bound: &SessionRef, revision: u64) -> Result<Goal, OxaError> {
    if session.st.shared.busy.load(Ordering::SeqCst) {
        return Err(OxaError::new(
            codes::BUSY,
            "Cannot checkpoint lead state during a run.",
        ));
    }
    if &session.st.sref != bound || session.goal_revision() != revision {
        return Err(OxaError::new(
            "goal_revision_conflict",
            "The session or goal revision changed.",
        ));
    }
    session
        .goal()
        .ok_or_else(|| OxaError::new(codes::NOT_FOUND, "The session has no goal."))
}

fn save(st: &mut SessionState, goal: Goal, sink: &mut dyn EventSink) -> Result<u64, OxaError> {
    goal.validate()?;
    let runtime = st
        .goal
        .clone()
        .ok_or_else(|| OxaError::new(codes::NOT_FOUND, "The session has no goal."))?;
    let body = goal.checkpoint_body(st.tree.entries());
    let entry = crate::agent_loop::append_entry(st, body)?;
    runtime.replace_snapshot(goal);
    st.store.sync(&st.sref)?;
    sink.emit(&AgentEvent::EntryAppended { entry });
    Ok(Goal::revision(st.tree.entries()))
}

/// Called before the continuation prompt. A crash after the message append
/// but before the goal update leaves the saved user entry as the witness.
pub(crate) fn deliver_saved(st: &mut SessionState, sink: &mut dyn EventSink) -> Result<(), OxaError> {
    let Some(mut goal) = st.goal.as_ref().map(|runtime| runtime.snapshot()) else {
        return Ok(());
    };
    if matches!(goal.status, GoalStatus::Paused | GoalStatus::Blocked) {
        return Ok(());
    }
    let Some(pending) = goal.lead.as_mut().and_then(|lead| lead.pending_steer.take()) else {
        return Ok(());
    };
    let wire = WireMsg::user(&pending.text);
    if let Some(existing) = st.tree.get(&pending.entry_id) {
        if !matches!(&existing.body, EntryBody::Msg { wire: saved, .. } if saved == &wire) {
            return Err(stale_steering());
        }
        // Also covers retry after an append succeeded but the first sync failed.
        st.store.sync(&st.sref)?;
        return save(st, goal, sink).map(|_| ());
    }

    let binding = SteeringBinding {
        session: st.sref.clone(),
        revision: pending.revision,
        lease: pending.lease,
    };
    let authority = st
        .shared
        .gate
        .lead_steering_authority
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .as_ref()
        .filter(|bound| bound.session == st.sref)
        .map(|bound| bound.authority.clone());
    if authority.is_none() && binding.lease.is_some() {
        return Err(stale_steering());
    }
    let mut result = Err(stale_steering());
    let mut body = Some(EntryBody::Msg {
        wire,
        parts: vec![Part::Text(pending.text)],
        tool: None,
    });
    let mut append = |lease: Option<&SteeringLease>| {
        if let Some(body) = body.take() {
            result = binding.validate(st, lease).and_then(|()| {
                let entry = crate::agent_loop::append_entry_with_id(st, pending.entry_id.clone(), body)?;
                st.store.sync(&st.sref)?;
                Ok(entry)
            });
        }
    };
    match authority {
        Some(authority) => authority(&binding.session, &mut append),
        None => append(None),
    }
    match result {
        Ok(entry) => {
            sink.emit(&AgentEvent::EntryAppended { entry });
            save(st, goal, sink)?;
            Ok(())
        }
        Err(error) if error.code == "goal_steer_stale" => {
            save(st, goal, sink)?;
            Err(error)
        }
        Err(error) => Err(error),
    }
}
