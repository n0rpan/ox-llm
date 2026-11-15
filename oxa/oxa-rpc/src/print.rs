//! C9: print mode and JSON mode drivers. Owner: task C9. Signatures below are frozen; bodies are yours.
// ported from pi packages/coding-agent/src/modes/print-mode.ts
use crate::events::{session_header_record, write_record, JsonSink};
use oxa_core::Session;
use oxa_proto::{
    names, shown_why, AgentEvent, EntryBody, EventMessage, EventSink, HeadlessUi, OxaError, Part, Role,
    StopReason,
};
use oxa_session::SessionHeader;
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

/// Collects what print mode needs from one run: the last assistant message (the reply that is
/// printed) and the notices that go to stderr.
#[derive(Default)]
struct PrintSink {
    last: Option<EventMessage>,
    notes: Vec<String>,
}

impl EventSink for PrintSink {
    fn emit(&mut self, e: &AgentEvent) {
        match e {
            AgentEvent::MessageEnd { message } if message.msg.role == Role::Assistant => {
                self.last = Some(message.clone());
            }
            AgentEvent::Notice { text, .. } => self.notes.push(text.clone()),
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
/// `err`. Exit code: 0 ok, 1 when the last run failed or was aborted, or when the `done`
/// checks still failed after their repair rounds (one stderr line says so).
pub fn run_print(
    session: &mut Session,
    prompts: Vec<(String, Vec<Part>)>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, OxaError> {
    let mut sink = PrintSink::default();
    let mut ui = HeadlessUi::default();
    let mut failed = false;
    for (text, parts) in prompts {
        sink.last = None;
        let before = session.inner().tree.leaf().to_string();
        let lines = match prompt_or_command(session, text, parts, &mut sink, &mut ui) {
            Ok(Message::Prompt | Message::Extension) => None,
            Ok(Message::Command(lines)) => Some(lines),
            Err(e) => {
                let _ = writeln!(err, "{}", command_error(&e));
                return Ok(1);
            }
        };
        if let Some(mut lines) = lines {
            // A command message: what it shows the user (its lines and its `info`
            // notifications) is the output; warnings and errors still go to stderr.
            if sink.last.is_none() {
                lines.extend(take_info(&mut ui));
            }
            for l in lines {
                out.write_all(format!("{l}\n").as_bytes())
                    .map_err(|e| OxaError::io("cannot write the command output", &e))?;
            }
        }
        failed = after_run(session, &before, &mut ui, &mut sink.notes, err);
    }

    let mut code = 0;
    if done_failed(session, err) {
        code = 1;
    }
    if failed {
        code = 1;
    } else if let Some(m) = &sink.last {
        if let Some(text) = m.msg.content.as_deref().filter(|t| !t.is_empty()) {
            out.write_all(text.as_bytes())
                .map_err(|e| OxaError::io("cannot write the reply", &e))?;
            out.write_all(b"\n")
                .map_err(|e| OxaError::io("cannot write the reply", &e))?;
        }
        if m.stop == Some(StopReason::Length) {
            let _ = writeln!(
                err,
                "Response was truncated: the model reached its output token limit."
            );
            code = 1;
        }
    }
    out.flush()
        .map_err(|e| OxaError::io("cannot flush the reply", &e))?;
    Ok(code)
}

/// `--mode json`: write the session header record, then every event as a record, for each
/// prompt in order. Same exit codes as print.
pub fn run_json(
    session: &mut Session,
    prompts: Vec<(String, Vec<Part>)>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, OxaError> {
    write_record(out, &session_header_record(&session_header(session)))?;
    let mut ui = HeadlessUi::default();
    let mut sink = JsonSink {
        out: &mut *out,
        error: None,
    };
    let mut failed = false;
    for (text, parts) in prompts {
        let before = session.inner().tree.leaf().to_string();
        match prompt_or_command(session, text, parts, &mut sink, &mut ui) {
            Ok(Message::Prompt | Message::Extension) => {}
            // A command's lines are `notice` records (info).
            Ok(Message::Command(mut lines)) => {
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
                return Ok(1);
            }
        }
        if let Some(e) = sink.error.take() {
            let _ = writeln!(err, "{e}");
            return Ok(1);
        }
        failed = after_run(session, &before, &mut ui, &mut Vec::new(), err);
    }
    let done = done_failed(session, err);
    Ok(if failed || done { 1 } else { 0 })
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
    match oxa_core::expand_input(session.inner(), &text) {
        oxa_core::Expanded::Hook { name, args } => session
            .run_command(&name, &args, sink, ui)
            .map(|_| Message::Extension),
        oxa_core::Expanded::Builtin { name, args } => {
            oxa_core::run_builtin(session, &name, &args, sink, ui).map(Message::Command)
        }
        _ => session.prompt(text, parts, sink, ui).map(|_| Message::Prompt),
    }
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
