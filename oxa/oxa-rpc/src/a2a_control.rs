//! The owner-side handoff for a trusted frontend carrying a bounded control request.

use oxa_core::{goal::GoalControl, Session};
use oxa_core::{
    goal::{GoalSteering, SteeringAuthority, SteeringBinding, SteeringLease},
    Handle,
};
use oxa_orchestrator::control::Request;
use oxa_proto::EventSink;
use oxa_session::SessionRef;
use serde_json::{json, Value};
use std::sync::Arc;

pub(super) fn write_control_receipt(out: &crate::Out, data: Value) {
    crate::server::write_out(out, &json!({"type": "goal_control_receipt", "data": data}));
}

/// Admit steering to the session's single owner queue, leaving other controls with the caller.
pub(super) fn queue_steering(
    handle: &Handle,
    session: &SessionRef,
    request: Request,
    authority: &Arc<SteeringAuthority>,
    receipt: impl FnOnce(Value) + Send + 'static,
) -> Result<Value, Request> {
    let Request::Steer {
        id,
        revision,
        text,
        lease,
    } = request
    else {
        return Err(request);
    };
    let lease = lease.map(|lease| SteeringLease {
        holder: lease.holder,
        generation: lease.generation,
    });
    let mut matches = false;
    authority(session, &mut |current| matches = current == lease.as_ref());
    if !matches {
        return Ok(json!({"id":id,"error":"goal_steer_stale"}));
    }
    let receipt_id = id.clone();
    let request = GoalSteering::new(
        SteeringBinding {
            session: session.clone(),
            revision,
            lease,
        },
        text,
        authority.clone(),
        move |result| {
            receipt(match result {
                Ok(()) => json!({"id":receipt_id,"appended":true}),
                Err(code) => json!({"id":receipt_id,"error":code}),
            });
        },
    );
    Ok(match handle.submit_goal_steering(request) {
        Ok(()) => json!({"id":id,"queued":true}),
        Err(code) => json!({"id":id,"error":code}),
    })
}

/// A request bound to the frontend's authorized session reference at admission.
/// The reference is never taken from the packet or included in a receipt.
pub(super) struct BoundControl {
    session: SessionRef,
    request: Request,
}

impl BoundControl {
    pub(super) fn new(session: SessionRef, request: Request) -> Self {
        Self { session, request }
    }

    pub(super) fn id(&self) -> &str {
        self.request.id()
    }

    /// A resume: once applied on the idle session it starts one continuation run.
    pub(super) fn is_resume(&self) -> bool {
        matches!(self.request, Request::Resume { .. })
    }

    /// Return a receipt from the session owner after attempting the durable transition.
    /// The frontend's earlier queue acknowledgement has its own meaning.
    pub(super) fn apply(self, owner: &mut Session, sink: &mut dyn EventSink) -> Value {
        let id = self.request.id();
        let (revision, control) = match &self.request {
            Request::Pause { revision, .. } => (*revision, GoalControl::Pause),
            Request::Resume { revision, .. } => (*revision, GoalControl::Resume),
            Request::Cancel { revision, .. } => (*revision, GoalControl::Cancel),
            Request::Steer { .. } => return json!({"id": id, "error": "unsupported_control"}),
        };
        match owner.control_goal(&self.session, revision, control, sink) {
            Ok(revision) => json!({"id": id, "applied": true, "revision": revision}),
            Err(error) => json!({"id": id, "error": error.code}),
        }
    }
}
