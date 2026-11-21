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
    out: &'a mut dyn Write,
    err: &'a mut dyn Write,
    /// The last assistant message of the current prompt.
    last: Option<EventMessage>,
    /// Only the last prompt's final reply goes to stdout.
    is_last_prompt: bool,
    /// Text of a message without tool calls that may still turn out to be the final reply: a
    /// `done` check or a steering message can start another turn after it.
    pending: Option<String>,
    /// A local length recovery continues this same answer, byte for byte. Cloud retries
    /// regenerate a complete answer instead; tool work starts a different final reply.
    /// With an output schema, only the last message is validated: never prepend chunks.
    local_model: bool,
    pending_length: bool,
    continued: Option<String>,
    /// Completed candidate selected at `AgentEnd` or successful goal settlement; choose stdout or progress after
    /// settlement, once it knows whether another prompt is admitted or the run failed.
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
        self.pending_length = false;
        if let Some(t) = self.pending.take() {
            self.progress(&t);
        }
    }

    fn next_turn(&mut self) {
        // A new turn proves the preceding completed candidate is progress.
        if let Some(text) = self.final_text.take() {
            self.progress(&text);
        }
        if self.local_model && self.pending_length {
            self.pending_length = false;
            if let Some(text) = self.pending.take() {
                self.progress(&text);
                match &mut self.continued {
                    Some(prefix) => prefix.push_str(&text),
                    None => self.continued = Some(text),
                }
            }
        } else {
            self.flush_pending();
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
                    self.continued = None;
                    if let Some(t) = text {
                        self.progress(t);
                    }
                } else if self.local_model && message.stop == Some(StopReason::Stop) {
                    if let Some(mut prefix) = self.continued.take() {
                        prefix.push_str(text.unwrap_or_default());
                        self.pending = Some(prefix);
                    } else {
                        self.pending = text.map(str::to_string);
                    }
                } else if let Some(t) = text {
                    self.pending = Some(t.to_string());
                    self.pending_length = self.local_model && message.stop == Some(StopReason::Length);
                }
            }
            AgentEvent::TurnStart => self.next_turn(),
            AgentEvent::AgentEnd { .. } => {
                // Settlement hooks may still stop the invocation. Keep the
                // completed candidate until the driver decides its destination.
                self.final_text = self.pending.take();
            }
            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            } => {
                self.continued = None;
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
                let mut line = format!("[tool] {tool_name} {word} {secs:.1}s");
                if result.is_error {
                    if let Some(why) = result.text.lines().find(|l| !l.trim().is_empty()) {
                        let why: String = why.trim().chars().take(160).collect();
                        line.push_str(&format!(": {why}"));
                    }
                }
                self.progress(&line);
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
    run_print_prompts(session, prompts, out, err, None)
}

/// [`run_print_outcome`] over an optional session inbox, told to reconsider retained
/// messages once each prompt has drained the queue (as the RPC and TUI owners do).
fn run_print_prompts(
    session: &mut Session,
    prompts: Vec<(String, Vec<Part>)>,
    out: &mut dyn Write,
    err: &mut dyn Write,
    inbox: Option<&oxa_session::Inbox>,
) -> Result<RunOutcome, OxaError> {
    let mut sink = PrintSink {
        out,
        err,
        last: None,
        is_last_prompt: false,
        pending: None,
        local_model: !session.inner().info.caps.remote && session.inner().output_schema.is_none(),
        pending_length: false,
        continued: None,
        final_text: None,
        notes: Vec::new(),
        started: HashMap::new(),
        io_error: None,
    };
    let mut ui = HeadlessUi::default();
    let retry = inbox.map(|inbox| inbox.retry_handle());
    let mut failed = false;
    let mut outcome = RunOutcome::NothingToDo;
    let n = prompts.len();
    for (i, (text, parts)) in prompts.into_iter().enumerate() {
        if session.handle().interrupted() {
            outcome.include(RunOutcome::Interrupted);
            break;
        }
        sink.last = None;
        sink.pending = None;
        sink.pending_length = false;
        sink.continued = None;
        sink.local_model = !session.inner().info.caps.remote && session.inner().output_schema.is_none();
        sink.final_text = None;
        sink.is_last_prompt = i + 1 == n;
        let _ = session.take_outcome();
        let before = session.inner().tree.leaf().to_string();
        let result = {
            let mut drain = DrainRetry {
                inner: &mut sink,
                retry: retry.clone(),
            };
            prompt_or_command(session, text, parts, &mut drain, &mut ui)
        };
        let lines = match result {
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
        // The session already settled this prompt's outcome (done checks, output limit
        // included); print only says why on stderr.
        let settled = session.take_outcome();
        outcome.include(settled);
        if let Some(inbox) = inbox {
            inbox.retry();
        }
        // goal_done can settle the run on the tool-call message itself. Its
        // content is final only after goal, schema and project checks succeed.
        if sink.is_last_prompt
            && settled == RunOutcome::Done
            && sink.final_text.is_none()
            && session
                .goal()
                .is_some_and(|goal| goal.status == oxa_session::GoalStatus::Done)
        {
            sink.final_text = sink
                .last
                .as_ref()
                .filter(|message| message.msg.tool_calls.iter().any(|call| call.name == "goal_done"))
                .and_then(|message| message.msg.content.clone());
        }
        // Decide once after settlement, retaining failure precedence and progress
        // routing for an intermediate prompt whose next prompt will be admitted.
        let stopping = session.handle().wrap_up_requested() || session.handle().interrupted();
        if let Some(t) = sink.final_text.clone() {
            if sink.is_last_prompt || stopping {
                // A failed or aborted run prints no final reply.
                if !failed {
                    sink.write_reply(&t);
                    if let Some(e) = sink.io_error.take() {
                        return output_failure(outcome, e, sink.err);
                    }
                }
            } else {
                sink.progress(&t);
                sink.final_text = None;
            }
        }
        explain_outcome(settled, &mut *sink.err);
        if stopping {
            outcome.include(RunOutcome::Interrupted);
            break;
        }
    }

    if !failed {
        if let Some(text) = sink.final_text.take() {
            if session.inner().active.direct.is_empty() && has_tool_markup(&text) {
                let _ = writeln!(
                    sink.err,
                    "warning: the model answered with tool-call markup but no tools are available (--no-tools); the answer may be unusable"
                );
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

/// Print mode with the plain session inbox (`oxa send`): [`run_print_outcome`] with
/// [`headless_inbox`] watched for the whole run.
pub fn run_print_outcome_with_inbox(
    session: &mut Session,
    prompts: Vec<(String, Vec<Part>)>,
    out: &mut dyn Write,
    err: &mut dyn Write,
    agent_dir: &std::path::Path,
    headless_inbox_enabled: bool,
) -> Result<RunOutcome, OxaError> {
    let inbox = headless_inbox(session, agent_dir, headless_inbox_enabled, err);
    let result = run_print_prompts(session, prompts, out, err, inbox.as_ref());
    let restored = restore_undelivered(session, inbox, agent_dir);
    restored.and(result)
}

/// `--mode json` with the plain session inbox: [`run_json_outcome`] with [`headless_inbox`]
/// watched for the whole run, after the header record.
pub fn run_json_outcome_with_inbox(
    session: &mut Session,
    prompts: Vec<(String, Vec<Part>)>,
    out: &mut dyn Write,
    err: &mut dyn Write,
    agent_dir: &std::path::Path,
    headless_inbox_enabled: bool,
) -> Result<RunOutcome, OxaError> {
    write_json_header(session, out)?;
    let inbox = headless_inbox(session, agent_dir, headless_inbox_enabled, err);
    let result = run_json_prompts(session, prompts, out, err, inbox.as_ref());
    let restored = restore_undelivered(session, inbox, agent_dir);
    restored.and(result)
}

/// The run is over: stop the watcher, then give back every message it admitted that no turn
/// consumed (one admitted after the last queue drain) as an inbox file, so the next owner of
/// the session takes it. A failed durable handoff fails the run instead of reporting success.
fn restore_undelivered(
    session: &Session,
    inbox: Option<oxa_session::Inbox>,
    agent_dir: &std::path::Path,
) -> Result<(), OxaError> {
    let Some(inbox) = inbox else { return Ok(()) };
    drop(inbox);
    let active = session.state().session_id;
    // A message admitted before the run switched sessions goes back to the inbox it came from.
    let undelivered = session
        .handle()
        .dequeue()
        .into_iter()
        .map(|text| (active.clone(), text))
        .chain(
            session
                .handle()
                .take_inactive_inbox()
                .into_iter()
                .flat_map(|(id, texts)| texts.into_iter().map(move |text| (id.clone(), text))),
        );
    let mut failed = None;
    for (id, text) in undelivered {
        if let Err(error) = oxa_session::send_inbox(agent_dir, &id, &text) {
            failed.get_or_insert_with(|| {
                OxaError::io("cannot restore an undelivered session inbox message", &error)
            });
        }
    }
    failed.map_or(Ok(()), Err)
}

/// Reports every queue drain to the owner's watcher, so a message refused at capacity is
/// admitted during the run that drained the queue. A goal run is one prompt, so the per-prompt
/// retry alone would leave it on disk until the process exits.
struct DrainRetry<'a> {
    inner: &'a mut dyn EventSink,
    retry: Option<oxa_session::InboxRetry>,
}

impl EventSink for DrainRetry<'_> {
    fn emit(&mut self, e: &AgentEvent) {
        if let (Some(retry), AgentEvent::QueueUpdate { .. }) = (&self.retry, e) {
            retry.retry();
        }
        self.inner.emit(e);
    }
}

/// The session inbox of a headless run, watched until the returned watcher is dropped. It is
/// installed only when `enabled` (`oxa.inbox.headless`), the session is persisted and its goal
/// is pursuing or the run is an `oxa work` worker (`OXA_WORK_INBOX=1`). Each admitted message is an ordinary follow-up of this session; a refused or
/// oversized message stays on disk. Not used by the managed-steering path.
fn headless_inbox(
    session: &Session,
    agent_dir: &std::path::Path,
    enabled: bool,
    err: &mut dyn Write,
) -> Option<oxa_session::Inbox> {
    if !enabled || session.state().session_file.is_none() {
        return None;
    }
    // #1446: an `oxa work` worker (`OXA_WORK_INBOX=1`, set by the runner that launched it) watches
    // its inbox without a goal, so `oxa send`, the lead chat and the web UI reach it at its next
    // turn boundary. The owner's `oxa.inbox.headless: false` still turns it off.
    let work_worker = std::env::var_os("OXA_WORK_INBOX").is_some_and(|v| v == "1");
    if !work_worker
        && !session
            .goal()
            .is_some_and(|goal| goal.status == oxa_session::GoalStatus::Pursuing)
    {
        return None;
    }
    let id = session.state().session_id;
    let handle = session.handle();
    let owner = id.clone();
    // Like the RPC owner, a message that arrives once the run is wrapping up stays on disk.
    // #2183: a steer reaches the run at its next tool boundary, a follow-up when it stops.
    let watch = oxa_session::Inbox::watch_messages(agent_dir, &id, move |item| {
        if handle.wrap_up_requested() {
            return false;
        }
        match item {
            oxa_session::InboxItem::Steer(text) => handle.steer_for(&owner, text),
            oxa_session::InboxItem::FollowUp(text) => handle.follow_up_for(&owner, text),
            oxa_session::InboxItem::Control(_) => false,
        }
    });
    match watch {
        Ok(inbox) => Some(inbox),
        Err(error) => {
            let _ = writeln!(err, "Session inbox unavailable: {error}");
            None
        }
    }
}

/// `--mode json`: write the session header record, then every event as a record, for each
/// prompt in order. Same terminal outcomes as print.
pub fn run_json_outcome(
    session: &mut Session,
    prompts: Vec<(String, Vec<Part>)>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<RunOutcome, OxaError> {
    write_json_header(session, out)?;
    run_json_prompts(session, prompts, out, err, None)
}

fn write_json_header(session: &Session, out: &mut dyn Write) -> Result<(), OxaError> {
    let mut header = session_header_record(&session_header(session));
    header["statusFile"] = session
        .state()
        .status_file
        .map(|p| p.to_string_lossy().into_owned())
        .into();
    write_record(out, &header)
}

/// The managed frontend has authenticated the assignment before mounting this worker.
/// The JSON header remains first. All model events and receipts share one line writer.
#[cfg(target_os = "linux")]
pub fn run_json_outcome_with_steering_authority(
    session: &mut Session,
    prompts: Vec<(String, Vec<Part>)>,
    out: crate::Out,
    err: &mut dyn Write,
    agent_dir: &std::path::Path,
    authority: std::sync::Arc<oxa_core::goal::SteeringAuthority>,
) -> Result<RunOutcome, OxaError> {
    use crate::a2a_control::{queue_steering, write_control_receipt};
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    session.bind_lead_steering_authority(authority.clone());
    let mut writer = SharedRecords(out.clone());
    write_json_header(session, &mut writer)?;
    let handle = session.handle();
    handle.enable_goal_steering();
    let binding = session.inner().sref.clone();
    let session_ref = binding.clone();
    let order = Arc::new(Mutex::new(()));
    let _inbox = oxa_session::Inbox::watch_control(agent_dir, &binding, move |packet| {
        if handle.wrap_up_requested() {
            return false;
        }
        let _admission = order.lock().unwrap_or_else(|poison| poison.into_inner());
        let request = match oxa_orchestrator::control::decode(packet.as_bytes()) {
            Ok(request) => request,
            Err(rejection) => {
                write_control_receipt(&out, rejection);
                return true;
            }
        };
        let result_order = order.clone();
        let result_out = out.clone();
        let response = queue_steering(&handle, &session_ref, request, &authority, move |receipt| {
            let _receipt = result_order.lock().unwrap_or_else(|poison| poison.into_inner());
            write_control_receipt(&result_out, receipt);
        })
        .unwrap_or_else(|request| json!({"id":request.id(),"error":"unsupported_control"}));
        write_control_receipt(&out, response);
        true
    })
    .map_err(|error| OxaError::io("cannot mount managed steering inbox", &error))?;
    run_json_prompts(session, prompts, &mut writer, err, None)
}

#[cfg(target_os = "linux")]
struct SharedRecords(crate::Out);

#[cfg(target_os = "linux")]
impl Write for SharedRecords {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .write(bytes)
    }

    fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.0
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .write_all(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.lock().unwrap_or_else(|poison| poison.into_inner()).flush()
    }
}

fn run_json_prompts(
    session: &mut Session,
    prompts: Vec<(String, Vec<Part>)>,
    out: &mut dyn Write,
    err: &mut dyn Write,
    inbox: Option<&oxa_session::Inbox>,
) -> Result<RunOutcome, OxaError> {
    let mut ui = HeadlessUi::default();
    let retry = inbox.map(|inbox| inbox.retry_handle());
    let mut sink = JsonSink {
        out: &mut *out,
        error: None,
    };
    let mut outcome = RunOutcome::NothingToDo;
    for (text, parts) in prompts {
        if session.handle().interrupted() {
            outcome.include(RunOutcome::Interrupted);
            break;
        }
        let _ = session.take_outcome();
        let before = session.inner().tree.leaf().to_string();
        let result = {
            let mut drain = DrainRetry {
                inner: &mut sink,
                retry: retry.clone(),
            };
            prompt_or_command(session, text, parts, &mut drain, &mut ui)
        };
        match result {
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
        let settled = session.take_outcome();
        outcome.include(settled);
        if let Some(inbox) = inbox {
            inbox.retry();
        }
        if let Some(e) = sink.error.take() {
            let _ = writeln!(err, "{e}");
            outcome.include(RunOutcome::from_error(&e));
            return Ok(outcome);
        }
        after_run(session, &before, &mut ui, &mut Vec::new(), err);
        explain_outcome(settled, err);
        if session.handle().wrap_up_requested() || session.handle().interrupted() {
            outcome.include(RunOutcome::Interrupted);
            break;
        }
    }
    Ok(outcome)
}

/// The stderr line for a settled outcome the session classified from the run's state; the
/// outcome itself is the session's, never worked out again here.
fn explain_outcome(settled: RunOutcome, err: &mut dyn Write) {
    match settled {
        RunOutcome::Failed("done_checks_failed") => {
            let _ = writeln!(err, "done checks failed: the project's done commands still fail");
        }
        RunOutcome::Failed("output_limit") => {
            let _ = writeln!(
                err,
                "Response was truncated: the model reached its output token limit."
            );
        }
        _ => {}
    }
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
    use oxa_proto::StopReason;

    /// #483: a failed call says why on the progress line.
    #[test]
    fn failed_tool_line_carries_the_reason() {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut sink = PrintSink {
            out: &mut out,
            err: &mut err,
            last: None,
            is_last_prompt: true,
            pending: None,
            local_model: true,
            pending_length: false,
            continued: None,
            final_text: None,
            notes: Vec::new(),
            started: HashMap::new(),
            io_error: None,
        };
        sink.emit(&AgentEvent::ToolExecutionEnd {
            tool_call_id: "c1".into(),
            tool_name: "read".into(),
            result: oxa_proto::ToolOutput::error("Invalid tool arguments (empty arguments)"),
            duration_ms: Some(0),
        });
        let e = String::from_utf8(err.clone()).unwrap();
        assert!(
            e.contains("[tool] read failed 0.0s: Invalid tool arguments (empty arguments)"),
            "{e}"
        );
    }

    fn assistant(text: &str) -> EventMessage {
        EventMessage {
            msg: oxa_proto::WireMsg {
                images: Vec::new(),
                role: Role::Assistant,
                content: Some(text.to_string()),
                reasoning: None,
                tool_calls: Vec::new(),
                provider_history: None,
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
            out: &mut out,
            err: &mut err,
            last: None,
            is_last_prompt: true,
            pending: None,
            local_model: true,
            pending_length: false,
            continued: None,
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
