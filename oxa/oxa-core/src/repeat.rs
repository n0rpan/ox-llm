//! The repeated-read guard (`oxa.toolRepeatGuard`, default on).
//!
//! A model can fall into a loop where every reply is the same read call and every result is the
//! same text (Block 1, 2026-11-14: 64 identical `read` calls of one 10-line range under greedy
//! decoding, the planted bug on screen in every result, no edit). Each repeat adds the same
//! bytes to the context and the next reply copies the last one again.
//!
//! ## Rule (exact)
//! A call is a repeat when its tool is a read-class tool (`ToolClass::Read`), it succeeded, and
//! an earlier tool result IN THE CURRENT WIRE answers a call with the same tool name, the same
//! arguments (compared as JSON values, so key order does not matter) and byte-identical result
//! text. Earlier results are counted walking back from the newest and the walk stops at:
//! * a user message (a new instruction may legitimately need the same range again);
//! * the result of any call that is NOT read-class, or of an unknown tool (an edit, a write or a
//!   command may have changed what the read returns; this is what keeps a re-read after an edit
//!   from ever being counted). The same holds inside one batch: a call that comes after a
//!   non-read call of its own batch (`[read A, edit A, read A]`) is never judged, since the
//!   wire does not hold that batch's results yet (`exec::run_calls`). This reset is wider than
//!   per file: any edit, write or command resets every read.
//!
//! Compaction needs no rule: a summarised result is no longer on the wire, so it is not counted.
//!
//! With [`LIMIT`] identical results already on the wire the new result is replaced by a short
//! notice ([`crate::prompt::repeat_notice`]) that says where the content already is and asks the
//! model to go on. The call still runs (a read is cheap), so a file changed by something outside
//! the session gives a different text and is never replaced. After [`NOTICES_BEFORE_STOP`]
//! notices for the same call, the next repeat ends the run with a shown stop message.
//! Nothing already sent is rewritten (W1): the notice is the tool result of the new call.
use crate::SessionState;
use oxa_proto::{Role, ToolClass, Value, WireMsg};

/// The setting that turns the guard off (default on).
pub const SETTING: &str = "oxa.toolRepeatGuard";
/// Identical results already on the wire before a repeat is answered with the notice.
pub const LIMIT: u32 = 3;
/// Notices for the same call before the next repeat stops the run.
pub const NOTICES_BEFORE_STOP: u32 = 2;

/// What to do with one finished call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Not a repeat (or below the limit): the result goes out unchanged.
    Pass,
    /// Replace the result with this notice text.
    Notice(String),
    /// Replace the result with this notice text and end the run after the batch.
    Stop(String),
}

/// Judge one successful call `name(args)` whose shaped result is `text`.
pub(crate) fn judge(st: &SessionState, name: &str, args_raw: &str, text: &str) -> Verdict {
    if !st.settings.bool(SETTING, true) || class_of(st, name) != Some(ToolClass::Read) {
        return Verdict::Pass;
    }
    let Some(args) = parse(args_raw) else {
        return Verdict::Pass;
    };
    let wire = st.transcript.wire();
    let found = scan(wire, &|n| class_of(st, n), name, &args, text);
    if found.identical < LIMIT {
        return Verdict::Pass;
    }
    let notice = crate::prompt::repeat_notice(name, args_raw, found.identical, found.back);
    if found.notices >= NOTICES_BEFORE_STOP {
        Verdict::Stop(notice)
    } else {
        Verdict::Notice(notice)
    }
}

/// The stop text shown to the user when [`Verdict::Stop`] ends a run.
pub fn stop_text(name: &str) -> String {
    oxa_proto::problem_text(
        "Run stopped: the model repeated the same tool call after being told its result is unchanged",
        &format!("`{name}` returned the same content {} times in a row", LIMIT + 1),
        "send a more specific instruction, or switch model with /model",
    )
}

/// What [`scan`] found on the wire for one call.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Found {
    /// Earlier results with the same call and byte-identical text.
    pub identical: u32,
    /// Earlier results of the same call that were already replaced by the notice.
    pub notices: u32,
    /// How many tool results back the newest identical one is (1 = the previous result).
    pub back: u32,
}

/// Walk the wire back from the newest message (rule in the module docs).
pub(crate) fn scan(
    wire: &[WireMsg],
    class: &dyn Fn(&str) -> Option<ToolClass>,
    name: &str,
    args: &Value,
    text: &str,
) -> Found {
    let mut found = Found::default();
    let mut results_back = 0u32;
    for (i, m) in wire.iter().enumerate().rev() {
        match m.role {
            Role::User => break,
            Role::Tool => {
                results_back += 1;
                let Some(call) = m.tool_call_id.as_deref().and_then(|id| find_call(&wire[..i], id)) else {
                    break;
                };
                if class(&call.name) != Some(ToolClass::Read) {
                    break;
                }
                if call.name != name || parse(&call.args_raw).as_ref() != Some(args) {
                    continue;
                }
                let content = m.content.as_deref().unwrap_or("");
                if content == text {
                    found.identical += 1;
                    if found.back == 0 {
                        found.back = results_back;
                    }
                } else if content.starts_with(crate::prompt::REPEAT_NOTICE_HEAD) {
                    found.notices += 1;
                }
            }
            _ => {}
        }
    }
    found
}

/// The call `id` answers: the newest assistant message before it that carries it.
fn find_call<'a>(before: &'a [WireMsg], id: &str) -> Option<&'a oxa_proto::ToolCall> {
    before
        .iter()
        .rev()
        .filter(|m| m.role == Role::Assistant)
        .find_map(|m| m.tool_calls.iter().find(|c| c.id == id))
}

/// Whether `name` is a read-class tool of the session (an unknown tool is not).
pub(crate) fn is_read(st: &SessionState, name: &str) -> bool {
    class_of(st, name) == Some(ToolClass::Read)
}

fn class_of(st: &SessionState, name: &str) -> Option<ToolClass> {
    st.active
        .get(name)
        .or_else(|| st.active.get_indirect(name))
        .map(|t| t.spec().class)
}

fn parse(raw: &str) -> Option<Value> {
    serde_json::from_str::<Value>(raw).ok().filter(Value::is_object)
}
