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
//! * any other call's result, except another read/grep/find/ls result when the call itself is one
//!   of those (#463: the unchanged range is still in context), and an `edit` or `write` that
//!   failed (#507: it changed nothing, so `read, edit (fails), read, edit (fails), ...` is a loop);
//!   a successful edit or write or a command ends the run: this is what keeps `cargo test` after
//!   an edit, and a read after an edit, from ever being counted;
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
//! hidden. The change-of-approach suggestion is written once, on the first notice of the loop;
//! every later notice is short, carries the attempt count (the identical results and notices of
//! this loop so far, plus this call) and points back to that suggestion, so a loop costs a new
//! number rather than the same paragraph per try. A healthy turn never reaches the notice and is
//! unchanged. After [`NOTICES_BEFORE_STOP`] notices in the same run the guard no longer ends the
//! run (#1448): the user is warned once — shown and persisted with the remedy ([`warning_text`])
//! — and the run continues, so a later reply can still finish the task. The advisor can still end
//! it on escalation (`agent_loop.rs`); `oxa.maxTurns` and cancellation bound a loop that never
//! recovers. Further repeats keep getting the notice. Nothing already sent is rewritten (W1):
//! the notice is the tool result of the new call.
use crate::SessionState;
use oxa_proto::{Role, Value, WireImage, WireMsg};
use std::hash::{DefaultHasher, Hash, Hasher};

/// The setting that turns the guard off (default on).
pub const SETTING: &str = "oxa.toolRepeatGuard";
/// The setting that caps the model replies of one run (default unset: no limit).
pub const MAX_TURNS: &str = "oxa.maxTurns";
/// Identical results already on the wire before a repeat is answered with the notice.
pub const LIMIT: u32 = 3;
/// Notices for the same call before the next repeat reaches the guard's give-up point: the
/// user is warned once and the run continues (#1448; it no longer ends the run).
pub const NOTICES_BEFORE_STOP: u32 = 2;

/// What to do with one finished call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Not a repeat (or below the limit): the result goes out unchanged.
    Pass,
    /// Replace the result with this notice text.
    Notice(String),
    /// Replace the result with this notice text and mark the give-up point: the loop warns the
    /// user once and continues (#1448).
    Warn(String),
}

/// Judge one call `name(args)` that ran and whose shaped result is `text`. `in_batch` = the
/// earlier calls of the same batch with their final result texts and error flags, in call order.
pub(crate) fn judge(
    st: &SessionState,
    name: &str,
    args_raw: &str,
    result: &str,
    in_batch: &[(&str, &str, &str, bool)],
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
        && result
            .lines()
            .next()
            .is_some_and(|h| h.starts_with("Job ") && h.contains(": running ("))
    {
        return Verdict::Pass;
    }
    // The batch first (newest), then the wire, both walked back from the newest.
    let mut found = Found::default();
    for (n, a, t, failed) in in_batch.iter().rev() {
        if !same_call(n, a, name, &args) {
            // Match the wire walk: failed writers only skip over a different call's run;
            // identical failures and their notices must still count as repeats themselves.
            if (READ_ONLY.contains(n) && READ_ONLY.contains(&name)) || (WRITERS.contains(n) && *failed) {
                continue;
            }
            return finish(found, name, args_raw);
        }
        match step(&mut found, t, result) {
            Step::Go => {}
            Step::Break => return finish(found, name, args_raw),
        }
    }
    let failed = |i: usize| result_failed(st, i);
    scan(st.transcript.wire(), &failed, name, &args, result, &mut found);
    finish(found, name, args_raw)
}

fn finish(found: Found, name: &str, args_raw: &str) -> Verdict {
    if found.identical < LIMIT {
        return Verdict::Pass;
    }
    // The attempt counts every call of this loop so far: the identical results and the notices
    // before this one, plus this one (#2298).
    let attempt = found.identical + found.notices + 1;
    let notice = crate::prompt::repeat_notice(name, args_raw, found.identical, attempt);
    if found.notices >= NOTICES_BEFORE_STOP {
        Verdict::Warn(notice)
    } else {
        Verdict::Notice(notice)
    }
}

/// The warning shown and persisted (once per run) when [`Verdict::Warn`] fires. The run
/// continues (#1448), so it says what happened, that it goes on, and the remedy; it names the
/// tool and quotes nothing else.
pub fn warning_text(name: &str) -> String {
    oxa_proto::problem_text(
        "The model repeated the same tool call after being told its result is unchanged",
        &format!(
            "`{name}` kept returning the same result ({LIMIT} times in a row, then \
{NOTICES_BEFORE_STOP} notices); the run continues with the notice in its place"
        ),
        "send a more specific instruction, or switch model with /model",
    )
}

/// The error that ends a run at `oxa.maxTurns` replies (#507).
pub fn turn_limit_error(max: u32) -> oxa_proto::OxaError {
    oxa_proto::problem(
        oxa_proto::codes::TURN_LIMIT,
        "Run stopped",
        &format!("it reached {MAX_TURNS} ({max} model replies)"),
        &format!("send a more specific instruction, or raise or unset {MAX_TURNS}"),
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

/// Tools that never change files: they do not end a run of identical results.
const READ_ONLY: [&str; 4] = ["read", "grep", "find", "ls"];

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

/// Tools that change files only when they succeed (#507).
pub(crate) const WRITERS: [&str; 2] = ["edit", "write"];

/// Whether the tool result at wire index `i` was stored as an error (`ToolMeta::is_error`).
fn result_failed(st: &SessionState, i: usize) -> bool {
    st.transcript
        .origin(i)
        .and_then(|id| st.tree.get(id))
        .is_some_and(|e| matches!(&e.body, oxa_proto::EntryBody::Msg { tool: Some(t), .. } if t.is_error))
}

/// Walk the wire back from the newest message (rule in the module docs). `failed(i)` tells
/// whether the tool result at wire index `i` is an error.
pub(crate) fn scan(
    wire: &[WireMsg],
    failed: &dyn Fn(usize) -> bool,
    name: &str,
    args: &Value,
    text: &str,
    found: &mut Found,
) {
    for (i, m) in wire.iter().enumerate().rev() {
        match m.role {
            Role::User => return,
            Role::Tool => {
                let Some(call) = m.tool_call_id.as_deref().and_then(|id| find_call(&wire[..i], id)) else {
                    return;
                };
                if !same_call(&call.name, &call.args_raw, name, args) {
                    // Another read-only call (a different range, a search) leaves the earlier
                    // copies in context and unchanged; anything else may change files (#463).
                    if READ_ONLY.contains(&call.name.as_str()) && READ_ONLY.contains(&name) {
                        continue;
                    }
                    // A failed edit or write changed nothing (#507).
                    if WRITERS.contains(&call.name.as_str()) && failed(i) {
                        continue;
                    }
                    return;
                }
                let earlier = result_key(m.content.as_deref().unwrap_or(""), &m.images);
                if let Step::Break = step(found, &earlier, text) {
                    return;
                }
            }
            _ => {}
        }
    }
}

/// Stable, bounded identity for repeat detection. Image bytes must participate, but copying their
/// base64 into the batch comparison would double the largest allocation in a tool result.
pub(crate) fn result_key(text: &str, images: &[WireImage]) -> String {
    if images.is_empty() {
        return text.to_string();
    }
    let mut hash = DefaultHasher::new();
    for image in images {
        image.mime.hash(&mut hash);
        image.base64.hash(&mut hash);
    }
    format!("{text}\n[image-content:{:016x}]", hash.finish())
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
            images: Vec::new(),
            role: Role::Assistant,
            content: None,
            reasoning: None,
            tool_calls: vec![ToolCall {
                id: format!("p{i}"),
                name: "bash".to_string(),
                args_raw: r#"{"job":"job-1"}"#.to_string(),
            }],
            provider_history: None,
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
        scan(wire, &|_| false, "bash", &args, newest, &mut found);
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

    #[test]
    fn image_bytes_are_part_of_the_result_identity() {
        let image = |base64: &str| WireImage {
            mime: "image/png".to_string(),
            base64: base64.to_string(),
        };
        assert_ne!(
            result_key("same dimensions", &[image("AAAA")]),
            result_key("same dimensions", &[image("AAAB")])
        );
        assert_eq!(result_key("plain", &[]), "plain");
    }
}
