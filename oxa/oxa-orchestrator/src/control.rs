//! Bounded control admission extracted from #129 f89ba2e control.rs.
//! The trusted frontend owns the queue and authenticates the caller. Admission
//! grants no authority, acknowledges no execution, and persists nothing. The
//! receiving session owner must check authority and the supplied revision before
//! acting; this module deliberately has no goal/task state or budget writer.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::mpsc::{SyncSender, TrySendError};

const REQUEST_BYTES: usize = 32 * 1024;

/// The sender's expected assignment. Only a trusted frontend can verify it.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SteeringLease {
    pub holder: String,
    pub generation: (u64, u32),
}

/// The steering/lifecycle subset of the retained protocol. Read-only follow uses
/// the separate history API; goal replacement awaits the current owner's API.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "method", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Pause {
        id: String,
        revision: u64,
    },
    Resume {
        id: String,
        revision: u64,
    },
    Steer {
        id: String,
        revision: u64,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lease: Option<SteeringLease>,
    },
    Cancel {
        id: String,
        revision: u64,
    },
}

impl Request {
    pub fn id(&self) -> &str {
        match self {
            Self::Pause { id, .. }
            | Self::Resume { id, .. }
            | Self::Steer { id, .. }
            | Self::Cancel { id, .. } => id,
        }
    }
}

/// Admit one request without blocking the owner. Even `queued:true` means only
/// in-memory queue admission, not durable receipt or successful revision check.
/// Raw parser/queue errors and rejected text are never echoed into the response.
pub fn admit(bytes: &[u8], controls: &SyncSender<Request>) -> Value {
    let request = match decode(bytes) {
        Ok(request) => request,
        Err(receipt) => return receipt,
    };
    let id = request.id().to_owned();
    match controls.try_send(request) {
        Ok(()) => json!({"id":id,"queued":true}),
        Err(TrySendError::Full(_)) => json!({"id":id,"error":"control_queue_full"}),
        Err(TrySendError::Disconnected(_)) => json!({"id":id,"error":"session_stopped"}),
    }
}

/// Validate a packet before offering it to an existing owner queue.
pub fn decode(bytes: &[u8]) -> Result<Request, Value> {
    if bytes.len() > REQUEST_BYTES {
        return Err(json!({"error":"request_too_large"}));
    }
    let request: Request = match serde_json::from_slice(bytes) {
        Ok(request) => request,
        Err(_) => return Err(json!({"error":"invalid_request"})),
    };
    let id = request.id();
    if id.is_empty() || id.len() > 128 || id.chars().any(char::is_control) {
        return Err(json!({"error":"invalid_id"}));
    }
    if let Request::Steer { text, lease, .. } = &request {
        if text.is_empty() || text.len() > 16 * 1024 || text.contains('\0') {
            return Err(json!({"id":id,"error":"invalid_steer"}));
        }
        if lease.as_ref().is_some_and(|lease| {
            lease.holder.is_empty()
                || lease.holder.len() > 128
                || lease.holder.chars().any(char::is_control)
                || lease.generation.1 == 0
        }) {
            return Err(json!({"id":id,"error":"invalid_steer_lease"}));
        }
    }
    Ok(request)
}
