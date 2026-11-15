//! The repeat guard for identical tool calls (`oxa.toolRepeatGuard`, default on).
//!
//! A model can fall into a loop where every reply is the same read call and every result is the
//! same text (Block 1, 2026-11-14: 64 identical `read` calls of one 10-line range under greedy
//! decoding, the planted bug on screen in every result, no edit). Each repeat adds the same
//! bytes to the context and the next reply copies the last one again.
//!
//! ## Rule (exact)
//! Every tool is covered (read, bash, grep/find/ls, web_search, MCP and extension tools): after
//! Block 1 Arm A the read-only guard moved the loop to an identical `bash` command. A call that
//! actually ran (not refused before running: unknown tool, bad JSON, a hook block, a denial) is
//! a repeat when the results right before it, walking back from the newest, are an unbroken run
//! of the SAME call (same tool name, same arguments compared as JSON values, so key order does
//! not matter) with byte-identical result text (success or error alike: a command that fails
//! the same way every time is still a loop). Only consecutive pairs count; the walk stops at:
//! * a user message (a new instruction may legitimately need the same call again);
//! * any other call's result (an edit, a write, another command, another range): this is what
//!   keeps `cargo test` after an edit, and a read after an edit, from ever being counted;
//! * a result of the same call with different text (a polling command whose output changes, a
//!   test whose output changed): a legitimate re-run.
//!
//! A background job poll (`bash {job}`, #150) follows the same rule: its result text names the
//! job's state and the bytes it has written, so a poll that saw progress is a different result
//! and breaks the run; identical polls of a job with no news still count.
//!
//! Inside one batch the same applies in call order (`exec::run_calls`): the batch's earlier
//! results are not on the wire yet, so `[read A, edit A, read A]` breaks the run at the edit,
//! and earlier identical calls of the batch extend it.
//!
//! Compaction needs no rule: a summarised result is no longer on the wire, so it is not counted.
//!
//! With [`LIMIT`] identical consecutive results the new result is replaced by a short notice
//! ([`crate::prompt::repeat_notice`]) that says the content is unchanged and already in context
//! and asks the model to change approach. The call still runs, so a changed outcome is never
//! hidden. After [`NOTICES_BEFORE_STOP`] notices in the same run, the next repeat ends the run
//! with a shown stop message. Nothing already sent is rewritten (W1): the notice is the tool
//! result of the new call.
use crate::SessionState;
use oxa_proto::{Role, Value, WireMsg};

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

/// Judge one call `name(args)` that ran and whose shaped result is `text`. `in_batch` = the
/// earlier calls of the same batch with their final result texts, in call order.
pub(crate) fn judge(
    st: &SessionState,
    name: &str,
    args_raw: &str,
    text: &str,
    in_batch: &[(&str, &str, &str)],
) -> Verdict {
    if !st.settings.bool(SETTING, true) {
        return Verdict::Pass;
    }
    let Some(args) = parse(args_raw) else {
        return Verdict::Pass;
    };
    // #439: a blocking poll (`wait_s`) of a job that is still running is not a repeat: it waited
    // for the job to change. Judged from the status header (first line) only, never from the
    // job's output, and a poll without a real wait (under 1 s) still trips the guard (#150).
    if name == "bash"
        && args.get("job").is_some()
        && args
            .get("wait_s")
            .and_then(|w| w.as_f64())
            .is_some_and(|w| w >= 1.0)
        && text
            .lines()
            .next()
            .is_some_and(|h| h.starts_with("Job ") && h.contains(": running ("))
    {
        return Verdict::Pass;
    }
    // The batch first (newest), then the wire, both walked back from the newest.
    let mut found = Found::default();
    for (n, a, t) in in_batch.iter().rev() {
        if !same_call(n, a, name, &args) {
            // A different call ends the run; the identical suffix found so far still counts.
            return finish(found, name, args_raw);
        }
        match step(&mut found, t, text) {
            Step::Go => {}
            Step::Break => return finish(found, name, args_raw),
        }
    }
    scan(st.transcript.wire(), name, &args, text, &mut found);
    finish(found, name, args_raw)
}

fn finish(found: Found, name: &str, args_raw: &str) -> Verdict {
    if found.identical < LIMIT {
        return Verdict::Pass;
    }
    let notice = crate::prompt::repeat_notice(name, args_raw, found.identical);
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
        &format!("`{name}` kept returning the same result ({LIMIT} times in a row, then {NOTICES_BEFORE_STOP} notices)"),
        "send a more specific instruction, or switch model with /model",
    )
}

/// What the walk found for one call.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Found {
    /// Consecutive earlier results of the same call with byte-identical text.
    pub identical: u32,
    /// Earlier results of the same call in that run that were already replaced by the notice.
    pub notices: u32,
}

enum Step {
    Go,
    Break,
}

/// One earlier result `content` of the same call.
fn step(found: &mut Found, content: &str, text: &str) -> Step {
    if content == text {
        found.identical += 1;
        Step::Go
    } else if content.starts_with(crate::prompt::REPEAT_NOTICE_HEAD) {
        found.notices += 1;
        Step::Go
    } else {
        Step::Break
    }
}

fn same_call(n: &str, a: &str, name: &str, args: &Value) -> bool {
    n == name && parse(a).as_ref() == Some(args)
}

/// Walk the wire back from the newest message (rule in the module docs).
pub(crate) fn scan(wire: &[WireMsg], name: &str, args: &Value, text: &str, found: &mut Found) {
    for (i, m) in wire.iter().enumerate().rev() {
        match m.role {
            Role::User => return,
            Role::Tool => {
                let Some(call) = m.tool_call_id.as_deref().and_then(|id| find_call(&wire[..i], id)) else {
                    return;
                };
                if !same_call(&call.name, &call.args_raw, name, args) {
                    return;
                }
                if let Step::Break = step(found, m.content.as_deref().unwrap_or(""), text) {
                    return;
                }
            }
            _ => {}
        }
    }
}

/// The call `id` answers: the newest assistant message before it that carries it.
fn find_call<'a>(before: &'a [WireMsg], id: &str) -> Option<&'a oxa_proto::ToolCall> {
    before
        .iter()
        .rev()
        .filter(|m| m.role == Role::Assistant)
        .find_map(|m| m.tool_calls.iter().find(|c| c.id == id))
}

fn parse(raw: &str) -> Option<Value> {
    serde_json::from_str::<Value>(raw).ok().filter(Value::is_object)
}

#[cfg(test)]
mod job_poll_tests {
    //! #150 (1): a job poll is a repeat only while its result text stays the same. The text
    //! carries the job's state and how much output it has written, so a poll that saw any
    //! progress differs from the one before; identical polls of a stuck job still count.
    use super::*;
    use oxa_proto::{ToolCall, WireMsg};

    fn poll_call(i: usize) -> WireMsg {
        WireMsg {
            role: Role::Assistant,
            content: None,
            reasoning: None,
            tool_calls: vec![ToolCall {
                id: format!("p{i}"),
                name: "bash".to_string(),
                args_raw: r#"{"job":"job-1"}"#.to_string(),
            }],
            tool_call_id: None,
        }
    }

    /// The wire of `texts` polls (each answered), oldest first.
    fn polls(texts: &[&str]) -> Vec<WireMsg> {
        let mut wire = Vec::new();
        for (i, t) in texts.iter().enumerate() {
            wire.push(poll_call(i));
            wire.push(WireMsg::tool_result(format!("p{i}"), *t));
        }
        wire
    }

    fn count_for(wire: &[WireMsg], newest: &str) -> Found {
        let mut found = Found::default();
        let args = parse(r#"{"job":"job-1"}"#).unwrap();
        scan(wire, "bash", &args, newest, &mut found);
        found
    }

    #[test]
    fn progress_between_polls_is_never_a_repeat() {
        let a = "Job job-1: running (10 bytes of output)\nStill running.\n\nx";
        let b = "Job job-1: running (20 bytes of output)\nStill running.\n\nx";
        let c = "Job job-1: running (30 bytes of output)\nStill running.\n\nx";
        // The newest poll saw progress since the one before it: nothing counted.
        assert_eq!(count_for(&polls(&[a, b]), c).identical, 0);
        // Even a poll identical to an older one does not count past a changed poll.
        assert_eq!(count_for(&polls(&[a, a, b, b]), c).identical, 0);
        let pass = finish(count_for(&polls(&[a, b, b]), c), "bash", r#"{"job":"job-1"}"#);
        assert_eq!(pass, Verdict::Pass);
    }

    #[test]
    fn identical_unchanged_polls_still_trip_the_guard() {
        let same = "Job job-1: running (0 bytes of output)\nStill running.";
        let found = count_for(&polls(&[same, same, same]), same);
        assert_eq!(found.identical, LIMIT);
        assert!(matches!(
            finish(found, "bash", r#"{"job":"job-1"}"#),
            Verdict::Notice(_)
        ));
    }
}
