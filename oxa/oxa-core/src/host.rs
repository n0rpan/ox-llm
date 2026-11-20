//! E4: the `Host` implementation handed to hook sets.
//!
//! Timing (the rule on `HostAction`), as implemented here:
//! * `IsIdle`, `Abort` and queue appends (`Steer` / `FollowUp`) are served from `Shared` at once.
//! * Queries (`GetState`, `GetEntries`, ..) and the sandboxed / policy-gated actions (`Exec`,
//!   `ReadFile`, `WriteFile`, `Fetch`, `CallTool`) block until `serve_pending` answers them.
//! * Appends (`NextTurn`, `AppendEntry`, names, labels, thinking, mode) and session-shape actions
//!   are fire-and-forget: the call returns at once and the core applies them at the next
//!   `serve_pending`. Session-shape actions (`Compact`, `NewSession`, `Fork`, `NavigateTree`,
//!   `SwitchSession`, `Reload`, `SetActiveTools`, `SetModel`, `Shutdown`) wait for settlement
//!   (`settled = true`); `WaitForIdle` blocks until settlement.
//!
//! Held-back requests live in `SessionState::host_deferred` (E-INT 1). Dropping the state drops
//! them, and their callers are answered `internal`.
//!
//! **Serving while the session thread waits (E-INT 2).** `Hooks::fire`, `Hooks::run_command`,
//! `Hooks::attach` and `Tool::run` may block on a `Host::call` made by a handler, an extension
//! tool or a codemode worker. The session thread therefore never waits inside them itself:
//! [`pumped`] runs the call on a scoped helper thread and serves the pending requests on the
//! session thread until the helper is done. A state without a host (`host_rx == None`) runs
//! the call inline, as before.
use crate::agent_loop::persist;
use crate::{SessionState, Shared, ToolSelection};
use oxa_proto::{
    codes, tool_errors, Access, AgentEvent, Answer, CompactReason, DangerAnswer, Decision, Delivery, Entry,
    EntryBody, EventSink, ExecSpec, Exposure, HookPoint, HookReply, Hooks, Host, HostAction, Level, OxaError,
    Tool, ToolCtx, Ui, Value, WireMsg,
};
use serde_json::json;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A `HostAction` waiting for the session thread, with the channel for its answer.
pub struct HostRequest {
    pub action: HostAction,
    pub reply: Sender<Result<Value, OxaError>>,
}

/// `Host` as given to `Hooks::attach`. It never touches the session directly (hooks may call
/// from script threads): every action is sent to the session thread, which answers at the
/// points allowed by the timing rule of `HostAction` (queries at once at the next poll;
/// session-shape actions when the run settles). `IsIdle`, `Abort`, queue appends and
/// `SendUserMessage { deliver: Steer | FollowUp }` are served from `Shared` without a round
/// trip. `call` blocks until answered; a dropped session answers `internal`.
pub struct CoreHost {
    tx: Mutex<Sender<HostRequest>>,
    shared: Arc<Shared>,
}

fn gone() -> OxaError {
    OxaError::new(codes::INTERNAL, "the session that serves this host has ended")
}

/// How far the session is from idle when requests are served.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Serve {
    /// Inside a run (or while a hook point fires): session-shape actions wait.
    Run,
    /// While a command or shortcut handler of a hook set runs. The session is claimed but no
    /// run is in progress, so session-shape actions and `WaitForIdle` are served at once (pi's
    /// command context). Only the actions that would drop the running handler itself wait for
    /// the handler to return (`Reload`, `Shutdown`, `NewSession`, `SwitchSession`).
    Command,
    /// The operation is over: everything is applied except a triggered turn (it stays held
    /// until a caller that can run the loop settles, [`Serve::Idle`]).
    Settled,
    /// Settled, and the caller runs the turns extensions triggered (`sendMessage(..,
    /// {triggerTurn: true})`): `Session::prompt`, `run_command`, `run_shortcut` and
    /// `serve_host`. See [`serve_idle`].
    Idle,
}

std::thread_local! {
    /// Set on the session thread while a background request that is not a run (the boot
    /// preload) pumps the host: a triggering message is held for the next idle serve.
    static HOLD_TRIGGERS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Run `f` with triggering messages held back (see [`HOLD_TRIGGERS`]).
pub(crate) fn holding_triggers<R>(f: impl FnOnce() -> R) -> R {
    let prev = HOLD_TRIGGERS.with(|h| h.replace(true));
    let out = f();
    HOLD_TRIGGERS.with(|h| h.set(prev));
    out
}

fn held_back(level: Serve, a: &HostAction) -> bool {
    match level {
        Serve::Run => defers(a) || (triggers_turn(a) && HOLD_TRIGGERS.with(|h| h.get())),
        Serve::Command => {
            triggers_turn(a)
                || matches!(
                    a,
                    HostAction::Reload
                        | HostAction::Shutdown
                        | HostAction::NewSession
                        | HostAction::SwitchSession { .. }
                )
        }
        Serve::Settled => triggers_turn(a),
        Serve::Idle => false,
    }
}

/// `sendMessage(.., {triggerTurn: true})` with a delivery that may start a run (pi: a
/// `nextTurn` message never triggers one). Served inside a run it is delivered per `deliver`
/// (steering / follow-up queue, no second run); served idle ([`Serve::Idle`]) it is appended as
/// a custom message and the loop runs from the wire.
fn triggers_turn(a: &HostAction) -> bool {
    matches!(
        a,
        HostAction::SendMessage {
            trigger_turn: true,
            deliver: Delivery::Steer | Delivery::FollowUp,
            ..
        }
    )
}

std::thread_local! {
    /// `true` on the helper thread of a [`pumped`] call at [`Serve::Run`]: the run (or the
    /// operation holding the session) cannot settle before that call returns, so a
    /// `WaitForIdle` made from it could never be answered (item 9).
    static DEPENDENT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The error of a `WaitForIdle` the current run depends on (see [`DEPENDENT`]).
pub fn wait_for_idle_refused() -> OxaError {
    OxaError::new(
        codes::BUSY,
        "waitForIdle called from a handler or tool that the current run is waiting for; \
         it can only be used from a command or shortcut handler or outside a run",
    )
}

/// Actions that wait for the run to settle (`WaitForIdle` also blocks its caller).
fn defers(a: &HostAction) -> bool {
    matches!(
        a,
        HostAction::WaitForIdle
            | HostAction::Compact { .. }
            | HostAction::NewSession
            | HostAction::Fork { .. }
            | HostAction::NavigateTree { .. }
            | HostAction::SwitchSession { .. }
            | HostAction::Reload
            | HostAction::SetActiveTools(_)
            | HostAction::SetModel(_)
            | HostAction::Shutdown
    )
}

/// Actions the caller does not wait for: accepted now, applied by `serve_pending`.
fn fire_and_forget(a: &HostAction) -> bool {
    matches!(
        a,
        HostAction::SendMessage { .. }
            | HostAction::SendUserMessage { .. }
            | HostAction::AppendEntry { .. }
            | HostAction::SetSessionName(_)
            | HostAction::SetLabel { .. }
            | HostAction::SetThinking(_)
            | HostAction::SetMode(_)
    ) || (defers(a) && !matches!(a, HostAction::WaitForIdle))
}

impl CoreHost {
    /// Returns the host and the receiving end the session polls (`serve_pending`).
    pub fn new(shared: Arc<Shared>) -> (Arc<CoreHost>, Receiver<HostRequest>) {
        let (tx, rx) = mpsc::channel();
        (
            Arc::new(CoreHost {
                tx: Mutex::new(tx),
                shared,
            }),
            rx,
        )
    }

    fn post(&self, action: HostAction) -> Result<Receiver<Result<Value, OxaError>>, OxaError> {
        let (reply, answer) = mpsc::channel();
        let tx = self.tx.lock().unwrap_or_else(|p| p.into_inner());
        tx.send(HostRequest { action, reply }).map_err(|_| gone())?;
        drop(tx);
        // The request is in the channel before the wake: a front end that waits instead of
        // polling (see `Handle::set_attention_waker`) finds it when it serves the host.
        self.shared.gate.attention.notify();
        Ok(answer)
    }

    fn busy(&self) -> bool {
        self.shared.busy.load(Ordering::SeqCst)
    }

    /// Steer and follow-up messages go straight to the queues (no round trip). Returns `false`
    /// for other actions.
    fn queue_now(&self, action: &HostAction) -> bool {
        let (text, deliver) = match action {
            HostAction::SendMessage { content, deliver, .. } => (content, *deliver),
            HostAction::SendUserMessage { text, deliver } => (text, *deliver),
            _ => return false,
        };
        let mut q = self.shared.queue.lock().unwrap_or_else(|p| p.into_inner());
        match deliver {
            Delivery::Steer => q.steering.push(text.clone()),
            Delivery::FollowUp => q.follow_up.push(text.clone()),
            Delivery::NextTurn => return false,
        }
        drop(q);
        self.shared.dirty.store(true, Ordering::SeqCst);
        true
    }
}

impl Host for CoreHost {
    fn call(&self, action: HostAction) -> Result<Value, OxaError> {
        match &action {
            HostAction::IsIdle => return Ok(Value::Bool(!self.busy())),
            HostAction::Abort => {
                if self.busy() {
                    self.shared.cancel.cancel();
                }
                return Ok(Value::Null);
            }
            HostAction::WaitForIdle if !self.busy() => return Ok(Value::Null),
            _ => {}
        }
        if matches!(action, HostAction::WaitForIdle) && self.busy() && DEPENDENT.with(|d| d.get()) {
            return Err(wait_for_idle_refused());
        }
        // A triggering message goes through the session thread, which knows whether a run is
        // in progress (queue it) or the session settles (run a turn).
        if !triggers_turn(&action) && self.queue_now(&action) {
            return Ok(Value::Null);
        }
        if fire_and_forget(&action) {
            self.post(action)?;
            return Ok(Value::Null);
        }
        let answer = self.post(action)?;
        answer.recv().unwrap_or_else(|_| Err(gone()))
    }
}

/// Serve the requests that are pending on `rx`. `settled = false` (inside a run): queries,
/// appends, `Exec` / `ReadFile` / `WriteFile` / `Fetch` / `CallTool` (through policy) are
/// answered; session-shape actions stay queued in `st.host_deferred`. `settled = true`:
/// everything is applied in arrival order. `CallTool` resolves the name among `st.active` first
/// and then freshly among every hook set's `Hooks::tools()` with a non-`Direct` exposure, so
/// codemode can reach the tools of an MCP server that connected after the epoch began (they
/// never touch the prefix; W3 holds).
///
/// At settlement the run is over: the `busy` flag is lowered while the settled actions run (an
/// epoch may open only when idle) and restored afterwards for the caller that holds it.
///
/// The session itself uses [`serve_host`], which reads `st.host_rx`; this form takes the
/// receiver from the caller (SDK users and tests that keep it themselves).
pub fn serve_pending(
    st: &mut SessionState,
    rx: &Receiver<HostRequest>,
    settled: bool,
    sink: &mut dyn oxa_proto::EventSink,
    ui: &mut dyn oxa_proto::Ui,
) {
    let mut work: Vec<HostRequest> = std::mem::take(&mut st.host_deferred);
    while let Ok(req) = rx.try_recv() {
        work.push(req);
    }
    let level = if settled { Serve::Settled } else { Serve::Run };
    serve_work(st, work, level, sink, ui);
}

/// [`serve_pending`] on the session's own receiver (`st.host_rx`). The loop calls it with
/// `settled = false` between steps; `Session::prompt`, `run_command`, `user_bash` and `compact`
/// call it with `settled = true` before they release the session. A state without a host is a
/// no-op.
pub fn serve_host(
    st: &mut SessionState,
    settled: bool,
    sink: &mut dyn oxa_proto::EventSink,
    ui: &mut dyn oxa_proto::Ui,
) {
    let level = if settled { Serve::Settled } else { Serve::Run };
    serve_level(st, level, sink, ui);
}

/// Settle like [`serve_host`] with `settled = true`, and also take the turns extensions
/// triggered (`sendMessage(.., {triggerTurn: true})`): each triggering message is appended as a
/// custom message entry (pi's `agent.prompt(customMessage)`). Returns `true` when at least one
/// was appended: the caller then runs the loop from the wire (no new user message) and calls
/// this again, so a turn triggered by that run is taken too.
pub(crate) fn serve_idle(st: &mut SessionState, sink: &mut dyn EventSink, ui: &mut dyn Ui) -> bool {
    serve_collect(st, Serve::Idle, sink, ui).1
}

/// Move what arrived on the session's receiver into `st.host_deferred` (order kept: the
/// deferred requests are older) and tell whether anything is waiting to be served. Claims
/// nothing: `Session::serve_host` checks it before it claims the session on an idle tick.
pub(crate) fn host_pending(st: &mut SessionState) -> bool {
    if let Some(rx) = &st.host_rx {
        st.host_deferred.extend(rx.try_iter());
    }
    !st.host_deferred.is_empty()
}

/// Returns `true` when at least one request was applied.
pub(crate) fn serve_level(
    st: &mut SessionState,
    level: Serve,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) -> bool {
    serve_collect(st, level, sink, ui).0
}

/// `(applied any, appended a triggering message)`.
fn serve_collect(
    st: &mut SessionState,
    level: Serve,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) -> (bool, bool) {
    if st.host_rx.is_none() && st.host_deferred.is_empty() {
        return (false, false);
    }
    let mut work: Vec<HostRequest> = std::mem::take(&mut st.host_deferred);
    if let Some(rx) = &st.host_rx {
        while let Ok(req) = rx.try_recv() {
            work.push(req);
        }
    }
    serve_work(st, work, level, sink, ui)
}

fn serve_work(
    st: &mut SessionState,
    work: Vec<HostRequest>,
    level: Serve,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) -> (bool, bool) {
    let mut ready = Vec::new();
    for req in work {
        if held_back(level, &req.action) {
            st.host_deferred.push(req);
        } else {
            ready.push(req);
        }
    }
    if ready.is_empty() {
        return (false, false);
    }

    let lower = level != Serve::Run;
    let was_busy = st.shared.busy.load(Ordering::SeqCst);
    if lower {
        st.shared.busy.store(false, Ordering::SeqCst);
    }
    let mut turn = false;
    for req in ready {
        let HostRequest { action, reply } = req;
        let fire = fire_and_forget(&action);
        let result = if level == Serve::Idle && triggers_turn(&action) {
            let r = append_trigger(st, action, sink);
            turn |= r.is_ok();
            r
        } else {
            apply(st, action, sink, ui)
        };
        match result {
            Ok(v) => {
                let _ = reply.send(Ok(v));
            }
            Err(e) if fire => notice(sink, Level::Error, e.to_string()),
            Err(e) => {
                let _ = reply.send(Err(e));
            }
        }
    }
    if lower {
        st.shared.busy.store(was_busy, Ordering::SeqCst);
    }
    (true, turn)
}

/// Append the message of a triggering `SendMessage` as a custom message entry (in context: the
/// fold replays it as one user message; shown when `display`).
fn append_trigger(
    st: &mut SessionState,
    action: HostAction,
    sink: &mut dyn EventSink,
) -> Result<Value, OxaError> {
    let HostAction::SendMessage {
        custom_type,
        content,
        display,
        details,
        ..
    } = action
    else {
        return Err(OxaError::new(codes::INTERNAL, "not a triggering message"));
    };
    persist(
        st,
        EntryBody::CustomMessage {
            custom_type,
            content,
            display,
            details,
        },
        sink,
    )?;
    Ok(Value::Null)
}

/// A `Ui` shared between the helper thread of [`pumped`] and the session thread: every call
/// takes the lock for its own duration only.
struct LockedUi<'m, 'u>(&'m Mutex<&'u mut dyn Ui>);

impl LockedUi<'_, '_> {
    fn with<R>(&self, f: impl FnOnce(&mut dyn Ui) -> R) -> R {
        let mut guard = self.0.lock().unwrap_or_else(|p| p.into_inner());
        f(&mut **guard)
    }
}

impl Ui for LockedUi<'_, '_> {
    fn select(&mut self, title: &str, options: &[String], timeout_ms: Option<u32>) -> Option<usize> {
        self.with(|u| u.select(title, options, timeout_ms))
    }
    fn confirm(&mut self, title: &str, body: &str, timeout_ms: Option<u32>) -> Option<bool> {
        self.with(|u| u.confirm(title, body, timeout_ms))
    }
    fn approve_publication(&mut self, title: &str, body: &str) -> Option<bool> {
        self.with(|u| u.approve_publication(title, body))
    }

    fn dialog_canceller(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        self.with(|u| u.dialog_canceller())
    }
    fn input(&mut self, title: &str, placeholder: &str) -> Option<String> {
        self.with(|u| u.input(title, placeholder))
    }
    fn editor(&mut self, title: &str, prefill: &str) -> Option<String> {
        self.with(|u| u.editor(title, prefill))
    }
    fn notify(&mut self, text: &str, level: Level) {
        self.with(|u| u.notify(text, level))
    }
    fn set_status(&mut self, key: &str, text: Option<&str>) {
        self.with(|u| u.set_status(key, text))
    }
    fn set_widget(&mut self, key: &str, lines: Option<&[String]>, at: oxa_proto::Placement) {
        self.with(|u| u.set_widget(key, lines, at))
    }
    fn set_title(&mut self, t: &str) {
        self.with(|u| u.set_title(t))
    }
    fn set_editor_text(&mut self, t: &str) {
        self.with(|u| u.set_editor_text(t))
    }
    fn ask_access(&mut self, a: &Access) -> Answer {
        self.with(|u| u.ask_access(a))
    }
    fn ask_dangerous(&mut self, a: &Access, reasons: &[String], keys: &[String]) -> DangerAnswer {
        self.with(|u| u.ask_dangerous(a, reasons, keys))
    }
    fn editor_text(&mut self) -> String {
        self.with(|u| u.editor_text())
    }
    fn set_footer(&mut self, lines: Option<&[String]>) {
        self.with(|u| u.set_footer(lines))
    }
    fn set_header(&mut self, lines: Option<&[String]>) {
        self.with(|u| u.set_header(lines))
    }
    fn set_working_message(&mut self, text: Option<&str>) {
        self.with(|u| u.set_working_message(text))
    }
    fn custom(&mut self, c: Box<dyn oxa_proto::UiComponent>, opts: oxa_proto::CustomOpts) -> Option<Value> {
        self.with(|u| u.custom(c, opts))
    }
    fn interactive(&self) -> bool {
        self.with(|u| u.interactive())
    }
}

/// The shortest and the longest pause between two polls of the host receiver in [`pumped`].
const POLL_MIN: Duration = Duration::from_micros(200);
const POLL_MAX: Duration = Duration::from_millis(10);

/// Run `f` (a call into a hook set or a tool) without blocking the session thread in it
/// (E-INT 2): `f` runs on a scoped helper thread while this thread serves the host requests
/// that arrive, at `level`, and calls `tick` after each pass (the caller's live output). `f`
/// gets the caller's UI; the permission prompts of served requests use the same one, one call
/// at a time. Returns when `f` returns; a panic in `f` is resumed here.
///
/// Without a host (`st.host_rx == None`) `f` runs inline unless an owner-confirm broker also
/// needs the session thread as its dialog pump.
pub(crate) fn pumped<R: Send>(
    st: &mut SessionState,
    level: Serve,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
    tick: &mut dyn FnMut(&mut dyn EventSink),
    f: impl FnOnce(&mut dyn Ui) -> R + Send,
) -> R {
    // Wave H: dialogs `f` opens are reported as `ui_prompt_start` / `ui_prompt_end`, and
    // deferred observer fires (partial tool output) are fired while `f` runs.
    let prompts: crate::fire::PromptLog = Arc::default();
    let tapped = !crate::fire::PromptTap::quiet();
    let mut tap = crate::fire::PromptTap::new(ui, prompts.clone());
    let ui: &mut dyn Ui = if tapped { &mut tap } else { tap.inner_mut() };
    if st.host_rx.is_none() && st.shared.owner_confirm().is_none() {
        let out = f(&mut *ui);
        tick(sink);
        crate::fire::drain(st, sink, ui);
        crate::fire::fire_prompts(st, &prompts, sink, ui);
        return out;
    }
    let cell: Mutex<&mut dyn Ui> = Mutex::new(ui);
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let previous_broker_state = st.shared.owner_confirm().map(|broker| {
        let previous = broker.set_servable(true);
        (broker, previous)
    });
    std::thread::scope(|scope| {
        let shared_ui = &cell;
        let dependent = level == Serve::Run;
        let helper = scope.spawn(move || {
            // Dropped when this closure ends (also by a panic): that is the "done" signal.
            let _done = done_tx;
            DEPENDENT.with(|d| d.set(dependent));
            let mut ui = LockedUi(shared_ui);
            f(&mut ui)
        });
        let mut pause = POLL_MIN;
        while let Err(mpsc::RecvTimeoutError::Timeout) = done_rx.recv_timeout(pause) {
            let mut ui = LockedUi(&cell);
            let served = serve_level(st, level, sink, &mut ui);
            serve_owner_confirm(st, sink, &mut ui);
            tick(sink);
            crate::fire::drain(st, sink, &mut ui);
            crate::fire::fire_prompts(st, &prompts, sink, &mut ui);
            pause = if served {
                POLL_MIN
            } else {
                (pause * 2).min(POLL_MAX)
            };
        }
        tick(sink);
        let mut ui = LockedUi(&cell);
        if let Some((broker, previous)) = previous_broker_state {
            broker.set_servable(previous);
        }
        drain_owner_confirm_records(st, sink);
        crate::fire::drain(st, sink, &mut ui);
        crate::fire::fire_prompts(st, &prompts, sink, &mut ui);
        match helper.join() {
            Ok(out) => out,
            Err(panic) => std::panic::resume_unwind(panic),
        }
    })
}

/// Serve the owner confirm broker from the session thread while a tool runs: show the dialog
/// of a waiting request, then report what was answered (a notice and a session record, so a
/// yolo answer is never silent).
fn serve_owner_confirm(st: &mut SessionState, sink: &mut dyn EventSink, ui: &mut dyn Ui) {
    let Some(broker) = st.shared.owner_confirm() else {
        return;
    };
    broker.serve(ui);
    drain_owner_confirm_records(st, sink);
}

/// Record answers without making the broker servable again after the pumped operation ends.
fn drain_owner_confirm_records(st: &mut SessionState, sink: &mut dyn EventSink) {
    let Some(broker) = st.shared.owner_confirm() else {
        return;
    };
    for r in broker.take_records() {
        let text = format!(
            "owner confirm {} ({}), preview sha256 {}",
            r.verdict.as_str(),
            r.origin.as_str(),
            r.preview_sha256
        );
        sink.emit(&AgentEvent::Notice {
            level: Level::Info,
            text,
        });
        let data = json!({
            "question": r.question,
            "preview_sha256": r.preview_sha256,
            "answer": r.verdict.as_str(),
            "source": r.origin.as_str(),
        });
        let body = EntryBody::Custom {
            custom_type: "owner_confirm".to_string(),
            data,
        };
        let _ = persist(st, body, sink);
    }
}

/// Fire one hook point on one hook set through [`pumped`].
pub(crate) fn fire_hook(
    st: &mut SessionState,
    h: &Arc<dyn Hooks>,
    point: HookPoint,
    payload: &Value,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) -> HookReply {
    // A source with nothing to run at `point` is skipped without a helper thread.
    if !h.wants(point) {
        return HookReply::Continue;
    }
    // A1/A2: a hook set that panics is reported like a throwing handler and treated as
    // `Continue`; the run goes on.
    let out = pumped(st, Serve::Run, sink, ui, &mut |_| {}, |ui| {
        oxa_proto::panic::contain(|| h.fire(point, payload, ui))
    });
    crate::session::report_hook_errors(h.as_ref(), sink);
    out.unwrap_or_else(|error| {
        report_panic(sink, h.as_ref(), point.as_str(), &error);
        HookReply::Continue
    })
}

/// A1/A2: a panic in a hook set, as an `ExtensionError` event.
pub(crate) fn report_panic(sink: &mut dyn EventSink, h: &dyn Hooks, event: &str, error: &str) {
    sink.emit(&AgentEvent::ExtensionError {
        extension_path: h.name().to_string(),
        event: event.to_string(),
        error: format!("internal error (panic): {error}"),
    });
}

/// Fire an observer point on every hook set; the replies are ignored.
pub(crate) fn observe_hooks(
    st: &mut SessionState,
    point: HookPoint,
    payload: &Value,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) {
    for h in st.hooks.clone() {
        let _ = fire_hook(st, &h, point, payload, sink, ui);
    }
}

/// Fire a point on the hook sets in order and return the first reply that is not `Continue`.
pub(crate) fn first_reply(
    st: &mut SessionState,
    point: HookPoint,
    payload: &Value,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) -> HookReply {
    for h in st.hooks.clone() {
        match fire_hook(st, &h, point, payload, sink, ui) {
            HookReply::Continue => {}
            other => return other,
        }
    }
    HookReply::Continue
}

/// Give the session a fresh host and hand it to every hook set (`Hooks::attach`). Called when a
/// session is entered (open, switch). Requests still held for the previous session are dropped
/// (their callers get `internal`). A session without hook sets gets no host.
pub(crate) fn attach_hosts(st: &mut SessionState, sink: &mut dyn EventSink, ui: &mut dyn Ui) {
    st.host_deferred.clear();
    crate::fire::clear_deferred();
    if st.hooks.is_empty() {
        st.host_rx = None;
        return;
    }
    let (host, rx) = CoreHost::new(st.shared.clone());
    st.host_rx = Some(rx);
    for h in st.hooks.clone() {
        let host: Arc<dyn Host> = host.clone();
        let attached = pumped(st, Serve::Run, sink, ui, &mut |_| {}, |_| {
            oxa_proto::panic::contain(|| h.attach(host))
        });
        if let Err(error) = attached {
            report_panic(sink, h.as_ref(), "attach", &error);
        }
    }
}

/// Apply one request on the session thread.
fn apply(
    st: &mut SessionState,
    action: HostAction,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) -> Result<Value, OxaError> {
    match action {
        HostAction::SendMessage {
            content: text,
            deliver,
            ..
        }
        | HostAction::SendUserMessage { text, deliver } => {
            match deliver {
                Delivery::NextTurn => st.next_turn.push(WireMsg::user(text)),
                Delivery::Steer => {
                    st.shared
                        .queue
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .steering
                        .push(text);
                    st.shared.dirty.store(true, Ordering::SeqCst);
                }
                Delivery::FollowUp => {
                    st.shared
                        .queue
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .follow_up
                        .push(text);
                    st.shared.dirty.store(true, Ordering::SeqCst);
                }
            }
            Ok(Value::Null)
        }
        HostAction::AppendEntry { custom_type, data } => {
            persist(st, EntryBody::Custom { custom_type, data }, sink)?;
            Ok(Value::Null)
        }
        HostAction::SetSessionName(name) => {
            let name = name.trim().to_string();
            persist(st, EntryBody::SessionInfo { name: name.clone() }, sink)?;
            let shown = if name.is_empty() { None } else { Some(name) };
            sink.emit(&AgentEvent::SessionInfoChanged { name: shown.clone() });
            crate::fire::session_info_changed(st, shown.as_deref(), sink, ui);
            Ok(Value::Null)
        }
        HostAction::SetLabel { target, label } => {
            if st.tree.get(&target).is_none() {
                return Err(OxaError::new(
                    codes::NOT_FOUND,
                    format!("entry {target} is not in this session"),
                ));
            }
            let label = label.map(|l| l.trim().to_string()).filter(|l| !l.is_empty());
            crate::append_entry(st, EntryBody::Label { target, label })?;
            Ok(Value::Null)
        }
        HostAction::SetThinking(level) => {
            persist(st, EntryBody::ThinkingLevelChange(level), sink)?;
            let previous = std::mem::replace(&mut st.thinking, level);
            sink.emit(&AgentEvent::ThinkingLevelChanged { level });
            crate::fire::thinking_level_select(st, level, previous, sink, ui);
            Ok(Value::Null)
        }
        HostAction::SetMode(mode) => {
            persist(st, EntryBody::ModeChange(mode), sink)?;
            st.mode = mode;
            sink.emit(&AgentEvent::ModeChanged { mode });
            Ok(Value::Null)
        }
        HostAction::GetActiveTools => Ok(Value::Array(
            st.active
                .direct
                .iter()
                .map(|t| Value::String(t.spec().name.clone()))
                .collect(),
        )),
        HostAction::GetAllTools => Ok(Value::Array(st.all_tools.iter().map(tool_json).collect())),
        HostAction::SetActiveTools(names) => {
            let sel = ToolSelection {
                tools: Some(names),
                ..st.selection.clone()
            };
            let (active, warnings) = crate::resolve_toolset(&st.all_tools, &sel);
            for w in warnings {
                notice(sink, Level::Warning, format!("unknown tool in selection: {w}"));
            }
            st.selection = sel;
            st.active = active;
            // Before the first epoch of a session (`session_start`) the selection only shapes
            // that epoch; afterwards a changed tool list is a new epoch (W3).
            if !st.prefix.sections.is_empty() {
                crate::open_epoch(st, "active_tools", false, sink)?;
            }
            Ok(Value::Null)
        }
        HostAction::SetModel(model) => {
            // One door for every model switch (S3 / O-145): `/model`, RPC and extensions.
            crate::session::switch_model(st, &model, crate::OverCap::CompactFirst, sink, ui)?;
            Ok(Value::Null)
        }
        HostAction::GetState => Ok(state_json(st)),
        HostAction::GetEntries => {
            let all = st.store.load(&st.sref)?;
            Ok(Value::Array(entries_json(&all)))
        }
        HostAction::GetBranch => {
            let branch: Vec<Entry> = st.tree.branch(st.tree.leaf()).into_iter().cloned().collect();
            Ok(Value::Array(entries_json(&branch)))
        }
        HostAction::ContextUsage => {
            let tokens = st.transcript.context();
            let window = st.info.n_ctx;
            let percent = if window == 0 {
                0.0
            } else {
                f64::from(tokens) * 100.0 / f64::from(window)
            };
            Ok(json!({ "tokens": tokens, "contextWindow": window, "percent": percent }))
        }
        HostAction::Compact { instructions } => {
            crate::compact_now(st, instructions.as_deref(), CompactReason::Manual, sink, ui)?;
            Ok(Value::Null)
        }
        action @ (HostAction::NewSession | HostAction::SwitchSession { .. }) => {
            if !st.shared.gate.swap_enabled.load(Ordering::SeqCst) {
                return Err(OxaError::new(
                    codes::UNSUPPORTED_IN_OXA,
                    "newSession and switchSession from an extension are not supported in oxa",
                )
                .with_hint("use /new or /resume"));
            }
            let req = match action {
                HostAction::SwitchSession { path } => crate::SwapRequest::Switch(path),
                _ => crate::SwapRequest::New,
            };
            *st.shared.gate.swap.lock().unwrap_or_else(|p| p.into_inner()) = Some(req);
            Ok(Value::Null)
        }
        HostAction::Fork { entry } => {
            let r = crate::fork_at(st, &entry, ui)?;
            Ok(json!({ "id": r.id }))
        }
        HostAction::NavigateTree { target, summarize } => {
            crate::navigate(st, &target, summarize, None, sink, ui)?;
            Ok(Value::Null)
        }
        HostAction::Reload => Ok(Value::Bool(crate::reload(st, sink, ui)?)),
        HostAction::Abort => {
            st.shared.cancel.cancel();
            Ok(Value::Null)
        }
        HostAction::IsIdle => Ok(Value::Bool(!st.shared.busy.load(Ordering::SeqCst))),
        HostAction::WaitForIdle => Ok(Value::Null),
        HostAction::Shutdown => {
            notice(
                sink,
                Level::Info,
                "an extension asked to shut down; the front end exits",
            );
            Ok(Value::Null)
        }
        HostAction::Exec(spec) => exec_gated(st, spec, ui),
        HostAction::ReadFile(path) => {
            let p = oxa_perm::normalize(&path, &st.cwd);
            gate(st, &Access::Read(p.clone()), ui)?;
            std::fs::read_to_string(&p)
                .map(Value::String)
                .map_err(|e| OxaError::io(&format!("read {}", p.display()), &e))
        }
        HostAction::WriteFile { path, data } => {
            let p = oxa_perm::normalize(&path, &st.cwd);
            gate(st, &Access::Write(p.clone()), ui)?;
            std::fs::write(&p, &data)
                .map(|_| Value::Null)
                .map_err(|e| OxaError::io(&format!("write {}", p.display()), &e))
        }
        HostAction::Fetch {
            url,
            method,
            headers,
            body,
        } => fetch(st, &url, &method, &headers, body, ui),
        HostAction::CallTool { name, args } => call_tool(st, &name, &args, sink, ui),
        HostAction::ParkSession => crate::compaction::park(st, sink, ui),
        HostAction::ForkSeed => crate::fork::seed_of(st),
        HostAction::ClearEngineHint(hint) => clear_engine_hint(st, &hint),
        HostAction::GetSessionName => Ok(session_name(st).map_or(Value::Null, Value::String)),
        HostAction::GetThinkingLevel => Ok(Value::String(st.thinking.as_str().to_string())),
        HostAction::GetSettings => Ok(st.settings.merged().clone()),
        HostAction::GetSystemPrompt => {
            // Before the first epoch of a session (e.g. at `session_start`) nothing is frozen
            // yet: the answer is the prefix the epoch is about to freeze, built from the same
            // inputs. Read-only either way (W3).
            if st.prefix.sections.is_empty() {
                let (specs, extra) = crate::epoch_inputs(st);
                let opts = crate::epoch::prefix_opts(st);
                let prefix = crate::build_prefix_with(&st.cwd, &st.resources, &specs, &extra, &opts);
                return Ok(Value::String(prefix.system_text()));
            }
            Ok(Value::String(st.prefix.system_text()))
        }
        HostAction::GetCommands => Ok(Value::Array(commands_json(st))),
        HostAction::ModelInfo(model) => {
            let remote = if model == st.model {
                st.info.caps.remote
            } else {
                st.engine
                    .info(Some(&model))
                    .map(|i| i.caps.remote)
                    .unwrap_or(false)
            };
            // Known to this install: the session's own model, or one of the engine's models.
            let known = model == st.model
                || st.info.models.iter().any(|m| *m == model)
                || st.engine.info(Some(&model)).is_ok();
            Ok(serde_json::json!({ "model": model, "remote": remote, "known": known }))
        }
        HostAction::AccountRunTokens(tokens) => {
            crate::Handle {
                shared: st.shared.clone(),
            }
            .account_tokens(tokens);
            Ok(Value::Null)
        }
        HostAction::EngineLimits => Ok(serde_json::json!({
            "remainingTokens": crate::Handle { shared: st.shared.clone() }.remaining_tokens(),
            "sessionId": st.sref.id,
            "model": st.model,
            // The table mode (plan, workspace or ask): K5b resumes workers no wider than it.
            "mode": st.mode.as_str(),
            "remote": st.info.caps.remote,
            "contextWindow": st.info.n_ctx,
            "maxOutput": st.info.max_output,
            "compactAt": st.info.compact_at,
            "softTokens": st.compact.soft,
            "hintClear": st.info.caps.hint_clear,
            "prefillOnly": st.info.prefill_only,
            // The Rust Flash engine (`park: "disk"`): `builtin:subagent` is off there by default.
            "parkDiskOnly": st.info.caps.park_disk_only,
        })),
    }
}

/// The display name recorded last on the active branch (`None` when unnamed or cleared).
fn session_name(st: &SessionState) -> Option<String> {
    st.tree
        .branch(st.tree.leaf())
        .into_iter()
        .rev()
        .find_map(|e| match &e.body {
            EntryBody::SessionInfo { name } => Some(name.clone()),
            _ => None,
        })
        .filter(|n| !n.is_empty())
}

/// pi's `SlashCommandInfo` list (`pi.getCommands()`, the RPC `get_commands` order): extension
/// commands, then prompt templates, then skills; built-in commands are not listed.
fn commands_json(st: &SessionState) -> Vec<Value> {
    use oxa_proto::Source;
    use oxa_resources::ResourceSource;
    let info = |path: String, src: &ResourceSource, base_dir: Option<String>| {
        let (scope, origin, source) = match src {
            ResourceSource::Project => ("project", "top-level", "local".to_string()),
            ResourceSource::Cli => ("temporary", "top-level", "local".to_string()),
            ResourceSource::Package(p) => ("user", "package", p.clone()),
            _ => ("user", "top-level", "local".to_string()),
        };
        let mut m = json!({"path": path, "source": source, "scope": scope, "origin": origin});
        if let Some(b) = base_dir {
            m["baseDir"] = Value::String(b);
        }
        m
    };
    let mut out = Vec::new();
    for c in crate::all_commands(st) {
        let path = match &c.source {
            Source::Builtin => continue,
            Source::Extension(p) => p.clone(),
            Source::Mcp(server) => format!("mcp:{server}"),
        };
        let skill = c
            .name
            .strip_prefix("skill:")
            .and_then(|n| st.resources.skills.iter().find(|s| s.name == n));
        let template = st
            .resources
            .templates
            .iter()
            .find(|t| t.name == c.name && t.path.display().to_string() == path);
        let (source, source_info) = if let Some(s) = skill {
            let base = Some(s.base_dir.display().to_string());
            ("skill", info(path, &s.source, base))
        } else if let Some(t) = template {
            ("prompt", info(path, &t.source, None))
        } else if matches!(c.source, Source::Mcp(_)) {
            ("prompt", info(path, &ResourceSource::Cli, None))
        } else {
            let scope = if path.starts_with("builtin:") {
                ResourceSource::Cli
            } else if std::path::Path::new(&path).starts_with(&st.cwd) {
                ResourceSource::Project
            } else {
                ResourceSource::User
            };
            ("extension", info(path, &scope, None))
        };
        let mut m = json!({"name": c.name, "source": source, "sourceInfo": source_info});
        if !c.description.is_empty() {
            m["description"] = Value::String(c.description.clone());
        }
        out.push(m);
    }
    out
}

/// `true` for a child hint of session `id`: `<id>.s<n>`, `n` decimal.
pub(crate) fn is_child_hint(id: &str, hint: &str) -> bool {
    hint.strip_prefix(id)
        .and_then(|rest| rest.strip_prefix(".s"))
        .is_some_and(|n| !n.is_empty() && n.len() <= 9 && n.bytes().all(|b| b.is_ascii_digit()))
}

/// EG5: clear a finished child's engine state by its hint. Only this session's own children;
/// the engine refuses while a request runs (the child's cancelled request may still be
/// winding down), so `busy` is retried for up to 3 s.
fn clear_engine_hint(st: &mut SessionState, hint: &str) -> Result<Value, OxaError> {
    if !is_child_hint(&st.sref.id, hint) {
        return Err(OxaError::new(
            codes::DENIED,
            format!("{hint} is not a child session of this session"),
        ));
    }
    // A remote parent may run a local child (#458), whose state the parent's caps don't describe.
    if !st.info.caps.hint_clear && !st.info.caps.remote {
        return Ok(serde_json::json!({ "cleared": false }));
    }
    let end = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        match st.engine.clear_hint(hint) {
            Ok(()) => return Ok(serde_json::json!({ "cleared": true })),
            Err(e) if e.code == codes::BUSY && std::time::Instant::now() < end => {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => return Err(e),
        }
    }
}

/// Policy verdict for `a`; an `Ask` goes to the user, `Session` grants are remembered.
fn gate(st: &mut SessionState, a: &Access, ui: &mut dyn Ui) -> Result<(), OxaError> {
    gate_pinned(st, a, ui).map(|_| ())
}

/// [`gate`] for a command that runs in `cwd` rather than in the session's directory: an
/// extension may pass its own `cwd` to `oxa.exec`. The session's directory stays the workspace
/// boundary, and relative operands resolve where the command really runs: the command is gated
/// in the session's directory as `cd <cwd> && <command>`, which the classifier follows (a `cd`
/// moves the current directory, never the workspace root). So `rm -rf /work/other/victim` and
/// `rm -rf victim` with `cwd` `/work/other` are both outside the workspace, and the prompt or
/// filed request shows the directory the command runs in. One gate, so one answer covers it.
fn gate_at(
    st: &mut SessionState,
    a: &Access,
    cwd: &std::path::Path,
    ui: &mut dyn Ui,
) -> Result<(), OxaError> {
    let session_cwd = st.cwd.clone();
    match a {
        Access::Exec { command, net } if cwd != session_cwd => {
            let located = Access::Exec {
                command: format!(
                    "cd {} && cd {} && {command}",
                    shell_quote(&session_cwd.to_string_lossy()),
                    shell_quote(&cwd.to_string_lossy())
                ),
                net: *net,
            };
            // The wrapper keeps the session directory as the classifier's workspace boundary.
            // `exec_gated` supplies a canonical cwd, so that real directory is also part of the
            // exact command identity an unattended approval records and revalidates.
            gate_pinned_in(st, &located, &session_cwd, ui).map(|_| ())
        }
        _ => gate_pinned_in(st, a, cwd, ui).map(|_| ()),
    }
}

/// `text` as one single-quoted shell word.
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// [`gate`], returning the real location the verdict covers (#518, `Policy::decide_pinned`).
fn gate_pinned(
    st: &mut SessionState,
    a: &Access,
    ui: &mut dyn Ui,
) -> Result<Option<std::path::PathBuf>, OxaError> {
    let cwd = st.cwd.clone();
    gate_pinned_in(st, a, &cwd, ui)
}

/// [`gate_pinned`] in an explicit directory.
fn gate_pinned_in(
    st: &mut SessionState,
    a: &Access,
    cwd: &std::path::Path,
    ui: &mut dyn Ui,
) -> Result<Option<std::path::PathBuf>, OxaError> {
    let (decision, pin) = st.policy.decide_pinned(st.mode, a, cwd);
    match decision {
        Decision::Allow => Ok(pin),
        Decision::Deny(reason) => Err(OxaError::new(codes::DENIED, reason)),
        Decision::Ask => {
            // #1512 S2: a dangerous command asks first; its answer covers only its rule keys.
            let danger = st.policy.danger_ask(a, cwd);
            if !danger.is_empty() {
                // #1512 S5: an extension's exec takes the same unattended answer as the bash
                // tool: refused, or filed at the approval desk for the owner, never paused for.
                if !ui.interactive() {
                    // Classified, and filed, in the directory the command actually runs in.
                    if let Some(text) = st.policy.danger_unattended(a, cwd) {
                        return Err(OxaError::new(codes::DENIED, text));
                    }
                }
                let (keys, reasons): (Vec<String>, Vec<String>) = danger.into_iter().unzip();
                return match ui.ask_dangerous(a, &reasons, &keys) {
                    DangerAnswer::Once => Ok(pin),
                    DangerAnswer::Session | DangerAnswer::Always => {
                        st.policy.grant_danger(&keys);
                        Ok(pin)
                    }
                    DangerAnswer::Deny if !ui.interactive() => Err(OxaError::new(
                        codes::DENIED,
                        tool_errors::DANGEROUS_UNATTENDED.replace("{why}", &reasons.join("; ")),
                    )),
                    DangerAnswer::Deny => Err(OxaError::new(codes::DENIED, tool_errors::DENIED_BY_USER)),
                };
            }
            match ui.ask_access(a) {
                Answer::Once => Ok(pin),
                Answer::Session => {
                    st.policy.remember(a);
                    Ok(pin)
                }
                Answer::Deny if !ui.interactive() => Err(OxaError::new(
                    codes::DENIED,
                    tool_errors::DENIED_UNATTENDED.replace("{what}", &crate::fire::access_title(a)),
                )),
                Answer::Deny => Err(OxaError::new(codes::DENIED, tool_errors::DENIED_BY_USER)),
            }
        }
    }
}

/// Sandboxed command: policy first, then the sandbox (no unsandboxed fallback).
fn exec_gated(st: &mut SessionState, mut spec: ExecSpec, ui: &mut dyn Ui) -> Result<Value, OxaError> {
    if spec.net {
        oxa_proto::check_online("a command with network access")?; // P4: no network grant offline
    }
    // A directory outside the session directory is mounted into the sandbox (writable when the
    // spec says so): it asks like the same file access outside the project (yolo allows, headless
    // without an approver refuses). A directory inside the session directory is unchanged.
    if !oxa_perm::is_inside(&spec.cwd, &st.cwd) {
        let mount = if spec.writable {
            Access::Write(spec.cwd.clone())
        } else {
            Access::Read(spec.cwd.clone())
        };
        gate(st, &mount, ui)?;
    }
    // Pin the directory before checking an exec grant, then hand that same real directory to
    // the sandbox. A symlink swap cannot move execution after its approval was revalidated.
    spec.cwd = std::fs::canonicalize(&spec.cwd).map_err(|e| {
        OxaError::new(
            codes::INVALID,
            format!("cannot resolve exec cwd {}: {e}", spec.cwd.display()),
        )
    })?;
    gate_at(
        st,
        &Access::Exec {
            command: spec.command.clone(),
            net: spec.net,
        },
        &spec.cwd,
        ui,
    )?;
    st.sandbox.check()?;
    let cancel = st.shared.cancel.clone();
    let res = st.sandbox.run(&spec, &cancel, &mut |_: &[u8]| {});
    Ok(json!({
        "output": String::from_utf8_lossy(&res.output).into_owned(),
        "exit": res.exit,
        "timedOut": res.timed_out,
    }))
}

/// HTTP request through the policy's `Net` decision. Only `http://` URLs are sent.
fn fetch(
    st: &mut SessionState,
    url: &str,
    method: &str,
    headers: &[(String, String)],
    body: Option<Vec<u8>>,
    ui: &mut dyn Ui,
) -> Result<Value, OxaError> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| OxaError::new(codes::INVALID, format!("fetch needs an http:// URL: {url}")))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let parsed = oxa_engine::Url::parse(&format!("http://{authority}"))?;
    // O-144: the engine has one inference slot. A request to the session's own engine endpoint
    // would be a second model request beside the run, so it is denied before the policy is
    // even asked; extensions cannot call the model.
    if is_engine_endpoint(&st.info.base_url, &parsed) {
        return Err(OxaError::new(
            codes::DENIED,
            "fetch to the engine endpoint is not allowed: extensions and scripts cannot call the model",
        )
        .with_hint("the engine has one inference slot; it serves the session's own requests only"));
    }
    oxa_proto::check_connect("extension fetch", &parsed.host)?; // P4, before the user is asked
    gate(
        st,
        &Access::Net {
            host: parsed.host.clone(),
        },
        ui,
    )?;
    let header_refs: Vec<(&str, String)> = headers.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
    let body = body.unwrap_or_default();
    let resp = oxa_engine::http_request(&oxa_engine::HttpRequest {
        method,
        url: &parsed,
        path,
        headers: &header_refs,
        body: &body,
        timeout_ms: 30_000,
    })?;
    let mut hdrs = serde_json::Map::new();
    for (k, v) in &resp.headers {
        hdrs.insert(k.clone(), Value::String(v.clone()));
    }
    Ok(json!({
        "status": resp.status,
        "headers": Value::Object(hdrs),
        "body": String::from_utf8_lossy(&resp.body).into_owned(),
    }))
}

/// `true` when `target` is the engine endpoint `base_url` (same port, and the same host or
/// both a loopback name).
fn is_engine_endpoint(base_url: &str, target: &oxa_engine::Url) -> bool {
    let Ok(engine) = oxa_engine::Url::parse(base_url) else {
        return false;
    };
    let loopback = |h: &str| {
        let h = h.trim_start_matches('[').trim_end_matches(']');
        h.eq_ignore_ascii_case("localhost")
            || h == "::1"
            || h == "0.0.0.0"
            || h.parse::<std::net::Ipv4Addr>().is_ok_and(|ip| ip.is_loopback())
    };
    engine.port == target.port
        && (engine.host.eq_ignore_ascii_case(&target.host)
            || (loopback(&engine.host) && loopback(&target.host)))
}

/// A registered tool run through the full path: its access check and the policy, then the tool
/// (through [`pumped`], so a tool that calls the host itself is served), then `shape` (W4).
/// Active tools first, then the hook sets' non-`Direct` tools.
fn call_tool(
    st: &mut SessionState,
    name: &str,
    args: &Value,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) -> Result<Value, OxaError> {
    let tool = find_tool(st, name)
        .ok_or_else(|| OxaError::new(codes::NOT_FOUND, format!("no tool named {name}")))?;
    let accesses = {
        let cancel = st.shared.cancel.clone();
        let cx = ToolCtx {
            checked: &[],
            cwd: &st.cwd,
            spill_dir: &st.spill_dir,
            sandbox: st.sandbox.as_ref(),
            mode: st.mode,
            cancel: &cancel,
            env: &st.tool_env,
            tune: st.tune,
            hidden: st.policy.path_filter_for_mode(st.mode),
        };
        tool.access(args, &cx)
            .map_err(|m| OxaError::new(codes::INVALID, m))?
    };
    let mut checked = Vec::new();
    for a in &accesses {
        if let (Access::Read(p) | Access::Write(p), Some(real)) = (a, gate_pinned(st, a, ui)?) {
            checked.push((p.clone(), real));
        }
    }
    let out = crate::exec::run_tool(st, &tool, args, "", &checked, sink, ui, &mut |_, _| {});
    let text = oxa_tools::shape(
        out.text.as_bytes(),
        crate::tune::output_cap(st),
        false,
        &st.spill_dir,
    );
    Ok(json!({ "text": text, "isError": out.is_error }))
}

fn find_tool(st: &SessionState, name: &str) -> Option<Arc<dyn Tool>> {
    if let Some(t) = st.active.get(name).or_else(|| st.active.get_indirect(name)) {
        return Some(t.clone());
    }
    st.hooks
        .iter()
        .flat_map(|h| h.tools())
        .find(|t| t.spec().name == name && t.spec().exposure != Exposure::Direct)
}

fn tool_json(t: &Arc<dyn Tool>) -> Value {
    let spec = t.spec();
    let source = match &spec.source {
        oxa_proto::Source::Builtin => "builtin".to_string(),
        oxa_proto::Source::Extension(path) => path.clone(),
        oxa_proto::Source::Mcp(server) => format!("mcp:{server}"),
    };
    json!({
        "name": spec.name,
        "description": spec.description,
        "parameters": serde_json::from_str::<Value>(&spec.schema_json).unwrap_or(Value::Null),
        "source": source,
        "exposure": spec.exposure.as_str(),
    })
}

fn state_json(st: &SessionState) -> Value {
    let (steering, follow_up) = st.shared.snapshot();
    let (steering, follow_up) = (steering.len(), follow_up.len());
    json!({
        "sessionId": st.sref.id,
        "model": st.model,
        "thinkingLevel": format!("{:?}", st.thinking).to_lowercase(),
        "mode": st.mode.as_str(),
        "isStreaming": st.shared.busy.load(Ordering::SeqCst),
        "leafId": st.tree.leaf(),
        "messageCount": st.transcript.wire().len(),
        "pendingMessageCount": steering + follow_up + st.next_turn.len(),
        "autoCompaction": st.auto_compaction,
        "autoRetry": st.auto_retry,
        "contextWindow": st.info.n_ctx,
        "models": st.info.models,
    })
}

/// The custom entry type that keeps an extension tool's result `details` (pi stores them on the
/// `toolResult` message; oxa's message entries carry the wire only). Written by `oxa-ext`.
pub const TOOL_DETAILS_ENTRY: &str = "oxa.toolDetails";

/// [`entry_json`] for a list of entries, with pi's `message` object on message entries: `role`
/// (`user`, `assistant`, `toolResult`, `system`), `content`, and for a tool result its
/// `toolCallId`, `toolName` (from the assistant call it answers) and `details` (from a
/// [`TOOL_DETAILS_ENTRY`] entry in the same list).
fn entries_json(entries: &[Entry]) -> Vec<Value> {
    let mut tool_names: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    let mut details: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
    for e in entries {
        match &e.body {
            EntryBody::Msg { wire, .. } => {
                for c in &wire.tool_calls {
                    tool_names.insert(c.id.as_str(), c.name.as_str());
                }
            }
            EntryBody::Custom { custom_type, data } if custom_type == TOOL_DETAILS_ENTRY => {
                if let Some(id) = data.get("toolCallId").and_then(Value::as_str) {
                    details.insert(
                        id.to_string(),
                        data.get("details").cloned().unwrap_or(Value::Null),
                    );
                }
            }
            _ => {}
        }
    }
    entries
        .iter()
        .map(|e| {
            let mut v = entry_json(e);
            if let EntryBody::Msg { wire, .. } = &e.body {
                let role = match wire.role {
                    oxa_proto::Role::Tool => "toolResult",
                    other => other.as_str(),
                };
                let mut m = json!({ "role": role, "content": wire.content });
                if let Some(id) = &wire.tool_call_id {
                    m["toolCallId"] = Value::String(id.clone());
                    if let Some(name) = tool_names.get(id.as_str()) {
                        m["toolName"] = Value::String(name.to_string());
                    }
                    if let Some(d) = details.get(id) {
                        m["details"] = d.clone();
                    }
                }
                if !wire.tool_calls.is_empty() {
                    m["toolCalls"] = Value::Array(
                        wire.tool_calls
                            .iter()
                            .map(|c| json!({ "id": c.id, "name": c.name, "arguments": c.args_raw }))
                            .collect(),
                    );
                }
                v["message"] = m;
            }
            if let EntryBody::Custom { custom_type, data } = &e.body {
                v["customType"] = Value::String(custom_type.clone());
                v["data"] = data.clone();
            }
            v
        })
        .collect()
}

fn entry_json(e: &Entry) -> Value {
    let (kind, content): (&str, Option<String>) = match &e.body {
        EntryBody::Prefix(_) => ("prefix", None),
        EntryBody::Msg { wire, .. } => ("message", wire.content.clone()),
        EntryBody::Shown { text, .. } => ("shown", Some(text.clone())),
        EntryBody::BashExecution { .. } => ("bashExecution", None),
        EntryBody::ModelChange(m) => ("model_change", Some(m.clone())),
        EntryBody::ThinkingLevelChange(_) => ("thinking_level_change", None),
        EntryBody::ModeChange(_) => ("mode_change", None),
        EntryBody::Usage { .. } => ("usage", None),
        EntryBody::Compaction { summary, .. } => ("compaction", Some(summary.clone())),
        EntryBody::ContextEdit { .. } => ("context_edit", None),
        EntryBody::BranchSummary { summary, .. } => ("branch_summary", Some(summary.clone())),
        EntryBody::Custom { custom_type, .. } => ("custom", Some(custom_type.clone())),
        EntryBody::CustomMessage { content, .. } => ("custom_message", Some(content.clone())),
        EntryBody::Label { .. } => ("label", None),
        EntryBody::SessionInfo { name } => ("session_info", Some(name.clone())),
    };
    json!({
        "id": e.id,
        "parentId": e.parent,
        "timestamp": e.ts_ms,
        "type": kind,
        "content": content,
    })
}

fn notice(sink: &mut dyn EventSink, level: Level, text: impl Into<String>) {
    sink.emit(&AgentEvent::Notice {
        level,
        text: text.into(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Handle;
    use oxa_config::{Dirs, Settings};
    use oxa_proto::{Autonomy, Engine, FrozenPrefix, HeadlessUi, Mode, ThinkingLevel, VecSink, YoloSwitch};
    use oxa_sandbox::NoSandbox;
    use oxa_session::{MemStore, SessionStore, Transcript, Tree};

    struct Fixture {
        env: oxa_fake::TestEnv,
        st: SessionState,
        host: Arc<CoreHost>,
        rx: Receiver<HostRequest>,
    }

    /// A session without a file on disk: a `MemStore`, the scripted engine and the default policy.
    fn fixture() -> Fixture {
        let env = oxa_fake::TestEnv::new();
        let settings = Settings::default();
        let cwd = env.work();
        let engine = oxa_fake::ScriptedEngine::new(Vec::new());
        let info = engine.info(None).expect("info");
        let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
        let sref = store.create(&cwd, None, None).expect("create");
        let shared = Arc::new(Shared::new(settings.steering_mode(), settings.follow_up_mode()));
        let (host, rx) = CoreHost::new(shared.clone());
        let compact = crate::session::policy_for(&info, &settings);
        let st = SessionState {
            goal: None,
            output_schema: None,
            engine: Arc::new(engine),
            store,
            hooks: Vec::new(),
            policy: Box::new(oxa_perm::ModePolicy::new()),
            sandbox: Arc::new(NoSandbox),
            settings,
            dirs: Dirs::under(env.root()),
            cwd,
            all_tools: Vec::new(),
            selection: ToolSelection::default(),
            active: crate::Toolset::default(),
            resources: oxa_resources::Resources::default(),
            sref,
            tree: Tree::new(Vec::new()),
            transcript: Transcript::default(),
            prefix: FrozenPrefix {
                sections: Vec::new(),
                tools_json: String::new(),
                sha256: [0u8; 32],
            },
            model: info.model.clone(),
            thinking: ThinkingLevel::Off,
            mode: Mode::Workspace,
            info,
            compact,
            shared,
            next_turn: Vec::new(),
            last_usage: None,
            stats: crate::Stats::default(),
            spill_dir: Dirs::under(env.root()).tmp_dir.join("e4"),
            tool_env: Vec::new(),
            tune: Default::default(),
            auto_compaction: compact.enabled,
            auto_retry: false,
            host_rx: None,
            host_deferred: Vec::new(),
            compact_memo: Default::default(),
            jobs: crate::fresh_jobs(),
        };
        Fixture { env, st, host, rx }
    }

    /// Ask on a fixed script: answers `Once` to every permission prompt and records them.
    #[derive(Default)]
    struct AskUi {
        asked: Vec<Access>,
    }

    impl Ui for AskUi {
        fn select(&mut self, _: &str, _: &[String], _: Option<u32>) -> Option<usize> {
            None
        }
        fn confirm(&mut self, _: &str, _: &str, _: Option<u32>) -> Option<bool> {
            None
        }
        fn input(&mut self, _: &str, _: &str) -> Option<String> {
            None
        }
        fn editor(&mut self, _: &str, _: &str) -> Option<String> {
            None
        }
        fn notify(&mut self, _: &str, _: Level) {}
        fn set_status(&mut self, _: &str, _: Option<&str>) {}
        fn set_widget(&mut self, _: &str, _: Option<&[String]>, _: oxa_proto::Placement) {}
        fn set_title(&mut self, _: &str) {}
        fn set_editor_text(&mut self, _: &str) {}
        fn ask_access(&mut self, a: &Access) -> Answer {
            self.asked.push(a.clone());
            Answer::Once
        }
    }

    /// Serve the pending requests on this thread until `handle` finishes.
    fn serve_until_done<T: Send + 'static>(f: &mut Fixture, handle: std::thread::JoinHandle<T>) -> T {
        let mut sink = VecSink::default();
        let mut ui = HeadlessUi::default();
        while !handle.is_finished() {
            serve_pending(&mut f.st, &f.rx, false, &mut sink, &mut ui);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        handle.join().expect("caller thread")
    }

    /// A child that asks after its foreground tool has returned has nobody to service a dialog,
    /// so it receives `none` instead of waiting behind a broker left servable by the final drain.
    #[test]
    fn pumped_tool_leaves_owner_confirm_unservable() {
        use std::io::{BufRead, Write};
        use std::os::unix::net::UnixStream;

        let mut f = fixture();
        let broker = crate::owner_confirm::OwnerConfirm::start(
            &f.env.root().join("confirm-after-tool"),
            YoloSwitch::new(Autonomy::Standard),
        )
        .expect("start owner confirm broker");
        *f.st
            .shared
            .gate
            .owner_confirm
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(broker.clone());

        let mut sink = VecSink::default();
        let mut ui = HeadlessUi::default();
        pumped(&mut f.st, Serve::Run, &mut sink, &mut ui, &mut |_| {}, |_| ());

        let mut stream = UnixStream::connect(broker.sock_path()).expect("connect after tool");
        writeln!(
            stream,
            "{}",
            json!({"question": "publish?", "preview_sha256": "abc"})
        )
        .expect("send request");
        let mut answer = String::new();
        std::io::BufReader::new(stream)
            .read_line(&mut answer)
            .expect("read answer");
        assert_eq!(
            serde_json::from_str::<Value>(&answer).expect("json")["answer"],
            "none"
        );
    }

    /// #1512 S2: answers the dangerous-command prompt with `answer` and counts the questions.
    struct DangerUi {
        answer: DangerAnswer,
        asked: usize,
    }

    impl Ui for DangerUi {
        fn select(&mut self, _: &str, _: &[String], _: Option<u32>) -> Option<usize> {
            None
        }
        fn confirm(&mut self, _: &str, _: &str, _: Option<u32>) -> Option<bool> {
            None
        }
        fn input(&mut self, _: &str, _: &str) -> Option<String> {
            None
        }
        fn editor(&mut self, _: &str, _: &str) -> Option<String> {
            None
        }
        fn notify(&mut self, _: &str, _: Level) {}
        fn set_status(&mut self, _: &str, _: Option<&str>) {}
        fn set_widget(&mut self, _: &str, _: Option<&[String]>, _: oxa_proto::Placement) {}
        fn set_title(&mut self, _: &str) {}
        fn set_editor_text(&mut self, _: &str) {}
        fn ask_access(&mut self, _: &Access) -> Answer {
            Answer::Deny
        }
        fn ask_dangerous(&mut self, _: &Access, _: &[String], _: &[String]) -> DangerAnswer {
            self.asked += 1;
            self.answer
        }
        fn interactive(&self) -> bool {
            true
        }
    }

    /// #1512 S2: a dangerous exec in a host gate asks through `ask_dangerous`. `Once` runs it;
    /// with no one to ask the refusal names the rule, and a generic exec still runs.
    #[test]
    fn extension_exec_gate_asks_dangerous() {
        let mut f = fixture();
        f.st.policy = Box::new(
            oxa_perm::ModePolicy::new().with_danger(Arc::new(oxa_perm::DangerRules::builtin()), Vec::new()),
        );
        let dangerous = Access::Exec {
            command: "git push --force origin main".into(),
            net: false,
        };
        let mut ui = DangerUi {
            answer: DangerAnswer::Once,
            asked: 0,
        };
        assert!(gate_pinned(&mut f.st, &dangerous, &mut ui).is_ok());
        assert_eq!(ui.asked, 1);
        let mut headless = HeadlessUi::default();
        let err = gate_pinned(&mut f.st, &dangerous, &mut headless).expect_err("no one to ask");
        assert_eq!(err.code, codes::DENIED);
        assert!(
            err.message
                .starts_with("Refused: dangerous command (git-push-force: "),
            "{}",
            err.message
        );
        let plain = Access::Exec {
            command: "cargo build".into(),
            net: false,
        };
        assert!(gate_pinned(&mut f.st, &plain, &mut headless).is_ok());
    }

    /// #1512 S5: an extension's dangerous exec in a run with no one to ask files an approval
    /// request at the desk (request mode), like the bash tool, instead of the S2 dead-end refusal.
    #[test]
    fn extension_exec_gate_unattended_files_an_approval_request() {
        let mut f = fixture();
        let desk =
            Arc::new(oxa_perm::ApprovalDesk::open(&f.env.root().join("agent")).expect("approval desk"));
        f.st.policy = Box::new(
            oxa_perm::ModePolicy::new()
                .with_danger(Arc::new(oxa_perm::DangerRules::builtin()), Vec::new())
                .with_desk(
                    desk.clone(),
                    "s1".into(),
                    oxa_perm::UnattendedMode::Request,
                    false,
                ),
        );
        let dangerous = Access::Exec {
            command: "git push --force origin main".into(),
            net: false,
        };
        let mut headless = HeadlessUi::default();
        let err = gate_pinned(&mut f.st, &dangerous, &mut headless).expect_err("not run");
        assert_eq!(err.code, codes::DENIED);
        assert!(
            err.message.starts_with("Needs owner approval (request "),
            "{}",
            err.message
        );
        assert_eq!(desk.pending().expect("pending").len(), 1);
    }

    /// #1512 S5: an extension may run in a directory other than the session's. The unattended
    /// classification and the filed request use that directory, so the request describes what
    /// will actually run.
    #[test]
    fn extension_exec_unattended_files_the_request_in_the_execution_directory() {
        let mut f = fixture();
        let desk =
            Arc::new(oxa_perm::ApprovalDesk::open(&f.env.root().join("agent")).expect("approval desk"));
        f.st.policy = Box::new(
            oxa_perm::ModePolicy::new()
                .with_danger(Arc::new(oxa_perm::DangerRules::builtin()), Vec::new())
                .with_desk(
                    desk.clone(),
                    "s1".into(),
                    oxa_perm::UnattendedMode::Request,
                    false,
                ),
        );
        let exec_dir = f.env.work().join("deep");
        std::fs::create_dir_all(&exec_dir).expect("exec dir");
        let dangerous = Access::Exec {
            command: "git push --force origin main".into(),
            net: false,
        };
        let mut headless = HeadlessUi::default();
        let err =
            super::gate_pinned_in(&mut f.st, &dangerous, &exec_dir, &mut headless).expect_err("not run");
        assert_eq!(err.code, codes::DENIED);
        let filed = desk.pending().expect("pending");
        assert_eq!(filed.len(), 1, "one request");
        assert_eq!(filed[0].cwd, exec_dir.display().to_string());
    }

    /// #1512 S5: a dangerous exec in a run with no one to ask is not paused for. The tool error
    /// names the approval request, and the same text goes out as a Warning notice in the event
    /// stream (the JSON stream the lead and the runner read). The tool does not run.
    #[test]
    fn refusal_emits_warning_notice_in_json_stream() {
        struct ExecTool(oxa_proto::ToolSpec);
        impl oxa_proto::Tool for ExecTool {
            fn spec(&self) -> &oxa_proto::ToolSpec {
                &self.0
            }
            fn access(
                &self,
                args: &oxa_proto::Value,
                _cx: &oxa_proto::ToolCtx<'_>,
            ) -> Result<Vec<Access>, String> {
                let command = args
                    .get("command")
                    .and_then(oxa_proto::Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                Ok(vec![Access::Exec { command, net: false }])
            }
            fn run(
                &self,
                _args: &oxa_proto::Value,
                _cx: &oxa_proto::ToolCtx<'_>,
                _live: &mut dyn FnMut(&str),
            ) -> oxa_proto::ToolOutput {
                oxa_proto::ToolOutput::error("the tool ran")
            }
        }
        let mut f = fixture();
        let desk =
            Arc::new(oxa_perm::ApprovalDesk::open(&f.env.root().join("agent")).expect("approval desk"));
        f.st.policy = Box::new(
            oxa_perm::ModePolicy::new()
                .with_danger(Arc::new(oxa_perm::DangerRules::builtin()), Vec::new())
                .with_desk(desk, "s1".into(), oxa_perm::UnattendedMode::Request, false),
        );
        f.st.active = crate::Toolset {
            direct: vec![Arc::new(ExecTool(oxa_proto::ToolSpec {
                name: "shell".into(),
                description: "test shell".into(),
                schema_json: "{}".into(),
                class: oxa_proto::ToolClass::Exec,
                exposure: oxa_proto::Exposure::Direct,
                source: oxa_proto::Source::Builtin,
            }))],
            indirect: Vec::new(),
        };
        let call = oxa_proto::ToolCall {
            id: "c1".into(),
            name: "shell".into(),
            args_raw: r#"{"command":"git push --force origin main"}"#.into(),
        };
        let mut sink = oxa_proto::VecSink::default();
        let mut ui = HeadlessUi::default();
        let results = crate::exec::run_calls(&mut f.st, &[call], &mut sink, &mut ui);
        assert_eq!(results.len(), 1);
        assert!(results[0].is_error);
        let notice = sink.0.iter().find_map(|e| match e {
            AgentEvent::Notice { level, text } => Some((*level, text.clone())),
            _ => None,
        });
        let (level, text) = notice.expect("a notice in the event stream");
        assert!(matches!(level, Level::Warning), "{text}");
        assert!(text.starts_with("Needs owner approval (request "), "{text}");
    }

    /// An extension's own `cwd` never moves the workspace boundary: `rm -rf` of an absolute
    /// path outside the session's directory is dangerous even when `cwd` is that path's parent.
    #[test]
    fn extension_exec_cwd_keeps_the_session_workspace_boundary() {
        let mut f = fixture();
        f.st.policy = Box::new(
            oxa_perm::ModePolicy::new().with_danger(Arc::new(oxa_perm::DangerRules::builtin()), Vec::new()),
        );
        let other = f.env.root().join("other");
        std::fs::create_dir_all(other.join("victim")).unwrap();
        let outside = Access::Exec {
            command: format!("rm -rf {}", other.join("victim").display()),
            net: false,
        };
        let mut ui = DangerUi {
            answer: DangerAnswer::Deny,
            asked: 0,
        };
        assert!(gate_at(&mut f.st, &outside, &other, &mut ui).is_err());
        assert_eq!(ui.asked, 1, "the session workspace boundary asked");
        let mut headless = HeadlessUi::default();
        let err = gate_at(&mut f.st, &outside, &other, &mut headless).expect_err("no one to ask");
        assert_eq!(err.code, codes::DENIED);
        // A relative operand resolves in the directory the command runs in.
        let relative = Access::Exec {
            command: "rm -rf victim".into(),
            net: false,
        };
        let err = gate_at(&mut f.st, &relative, &other, &mut headless).expect_err("outside");
        assert_eq!(err.code, codes::DENIED);
        // Inside the workspace a relative delete stays an ordinary command.
        let inside = f.env.work().join("sub");
        std::fs::create_dir_all(inside.join("build")).unwrap();
        let build = Access::Exec {
            command: "rm -rf build".into(),
            net: false,
        };
        assert!(gate_at(&mut f.st, &build, &inside, &mut headless).is_ok());
    }

    /// An unattended extension approval is bound to the real execution directory, not the
    /// session root. Retargeting a symlink cwd therefore needs a new owner approval.
    #[cfg(unix)]
    #[test]
    fn extension_exec_approval_is_bound_to_its_canonical_cwd() {
        let mut f = fixture();
        let desk =
            Arc::new(oxa_perm::ApprovalDesk::open(&f.env.root().join("agent")).expect("approval desk"));
        f.st.policy = Box::new(
            oxa_perm::ModePolicy::new()
                .with_danger(Arc::new(oxa_perm::DangerRules::builtin()), Vec::new())
                .with_desk(
                    desk.clone(),
                    "s1".into(),
                    oxa_perm::UnattendedMode::Request,
                    false,
                ),
        );
        let repo_a = f.env.work().join("repo-a");
        let repo_b = f.env.work().join("repo-b");
        std::fs::create_dir_all(&repo_a).unwrap();
        std::fs::create_dir_all(&repo_b).unwrap();
        let link = f.env.work().join("repo");
        std::os::unix::fs::symlink(&repo_a, &link).unwrap();
        let spec = || ExecSpec {
            command: "git push --force origin main".into(),
            cwd: link.clone(),
            writable: false,
            net: false,
            timeout_s: None,
            env: Vec::new(),
        };
        let mut headless = HeadlessUi::default();

        let request = exec_gated(&mut f.st, spec(), &mut headless).expect_err("owner approval requested");
        let request_id = request
            .message
            .strip_prefix("Needs owner approval (request ")
            .and_then(|text| text.split_once("): "))
            .map(|(id, _)| id)
            .expect("request id in refusal");
        desk.answer(request_id, "session").expect("approve request");
        assert_eq!(
            exec_gated(&mut f.st, spec(), &mut headless)
                .expect_err("sandbox unavailable")
                .code,
            codes::NO_SANDBOX
        );

        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&repo_b, &link).unwrap();
        let next =
            exec_gated(&mut f.st, spec(), &mut headless).expect_err("retargeted cwd needs another approval");
        assert!(next.message.starts_with("Needs owner approval (request "));
    }

    /// O-386: a host request posted while the front end waits (instead of polling) wakes it, so
    /// the request is served at once and not at the front end's next idle tick. The request is
    /// in the channel before the wake, so a woken front end finds it.
    #[test]
    fn a_posted_host_request_wakes_the_front_end() {
        let f = fixture();
        let fired = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = fired.clone();
        let handle = Handle {
            shared: f.st.shared.clone(),
        };
        handle.set_attention_waker(Arc::new(move || {
            count.fetch_add(1, Ordering::SeqCst);
        }));
        f.host
            .call(HostAction::SendMessage {
                custom_type: "test".into(),
                content: "wake".into(),
                display: true,
                details: Value::Null,
                deliver: Delivery::FollowUp,
                trigger_turn: true,
            })
            .expect("accepted");
        assert_eq!(
            fired.load(Ordering::SeqCst),
            1,
            "the posted request wakes the front end"
        );
        assert!(
            f.rx.try_recv().is_ok(),
            "the request is in the channel the front end serves"
        );
    }

    /// #554 / #557: host writes use the same truthful unattended refusal as built-in tools,
    /// without promising workarounds that Ask mode or protected paths would refuse too.
    #[test]
    fn host_unattended_write_requires_approval() {
        let mut f = fixture();
        for (mode, path) in [
            (Mode::Ask, f.st.cwd.join("out.txt")),
            (Mode::Workspace, f.st.cwd.join(".oxa/settings.json")),
            (Mode::Workspace, f.env.tmp().join("scratch.txt")),
        ] {
            f.st.mode = mode;
            let host = f.host.clone();
            let target = path.clone();
            let caller = std::thread::spawn(move || {
                host.call(HostAction::WriteFile {
                    path: target,
                    data: "must not be written".into(),
                })
            });
            let err = serve_until_done(&mut f, caller).expect_err("approval is unavailable");
            assert_eq!(err.code, codes::DENIED);
            assert_eq!(
                err.message,
                format!(
                    "Permission denied: write {} needs approval, but this run cannot ask (non-interactive), so it was refused; nobody declined it.",
                    path.display()
                )
            );
            assert!(!path.exists(), "an unattended refusal must not write the file");
        }
    }

    /// Queries answer during a run: the caller blocks until the next poll, which runs mid-run.
    #[test]
    fn host_queries_answer_mid_run() {
        let mut f = fixture();
        let host = f.host.clone();
        let caller = std::thread::spawn(move || host.call(HostAction::GetState));
        let state = serve_until_done(&mut f, caller).expect("state");
        assert_eq!(state["mode"], json!("workspace"));
        assert_eq!(state["leafId"], json!(f.st.tree.leaf()));
    }

    /// `IsIdle` is answered at once from `Shared`, no poll needed.
    #[test]
    fn idle_and_abort_do_not_wait_for_a_poll() {
        let f = fixture();
        assert_eq!(f.host.call(HostAction::IsIdle), Ok(Value::Bool(true)));
        f.st.shared.busy.store(true, Ordering::SeqCst);
        assert_eq!(f.host.call(HostAction::IsIdle), Ok(Value::Bool(false)));
        assert_eq!(f.host.call(HostAction::Abort), Ok(Value::Null));
        assert!(f.st.shared.cancel.is_cancelled());
    }

    /// Session-shape actions are held until the run settles; the model is unchanged until then.
    #[test]
    fn session_shape_actions_deferred_to_settle() {
        let mut f = fixture();
        f.st.shared.busy.store(true, Ordering::SeqCst);
        let before = f.st.model.clone();
        assert_eq!(
            f.host.call(HostAction::SetModel("other-model".into())),
            Ok(Value::Null)
        );
        let mut sink = VecSink::default();
        let mut ui = HeadlessUi::default();
        serve_pending(&mut f.st, &f.rx, false, &mut sink, &mut ui);
        assert_eq!(f.st.model, before, "not applied during the run");
        serve_pending(&mut f.st, &f.rx, true, &mut sink, &mut ui);
        assert_eq!(f.st.model, "other-model", "applied at settlement");
        assert!(
            f.st.shared.busy.load(Ordering::SeqCst),
            "the caller keeps its claim"
        );
    }

    /// `WaitForIdle` resolves at settlement, not before.
    #[test]
    fn wait_for_idle_resolves_at_settle() {
        let mut f = fixture();
        f.st.shared.busy.store(true, Ordering::SeqCst);
        let host = f.host.clone();
        let waiter = std::thread::spawn(move || host.call(HostAction::WaitForIdle));
        let mut sink = VecSink::default();
        let mut ui = HeadlessUi::default();
        for _ in 0..20 {
            serve_pending(&mut f.st, &f.rx, false, &mut sink, &mut ui);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(!waiter.is_finished(), "must still wait during the run");
        serve_pending(&mut f.st, &f.rx, true, &mut sink, &mut ui);
        assert_eq!(waiter.join().expect("join"), Ok(Value::Null));
    }

    /// A pending `WaitForIdle` is released with an error when the session is dropped (item 9).
    #[test]
    fn pending_wait_is_released_when_the_session_drops() {
        let mut f = fixture();
        f.st.shared.busy.store(true, Ordering::SeqCst);
        let host = f.host.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(host.call(HostAction::WaitForIdle));
        });
        let mut sink = VecSink::default();
        let mut ui = HeadlessUi::default();
        while f.st.host_deferred.is_empty() {
            serve_pending(&mut f.st, &f.rx, false, &mut sink, &mut ui);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let Fixture { st, rx: host_rx, .. } = f;
        drop(st);
        drop(host_rx);
        let r = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("released, not hanging");
        assert_eq!(r.expect_err("an error").code, codes::INTERNAL);
    }

    /// A triggering message is held by a plain settle (no caller to run the turn) and taken by
    /// [`serve_idle`], which appends it as a custom message.
    #[test]
    fn trigger_waits_for_a_caller_that_can_run() {
        let mut f = fixture();
        f.st.host_rx = Some(std::mem::replace(&mut f.rx, mpsc::channel().1));
        f.host
            .call(HostAction::SendMessage {
                custom_type: "t".into(),
                content: "go".into(),
                display: false,
                details: Value::Null,
                deliver: Delivery::Steer,
                trigger_turn: true,
            })
            .expect("accepted");
        assert!(f.st.shared.take_steering().is_empty(), "not queued while idle");
        let mut sink = VecSink::default();
        let mut ui = HeadlessUi::default();
        serve_host(&mut f.st, true, &mut sink, &mut ui);
        assert_eq!(f.st.host_deferred.len(), 1, "held by a plain settle");
        assert!(serve_idle(&mut f.st, &mut sink, &mut ui));
        assert_eq!(f.st.transcript.wire(), &[WireMsg::user("go")]);
        assert!(!serve_idle(&mut f.st, &mut sink, &mut ui), "taken once");
    }

    /// `WaitForIdle` on an idle session returns at once.
    #[test]
    fn wait_for_idle_when_idle_returns_now() {
        let f = fixture();
        assert_eq!(f.host.call(HostAction::WaitForIdle), Ok(Value::Null));
    }

    /// A dropped session answers `internal` instead of hanging.
    #[test]
    fn dropped_session_answers_internal() {
        let f = fixture();
        let Fixture { host, rx, .. } = f;
        drop(rx);
        let err = host.call(HostAction::GetState).expect_err("must fail");
        assert_eq!(err.code, codes::INTERNAL);
        let err = host.call(HostAction::Reload).expect_err("fire-and-forget too");
        assert_eq!(err.code, codes::INTERNAL);
    }

    /// Entries and the branch are read through the session file, root first.
    #[test]
    fn session_read_api() {
        let mut f = fixture();
        crate::append_entry(&mut f.st, EntryBody::SessionInfo { name: "demo".into() }).expect("append");
        let host = f.host.clone();
        let entries = std::thread::spawn(move || host.call(HostAction::GetEntries));
        let all = serve_until_done(&mut f, entries).expect("entries");
        assert!(all.as_array().is_some_and(|a| !a.is_empty()));
        let host = f.host.clone();
        let branch = std::thread::spawn(move || host.call(HostAction::GetBranch));
        let branch = serve_until_done(&mut f, branch).expect("branch");
        assert_eq!(branch.as_array().map(Vec::len), Some(1));
        assert_eq!(branch[0]["type"], json!("session_info"));
    }

    /// Context usage is read from the transcript and the window of the model; compaction with an
    /// empty transcript is a no-op at settlement.
    #[test]
    fn compact_and_context_usage() {
        let mut f = fixture();
        let host = f.host.clone();
        let usage = std::thread::spawn(move || host.call(HostAction::ContextUsage));
        let usage = serve_until_done(&mut f, usage).expect("usage");
        assert_eq!(usage["contextWindow"], json!(f.st.info.n_ctx));
        assert_eq!(usage["tokens"], json!(0));
        assert_eq!(usage["percent"], json!(0.0));
        f.st.shared.busy.store(true, Ordering::SeqCst);
        assert_eq!(
            f.host.call(HostAction::Compact { instructions: None }),
            Ok(Value::Null)
        );
        let mut sink = VecSink::default();
        let mut ui = HeadlessUi::default();
        serve_pending(&mut f.st, &f.rx, true, &mut sink, &mut ui);
        assert!(f.st.shared.busy.load(Ordering::SeqCst));
    }

    /// Exec goes through the policy, then the sandbox. Plan mode denies a write-class command
    /// outright; here the sandbox is missing, so an allowed command stops with `no_sandbox`.
    #[test]
    fn exec_through_policy_and_sandbox() {
        let mut f = fixture();
        f.st.mode = Mode::Plan;
        let cwd = f.st.cwd.clone();
        let spec = |cmd: &str| ExecSpec {
            command: cmd.to_string(),
            cwd: cwd.clone(),
            writable: false,
            net: false,
            timeout_s: None,
            env: Vec::new(),
        };
        let mut sink = VecSink::default();
        let mut ui = AskUi::default();
        // Deny path: plan mode refuses a command that writes.
        let host = f.host.clone();
        let denied_spec = spec("rm -rf target");
        let caller = std::thread::spawn(move || host.call(HostAction::Exec(denied_spec)));
        while !caller.is_finished() {
            serve_pending(&mut f.st, &f.rx, false, &mut sink, &mut ui);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let denied = caller.join().expect("join").expect_err("denied");
        assert_eq!(denied.code, codes::DENIED);
        // Ask path: in workspace mode a command that is not read-only asks, the answer is Once,
        // and the sandbox then refuses (no bwrap in tests).
        f.st.mode = Mode::Ask;
        let host = f.host.clone();
        let asked_spec = spec("touch made-by-test");
        let caller = std::thread::spawn(move || host.call(HostAction::Exec(asked_spec)));
        while !caller.is_finished() {
            serve_pending(&mut f.st, &f.rx, false, &mut sink, &mut ui);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let err = caller.join().expect("join").expect_err("sandbox missing");
        assert_eq!(err.code, codes::NO_SANDBOX);
        assert_eq!(ui.asked.len(), 1, "the user was asked once");
    }

    /// Files: a read outside the working directory asks; a write inside it needs no prompt in
    /// workspace mode; plan mode refuses writes.
    #[test]
    fn fs_through_policy() {
        let mut f = fixture();
        let mut sink = VecSink::default();
        let mut ui = AskUi::default();
        let inside = f.st.cwd.join("note.txt");
        let host = f.host.clone();
        let write = HostAction::WriteFile {
            path: inside.clone(),
            data: b"hello".to_vec(),
        };
        let caller = std::thread::spawn(move || host.call(write));
        while !caller.is_finished() {
            serve_pending(&mut f.st, &f.rx, false, &mut sink, &mut ui);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        caller.join().expect("join").expect("write inside cwd");
        assert_eq!(std::fs::read_to_string(&inside).unwrap(), "hello");
        assert!(ui.asked.is_empty(), "no prompt inside cwd in workspace mode");

        let outside = f.env.root().join("outside.txt");
        std::fs::write(&outside, "secret-free").unwrap();
        let host = f.host.clone();
        let read = HostAction::ReadFile(outside);
        let caller = std::thread::spawn(move || host.call(read));
        while !caller.is_finished() {
            serve_pending(&mut f.st, &f.rx, false, &mut sink, &mut ui);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(
            caller.join().expect("join").expect("read after the prompt"),
            json!("secret-free")
        );
        assert_eq!(ui.asked.len(), 1, "a read outside cwd asks once");

        f.st.mode = Mode::Plan;
        let host = f.host.clone();
        let write = HostAction::WriteFile {
            path: inside,
            data: b"again".to_vec(),
        };
        let caller = std::thread::spawn(move || host.call(write));
        while !caller.is_finished() {
            serve_pending(&mut f.st, &f.rx, false, &mut sink, &mut ui);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(
            caller.join().expect("join").expect_err("plan refuses").code,
            codes::DENIED
        );
    }

    /// Fetch needs a net grant: plan mode refuses before any connection is made.
    #[test]
    fn fetch_needs_net_grant() {
        let mut f = fixture();
        f.st.mode = Mode::Plan;
        let mut sink = VecSink::default();
        let mut ui = AskUi::default();
        let host = f.host.clone();
        let fetch = HostAction::Fetch {
            url: "http://127.0.0.1:9/never".into(),
            method: "GET".into(),
            headers: Vec::new(),
            body: None,
        };
        let caller = std::thread::spawn(move || host.call(fetch));
        while !caller.is_finished() {
            serve_pending(&mut f.st, &f.rx, false, &mut sink, &mut ui);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(
            caller.join().expect("join").expect_err("denied").code,
            codes::DENIED
        );
        assert!(ui.asked.is_empty());
    }

    /// Messages for the next turn are queued in order and do not wait for a poll.
    #[test]
    fn next_turn_messages_are_queued_at_once() {
        let mut f = fixture();
        let host = f.host.clone();
        host.call(HostAction::SendMessage {
            custom_type: "t".into(),
            content: "first".into(),
            display: false,
            details: Value::Null,
            deliver: Delivery::NextTurn,
            trigger_turn: false,
        })
        .expect("accepted");
        host.call(HostAction::SendUserMessage {
            text: "second".into(),
            deliver: Delivery::NextTurn,
        })
        .expect("accepted");
        let mut sink = VecSink::default();
        let mut ui = HeadlessUi::default();
        serve_pending(&mut f.st, &f.rx, false, &mut sink, &mut ui);
        let texts: Vec<String> = f.st.next_turn.iter().filter_map(|m| m.content.clone()).collect();
        assert_eq!(texts, vec!["first".to_string(), "second".to_string()]);
    }

    /// Steer and follow-up messages go to the queues directly.
    #[test]
    fn steer_and_follow_up_use_the_queues() {
        let f = fixture();
        f.host
            .call(HostAction::SendUserMessage {
                text: "now".into(),
                deliver: Delivery::Steer,
            })
            .expect("steer");
        f.host
            .call(HostAction::SendUserMessage {
                text: "later".into(),
                deliver: Delivery::FollowUp,
            })
            .expect("follow up");
        assert_eq!(f.st.shared.take_steering(), vec!["now".to_string()]);
        assert_eq!(f.st.shared.take_follow_up(), vec!["later".to_string()]);
    }
}
