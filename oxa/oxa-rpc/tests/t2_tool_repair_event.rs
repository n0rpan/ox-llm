//! T2 (wave H lane `local`): the `tool_repair` event record of the JSON / RPC modes.

use oxa_proto::{repair_cause, AgentEvent};
use oxa_rpc::event_to_json;
use serde_json::json;

#[test]
fn tool_repair_record() {
    assert_eq!(
        event_to_json(&AgentEvent::ToolRepair {
            attempt: 2,
            cause: repair_cause::UNKNOWN_PARAMETER,
            total: 3,
            stopped: true,
        }),
        json!({"type": "tool_repair", "attempt": 2, "cause": "unknown_parameter", "total": 3, "stopped": true})
    );
}
