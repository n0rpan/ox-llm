//! R1 (wave H lane `local`): resume after a crash mid-tool, and per-turn durability.
//!
//! **Interrupted calls.** A session file whose wire ends in an assistant message with tool
//! calls followed only by results of some of those calls (oxa stopped while a tool ran) would
//! send a malformed request: every call needs its result. On resume, one fixed result
//! ([`crate::prompt::TOOL_INTERRUPTED`]) is appended per unanswered call, in call order, after
//! the results that exist. Pure appends (W1); nothing on disk is rewritten. When anything else
//! follows the assistant message (a user message, a compaction), nothing is appended: the wire
//! cannot be repaired by appending and the next request reports the engine's own error.
//!
//! **Durability.** [`sync_turn`] `fdatasync`s the session file at the end of each turn; a
//! power loss then costs at most the turn in progress (a torn last line is dropped on load).
use crate::agent_loop::persist;
use crate::SessionState;
use oxa_proto::{AgentEvent, EntryBody, EventSink, Level, OxaError, Role, WireMsg};

/// Append the interrupted results (see the module docs). Returns how many were appended.
pub(crate) fn close_interrupted(st: &mut SessionState, sink: &mut dyn EventSink) -> Result<u32, OxaError> {
    let missing = unanswered(st.transcript.wire());
    for id in &missing {
        persist(
            st,
            EntryBody::Msg {
                wire: WireMsg::tool_result(id.clone(), crate::prompt::TOOL_INTERRUPTED),
                parts: Vec::new(),
            },
            sink,
        )?;
    }
    let n = missing.len() as u32;
    st.stats.tool_results += n;
    Ok(n)
}

/// The ids of the calls of the last assistant message that have no result, when only results
/// of that message follow it on the wire; empty otherwise.
pub(crate) fn unanswered(wire: &[WireMsg]) -> Vec<String> {
    let Some(at) = wire.iter().rposition(|m| m.role == Role::Assistant) else {
        return Vec::new();
    };
    let calls = &wire[at].tool_calls;
    if calls.is_empty() {
        return Vec::new();
    }
    let after = &wire[at + 1..];
    let mut answered: Vec<&str> = Vec::new();
    for m in after {
        match (m.role, m.tool_call_id.as_deref()) {
            (Role::Tool, Some(id)) if calls.iter().any(|c| c.id == id) => answered.push(id),
            _ => return Vec::new(),
        }
    }
    calls
        .iter()
        .filter(|c| !answered.contains(&c.id.as_str()))
        .map(|c| c.id.clone())
        .collect()
}

/// `fdatasync` the session file at the end of a turn. A failure is a warning, once per turn:
/// the entries are written; only their durability against a power loss is in doubt.
pub(crate) fn sync_turn(st: &SessionState, sink: &mut dyn EventSink) {
    if let Err(e) = st.store.sync(&st.sref) {
        sink.emit(&AgentEvent::Notice {
            level: Level::Warning,
            text: e.to_string(),
        });
    }
}
