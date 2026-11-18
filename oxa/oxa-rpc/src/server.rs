//! C10: the RPC server loop.
// ported from pi packages/coding-agent/src/modes/rpc/rpc-mode.ts (docs: rpc.md)
//
// Threads: the calling thread reads stdin and answers the commands that must not wait for a
// run (`steer`, `follow_up`, `abort`, `abort_retry`, `clear_queue`, `get_state` while a run is
// in progress, `extension_ui_response`). Everything that needs the `Session` goes to one
// session thread, which owns the `RpcCx`. Stdin EOF closes the job channel: the session thread
// finishes the queued work, shuts the session down and returns the exit code; dialogs waiting
// for an answer at EOF (and any opened later) resolve as cancelled.
//
// Ordering (wave H): a record is never visible before the state it reports. The session thread
// finishes a job in this order: refresh the `get_state` snapshot, release the job's claims
// (`pending`, the prompt's run claim), then write the response, the run's `agent_settled` (held
// back while the job ran) and the `abort` responses that waited for it. A command a client
// sends after reading any of those records therefore meets an idle session and fresh state.
#[cfg(all(target_os = "linux", feature = "a2a-control"))]
use crate::a2a_control::BoundControl;
use crate::events::{event_to_json, session_state, write_record};
use crate::{cmd_run, handle_session, Out, RpcCx, RpcUi, UiRouter};
use oxa_core::{Handle, RunOutcome, Session};
use oxa_proto::{codes, AgentEvent, Level, OxaError, Value};
use serde_json::{json, Map};
use std::cell::RefCell;
use std::collections::HashSet;
use std::io::BufRead;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;

/// One parsed command line.
#[derive(Debug, Clone, PartialEq)]
pub struct RpcCommand {
    /// Optional client correlation id, echoed in the response.
    pub id: Option<String>,
    /// The `type` field (`prompt`, `steer`, ..).
    pub kind: String,
    /// The whole command object (handlers read their fields from it).
    pub body: Value,
}

/// Parse one stdin line. Not JSON or no string `type` = `invalid` (the server answers with pi's
/// parse-error response and keeps running).
pub fn parse_command(line: &str) -> Result<RpcCommand, OxaError> {
    let body: Value =
        serde_json::from_str(line.trim()).map_err(|e| OxaError::new(codes::INVALID, e.to_string()))?;
    let kind = body
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| OxaError::new(codes::INVALID, "the command has no string \"type\""))?
        .to_string();
    let id = body.get("id").and_then(Value::as_str).map(str::to_string);
    Ok(RpcCommand { id, kind, body })
}

fn envelope(cmd: &RpcCommand, success: bool) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("type".to_string(), Value::from("response"));
    if let Some(id) = &cmd.id {
        m.insert("id".to_string(), Value::from(id.as_str()));
    }
    m.insert("command".to_string(), Value::from(cmd.kind.as_str()));
    m.insert("success".to_string(), Value::from(success));
    m
}

/// `{"type":"response","command":<kind>,"success":true[,"id":..][,"data":..]}`.
pub fn response_ok(cmd: &RpcCommand, data: Value) -> Value {
    let mut m = envelope(cmd, true);
    if !data.is_null() {
        m.insert("data".to_string(), data);
    }
    Value::Object(m)
}

/// `{"type":"response","command":<kind>,"success":false,"error":<message>[,"id":..]}`.
pub fn response_err(cmd: &RpcCommand, e: &OxaError) -> Value {
    let mut m = envelope(cmd, false);
    m.insert("error".to_string(), Value::from(e.message.as_str()));
    Value::Object(m)
}

/// pi's answer to a line that is not a command: no id, command `parse`.
fn parse_error(e: &OxaError) -> Value {
    json!({
        "type": "response",
        "command": "parse",
        "success": false,
        "error": format!("Failed to parse command: {}", e.message),
    })
}

thread_local! {
    /// `agent_settled` records held back on the session thread while a job runs (see
    /// [`hold_settled`]). `None` = not holding: records are written at once.
    static HELD_SETTLED: RefCell<Option<Vec<Value>>> = const { RefCell::new(None) };
}

/// Start holding back the `agent_settled` records this thread writes (the session thread, for
/// the duration of one job), so the job can release its claims before a client sees them.
fn hold_settled() {
    HELD_SETTLED.with(|h| *h.borrow_mut() = Some(Vec::new()));
}

/// Stop holding; returns the records held since [`hold_settled`].
fn release_settled() -> Vec<Value> {
    HELD_SETTLED.with(|h| h.borrow_mut().take().unwrap_or_default())
}

/// Writes one record to `out`, line-atomic. A broken stdout is ignored: the loop ends on EOF.
/// An `agent_settled` record written while the thread holds them is kept for later.
pub(crate) fn write_out(out: &Out, v: &Value) {
    if v.get("type").and_then(Value::as_str) == Some("agent_settled") {
        let held = HELD_SETTLED.with(|h| match h.borrow_mut().as_mut() {
            Some(held) => {
                held.push(v.clone());
                true
            }
            None => false,
        });
        if held {
            return;
        }
    }
    let mut w = lock(out);
    let _ = write_record(&mut **w, v);
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

/// A command that needs the session, queued for the session thread. `reply` is false for a
/// prompt: its response was written when it was accepted. `claimed` marks a prompt that took
/// the run claim ([`Gate::claimed`]) when it was accepted.
enum Job {
    Command {
        cmd: RpcCommand,
        reply: bool,
        claimed: bool,
    },
    /// Reader failures share the command FIFO so they cannot overtake an earlier run.
    Outcome(RunOutcome),
    /// A follow-up is already in Handle's queue; wake the session even with stdin idle.
    Inbox(String),
    #[cfg(all(target_os = "linux", feature = "a2a-control"))]
    GoalControl(BoundControl),
    Shutdown,
}

/// What the reader and the session thread decide together, under one lock.
#[derive(Default)]
struct Gate {
    /// Session commands sent to the worker and not finished (outcome-only jobs claim nothing).
    pending: usize,
    /// Bound the typed inbox lane to one pending job on the existing session FIFO.
    #[cfg(all(target_os = "linux", feature = "a2a-control"))]
    control_pending: bool,
    /// EOF is ordered with inbox admission: leave later files for the next owner.
    closed: bool,
    /// A prompt was answered `started` and its run has not settled yet. Set by the reader when
    /// it accepts the prompt (pi: the run is active once `started` is sent), cleared by the
    /// session thread before the run's `agent_settled` is written.
    claimed: bool,
    /// `abort` responses waiting for the jobs in flight to finish (pi answers `abort` once
    /// the session is idle, after `agent_settled`).
    after_jobs: Vec<Value>,
}

/// The state the session thread publishes for the reader after each job.
struct Published {
    /// The last `get_state` data (fields that change during a run are overridden on read).
    state: Value,
    /// The names a `prompt` runs as a command (no agent run): `disposition: handled`.
    commands: HashSet<String>,
}

/// What the reading side of the server holds.
struct Front {
    handle: Handle,
    out: Out,
    router: Arc<UiRouter>,
    /// What the session thread published after its last job.
    cache: Arc<Mutex<Published>>,
    gate: Arc<Mutex<Gate>>,
    /// True while the session thread runs a `bash` command (`abort_bash` cancels only that).
    bash_running: Arc<AtomicBool>,
    tx: Sender<Job>,
}

impl Front {
    fn reply(&self, cmd: &RpcCommand, r: Result<Value, OxaError>) {
        let v = match r {
            Ok(data) => {
                // A started prompt is only acknowledged here; its eventual turn (or
                // hook interception) decides its outcome on the worker. Reader-handled
                // controls have completed and must enter that same ordered aggregate.
                if cmd.kind != "prompt" || data["disposition"] != "started" {
                    let _ = self.tx.send(Job::Outcome(RunOutcome::Done));
                }
                response_ok(cmd, data)
            }
            Err(e) => {
                self.record_error(&e);
                response_err(cmd, &e)
            }
        };
        write_out(&self.out, &v);
    }

    fn record_error(&self, error: &OxaError) {
        // No session claim or gate lock: this can be called while a rejected command holds
        // the gate. The worker folds the error after all earlier accepted commands.
        let _ = self.tx.send(Job::Outcome(RunOutcome::from_error(error)));
    }

    /// After a queue change made here: the `queue_update` record, at once (pi emits it when the
    /// message is queued, before the command's response). The snapshot is taken under the
    /// output lock, so a later change the session thread reports is never overtaken.
    fn report_queue(&self) {
        let mut w = lock(&self.out);
        if let Some((steering, follow_up)) = self.handle.take_queue_update() {
            let e = AgentEvent::QueueUpdate { steering, follow_up };
            let _ = write_record(&mut **w, &event_to_json(&e));
        }
    }

    /// Queue a job (the caller holds the gate). `claimed` = the job carries the run claim.
    fn enqueue(&self, gate: &mut Gate, cmd: &RpcCommand, reply: bool, claimed: bool) {
        gate.pending += 1;
        let job = Job::Command {
            cmd: cmd.clone(),
            reply,
            claimed,
        };
        if self.tx.send(job).is_err() {
            gate.pending -= 1;
            if claimed {
                gate.claimed = false;
            }
            if reply {
                let e = OxaError::new(codes::INTERNAL, "the session is not running");
                self.reply(cmd, Err(e));
            }
        }
    }

    /// A run is in progress: a prompt was accepted and has not settled, or the session itself
    /// is claimed (a run, a compaction, a `bash` command).
    fn busy(&self, gate: &Gate) -> bool {
        gate.claimed || self.handle.is_busy()
    }

    /// `get_state` during a run: the data published after the last job, with the fields that
    /// the reader knows live (`isStreaming`, `pendingMessageCount`) read now.
    fn cached_state(&self, gate: &Gate) -> Value {
        let mut v = lock(&self.cache).state.clone();
        if let Value::Object(m) = &mut v {
            m.insert("isStreaming".to_string(), Value::from(self.busy(gate)));
            let (steering, follow_up) = self.handle.queued();
            m.insert(
                "pendingMessageCount".to_string(),
                Value::from((steering.len() + follow_up.len()) as u64),
            );
        }
        v
    }

    fn accept(&self, cmd: RpcCommand) {
        match cmd.kind.as_str() {
            "extension_ui_response" => {
                // No response record: the answer goes to the dialog waiting on the session.
                self.router.deliver(&cmd);
            }
            "prompt" => self.prompt(&cmd),
            "steer" | "follow_up" | "abort_retry" | "clear_queue" => {
                let r = cmd_run::queue_command(&self.handle, &cmd)
                    .unwrap_or_else(|| Err(OxaError::new(codes::INTERNAL, "not a queue command")));
                self.report_queue();
                self.reply(&cmd, r);
            }
            "abort" => {
                // The response waits for the run to settle: the session thread writes it after
                // the run's `agent_settled`. The reader keeps reading meanwhile, so a dialog
                // the run waits on can still receive its answer.
                let mut gate = lock(&self.gate);
                if gate.claimed {
                    // The accepted prompt may not have claimed the session yet: its run then
                    // starts cancelled. The session thread withdraws this when the job ends.
                    self.handle.abort_next();
                }
                self.handle.abort();
                let _ = self.tx.send(Job::Outcome(RunOutcome::Done));
                let response = response_ok(&cmd, Value::Null);
                if gate.pending > 0 {
                    gate.after_jobs.push(response);
                } else {
                    write_out(&self.out, &response);
                }
            }
            "abort_bash" => {
                // Cancels a running `bash` only; a no-op during an agent run or when idle.
                if self.bash_running.load(Ordering::SeqCst) {
                    self.handle.abort();
                }
                self.reply(&cmd, Ok(Value::Null));
            }
            _ => {
                let mut gate = lock(&self.gate);
                if !self.busy(&gate) {
                    // In order behind the jobs already queued, so the answer includes them.
                    self.enqueue(&mut gate, &cmd, true, false);
                } else if cmd.kind == "get_state" {
                    let v = self.cached_state(&gate);
                    self.reply(&cmd, Ok(v));
                } else {
                    let e = OxaError::new(
                        codes::BUSY,
                        format!("the agent is busy; send {} after agent_settled", cmd.kind),
                    );
                    self.reply(&cmd, Err(e));
                }
            }
        }
    }

    /// The name of a command `prompt` runs without an agent run (`/name args`), when `text`
    /// is one.
    fn command_name<'t>(&self, text: &'t str) -> Option<&'t str> {
        let body = text.trim_start().strip_prefix('/')?;
        let name = body.split(char::is_whitespace).next().unwrap_or("");
        lock(&self.cache).commands.contains(name).then_some(name)
    }

    fn prompt(&self, cmd: &RpcCommand) {
        let text = match cmd.body.get("message").and_then(Value::as_str) {
            Some(t) => t.to_string(),
            None => {
                let e = OxaError::new(codes::INVALID, "prompt needs a string \"message\"");
                self.reply(cmd, Err(e));
                return;
            }
        };
        let mut gate = lock(&self.gate);
        if self.command_name(&text).is_some() {
            // pi (agent-session.ts `prompt`): an extension command runs instead of a prompt
            // and the prompt answers `handled` once the handler has returned; no run starts
            // for it, so no `agent_settled` is owed (unless the command triggers a turn).
            // The session thread writes that response (`cmd_run::run_prompt`). pi runs it
            // even during a run; here the session thread runs it once the run has settled.
            self.enqueue(&mut gate, cmd, true, false);
            return;
        }
        if self.busy(&gate) {
            let behavior = cmd.body.get("streamingBehavior").and_then(Value::as_str);
            match behavior {
                Some("steer") => {
                    self.handle.steer(text);
                    self.report_queue();
                    self.reply(cmd, Ok(json!({"disposition": "queued"})));
                }
                Some("followUp") | Some("follow_up") => {
                    self.handle.follow_up(text);
                    self.report_queue();
                    self.reply(cmd, Ok(json!({"disposition": "queued"})));
                }
                _ => {
                    let e = OxaError::new(
                        codes::BUSY,
                        "Agent is already processing. Specify streamingBehavior ('steer' or 'followUp') to queue the message.",
                    );
                    self.reply(cmd, Err(e));
                }
            }
            return;
        }
        // pi (agent-session.ts `_startRun`): the run is active from the moment `started` is
        // sent, so a second prompt right after this response is refused or queued above.
        gate.claimed = true;
        self.reply(cmd, Ok(json!({"disposition": "started"})));
        self.enqueue(&mut gate, cmd, false, true);
    }
}

/// What the reader needs after a job: the `get_state` data and the command names.
fn publish(cx: &RpcCx) -> Published {
    // Every name a `/name` prompt runs as a command: extension commands and the built-ins
    // (`oxa_core::run_builtin`), as `expand_input` resolves them now.
    let st = cx.session.inner();
    let candidates = st
        .hooks
        .iter()
        .flat_map(|h| h.commands())
        .map(|c| c.name)
        .chain(oxa_core::builtin_commands().into_iter().map(|c| c.name))
        .chain(["login", "logout", "share"].map(String::from));
    let commands = candidates
        .filter(|name| {
            matches!(
                oxa_core::expand_input(st, &format!("/{name}")),
                oxa_core::Expanded::Hook { .. } | oxa_core::Expanded::Builtin { .. }
            )
        })
        .collect();
    Published {
        state: session_state(&cx.session),
        commands,
    }
}

#[cfg(all(target_os = "linux", feature = "a2a-control"))]
pub(super) fn write_control_receipt(out: &Out, data: Value) {
    write_out(out, &json!({"type": "goal_control_receipt", "data": data}));
}

/// The session thread: runs each job in order and writes its response. Returns when the job
/// channel closes (stdin EOF), after the session has shut down.
fn session_loop(
    mut cx: RpcCx,
    rx: Receiver<Job>,
    cache: Arc<Mutex<Published>>,
    gate: Arc<Mutex<Gate>>,
    bash_running: Arc<AtomicBool>,
    tx: Sender<Job>,
    #[cfg(all(target_os = "linux", feature = "a2a-control"))] steering_authority: Arc<
        oxa_core::goal::SteeringAuthority,
    >,
) -> RunOutcome {
    let mut outcome = RunOutcome::NothingToDo;
    let mut inbox = None;
    let mut inbox_session = None;
    loop {
        let session = &cx.session.inner().sref;
        if inbox_session.as_ref() != Some(session) || inbox.is_none() {
            drop(inbox.take());
            inbox_session = Some(session.clone());
            let id = session.id.clone();
            // A prior wake may have been discarded while this destination was inactive.
            // Re-arm it on return even when its admitted files are already gone.
            if cx.handle.has_inbox_input() && !cx.handle.wrap_up_requested() {
                let mut g = lock(&gate);
                if !g.closed && cx.handle.request_inbox_wake() {
                    g.pending += 1;
                    if tx.send(Job::Inbox(id.clone())).is_err() {
                        g.pending -= 1;
                    }
                }
            }
            let handle = cx.handle.clone();
            let sender = tx.clone();
            let follow_up_gate = gate.clone();
            let follow_up = move |text| {
                let mut gate = lock(&follow_up_gate);
                if gate.closed || handle.wrap_up_requested() {
                    return false;
                }
                if !handle.follow_up_for(&id, text) {
                    return false;
                }
                if !handle.request_inbox_wake() {
                    return true;
                }
                gate.pending += 1;
                if sender.send(Job::Inbox(id.clone())).is_err() {
                    gate.pending -= 1;
                    return false;
                }
                true
            };
            #[cfg(not(all(target_os = "linux", feature = "a2a-control")))]
            let watch = oxa_session::Inbox::watch(&cx.dirs.agent_dir, &session.id, follow_up);
            #[cfg(all(target_os = "linux", feature = "a2a-control"))]
            let watch = {
                use oxa_session::InboxItem;
                let control_gate = gate.clone();
                let sender = tx.clone();
                let out = cx.out.clone();
                let handle = cx.handle.clone();
                let binding = session.clone();
                let steering_authority = steering_authority.clone();
                oxa_session::Inbox::watch_bound(&cx.dirs.agent_dir, session, move |item| match item {
                    InboxItem::FollowUp(text) => follow_up(text),
                    InboxItem::Control(packet) => {
                        let mut gate = lock(&control_gate);
                        if gate.closed || gate.control_pending || handle.wrap_up_requested() {
                            return false;
                        }
                        let request = match oxa_orchestrator::control::decode(packet.as_bytes()) {
                            Ok(request) => request,
                            Err(rejection) => {
                                write_control_receipt(&out, rejection);
                                return true;
                            }
                        };
                        let result_gate = control_gate.clone();
                        let result_out = out.clone();
                        let request = match crate::a2a_control::queue_steering(
                            &handle,
                            &binding,
                            request,
                            &steering_authority,
                            move |receipt| {
                                // Mark admission before publishing a result from the owner boundary.
                                let _gate = lock(&result_gate);
                                write_control_receipt(&result_out, receipt);
                            },
                        ) {
                            Ok(admission) => {
                                write_control_receipt(&out, admission);
                                return true;
                            }
                            Err(request) => request,
                        };
                        let control = BoundControl::new(binding.clone(), request);
                        if sender.send(Job::GoalControl(control)).is_err() {
                            return false;
                        }
                        gate.control_pending = true;
                        gate.pending += 1;
                        true
                    }
                })
            };
            match watch {
                Ok(watch) => inbox = Some(watch),
                Err(e) => write_out(
                    &cx.out,
                    &event_to_json(&AgentEvent::Notice {
                        level: Level::Warning,
                        text: format!("Session inbox unavailable: {e}"),
                    }),
                ),
            }
        }
        let job = if (cx.handle.wrap_up_requested() || cx.handle.interrupted()) && lock(&gate).pending == 0 {
            match rx.try_recv() {
                Ok(job) => job,
                Err(_) => {
                    outcome.include(RunOutcome::Interrupted);
                    break;
                }
            }
        } else {
            match rx.recv_timeout(std::time::Duration::from_millis(20)) {
                Ok(job) => job,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        };
        let inbox_wake = matches!(&job, Job::Inbox(_));
        let (cmd, reply, claimed) = match job {
            Job::Command { cmd, reply, claimed } => (cmd, reply, claimed),
            Job::Outcome(next) => {
                outcome.include(next);
                continue;
            }
            Job::Inbox(id) => {
                cx.handle.clear_inbox_wake();
                (
                    RpcCommand {
                        id: None,
                        kind: "inbox".into(),
                        body: json!({"session_id": id}),
                    },
                    false,
                    false,
                )
            }
            #[cfg(all(target_os = "linux", feature = "a2a-control"))]
            Job::GoalControl(control) => {
                let id = control.id().to_owned();
                let mut sink = cmd_run::OutSink(cx.out.clone());
                let receipt = oxa_proto::panic::contain(|| control.apply(&mut cx.session, &mut sink))
                    .unwrap_or_else(|_| json!({"id": id, "error": codes::INTERNAL}));
                *lock(&cache) = publish(&cx);
                {
                    let mut gate = lock(&gate);
                    gate.control_pending = false;
                    gate.pending -= 1;
                    write_control_receipt(&cx.out, receipt);
                    if gate.pending == 0 {
                        for response in std::mem::take(&mut gate.after_jobs) {
                            write_out(&cx.out, &response);
                        }
                    }
                }
                if let Some(inbox) = &inbox {
                    inbox.retry();
                }
                continue;
            }
            Job::Shutdown => break,
        };
        // Join the old watcher before any explicit session transition. Already admitted
        // follow-ups retain their destination in the core queue.
        if matches!(
            cmd.kind.as_str(),
            "new_session" | "switch_session" | "fork" | "clone"
        ) {
            drop(inbox.take());
        }
        let _ = cx.session.take_outcome();
        let is_bash = cmd.kind == "bash";
        bash_running.store(is_bash, Ordering::SeqCst);
        hold_settled();
        // A1/A2: a panic in a command answers that command with an error; the session thread
        // and the session go on (a run's busy flag is released by its guard).
        let result = oxa_proto::panic::contain(|| {
            if inbox_wake {
                if cmd.body["session_id"].as_str() == Some(&cx.session.inner().sref.id)
                    || cx.handle.has_inbox_input()
                {
                    let mut sink = cmd_run::OutSink(cx.out.clone());
                    let mut ui = RpcUi {
                        out: cx.out.clone(),
                        router: cx.ui.clone(),
                    };
                    Some(cx.session.run_queued(&mut sink, &mut ui).map(|_| Value::Null))
                } else {
                    Some(Ok(Value::Null))
                }
            } else {
                match cmd_run::handle(&mut cx, &cmd) {
                    Some(r) => Some(r),
                    None => handle_session(&mut cx, &cmd),
                }
            }
        })
        .unwrap_or_else(|error| {
            Some(Err(OxaError::new(
                codes::INTERNAL,
                format!("internal error (panic): {error}"),
            )))
        });
        let mut settled = release_settled();
        if let Some(inbox) = &inbox {
            inbox.retry();
        }
        bash_running.store(false, Ordering::SeqCst);
        if claimed && settled.is_empty() {
            // The client was told `started`: it is owed the closing record even when the
            // session ran no turn for the prompt after all.
            settled.push(event_to_json(&AgentEvent::AgentSettled { aborted: false }));
        }
        let current = match &result {
            Some(Err(error)) => RunOutcome::from_error(error),
            None => RunOutcome::Failed(codes::INVALID),
            Some(Ok(_)) if cmd.kind != "prompt" && settled.is_empty() => RunOutcome::Done,
            _ => *cx.session.outcome(),
        };
        for record in &mut settled {
            if let Some(fields) = record.as_object_mut() {
                fields.insert("result".to_string(), current.record());
            }
        }
        let record = match (result, reply) {
            (Some(Ok(data)), true) => Some(response_ok(&cmd, data)),
            (Some(Err(e)), true) => Some(response_err(&cmd, &e)),
            (None, true) => {
                let e = OxaError::new(codes::INVALID, format!("Unknown command: {}", cmd.kind));
                Some(response_err(&cmd, &e))
            }
            // A prompt that failed after it was accepted: the failure is reported as a notice.
            (Some(Err(e)), false) => Some(event_to_json(&AgentEvent::Notice {
                level: Level::Error,
                text: e.message.clone(),
            })),
            (_, false) => None,
        };
        // State first, claims next, records last: see the ordering note at the top.
        let published = publish(&cx);
        *lock(&cache) = published;
        let mut g = lock(&gate);
        g.pending -= 1;
        if claimed {
            g.claimed = false;
            cx.handle.clear_abort_next();
        }
        let out = cx.out.clone();
        for v in record.iter().chain(settled.iter()) {
            write_out(&out, v);
        }
        if g.pending == 0 {
            for v in std::mem::take(&mut g.after_jobs) {
                write_out(&out, &v);
            }
        }
        if cx.handle.wrap_up_requested() || cx.handle.interrupted() {
            // A real prompt failure precedes later reader errors. Cooperative
            // interruption is deferred so it cannot hide an already-reported error.
            if current != RunOutcome::Interrupted {
                outcome.include(current);
            }
            for pending in rx.try_iter() {
                if let Job::Outcome(next) = pending {
                    outcome.include(next);
                }
            }
            outcome.include(RunOutcome::Interrupted);
            break;
        }
        outcome.include(current);
    }
    let mut ui = RpcUi {
        out: cx.out.clone(),
        router: cx.ui.clone(),
    };
    cx.session.shutdown(&mut ui);
    outcome
}

/// Reads commands until EOF, answering each one as the front does.
fn read_commands(mut input: Box<dyn BufRead + Send>, front: &Front) {
    let mut line: Vec<u8> = Vec::new();
    loop {
        line.clear();
        match input.read_until(b'\n', &mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                front.record_error(&OxaError::io("cannot read RPC commands", &e));
                eprintln!("oxa rpc: cannot read commands: {e}");
                break;
            }
        }
        let text = match std::str::from_utf8(&line) {
            Ok(t) => t,
            Err(e) => {
                let err = OxaError::new(codes::INVALID, format!("the line is not UTF-8: {e}"));
                front.record_error(&err);
                write_out(&front.out, &parse_error(&err));
                continue;
            }
        };
        let text = text.trim_end_matches(['\n', '\r']);
        if text.trim().is_empty() {
            continue;
        }
        match parse_command(text) {
            Ok(cmd) => front.accept(cmd),
            Err(e) => {
                front.record_error(&e);
                write_out(&front.out, &parse_error(&e));
            }
        }
    }
}

/// Run RPC mode until stdin closes or the session shuts down. Dispatch order per command:
/// extension-UI responses (`ext_ui`), `handle_run`, `handle_session`, else pi's
/// unknown-command error. Events are written to `out` as records while prompts run. Returns
/// the process exit code.
pub fn serve_outcome(
    session: Session,
    dirs: oxa_config::Dirs,
    input: Box<dyn BufRead + Send>,
    out: Out,
) -> Result<RunOutcome, OxaError> {
    serve_inner(
        session,
        dirs,
        input,
        out,
        #[cfg(all(target_os = "linux", feature = "a2a-control"))]
        Arc::new(|_, with_lease| with_lease(None)),
    )
}

/// Serve an assigned session using the frontend's existing guarded lease read.
#[cfg(all(target_os = "linux", feature = "a2a-control"))]
pub fn serve_outcome_with_steering_authority(
    session: Session,
    dirs: oxa_config::Dirs,
    input: Box<dyn BufRead + Send>,
    out: Out,
    authority: Arc<oxa_core::goal::SteeringAuthority>,
) -> Result<RunOutcome, OxaError> {
    serve_inner(session, dirs, input, out, authority)
}

fn serve_inner(
    session: Session,
    dirs: oxa_config::Dirs,
    input: Box<dyn BufRead + Send>,
    out: Out,
    #[cfg(all(target_os = "linux", feature = "a2a-control"))] steering_authority: Arc<
        oxa_core::goal::SteeringAuthority,
    >,
) -> Result<RunOutcome, OxaError> {
    let handle = session.handle();
    #[cfg(all(target_os = "linux", feature = "a2a-control"))]
    handle.enable_goal_steering();
    let router = Arc::new(UiRouter::new());
    let gate = Arc::new(Mutex::new(Gate::default()));
    let bash_running = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let cx = RpcCx {
        session,
        handle: handle.clone(),
        out: out.clone(),
        ui: router.clone(),
        dirs,
    };
    let cache = Arc::new(Mutex::new(publish(&cx)));
    let worker = {
        let cache = cache.clone();
        let gate = gate.clone();
        let bash_flag = bash_running.clone();
        let tx = tx.clone();
        thread::Builder::new()
            .name("oxa-rpc-session".to_string())
            .spawn(move || {
                session_loop(
                    cx,
                    rx,
                    cache,
                    gate,
                    bash_flag,
                    tx,
                    #[cfg(all(target_os = "linux", feature = "a2a-control"))]
                    steering_authority,
                )
            })
            .map_err(|e| OxaError::io("cannot start the session thread", &e))?
    };
    let control = handle.clone();
    let shutdown_tx = tx.clone();
    let front = Front {
        handle,
        out,
        router: router.clone(),
        cache,
        gate,
        bash_running,
        tx,
    };
    // A blocking stdin reader must not keep the process alive after soft termination. On
    // EOF it closes dialogs and the job channel as before. If the caller terminates while
    // stdin is open, the reader is detached until EOF (the CLI exits immediately afterwards).
    let input_router = router.clone();
    let reader = match thread::Builder::new()
        .name("oxa-rpc-input".to_string())
        .spawn(move || {
            read_commands(input, &front);
            input_router.close();
            {
                let mut gate = lock(&front.gate);
                gate.closed = true;
                let _ = front.tx.send(Job::Shutdown);
            }
            drop(front);
        }) {
        Ok(reader) => reader,
        Err(error) => {
            router.close();
            let _ = shutdown_tx.send(Job::Shutdown);
            let _ = worker.join();
            return Err(OxaError::io("cannot start the RPC input thread", &error));
        }
    };
    while !worker.is_finished() {
        if control.wrap_up_requested() || control.interrupted() {
            router.close();
        }
        thread::sleep(std::time::Duration::from_millis(20));
    }
    router.close();
    if reader.is_finished() {
        let _ = reader.join();
    }
    Ok(worker.join().unwrap_or(RunOutcome::Failed(codes::INTERNAL)))
}

/// Serve RPC commands through EOF and return the aggregate process exit code.
pub fn serve(
    session: Session,
    dirs: oxa_config::Dirs,
    input: Box<dyn BufRead + Send>,
    out: Out,
) -> Result<i32, OxaError> {
    serve_outcome(session, dirs, input, out).map(|outcome| outcome.exit_code())
}
