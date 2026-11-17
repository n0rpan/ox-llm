//! The owner-side handoff for a trusted frontend carrying a bounded control request.

use oxa_core::{goal::GoalControl, Session};
use oxa_orchestrator::control::Request;
use oxa_proto::EventSink;
use oxa_session::SessionRef;
use serde_json::{json, Value};

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
