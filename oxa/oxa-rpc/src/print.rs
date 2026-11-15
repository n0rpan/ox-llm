//! C9: print mode and JSON mode drivers.
// ported from pi packages/coding-agent/src/modes/print-mode.ts
use crate::events::{session_header_record, write_record, JsonSink};
use oxa_core::{RunOutcome, Session};
use oxa_proto::{
    names, shown_why, AgentEvent, EntryBody, EventMessage, EventSink, HeadlessUi, OxaError, Part, Role,
    StopReason,
};
use oxa_session::SessionHeader;
use std::collections::HashMap;
use std::io::Write;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Collects what print mode needs from one run and writes progress as it happens: the text of
/// a message followed by tool calls and one line per tool call go to `err`, the final reply of
/// the last prompt goes to `out`; every write is flushed at once, so a killed run leaves a log.
struct PrintSink<'a> {
    wrap_up: Option<oxa_core::Handle>,
    out: &'a mut dyn Write,
    err: &'a mut dyn Write,
    /// The last assistant message of the current prompt.
    last: Option<EventMessage>,
    /// Only the last prompt's final reply goes to stdout.
    is_last_prompt: bool,
    /// Text of a message without tool calls that may still turn out to be the final reply: a
    /// `done` check or a steering message can start another turn after it.
    pending: Option<String>,
    /// The final reply of the last prompt, set at `AgentEnd`; written to `out` by `run_print`
    /// once it knows the run did not fail or abort.
    final_text: Option<String>,
    notes: Vec<String>,
    started: HashMap<String, Instant>,
    /// The first write error (events cannot fail).
    io_error: Option<OxaError>,
}

impl PrintSink<'_> {
    fn progress(&mut self, line: &str) {
        // Models end progress text with blank lines; one newline is the line terminator.
        let line = line.trim_end();
        let _ = self.err.write_all(format!("{line}\n").as_bytes());
        let _ = self.err.flush();
    }

    /// Another turn follows: the buffered text was not the final reply, so it is progress.
    fn flush_pending(&mut self) {
        if let Some(t) = self.pending.take() {
            self.progress(&t);
        }
    }

    fn write_reply(&mut self, text: &str) {
        let r = self
            .out
            .write_all(text.as_bytes())
            .and_then(|_| self.out.write_all(b"\n"))
            .and_then(|_| self.out.flush());
        if let Err(e) = r {
            if self.io_error.is_none() {
                self.io_error = Some(OxaError::io("cannot write the reply", &e));
            }
        }
    }
}

/// One line for a tool call's start: `[tool] bash: <first 100 chars>`.
fn tool_start_line(name: &str, args: &serde_json::Value) -> String {
    let detail = ["command", "path", "pattern", "file_path"]
        .iter()
        .find_map(|k| args.get(k).and_then(|v| v.as_str()))
        .map(str::to_string)
        .unwrap_or_else(|| {
            if args.is_null() {
                String::new()
            } else {
                args.to_string()
            }
        });
    let one: String = detail
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(100)
        .collect();
    if one.is_empty() {
        format!("[tool] {name}")
    } else {
        format!("[tool] {name}: {one}")
    }
}

impl EventSink for PrintSink<'_> {
    fn emit(&mut self, e: &AgentEvent) {
        match e {
            AgentEvent::MessageEnd { message } if message.msg.role == Role::Assistant => {
                self.flush_pending();
                self.last = Some(message.clone());
                let text = message.msg.content.as_deref().filter(|t| !t.is_empty());
                if !message.msg.tool_calls.is_empty() {
                    if let Some(t) = text {
                        self.progress(t);
                    }
                } else if let Some(t) = text {
                    self.pending = Some(t.to_string());
                }
            }
            AgentEvent::TurnStart => self.flush_pending(),
            AgentEvent::AgentEnd { .. } => {
                if self.is_last_prompt
                    || self
                        .wrap_up
                        .as_ref()
                        .is_some_and(|handle| handle.wrap_up_requested())
                {
                    self.final_text = self.pending.take();
                } else {
                    self.flush_pending();
                }
            }
            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            } => {
                self.flush_pending();
                self.started.insert(tool_call_id.clone(), Instant::now());
                self.progress(&tool_start_line(tool_name, args));
            }
            AgentEvent::ToolExecutionEnd {
                tool_call_id,
                tool_name,
                result,
                duration_ms,
            } => {
                let secs = duration_ms.map(|d| d as f64 / 1000.0).unwrap_or_else(|| {
                    self.started
                        .remove(tool_call_id)
                        .map(|t| t.elapsed().as_secs_f64())
                        .unwrap_or(0.0)
                });
                self.started.remove(tool_call_id);
                let word = if result.is_error { "failed" } else { "ok" };
                self.progress(&format!("[tool] {tool_name} {word} {secs:.1}s"));
            }
            AgentEvent::Notice { text, .. } => self.progress(text),
            AgentEvent::ExtensionError {
                extension_path,
                error,
                ..
            } => {
                self.notes
                    .push(format!("Extension error ({extension_path}): {error}"));
            }
            _ => {}
        }
    }
}

/// `-p`: run each prompt in order (piped stdin text is prepended to the first one by the
/// caller), then write only the final assistant text + `\n` to `out`. Notices and errors go to
/// `err`. Returns the aggregate typed outcome, retaining the first unsuccessful prompt.
pub fn run_print_outcome(
    session: &mut Session,
    prompts: Vec<(String, Vec<Part>)>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<RunOutcome, OxaError> {
    let mut sink = PrintSink {
        wrap_up: Some(session.handle()),
        out,
        err,
        last: None,
        is_last_prompt: false,
        pending: None,
        final_text: None,
        notes: Vec::new(),
        started: HashMap::new(),
        io_error: None,
    };
    let mut ui = HeadlessUi::default();
    let mut failed = false;
    let mut outcome = RunOutcome::NothingToDo;
    let n = prompts.len();
    for (i, (text, parts)) in prompts.into_iter().enumerate() {
        sink.last = None;
        sink.pending = None;
        sink.final_text = None;
        sink.is_last_prompt = i + 1 == n;
        let _ = session.take_outcome();
        let before = session.inner().tree.leaf().to_string();
        let lines = match prompt_or_command(session, text, parts, &mut sink, &mut ui) {
            Ok(Message::Prompt | Message::Extension) => None,
            Ok(Message::Command(lines)) => Some(lines),
            Err(e) => {
                let _ = writeln!(sink.err, "{}", command_error(&e));
                outcome.include(RunOutcome::from_error(&e));
                return Ok(outcome);
            }
        };
        if let Some(e) = sink.io_error.take() {
            return output_failure(outcome, e, sink.err);
        }
        if let Some(mut lines) = lines {
            outcome.include(RunOutcome::Done);
            // A command message: what it shows the user (its lines and its `info`
            // notifications) is the output; warnings and errors still go to stderr.
            if sink.last.is_none() {
                lines.extend(take_info(&mut ui));
            }
            for l in lines {
                if let Err(error) = sink.out.write_all(format!("{l}\n").as_bytes()) {
                    return output_failure(
                        outcome,
                        OxaError::io("cannot write the command output", &error),
                        sink.err,
                    );
                }
            }
        }
        failed = after_run(session, &before, &mut ui, &mut sink.notes, sink.err);
        outcome.include(*session.outcome());
        // A failed or aborted run prints no reply (only known after the run).
        if !failed {
            if let Some(t) = sink.final_text.clone() {
                sink.write_reply(&t);
                if let Some(e) = sink.io_error.take() {
                    return output_failure(outcome, e, sink.err);
                }
            }
        }
        if session.handle().wrap_up_requested() {
            outcome.include(RunOutcome::Interrupted);
            break;
        }
    }

    if done_failed(session, &mut *sink.err) {
        outcome.include(RunOutcome::Failed("done_checks_failed"));
    }
    if !failed {
        // The reply is already printed; a length stop still needs a failed result.
        if let Some(m) = sink.last.clone() {
            if let Some(text) = sink.final_text.take() {
                if session.inner().active.direct.is_empty() && has_tool_markup(&text) {
                    let _ = writeln!(
                    sink.err,
                    "warning: the model answered with tool-call markup but no tools are available (--no-tools); the answer may be unusable"
                );
                }
            }
            if m.stop == Some(StopReason::Length) {
                let _ = writeln!(
                    sink.err,
                    "Response was truncated: the model reached its output token limit."
                );
                outcome.include(RunOutcome::Failed("output_limit"));
            }
        }
    }
    if let Err(error) = sink.out.flush() {
        return output_failure(outcome, OxaError::io("cannot flush the reply", &error), sink.err);
    }
    Ok(outcome)
}

fn output_failure(outcome: RunOutcome, error: OxaError, err: &mut dyn Write) -> Result<RunOutcome, OxaError> {
    if matches!(outcome, RunOutcome::Done | RunOutcome::NothingToDo) {
        return Err(error);
    }
    let _ = writeln!(err, "{error}");
    Ok(outcome)
}

/// Whether `text` contains tool-call markup a model writes when it believes tools exist.
fn has_tool_markup(text: &str) -> bool {
    ["<tool_calls>", "<invoke name=", "<tool_call>", "<function="]
        .iter()
        .any(|m| text.contains(m))
}

/// `--mode json`: write the session header record, then every event as a record, for each
/// prompt in order. Same terminal outcomes as print.
pub fn run_json_outcome(
    session: &mut Session,
    prompts: Vec<(String, Vec<Part>)>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<RunOutcome, OxaError> {
    write_record(out, &session_header_record(&session_header(session)))?;
    let mut ui = HeadlessUi::default();
    let mut sink = JsonSink {
        out: &mut *out,
        error: None,
    };
    let mut outcome = RunOutcome::NothingToDo;
    for (text, parts) in prompts {
        let _ = session.take_outcome();
        let before = session.inner().tree.leaf().to_string();
        match prompt_or_command(session, text, parts, &mut sink, &mut ui) {
            Ok(Message::Prompt | Message::Extension) => {}
            // A command's lines are `notice` records (info).
            Ok(Message::Command(mut lines)) => {
                outcome.include(RunOutcome::Done);
                lines.extend(take_info(&mut ui));
                for text in lines {
                    sink.emit(&AgentEvent::Notice {
                        level: oxa_proto::Level::Info,
                        text,
                    });
                }
            }
            Err(e) => {
                let _ = writeln!(err, "{}", command_error(&e));
                outcome.include(RunOutcome::from_error(&e));
                return Ok(outcome);
            }
        }
        // Event writes cannot interrupt the request: retain its completed result
        // before classifying a concurrent output failure.
        outcome.include(*session.outcome());
        if let Some(e) = sink.error.take() {
            let _ = writeln!(err, "{e}");
            outcome.include(RunOutcome::from_error(&e));
            return Ok(outcome);
        }
        after_run(session, &before, &mut ui, &mut Vec::new(), err);
        if session.handle().wrap_up_requested() {
            outcome.include(RunOutcome::Interrupted);
            break;
        }
    }
    if done_failed(session, err) {
        outcome.include(RunOutcome::Failed("done_checks_failed"));
    }
    Ok(outcome)
}

/// Whether the `done` checks still failed after the repair rounds; says so on `err` once.
fn done_failed(session: &Session, err: &mut dyn Write) -> bool {
    let failed = session.inner().stats.done_failed;
    if failed {
        let _ = writeln!(err, "done checks failed: the project's done commands still fail");
    }
    failed
}

/// The header of the session in use: the stored one, or one built from the session state when
/// the store cannot read it (an ephemeral session).
fn session_header(session: &Session) -> SessionHeader {
    let st = session.inner();
    st.store.header(&st.sref).unwrap_or_else(|_| SessionHeader {
        version: names::SESSION_VERSION,
        id: st.sref.id.clone(),
        ts_ms: now_ms(),
        cwd: st.cwd.clone(),
        parent_session: None,
    })
}

/// Writes the notices of a finished prompt to `err` and reports whether the run failed. A run
/// that failed or was aborted leaves a `Shown` entry at the leaf (W2); a prompt that appended
/// nothing (blocked by a hook) is not a failure.
/// What one command-line message was.
enum Message {
    Prompt,
    /// An extension command: it has no output of its own. What it tells the user through
    /// `ctx.ui.notify` is reported like every other notification (stderr in print mode), so
    /// stdout stays the reply text.
    Extension,
    /// A built-in command; the lines it shows the user.
    Command(Vec<String>),
}

/// One command-line message: a `/name args` line of a command a hook set registered (an
/// extension command) runs that command, as pi does in print mode; a built-in command runs
/// headless (`oxa_core::run_builtin`); everything else is a prompt.
fn prompt_or_command(
    session: &mut Session,
    text: String,
    parts: Vec<Part>,
    sink: &mut dyn oxa_proto::EventSink,
    ui: &mut dyn oxa_proto::Ui,
) -> Result<Message, OxaError> {
    // Extension session switches are held until the message is over, whatever the message was
    // (a command, or a prompt whose event handlers or tools asked for one).
    session.enable_swap();
    let message = match oxa_core::expand_input(session.inner(), &text) {
        oxa_core::Expanded::Hook { name, args } => session
            .run_command(&name, &args, sink, ui)
            .map(|_| Message::Extension),
        oxa_core::Expanded::Builtin { name, args } => {
            oxa_core::run_builtin(session, &name, &args, sink, ui).map(Message::Command)
        }
        _ => session.prompt(text, parts, sink, ui).map(|_| Message::Prompt),
    }?;
    apply_swap(session, sink, ui)?;
    Ok(message)
}

/// Apply the session swap an extension asked for, between two messages.
fn apply_swap(
    session: &mut Session,
    sink: &mut dyn oxa_proto::EventSink,
    ui: &mut dyn oxa_proto::Ui,
) -> Result<(), OxaError> {
    let how = match session.take_swap_request() {
        None => return Ok(()),
        Some(oxa_core::SwapRequest::New) => oxa_core::Open::New,
        Some(oxa_core::SwapRequest::Switch(path)) => {
            oxa_core::Open::Resume(session.inner().store.resolve(&path)?)
        }
    };
    session.switch(how, sink, ui)
}

/// A failed message on stderr: the error code first for a coded refusal (`unsupported_in_oxa:
/// /share is not supported in oxa`), as the interactive mode shows it.
fn command_error(e: &OxaError) -> String {
    if e.code == oxa_proto::codes::UNSUPPORTED_IN_OXA {
        format!("{}: {e}", e.code)
    } else {
        e.to_string()
    }
}

/// The `info` notifications a command made (they are its output); the others stay.
fn take_info(ui: &mut HeadlessUi) -> Vec<String> {
    let (info, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut ui.notes)
        .into_iter()
        .partition(|(level, _)| *level == oxa_proto::Level::Info);
    ui.notes = rest;
    info.into_iter().map(|(_, t)| t).collect()
}

fn after_run(
    session: &Session,
    before: &str,
    ui: &mut HeadlessUi,
    notes: &mut Vec<String>,
    err: &mut dyn Write,
) -> bool {
    for t in notes.drain(..) {
        let _ = writeln!(err, "{t}");
    }
    for (_, t) in ui.notes.drain(..) {
        let _ = writeln!(err, "{t}");
    }
    let tree = &session.inner().tree;
    let leaf = tree.leaf();
    if leaf == before {
        return false;
    }
    match tree.get(leaf).map(|e| &e.body) {
        Some(EntryBody::Shown { text, why })
            if matches!(*why, shown_why::ABORTED | shown_why::FAILED | shown_why::CUT) =>
        {
            let _ = writeln!(err, "{text}");
            true
        }
        _ => false,
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Run print mode and return its process exit code.
pub fn run_print(
    session: &mut Session,
    prompts: Vec<(String, Vec<Part>)>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, OxaError> {
    run_print_outcome(session, prompts, out, err).map(|outcome| outcome.exit_code())
}

/// Run JSON mode and return its process exit code.
pub fn run_json(
    session: &mut Session,
    prompts: Vec<(String, Vec<Part>)>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, OxaError> {
    run_json_outcome(session, prompts, out, err).map(|outcome| outcome.exit_code())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assistant(text: &str) -> EventMessage {
        EventMessage {
            msg: oxa_proto::WireMsg {
                role: Role::Assistant,
                content: Some(text.to_string()),
                reasoning: None,
                tool_calls: Vec::new(),
                tool_call_id: None,
            },
            ts_ms: 0,
            stop: Some(StopReason::Stop),
            usage: None,
            model: None,
            tool_name: None,
            is_error: false,
            error_message: None,
        }
    }

    /// A no-tool-call message followed by another turn (a failed `done` check injects a user
    /// message, or a steering message arrives) is progress; only the last reply is the final one.
    #[test]
    fn reply_followed_by_another_turn_is_progress_not_the_final_reply() {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut sink = PrintSink {
            wrap_up: None,
            out: &mut out,
            err: &mut err,
            last: None,
            is_last_prompt: true,
            pending: None,
            final_text: None,
            notes: Vec::new(),
            started: HashMap::new(),
            io_error: None,
        };
        sink.emit(&AgentEvent::TurnStart);
        sink.emit(&AgentEvent::MessageEnd {
            message: assistant("first draft"),
        });
        sink.emit(&AgentEvent::TurnStart);
        sink.emit(&AgentEvent::MessageEnd {
            message: assistant("real final"),
        });
        sink.emit(&AgentEvent::AgentEnd {
            messages: Vec::new(),
            will_retry: false,
        });
        assert_eq!(sink.final_text.as_deref(), Some("real final"));
        drop(sink);
        assert!(out.is_empty(), "stdout is written by run_print, once");
        assert_eq!(String::from_utf8(err).unwrap(), "first draft\n");
    }
}
