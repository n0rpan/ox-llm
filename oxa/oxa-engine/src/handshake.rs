//! Version / capability handshake: what the server advertises against what this oxa knows.
//!
//! No request of its own: it reads the `/_local/status` and `/props` documents that
//! [`crate::info`] already fetched. The result is one warning line (or nothing); it never fails
//! the start.
//!
//! * `protocol_version` (an integer in `/_local/status`, or in `/props` `oxe_hints`): the
//!   version of the harness-facing contract the server speaks. A value above
//!   [`PROTOCOL_UNDERSTOOD`] means it offers features this oxa does not know. A server that
//!   publishes none is treated as version 1.
//! * `capabilities` (the controller's top-level list): when the controller publishes one, it
//!   must contain [`NEEDED_CAPABILITIES`]; a plain engine has no such list and is not checked.
use serde_json::Value;

/// The highest `protocol_version` this oxa understands.
pub const PROTOCOL_UNDERSTOOD: u64 = 1;

/// Capabilities oxa's own use of the controller depends on (the per-model limits and the
/// context modes). Others in the list (`storage`, `harness`, ...) belong to the controller's
/// own panel, so unknown extra entries are never a warning.
pub const NEEDED_CAPABILITIES: [&str; 2] = ["models", "context_modes"];

fn protocol_of(status: Option<&Value>, props: Option<&Value>) -> Option<u64> {
    status
        .and_then(|s| s.get("protocol_version"))
        .or_else(|| props.and_then(|p| p.pointer("/oxe_hints/protocol_version")))
        .and_then(Value::as_u64)
}

/// The warning line for these documents, or `None` when the server and oxa match.
pub fn handshake_warning(status: Option<&Value>, props: Option<&Value>) -> Option<String> {
    let who = if status.is_some_and(|s| s.get("capabilities").is_some()) {
        "oxc"
    } else {
        "the engine"
    };
    if let Some(v) = protocol_of(status, props) {
        if v > PROTOCOL_UNDERSTOOD {
            return Some(format!(
                "{who} offers features this oxa doesn't know (protocol {v}, oxa knows {PROTOCOL_UNDERSTOOD}); update oxa"
            ));
        }
    }
    let caps = status
        .and_then(|s| s.get("capabilities"))
        .and_then(Value::as_array)?;
    let missing: Vec<&str> = NEEDED_CAPABILITIES
        .iter()
        .copied()
        .filter(|n| !caps.iter().any(|c| c.as_str() == Some(n)))
        .collect();
    if missing.is_empty() {
        None
    } else {
        Some(format!(
            "{who} lacks features this oxa expects ({}); update {who}",
            missing.join(", ")
        ))
    }
}
