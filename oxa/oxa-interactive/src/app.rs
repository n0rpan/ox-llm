//! D1: the interactive main loop and the pure event reducer. Owner: task D1. Signatures below are frozen; bodies are yours.
// ported from pi packages/coding-agent/src/modes/interactive/interactive-mode.ts
use crate::driver::Loop;
use crate::{
    render_block, Activity, Block, ImageRef, InteractiveOpts, Reopen, ReopenFn, SessionCmd, SessionInfo,
    StatusModel, ToolBlock, ToolState, UiProxy, UiRequest, ViewCx, ViewModel, ViewSettings, WorkerMsg,
};
use oxa_config::Keymap;
use oxa_core::{expand_input, Expanded, Handle, OverCap, Session, State, Stats, SwitchPlan};
use oxa_export::{export_html, write_export, ExportOpts, ExportTheme};
use oxa_proto::{
    codes, shown_why, AgentEvent, Entry, EntryBody, EventMessage, EventSink, Hooks, Level, MessageDelta,
    OxaError, Part, Role, StopReason, ThinkingLevel, ToolOutput, Ui, Value,
};
use oxa_tui::{
    appearance_from_colorfgbg, detect_caps, load_omarchy_palette, omarchy_colors_paths, oxa_builtin_palette,
    oxa_theme, resolve_theme, Appearance, ProcessTerminal, TermCaps, TermOpts, Terminal, TerminalColors,
    Theme,
};
use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Run the interactive mode on the process terminal until the user quits.
///
/// Returns the process exit code (0 normally). On exit the terminal is restored and pi's exit
/// output is printed (the session file path and the `oxa -c` / `--session` resume hint).
/// Errors: `invalid` when stdin/stdout is not a terminal; `io`.
///
/// Sequence: detect capabilities, open the terminal, install the panic hook, load the theme,
/// spawn the worker thread with `session`, rebuild the view model from the active branch, send
/// the initial prompt, then [`run_loop`].
pub fn run(session: Session, mut opts: InteractiveOpts) -> Result<i32, OxaError> {
    let detected = opts.caps.unwrap_or_else(|| detect_caps(&opts.env));
    let mut term = ProcessTerminal::open(TermOpts {
        caps: detected,
        ..TermOpts::default()
    })?;
    opts.caps = Some(term.caps());
    let (code, last) = run_session(&mut term, session, &mut opts);
    drop(term);
    if let Some(info) = last {
        let talked = info
            .file
            .as_deref()
            .is_some_and(|f| has_conversation(opts.store.as_ref(), f, &info.id));
        if talked {
            for line in exit_lines(info.file.as_deref(), &info.id) {
                println!("{line}");
            }
        }
    }
    Ok(code)
}

/// The profile word of the start-up header: `local` under the `local` profile
/// (`OXA_PROFILE`, else the `oxa.profile` setting, else the default), nothing under `pi`.
pub fn profile_word(env: &[(String, String)], settings: &oxa_config::Settings) -> Option<String> {
    let valid = |v: &str| matches!(v, "pi" | "local").then(|| v.to_string());
    let from_env = env
        .iter()
        .find(|(k, _)| k == "OXA_PROFILE")
        .and_then(|(_, v)| valid(v.trim()));
    let profile = from_env
        .or_else(|| settings.string("oxa.profile").and_then(|v| valid(v.trim())))
        .unwrap_or_else(|| oxa_proto::Profile::DEFAULT.as_str().to_string());
    (profile == "local").then_some(profile)
}

/// pi interactive-mode.ts `showLoadedResources`, compact form: `Context` (the context files
/// in prompt order, relative to the cwd or `~`-shortened), `Skills` (names, sorted),
/// `Prompts` (`/name`, sorted), `Extensions` (entry file labels: the file name, or the
/// directory of an `index.js` / `index.ts`). Empty sections are left out.
pub fn loaded_resources(
    res: &oxa_resources::Resources,
    cwd: &Path,
    home: &Path,
) -> Vec<(String, Vec<String>)> {
    let shown = |p: &Path| -> String {
        if let Ok(rel) = p.strip_prefix(cwd) {
            return rel.display().to_string();
        }
        if !home.as_os_str().is_empty() {
            if let Ok(rel) = p.strip_prefix(home) {
                return format!("~/{}", rel.display());
            }
        }
        p.display().to_string()
    };
    let mut out = Vec::new();
    let context: Vec<String> = res.context_files.iter().map(|c| shown(&c.path)).collect();
    out.push(("Context".to_string(), context));
    let mut skills: Vec<String> = res.skills.iter().map(|s| s.name.clone()).collect();
    skills.sort();
    out.push(("Skills".to_string(), skills));
    let mut prompts: Vec<String> = res.templates.iter().map(|t| format!("/{}", t.name)).collect();
    prompts.sort();
    out.push(("Prompts".to_string(), prompts));
    let mut extensions: Vec<String> = res
        .extension_paths
        .iter()
        .map(|p| {
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if name == "index.js" || name == "index.ts" {
                p.parent()
                    .and_then(|d| d.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or(name)
            } else {
                name
            }
        })
        .collect();
    extensions.sort();
    out.push(("Extensions".to_string(), extensions));
    out.retain(|(_, items)| !items.is_empty());
    out
}

/// [`run`] on an arbitrary terminal (D-INT; the end-to-end tests pass a scripted terminal):
/// spawn the worker with `session`, run the UI loop, then stop the worker. Returns the exit
/// code and the session the worker ended with (`/new`, `/resume`, `/fork` and `/clone` swap
/// it; the exit lines must name the one the user left).
///
/// Quit is a stop signal, not one more queued command: a run in progress is aborted, queued
/// steering and follow-up messages are dropped, dialogs the worker still asks for are answered
/// with `None` / `Deny`, and commands still waiting in the channel are never started (see
/// [`worker_loop`]). Nothing is left running when this returns.
pub fn run_session(
    term: &mut dyn Terminal,
    session: Session,
    opts: &mut InteractiveOpts,
) -> (i32, Option<SessionInfo>) {
    let state = session.state();
    let stats = session.stats();
    let inner = session.inner();
    let leaf = inner.tree.leaf().to_string();
    let branch: Vec<Entry> = inner.tree.branch(&leaf).into_iter().cloned().collect();
    let cwd = inner.cwd.clone();
    // pi: `quietStartup: "header"` keeps the header and hides the resource listing.
    let resources = if opts.settings.string("quietStartup").as_deref() == Some("header") {
        Vec::new()
    } else {
        loaded_resources(&inner.resources, &cwd, &opts.dirs.home)
    };

    let mut vm = ViewModel {
        blocks: blocks_from_entries(&branch),
        ..ViewModel::default()
    };
    for (level, text) in std::mem::take(&mut opts.notices) {
        vm.blocks.push(Block::Notice { level, text });
    }
    let prev = StatusModel {
        cwd: cwd.display().to_string(),
        home: opts.dirs.home.display().to_string(),
        git_branch: git_branch(&cwd),
        ..StatusModel::default()
    };
    vm.status = status_from(&state, &stats, &prev);

    let (cmd_tx, cmd_rx) = mpsc::channel::<SessionCmd>();
    let (msg_tx, msg_rx) = mpsc::channel::<WorkerMsg>();
    let (ui_tx, ui_rx) = mpsc::channel::<UiRequest>();
    let handle = session.handle();

    if let Some(text) = opts.initial_prompt.take() {
        let parts = std::mem::take(&mut opts.initial_parts);
        let _ = cmd_tx.send(SessionCmd::Prompt { text, parts });
    }
    for text in std::mem::take(&mut opts.initial_follow_ups) {
        let _ = cmd_tx.send(SessionCmd::Prompt {
            text,
            parts: Vec::new(),
        });
    }

    let reopen = std::mem::replace(
        &mut opts.reopen,
        Box::new(
            |_: Reopen, _: Option<(String, ThinkingLevel)>| -> Result<Session, OxaError> {
                Err(OxaError::new(
                    codes::INVALID,
                    "switching sessions is not available here",
                ))
            },
        ),
    );
    let stop = WorkerStop::new();
    let worker_stop = stop.clone();
    let spawned = thread::Builder::new()
        .name("oxa-worker".to_string())
        .spawn(move || worker_loop_with(session, reopen, cmd_rx, msg_tx, ui_tx, worker_stop));
    let worker = match spawned {
        Ok(w) => w,
        Err(_) => return (1, None),
    };

    let (code, mut handle) = drive(
        term,
        opts,
        handle,
        cmd_tx.clone(),
        &msg_rx,
        &ui_rx,
        vm,
        stop.clone(),
        resources,
    );

    // Stop: the signal overtakes queued commands in the worker; a run that is in progress (or
    // that the worker began in the same instant) is aborted until the worker is gone.
    //
    // The handle to abort is the one of the session the worker holds NOW. The UI may have quit
    // with a swap still on its way (`/new` and a prompt typed just before the quit): the
    // worker's `Rebind` is then still in the channel, and the run to stop belongs to the new
    // session. Every late `Rebind` is adopted here; the superseded handle is stopped too, so
    // nothing of either session outlives the quit.
    //
    // OXA-WORKER-STOPFLAG: the worker owns a stop flag and records the handle of every session
    // it enters before anything runs on it, so the session to abort is known without the
    // `Rebind` message (which is still adopted, for the queues the UI holds).
    stop.stop();
    let _ = cmd_tx.send(SessionCmd::Shutdown);
    while !worker.is_finished() {
        stop.abort_current();
        while let Ok(msg) = msg_rx.try_recv() {
            if let WorkerMsg::Rebind(next) = msg {
                let old = std::mem::replace(&mut handle, next);
                stop_handle(&old);
            }
        }
        stop_handle(&handle);
        // A dialog asked for now is dismissed: dropping the request answers `None` / `Deny`.
        while let Ok(req) = ui_rx.try_recv() {
            drop(req);
        }
        thread::sleep(Duration::from_millis(5));
    }
    let last = worker.join().ok().flatten();
    (code, last)
}

/// Stop everything a handle controls: the run in progress, a retry wait, and the queues.
fn stop_handle(handle: &Handle) {
    handle.abort();
    handle.abort_retry();
    let _ = handle.dequeue();
}

/// The worker's own stop signal (OXA-WORKER-STOPFLAG). Created with the worker; [`stop`] sets
/// it and aborts the session the worker holds. The worker records the handle of every session
/// it enters ([`WorkerStop::enter`]) before anything can run on it, and checks the flag before
/// each command and each idle tick: no command starts after the flag is set, and a session
/// entered after it (a swap that was in flight) is aborted on entry.
///
/// `Handle::abort` only reaches a run in progress (a claimed session), so the owner of the
/// worker calls [`WorkerStop::abort_current`] until the worker is gone: a run that began in the
/// same instant as the stop is caught by the next call.
///
/// [`stop`]: WorkerStop::stop
#[derive(Clone, Default)]
pub struct WorkerStop {
    inner: Arc<StopInner>,
}

#[derive(Default)]
struct StopInner {
    stopped: AtomicBool,
    current: Mutex<Option<Handle>>,
    /// O-152: the worker is inside the boot preload (a background prefill nobody waits for).
    preloading: AtomicBool,
    /// V020-OPTIMIZE #13: the worker is inside a draft prefill (`SessionCmd::PrefillDraft`).
    drafting: AtomicBool,
    /// V020-OPTIMIZE #13: bumped by the UI on every key, paste and command: a draft prefill
    /// taken at an older generation is stale.
    draft_gen: AtomicU64,
    /// V020-OPTIMIZE #13: 1 + the generation of the last draft prefill that completed (0 =
    /// none): the UI sends a draft again whose prefill was skipped or cancelled.
    draft_done: AtomicU64,
}

impl WorkerStop {
    pub fn new() -> WorkerStop {
        WorkerStop::default()
    }

    /// Set the flag and stop the current session's run, retry wait and queues.
    pub fn stop(&self) {
        self.inner.stopped.store(true, Ordering::SeqCst);
        self.abort_current();
    }

    pub fn is_stopped(&self) -> bool {
        self.inner.stopped.load(Ordering::SeqCst)
    }

    /// O-152: the worker is running the boot preload. The session is claimed (`is_busy`), but
    /// nothing the user waits for runs: input is a prompt, not steering, and cancels it.
    pub fn is_preloading(&self) -> bool {
        self.inner.preloading.load(Ordering::SeqCst)
    }

    fn set_preloading(&self, on: bool) {
        self.inner.preloading.store(on, Ordering::SeqCst);
    }

    /// V020-OPTIMIZE #13: the worker is prefilling the draft. The session is claimed, but
    /// nothing the user waits for runs: input is a prompt, not steering, and cancels it.
    pub fn is_drafting(&self) -> bool {
        self.inner.drafting.load(Ordering::SeqCst)
    }

    fn set_drafting(&self, on: bool) {
        self.inner.drafting.store(on, Ordering::SeqCst);
    }

    /// The current draft generation (see [`WorkerStop::draft_changed`]).
    pub fn draft_gen(&self) -> u64 {
        self.inner.draft_gen.load(Ordering::SeqCst)
    }

    /// The generation of the last draft prefill that completed (`None`: none yet).
    pub fn draft_done(&self) -> Option<u64> {
        self.inner.draft_done.load(Ordering::SeqCst).checked_sub(1)
    }

    fn set_draft_done(&self, gen: u64) {
        self.inner.draft_done.store(gen.wrapping_add(1), Ordering::SeqCst);
    }

    /// The user acted (a key, a paste, a command): a draft prefill taken before is stale.
    /// Returns whether one is running now (the caller then cancels it with
    /// `Handle::cancel_background`, never `Handle::abort`).
    pub fn draft_changed(&self) -> bool {
        self.inner.draft_gen.fetch_add(1, Ordering::SeqCst);
        self.is_drafting()
    }

    /// Abort whatever the current session runs (no-op before the first session).
    pub fn abort_current(&self) {
        let current = self.inner.current.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(h) = current.as_ref() {
            stop_handle(h);
        }
    }

    /// The worker entered a session: it is the one to abort from now on. Entered after the
    /// stop, it is stopped at once.
    pub fn enter(&self, handle: Handle) {
        let mut current = self.inner.current.lock().unwrap_or_else(|p| p.into_inner());
        if self.is_stopped() {
            stop_handle(&handle);
        }
        *current = Some(handle);
    }
}

/// How long the worker waits for a command before its idle tick serves the session's host
/// (`Session::serve_host`: timers, MCP callbacks, `sendMessage` with `triggerTurn`).
pub const WORKER_IDLE_TICK: Duration = Duration::from_millis(50);

/// The UI loop over an arbitrary terminal (tests pass `oxa_tui::TestTerminal` and scripted
/// channels). Ends when an [`AppAction::Quit`] was carried out or `worker` disconnects.
///
/// Per iteration: poll the terminal (at most 16 ms while something animates, else 250 ms),
/// drain `worker` and `ui`, route input (dialog > selector > editor > global `app.*` keys),
/// carry out actions, and draw one frame if anything changed.
///
/// Key rules (pi): Enter while a run is active = steer, Alt+Enter = follow-up
/// (`Handle::steer` / `Handle::follow_up`); Escape = abort (`Handle::abort`), or
/// `Handle::abort_retry` during a retry wait; Ctrl+C clears the editor, twice = quit; Ctrl+D on
/// an empty editor = quit; `!cmd` and `!!cmd` become `SessionCmd::UserBash`.
pub fn run_loop(
    term: &mut dyn Terminal,
    opts: &mut InteractiveOpts,
    handle: Handle,
    cmds: Sender<SessionCmd>,
    worker: Receiver<WorkerMsg>,
    ui: Receiver<UiRequest>,
    initial: ViewModel,
) -> Result<i32, OxaError> {
    let (code, _) = drive(
        term,
        opts,
        handle,
        cmds,
        &worker,
        &ui,
        initial,
        WorkerStop::new(),
        Vec::new(),
    );
    Ok(code)
}

/// The loop behind [`run_loop`]. Returns the exit code and the control handle in force at
/// the end (the worker rebinds it on every session swap, `WorkerMsg::Rebind`).
fn drive(
    term: &mut dyn Terminal,
    opts: &mut InteractiveOpts,
    handle: Handle,
    cmds: Sender<SessionCmd>,
    worker: &Receiver<WorkerMsg>,
    ui: &Receiver<UiRequest>,
    initial: ViewModel,
    stop: WorkerStop,
    resources: Vec<(String, Vec<String>)>,
) -> (i32, Handle) {
    let profile = profile_word(&opts.env, &opts.settings);
    let mut lp = Loop::new(term, opts, handle, cmds, initial);
    lp.worker = stop;
    lp.resources = resources;
    lp.profile = profile;
    let code = loop {
        // State first: worker and dialog messages, then input (so a key sees the run's state).
        let mut disconnected = false;
        loop {
            match worker.try_recv() {
                Ok(msg) => lp.worker_msg(msg),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        while let Ok(req) = ui.try_recv() {
            lp.ui_req(req);
        }
        lp.poll_asks();

        let animating = lp.animating();
        let events = term.poll(if animating { 16 } else { 250 });
        for ev in events {
            lp.input(term, ev);
        }
        lp.tick(animating);
        if lp.dirty {
            lp.draw(term);
        }
        if let Some(code) = lp.quit {
            break code;
        }
        if disconnected {
            break 1;
        }
    };
    (code, lp.finish(term))
}

/// The worker thread body: owns the session, executes commands in order, forwards events.
/// Returns when `SessionCmd::Shutdown` arrives or `cmds` disconnects. `Session::prompt` gets a
/// `crate::UiProxy` built from `ui` and a sink that sends `WorkerMsg::Event`.
///
/// D-INT: the worker announces its session with `WorkerMsg::Session` when it starts and after
/// every swap, and returns the session it ended with (`None` when a swap failed and nothing
/// could be opened), so the exit lines name the right session.
pub fn worker_loop(
    session: Session,
    reopen: ReopenFn,
    cmds: Receiver<SessionCmd>,
    out: Sender<WorkerMsg>,
    ui: Sender<UiRequest>,
) -> Option<SessionInfo> {
    worker_loop_with(session, reopen, cmds, out, ui, WorkerStop::new())
}

/// [`worker_loop`] with the worker's stop signal (see [`WorkerStop`]). Between commands the
/// worker waits at most [`WORKER_IDLE_TICK`] and then serves the session's host, so host
/// requests made while idle are answered and a turn an extension triggered runs, its events
/// reaching the UI like a prompt's.
pub fn worker_loop_with(
    session: Session,
    mut reopen: ReopenFn,
    cmds: Receiver<SessionCmd>,
    out: Sender<WorkerMsg>,
    ui: Sender<UiRequest>,
    stop: WorkerStop,
) -> Option<SessionInfo> {
    let mut ui_proxy = UiProxy::new(ui.clone());
    #[cfg(feature = "session-control")]
    let _control = oxa_orchestrator::tui::attach(&session, {
        let out = out.clone();
        move |event| {
            let _ = out.send(WorkerMsg::Event(event));
        }
    });
    let mut last = StatusModel::default();
    stop.enter(session.handle());
    let _ = out.send(WorkerMsg::Session(session_info(&session)));
    let mut session = Some(session);
    // Commands are taken through a backlog so a `Shutdown` anywhere behind queued work is
    // seen before that work starts: stop takes precedence (a prompt queued before the quit
    // must not begin after it).
    let mut backlog: VecDeque<SessionCmd> = VecDeque::new();
    loop {
        while let Ok(more) = cmds.try_recv() {
            backlog.push_back(more);
        }
        if stop.is_stopped() || backlog.iter().any(|c| matches!(c, SessionCmd::Shutdown)) {
            break;
        }
        #[cfg(feature = "session-control")]
        if let Some(s) = session.as_mut() {
            oxa_orchestrator::tui::bind(s);
        }
        let cmd = match backlog.pop_front() {
            Some(cmd) => cmd,
            None => match cmds.recv_timeout(WORKER_IDLE_TICK) {
                Ok(cmd) => cmd,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if let Some(s) = session.as_mut() {
                        idle_tick(s, &out, &mut ui_proxy, &mut last, &stop);
                    }
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            },
        };
        // A command taken in the same instant as the stop does not start.
        if stop.is_stopped() {
            break;
        }
        match cmd {
            SessionCmd::Shutdown => break,
            SessionCmd::Reopen(how) => {
                // The session to return to when the new one cannot be opened (a bad import
                // path, an unreadable file): the old one is already shut down by then.
                let back = session.as_ref().map(|s| s.inner().sref.clone());
                // `/new` keeps the model and thinking level of the session it leaves (the
                // status bar shows them); the other modes restore the target's own.
                let carry = match &how {
                    Reopen::New => session.as_ref().map(|s| {
                        let st = s.state();
                        (st.model.clone(), st.thinking)
                    }),
                    Reopen::Resume(_) | Reopen::Fork(_) | Reopen::Import(_) => None,
                };
                // pi's status line for the swap, shown at the top of the new transcript.
                let status = match &how {
                    Reopen::New => "New session started".to_string(),
                    Reopen::Resume(_) => "Resumed session".to_string(),
                    Reopen::Fork(_) => "Forked to new session".to_string(),
                    Reopen::Import(path) => format!("Session imported from: {}", path.display()),
                };
                let mut opened = false;
                if let Some(mut old) = session.take() {
                    // The hook sets are shared with the session that replaces this one, so
                    // they are not shut down here (E-INT 4).
                    old.close(&mut ui_proxy);
                }
                match reopen(how, carry) {
                    Ok(new) => {
                        stop.enter(new.handle());
                        session = Some(new);
                        opened = true;
                    }
                    Err(e) => {
                        let _ = out.send(WorkerMsg::Done(Err(e)));
                        let file_backed = back.as_ref().is_some_and(|r| {
                            !r.path.as_os_str().is_empty() && !r.path.to_string_lossy().starts_with("mem:")
                        });
                        let again = match back {
                            Some(sref) if file_backed => reopen(Reopen::Resume(sref), None),
                            _ => reopen(Reopen::New, None),
                        };
                        match again {
                            Ok(s) => {
                                stop.enter(s.handle());
                                session = Some(s);
                            }
                            Err(e) => {
                                let _ = out.send(WorkerMsg::Done(Err(e)));
                                break;
                            }
                        }
                    }
                }
                if let Some(s) = session.as_ref() {
                    // The new session has its own queues and cancel token: the UI's controls
                    // are rebound with the new transcript (the same batch of messages), and
                    // what the rebinding has to tell the user lands in the new transcript.
                    let msg = reopened(s, &mut last);
                    let _ = out.send(msg);
                    let _ = out.send(WorkerMsg::Rebind(s.handle()));
                    let _ = out.send(WorkerMsg::Session(session_info(s)));
                    if opened {
                        let _ = out.send(WorkerMsg::Event(notice_event(&status)));
                    }
                }
            }
            SessionCmd::Snapshot => {
                let Some(s) = session.as_ref() else { break };
                let _ = out.send(WorkerMsg::Tree(s.inner().tree.clone()));
            }
            // V020-OPTIMIZE #13: invisible like the boot preload (no progress, no `Done`).
            // The flag is set before the session is claimed and the generation is checked
            // again after the cancel is armed (`stale`), so a key at any moment either skips
            // the request or cancels it (`Handle::cancel_background` in `Loop::send`,
            // `Loop::input`).
            SessionCmd::PrefillDraft { text, parts, gen } => {
                #[cfg(feature = "session-control")]
                if oxa_orchestrator::tui::owns_goal() {
                    continue;
                }
                let Some(s) = session.as_mut() else { break };
                if stop.draft_gen() == gen {
                    stop.set_drafting(true);
                    let stale = || stop.draft_gen() != gen;
                    let mut sink = PreloadSink(ChanSink(out.clone()));
                    if s.prefill_draft(&text, &parts, &stale, &mut sink, &mut ui_proxy) {
                        stop.set_draft_done(gen);
                    }
                    stop.set_drafting(false);
                }
            }
            other => {
                let Some(s) = session.as_mut() else { break };
                let reload = matches!(other, SessionCmd::Reload);
                let status = match &other {
                    SessionCmd::Navigate { .. } => Some("Navigated to selected point"),
                    SessionCmd::ForkAt { .. } => Some("Forked to new session"),
                    SessionCmd::CloneSession => Some("Cloned to new session"),
                    _ => None,
                };
                let is_nav = matches!(other, SessionCmd::Navigate { .. });
                let mut sink = ChanSink(out.clone());
                // A1/A2: a panic in a command is reported like its error; the worker and the
                // session go on (a run's busy flag is released by its guard).
                let (mut result, swapped) = oxa_proto::panic::contain(|| {
                    #[cfg(feature = "session-control")]
                    if let Err(error) = oxa_orchestrator::tui::admit(s) {
                        return (Err(error), false);
                    }
                    execute(s, other, &mut sink, &mut ui_proxy, &ui, &out)
                })
                .unwrap_or_else(|error| {
                    let e = OxaError::new(codes::INTERNAL, format!("internal error (panic): {error}"));
                    (Err(e), false)
                });
                #[cfg(feature = "session-control")]
                oxa_orchestrator::tui::settled(result.is_ok());
                // pi: a navigation an extension cancelled (`session_before_tree` -> cancel) or
                // whose summary the user aborted is a status, not an error.
                if is_nav && result.as_ref().is_err_and(|e| e.code == codes::ABORTED) {
                    let _ = out.send(WorkerMsg::Event(notice_event("Navigation cancelled")));
                    result = Ok(());
                }
                if swapped && result.is_ok() {
                    let msg = reopened(s, &mut last);
                    let _ = out.send(msg);
                    if let Some(text) = status {
                        let _ = out.send(WorkerMsg::Event(notice_event(text)));
                    }
                }
                if swapped || reload {
                    let _ = out.send(WorkerMsg::Session(session_info(s)));
                }
                let _ = out.send(WorkerMsg::Done(result));
                let fresh = fresh_status(s, &mut last);
                let _ = out.send(WorkerMsg::Status(fresh));
            }
        }
    }
    let ended = session.as_ref().map(session_info);
    if let Some(mut s) = session.take() {
        s.shutdown(&mut ui_proxy);
    }
    ended
}

/// The worker's idle tick: serve what hook sets asked for while the session was idle. A turn
/// an extension triggered runs here (its events go out like a prompt's); a fresh status
/// follows whenever something was reported.
fn idle_tick(
    s: &mut Session,
    out: &Sender<WorkerMsg>,
    ui: &mut UiProxy,
    last: &mut StatusModel,
    stop: &WorkerStop,
) {
    #[cfg(feature = "session-control")]
    if oxa_orchestrator::tui::drive(s, &mut ChanSink(out.clone()), ui) {
        let fresh = fresh_status(s, last);
        let _ = out.send(WorkerMsg::Status(fresh));
        return;
    }
    // O-152: the first idle tick of a session with an empty wire prefills its frozen prefix
    // (H6, background), so the first prompt starts warm. Once per entered session. The preload
    // is invisible: its progress events are not shown (they would set the foreground activity
    // and make the UI treat the next prompt as steering), and the UI cancels it on input
    // (`WorkerStop::is_preloading`).
    stop.set_preloading(true);
    s.preload_prefix(&mut PreloadSink(ChanSink(out.clone())), ui);
    stop.set_preloading(false);
    let mut sink = CountingSink {
        inner: ChanSink(out.clone()),
        count: 0,
    };
    let result = s.serve_host(&mut sink, ui);
    if let Err(e) = result {
        if e.code != codes::BUSY {
            let _ = out.send(WorkerMsg::Done(Err(e)));
        }
    }
    if sink.count > 0 {
        let fresh = fresh_status(s, last);
        let _ = out.send(WorkerMsg::Status(fresh));
    }
}

/// The sink of the boot preload: drops the progress of the background request (`Prefill`,
/// `EngineQueue`), forwards the rest (a warning when the preload was skipped).
struct PreloadSink(ChanSink);

impl EventSink for PreloadSink {
    fn emit(&mut self, e: &AgentEvent) {
        if !preload_progress(e) {
            self.0.emit(e);
        }
    }
}

/// Progress events of a background request: not shown for the boot preload.
fn preload_progress(e: &AgentEvent) -> bool {
    matches!(e, AgentEvent::Prefill { .. } | AgentEvent::EngineQueue { .. })
}

/// Forwards every event and notes whether a `CompactionStart` went by (a `/compact` that cut
/// nothing emits none).
struct StartWatch<'a> {
    inner: &'a mut dyn EventSink,
    started: bool,
}

impl EventSink for StartWatch<'_> {
    fn emit(&mut self, e: &AgentEvent) {
        self.started |= matches!(e, AgentEvent::CompactionStart { .. });
        self.inner.emit(e);
    }
}

/// A [`ChanSink`] that counts what it forwarded.
struct CountingSink {
    inner: ChanSink,
    count: usize,
}

impl EventSink for CountingSink {
    fn emit(&mut self, e: &AgentEvent) {
        self.count += 1;
        self.inner.emit(e);
    }
}

fn notice_event(text: &str) -> AgentEvent {
    AgentEvent::Notice {
        level: Level::Info,
        text: text.to_string(),
    }
}

/// What the UI thread needs to know about the worker's session.
fn session_info(s: &Session) -> SessionInfo {
    let state = s.state();
    let info = &s.inner().info;
    let mut models = info.models.clone();
    if !info.model.is_empty() && !models.contains(&info.model) {
        models.insert(0, info.model.clone());
    }
    SessionInfo {
        file: state.session_file,
        id: state.session_id,
        commands: s.commands(),
        engine_url: info.base_url.clone(),
        models,
        queue: info.queue,
        shortcuts: s
            .inner()
            .hooks
            .iter()
            .flat_map(|h| h.shortcuts())
            .map(|sc| sc.key)
            .collect(),
    }
}

/// The pure reducer: fold one event into the view model.
///
/// * `message_start` (assistant) opens an `Assistant` block with `streaming: true`;
///   `message_update` deltas extend `text` / `thinking` and open or extend `Tool` blocks
///   (`ToolcallStart` -> `ToolState::Streaming`); `message_end` closes the block.
/// * A user message adds a `User` block when its entry is stored (`entry_appended`; the core
///   sends no `message_start` for it). A user `message_start` from another producer adds one too.
/// * A stored `!cmd` entry closes the live `Bash` block of that command (or adds the block); a
///   displayed custom message adds a `Custom` block.
/// * `tool_execution_start/update/end` move the matching `ToolBlock` (by `call_id`) through
///   `Running` to `Done` / `Error`; an unknown id adds a block (never a panic).
/// * `compaction_end` with a result adds a `Compaction` block; `queue_update` replaces both
///   queues; `Prefill`, `auto_retry_*`, `compaction_start` set `status.activity`;
///   `agent_settled` sets `Activity::Idle`; `CacheMiss`, `PrefixEpoch`, `ModeChanged`,
///   `Notice`, `extension_error` add `Notice` blocks (a `CacheMiss` with cause `unknown` also
///   counts in `status.unknown_misses`).
/// * An aborted, failed or cut reply becomes a `Shown` block (W2: shown, never sent). The core
///   reports it as `entry_appended` with a `Shown` entry and sends no `message_end` for it;
///   that entry replaces the open assistant block.
/// * Unknown combinations are ignored; the function never panics.
pub fn apply_event(vm: &mut ViewModel, e: &AgentEvent) {
    if matches!(vm.status.activity, Activity::Queued { .. }) && ends_queue_wait(e) {
        // The engine took the request (or the request ended): the wait is over. Events that
        // set their own activity below overwrite this.
        vm.status.activity = Activity::Thinking;
    }
    match e {
        AgentEvent::AgentStart => {
            vm.status.activity = Activity::Thinking;
            vm.status.activity_ms = 0;
        }
        AgentEvent::AgentSettled { .. } => {
            close_open_blocks(vm);
            vm.status.activity = Activity::Idle;
            vm.status.activity_ms = 0;
        }
        AgentEvent::MessageStart { message } => match message.msg.role {
            // The core announces a user message twice: its stored entry (with images and the
            // provenance of the environment note, `oxa_core::envnote`) and pi's `message_start`
            // at the turn that sends it. One block: the entry's, shown as typed.
            Role::User if !user_block_pending(vm, message.msg.content.as_deref().unwrap_or_default()) => {
                vm.blocks.push(Block::User {
                    text: message.msg.content.clone().unwrap_or_default(),
                    images: Vec::new(),
                })
            }
            Role::User => {}
            Role::Assistant => {
                vm.blocks.push(Block::Assistant {
                    text: String::new(),
                    thinking: String::new(),
                    streaming: true,
                    stop: None,
                    error: None,
                    model: message.model.clone(),
                });
                vm.status.activity = Activity::Thinking;
            }
            _ => {}
        },
        AgentEvent::MessageUpdate { delta, .. } => apply_delta(vm, delta),
        AgentEvent::MessageEnd { message } => {
            if message.msg.role == Role::Assistant {
                finish_assistant(vm, message);
            }
        }
        AgentEvent::ToolExecutionStart {
            tool_call_id,
            tool_name,
            args,
        } => {
            vm.status.activity = Activity::Tool(tool_name.clone());
            match tool_index(vm, tool_call_id) {
                Some(i) => {
                    if let Block::Tool(t) = &mut vm.blocks[i] {
                        t.name = tool_name.clone();
                        t.args = args.clone();
                        t.state = ToolState::Running;
                    }
                }
                None => vm.blocks.push(Block::Tool(new_tool(
                    tool_call_id,
                    tool_name,
                    args.clone(),
                    ToolState::Running,
                ))),
            }
        }
        AgentEvent::ToolExecutionUpdate {
            tool_call_id,
            tool_name,
            args,
            partial,
        } => match tool_index(vm, tool_call_id) {
            Some(i) => {
                if let Block::Tool(t) = &mut vm.blocks[i] {
                    t.partial = partial.clone();
                    t.state = ToolState::Running;
                }
            }
            None => {
                let mut t = new_tool(tool_call_id, tool_name, args.clone(), ToolState::Running);
                t.partial = partial.clone();
                vm.blocks.push(Block::Tool(t));
            }
        },
        AgentEvent::ToolExecutionEnd {
            tool_call_id,
            tool_name,
            result,
            duration_ms,
        } => {
            let state = if result.is_error {
                ToolState::Error
            } else {
                ToolState::Done
            };
            match tool_index(vm, tool_call_id) {
                Some(i) => {
                    if let Block::Tool(t) = &mut vm.blocks[i] {
                        t.state = state;
                        t.result = Some(result.clone());
                        t.duration_ms = *duration_ms;
                        t.partial.clear();
                    }
                }
                None => {
                    let mut t = new_tool(tool_call_id, tool_name, Value::Null, state);
                    t.result = Some(result.clone());
                    t.duration_ms = *duration_ms;
                    vm.blocks.push(Block::Tool(t));
                }
            }
            // Calls of one reply may run side by side: while another one runs, a tool runs.
            let running = vm.blocks.iter().rev().find_map(|b| match b {
                Block::Tool(t) if t.state == ToolState::Running => Some(t.name.clone()),
                _ => None,
            });
            vm.status.activity = running.map_or(Activity::Thinking, Activity::Tool);
        }
        AgentEvent::CompactionStart { .. } => vm.status.activity = Activity::Compacting,
        AgentEvent::CompactionEnd {
            result, will_retry, ..
        } => {
            if let Some(r) = result {
                vm.status.degraded_context = r.details["mechanical"] == serde_json::json!(true);
                vm.status.context_tokens = r.estimated_tokens_after;
                push_notice(
                    vm,
                    Level::Info,
                    format!(
                        "Compacted from {} to about {} tokens",
                        r.tokens_before, r.estimated_tokens_after
                    ),
                );
                vm.blocks.push(Block::Compaction {
                    summary: r.summary.clone(),
                    tokens_before: r.tokens_before,
                    expanded: false,
                });
            }
            if !will_retry {
                vm.status.activity = Activity::Idle;
            }
        }
        AgentEvent::AutoRetryStart {
            attempt,
            max_attempts,
            delay_ms,
            ..
        } => {
            vm.status.activity = Activity::Retrying {
                attempt: *attempt,
                max_attempts: *max_attempts,
                delay_ms: *delay_ms,
            };
            vm.status.activity_ms = 0;
        }
        AgentEvent::AutoRetryEnd { success, .. } => {
            if !success {
                vm.status.activity = Activity::Idle;
            }
        }
        AgentEvent::QueueUpdate { steering, follow_up } => {
            vm.status.steering = steering.clone();
            vm.status.follow_up = follow_up.clone();
        }
        AgentEvent::ModelSelect { model, .. } => {
            if vm.status.model != *model {
                vm.status.tok_per_s = None;
                vm.status.last_cached = None;
                vm.status.last_prompt = None;
            }
            vm.status.model = model.clone();
        }
        AgentEvent::ThinkingLevelChanged { level } => vm.status.thinking = Some(*level),
        AgentEvent::SessionInfoChanged { name } => vm.status.session_name = name.clone(),
        AgentEvent::BashExecutionUpdate { delta, .. } => {
            if let Some(Block::Bash {
                output,
                running: true,
                ..
            }) = vm.blocks.last_mut()
            {
                output.push_str(delta);
            }
        }
        AgentEvent::ExtensionError {
            extension_path,
            error,
            ..
        } => {
            push_notice(
                vm,
                Level::Error,
                format!("Extension error ({extension_path}): {error}"),
            );
        }
        AgentEvent::Prefill { done, total, secs } => {
            vm.status.activity = Activity::Prefill {
                done: *done,
                total: *total,
                secs: *secs,
            };
        }
        // S2: the request waits for the engine's one slot (not an error).
        AgentEvent::EngineQueue { pos, wait_s } => {
            vm.status.activity = Activity::Queued {
                pos: *pos,
                wait_s: *wait_s,
            };
        }
        // The core's one notice carries the epoch and its cost.
        AgentEvent::PrefixEpoch { .. } => {}
        AgentEvent::CacheMiss {
            expected,
            cached,
            cause,
        } => {
            if *cause == "unknown" {
                vm.status.unknown_misses += 1;
            }
            push_notice(
                vm,
                Level::Warning,
                format!("Cache miss ({cause}): {cached} of {expected} tokens cached"),
            );
        }
        AgentEvent::ModeChanged { mode } => {
            vm.status.mode = Some(*mode);
            push_notice(vm, Level::Info, format!("Mode: {}", mode.as_str()));
        }
        AgentEvent::Notice { level, text } => push_notice(vm, *level, text.clone()),
        // T2: the notice that goes with it carries the text; this only updates the counter.
        AgentEvent::ToolRepair { total, .. } => vm.status.tool_repairs = *total,
        // The core ends an aborted, failed or cut reply with a stored `Shown` entry and NO
        // `message_end` (W2: shown, never sent). That entry is what closes the open block.
        //
        // The stored entry is also the only report of a user message (the prompt, a delivered
        // steering or follow-up message), of a finished `!cmd` and of an extension's custom
        // message: the core sends no `message_start` for those.
        AgentEvent::EntryAppended { entry } => match &entry.body {
            EntryBody::Shown { text, why } => {
                if [shown_why::ABORTED, shown_why::FAILED, shown_why::CUT].contains(why) {
                    shown_reply(vm, text, why);
                }
            }
            EntryBody::Msg { wire, parts } if wire.role == Role::User => {
                vm.blocks
                    .push(user_block(wire.content.as_deref().unwrap_or_default(), parts));
            }
            EntryBody::BashExecution {
                command,
                output,
                exit,
                excluded,
            } => {
                let done = Block::Bash {
                    command: command.clone(),
                    output: output.clone(),
                    exit: *exit,
                    excluded: *excluded,
                    running: false,
                    expanded: false,
                };
                let live = vm.blocks.iter().rposition(
                    |b| matches!(b, Block::Bash { running: true, command: c, .. } if c == command),
                );
                match live {
                    Some(i) => vm.blocks[i] = done,
                    None => vm.blocks.push(done),
                }
            }
            EntryBody::CustomMessage {
                custom_type,
                content,
                display: true,
                details,
            } => {
                vm.blocks.push(Block::Custom {
                    custom_type: custom_type.clone(),
                    content: content.clone(),
                    details: details.clone(),
                    expanded: false,
                });
            }
            _ => {}
        },
        AgentEvent::AgentEnd { .. }
        | AgentEvent::TurnStart
        | AgentEvent::TurnEnd { .. }
        | AgentEvent::SummarizationRetryScheduled { .. }
        | AgentEvent::SummarizationRetryAttemptStart { .. }
        | AgentEvent::SummarizationRetryFinished => {}
    }
}

/// Events that end a wait for the engine's slot (`Activity::Queued`): the first prefill or
/// data event, the end of the request (also by an abort or a failure) and the end of the run.
fn ends_queue_wait(e: &AgentEvent) -> bool {
    match e {
        AgentEvent::Prefill { .. }
        | AgentEvent::MessageStart { .. }
        | AgentEvent::MessageUpdate { .. }
        | AgentEvent::MessageEnd { .. }
        | AgentEvent::AgentEnd { .. }
        | AgentEvent::AgentSettled { .. }
        | AgentEvent::AutoRetryStart { .. }
        | AgentEvent::AutoRetryEnd { .. }
        | AgentEvent::CompactionStart { .. } => true,
        AgentEvent::EntryAppended { entry } => matches!(entry.body, EntryBody::Shown { .. }),
        _ => false,
    }
}

/// Blocks for the entries of the active branch, root first (resume, `/tree`, reopen).
/// Tool-role messages are attached to the `ToolBlock` of their call; entries that are not
/// displayed (usage, labels, mode changes, hidden custom entries) give no block.
pub fn blocks_from_entries(branch: &[Entry]) -> Vec<Block> {
    let mut out: Vec<Block> = Vec::new();
    let mut calls: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for e in branch {
        match &e.body {
            EntryBody::Msg { wire, parts } => match wire.role {
                Role::User => out.push(user_block(wire.content.as_deref().unwrap_or_default(), parts)),
                Role::Assistant => {
                    let stop = if wire.tool_calls.is_empty() {
                        StopReason::Stop
                    } else {
                        StopReason::ToolUse
                    };
                    out.push(Block::Assistant {
                        text: wire.content.clone().unwrap_or_default(),
                        thinking: wire.reasoning.clone().unwrap_or_default(),
                        streaming: false,
                        stop: Some(stop),
                        error: None,
                        model: None,
                    });
                    for tc in &wire.tool_calls {
                        calls.insert(tc.id.clone(), out.len());
                        out.push(Block::Tool(ToolBlock {
                            call_id: tc.id.clone(),
                            name: tc.name.clone(),
                            args: parse_args(&tc.args_raw),
                            args_raw: tc.args_raw.clone(),
                            state: ToolState::Done,
                            partial: String::new(),
                            result: None,
                            duration_ms: None,
                            expanded: false,
                        }));
                    }
                }
                Role::Tool => {
                    if let Some(id) = &wire.tool_call_id {
                        if let Some(&i) = calls.get(id) {
                            if let Block::Tool(t) = &mut out[i] {
                                t.result = Some(ToolOutput::ok(wire.content.clone().unwrap_or_default()));
                            }
                        }
                    }
                }
                Role::System => {}
            },
            EntryBody::Shown { text, why } => out.push(Block::Shown {
                text: text.clone(),
                why: why.to_string(),
            }),
            EntryBody::BashExecution {
                command,
                output,
                exit,
                excluded,
            } => out.push(Block::Bash {
                command: command.clone(),
                output: output.clone(),
                exit: *exit,
                excluded: *excluded,
                running: false,
                expanded: false,
            }),
            EntryBody::Compaction {
                summary,
                tokens_before,
                ..
            } => out.push(Block::Compaction {
                summary: summary.clone(),
                tokens_before: *tokens_before,
                expanded: false,
            }),
            EntryBody::BranchSummary { summary, .. } => out.push(Block::BranchSummary {
                summary: summary.clone(),
                expanded: false,
            }),
            EntryBody::CustomMessage {
                custom_type,
                content,
                display: true,
                details,
            } => out.push(Block::Custom {
                custom_type: custom_type.clone(),
                content: content.clone(),
                details: details.clone(),
                expanded: false,
            }),
            _ => {}
        }
    }
    out
}

/// The block of a stored user message: its wire text and its attached images.
/// A `User` block for the wire text `content` was added since the last reply (its entry was
/// stored): its text is `content`, or `content` without the note the entry recorded (the
/// block's text then ends where the `\n\n` before the note starts).
fn user_block_pending(vm: &ViewModel, content: &str) -> bool {
    let shown_from = |t: &str| {
        content == t
            || content
                .strip_prefix(t)
                .is_some_and(|note| note.starts_with("\n\n"))
    };
    for b in vm.blocks.iter().rev() {
        match b {
            Block::User { text: t, .. } if shown_from(t) => return true,
            Block::Assistant { .. } | Block::Tool(_) => return false,
            _ => {}
        }
    }
    false
}

/// A stored user message as typed: without the environment note oxa recorded appending.
fn user_block(text: &str, parts: &[Part]) -> Block {
    Block::User {
        text: oxa_core::envnote::strip_note(text, parts).to_string(),
        images: parts
            .iter()
            .filter_map(|p| match p {
                Part::Image { path, mime, w, h, .. } => Some(ImageRef {
                    path: path.clone(),
                    mime: mime.clone(),
                    w: *w,
                    h: *h,
                }),
                Part::Text(_) | Part::EnvNote(_) => None,
            })
            .collect(),
    }
}

/// The snapshot-testable core: apply `events` to an empty model and return the transcript
/// lines (blocks through `crate::render_block`, one empty line between blocks) at `width`.
/// No chrome, no editor. Acceptance (plan D1): event list -> frames at 80 and 120 columns.
pub fn frames(events: &[AgentEvent], width: u16, theme: &Theme, settings: &ViewSettings) -> Vec<String> {
    let mut vm = ViewModel::default();
    for e in events {
        apply_event(&mut vm, e);
    }
    transcript_lines(&vm, width, theme, settings).0
}

/// All lines of the transcript for a model (what [`frames`] renders after reducing), plus the
/// index of the first line of every block (for prompt jumps and regular-mode commits).
///
/// The index of a block is where its segment starts: the blank separator line before it (none
/// for the first rendered block) belongs to the segment. A block that renders nothing has an
/// empty segment.
pub fn transcript_lines(
    vm: &ViewModel,
    width: u16,
    theme: &Theme,
    settings: &ViewSettings,
) -> (Vec<String>, Vec<usize>) {
    transcript_lines_from(vm, 0, false, width, theme, settings)
}

/// [`transcript_lines`] for the blocks from index `first` on: only those blocks are rendered
/// (the regular layout calls this every frame and must not re-render the printed history).
/// `printed_before` says whether an earlier block rendered a line, so the first rendered block
/// here gets its separator. The start indices are for `blocks[first..]`.
pub(crate) fn transcript_lines_from(
    vm: &ViewModel,
    first: usize,
    printed_before: bool,
    width: u16,
    theme: &Theme,
    settings: &ViewSettings,
) -> (Vec<String>, Vec<usize>) {
    let env = ViewEnv::default();
    transcript_lines_in(vm, first, printed_before, width, theme, settings, &env)
}

/// What the app adds to a view context beyond the model: the terminal's capabilities (inline
/// images), the session's hook sets (extension renderers) and the live keymap (key labels).
/// The default is what the pure snapshot functions use: no images, no hooks, default keys.
#[derive(Clone, Copy, Default)]
pub(crate) struct ViewEnv<'a> {
    pub caps: TermCaps,
    pub hooks: &'a [Arc<dyn Hooks>],
    pub keymap: Option<&'a Keymap>,
}

/// [`transcript_lines_from`] with the app's view environment.
pub(crate) fn transcript_lines_in(
    vm: &ViewModel,
    first: usize,
    printed_before: bool,
    width: u16,
    theme: &Theme,
    settings: &ViewSettings,
    env: &ViewEnv,
) -> (Vec<String>, Vec<usize>) {
    let cx = ViewCx {
        width,
        theme,
        settings,
        caps: env.caps,
        hooks: env.hooks,
        cwd: &vm.status.cwd,
        home: &vm.status.home,
        keymap: env.keymap,
    };
    let blocks = vm.blocks.get(first..).unwrap_or(&[]);
    let mut lines: Vec<String> = Vec::new();
    let mut starts: Vec<usize> = Vec::with_capacity(blocks.len());
    for block in blocks {
        starts.push(lines.len());
        let block_lines = render_block(block, &cx);
        if block_lines.is_empty() {
            continue;
        }
        if printed_before || !lines.is_empty() {
            lines.push(String::new());
        }
        lines.extend(block_lines);
    }
    (lines, starts)
}

/// Status from the session after a settle (`Session::state()` and `Session::stats()`), keeping
/// the display-only fields of `prev` (extension status, git branch, working message).
pub fn status_from(state: &State, stats: &Stats, prev: &StatusModel) -> StatusModel {
    let switched = prev.model != state.model;
    let fresh = switched || stats.context_tokens == 0;
    StatusModel {
        cwd: prev.cwd.clone(),
        home: prev.home.clone(),
        git_branch: prev.git_branch.clone(),
        session_name: state.session_name.clone(),
        model: state.model.clone(),
        thinking: Some(state.thinking),
        thinking_note: prev.thinking_note.clone(),
        mode: Some(state.mode),
        context_tokens: stats.context_tokens,
        n_ctx: state.n_ctx,
        last_cached: if fresh { None } else { prev.last_cached },
        last_prompt: if fresh { None } else { prev.last_prompt },
        tok_per_s: if stats.last_tok_s > 0.0 && !switched {
            Some(stats.last_tok_s)
        } else {
            None
        },
        total_in: stats.prompt_tokens,
        total_out: stats.completion_tokens,
        activity: prev.activity.clone(),
        activity_ms: prev.activity_ms,
        steering: prev.steering.clone(),
        follow_up: prev.follow_up.clone(),
        ext_status: prev.ext_status.clone(),
        working_message: prev.working_message.clone(),
        auto_compact: state.auto_compaction,
        unknown_misses: prev.unknown_misses,
        tool_repairs: stats.tool_repairs,
        degraded_context: prev.degraded_context,
        yolo: prev.yolo,
        yolo_approved: prev.yolo_approved,
    }
}

/// pi's exit output (interactive-mode.ts `shutdown` / `formatResumeCommand`): one line, the
/// label dimmed: `To resume this session: oxa --session <id>`. Empty for an ephemeral
/// session; [`run`] also prints nothing for a session without a conversation (pi has not
/// written its file).
pub fn exit_lines(session_file: Option<&Path>, session_id: &str) -> Vec<String> {
    if session_file.is_none() {
        return Vec::new();
    }
    vec![format!(
        "\x1b[2mTo resume this session:\x1b[22m oxa --session {session_id}"
    )]
}

/// The session has a user or assistant message (pi `_hasConversation`).
fn has_conversation(store: &dyn oxa_session::SessionStore, file: &Path, id: &str) -> bool {
    let sref = oxa_session::SessionRef {
        path: file.to_path_buf(),
        id: id.to_string(),
    };
    store.load(&sref).is_ok_and(|entries| {
        entries.iter().any(|e| {
            matches!(&e.body, EntryBody::Msg { wire, .. }
                if matches!(wire.role, oxa_proto::Role::User | oxa_proto::Role::Assistant))
        })
    })
}

// ---------------------------------------------------------------------------------------------
// Worker helpers
// ---------------------------------------------------------------------------------------------

/// Sends the session's events to the UI thread.
struct ChanSink(Sender<WorkerMsg>);

impl EventSink for ChanSink {
    fn emit(&mut self, e: &AgentEvent) {
        #[cfg(feature = "session-control")]
        oxa_orchestrator::tui::observe(e);
        let _ = self.0.send(WorkerMsg::Event(e.clone()));
    }
}

/// S1 / ENG-002: input typed while a command that is not a prompt ran (a navigation's idle
/// prefill, `/compact`) cancelled that command's background request and waits in the queues;
/// run it now (`Session::run_queued`; nothing happens when the queues are empty). The
/// command's own error wins.
fn then_queued(
    s: &mut Session,
    done: Result<(), OxaError>,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) -> Result<(), OxaError> {
    let queued = s.run_queued(sink, ui).map(|_| ());
    done.and(queued)
}

/// `/export` as HTML (pi): the session written to `path` the way the RPC `export_html` command
/// writes it (the session's own system prompt, tools, leaf and name; the dark theme). The
/// outcome is a notice: the written path, or a warning.
fn export_html_file(s: &Session, path: &Path, sink: &mut dyn EventSink) {
    let st = s.inner();
    let state = s.state();
    let outcome = (|| -> Result<std::path::PathBuf, OxaError> {
        let header = st.store.header(&st.sref)?;
        let entries = st.store.load(&st.sref)?;
        let opts = ExportOpts {
            theme: ExportTheme::dark(),
            session_name: state.session_name.clone(),
            system_prompt: Some(st.prefix.system_text()),
            tools: serde_json::from_str::<Value>(&st.prefix.tools_json).unwrap_or(Value::Null),
            rendered_tools: Vec::new(),
            leaf: (!st.tree.leaf().is_empty()).then(|| st.tree.leaf().to_string()),
        };
        let html = export_html(&header, &entries, &opts)?;
        write_export(path, &html)
    })();
    let event = match outcome {
        Ok(written) => AgentEvent::Notice {
            level: Level::Info,
            text: format!("Exported the session to {}", written.display()),
        },
        Err(e) => AgentEvent::Notice {
            level: Level::Warning,
            text: format!("/export: {e}"),
        },
    };
    sink.emit(&event);
}

/// Run one command on the session. Returns the result and whether the session switched to
/// another file (fork, clone).
fn execute(
    s: &mut Session,
    cmd: SessionCmd,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
    ui_tx: &Sender<UiRequest>,
    out: &Sender<WorkerMsg>,
) -> (Result<(), OxaError>, bool) {
    match cmd {
        SessionCmd::Prompt { text, parts } => (run_prompt(s, text, parts, sink, ui), false),
        SessionCmd::Compact { instructions } => {
            let mut watch = StartWatch {
                inner: &mut *sink,
                started: false,
            };
            let done = s.compact(instructions.as_deref(), &mut watch, ui);
            // Nothing was cut when no compaction started (the core then emits no event at all).
            let nothing_to_cut = done.is_ok() && !watch.started;
            if nothing_to_cut {
                sink.emit(&notice_event("Nothing to compact"));
            }
            (then_queued(s, done, sink, ui), false)
        }
        // A navigation changes the active branch: the view is rebuilt like after a swap.
        SessionCmd::Navigate {
            target,
            summarize,
            instructions,
        } => match s.navigate(&target, summarize, instructions.as_deref(), sink, ui) {
            Ok(refill) => {
                if let Some(text) = refill {
                    let _ = ui_tx.send(UiRequest::SetEditorText(text));
                }
                (then_queued(s, Ok(()), sink, ui), true)
            }
            Err(e) => (then_queued(s, Err(e), sink, ui), false),
        },
        SessionCmd::ForkAt { entry } => (s.fork(&entry).map(|_| ()), true),
        SessionCmd::CloneSession => (s.clone_session().map(|_| ()), true),
        SessionCmd::SetModel(m) => (switch_model_guarded(s, &m, sink, ui, out), false),
        SessionCmd::Shortcut(key) => (s.run_shortcut(&key, sink, ui), false),
        SessionCmd::SetThinking(l) => {
            s.set_thinking(l, sink);
            (Ok(()), false)
        }
        SessionCmd::SetMode(m) => {
            s.set_mode(m, sink);
            (Ok(()), false)
        }
        SessionCmd::SetName(n) => (s.set_name(&n, sink), false),
        SessionCmd::Export { path } => {
            export_html_file(s, &path, sink);
            (Ok(()), false)
        }
        SessionCmd::SetLabel { target, label } => (s.set_label(&target, label.as_deref()), false),
        SessionCmd::Reload => (s.reload(sink, ui).map(|_| ()), false),
        SessionCmd::UserBash { command, excluded } => (s.user_bash(&command, excluded, sink, ui), false),
        SessionCmd::Reopen(_)
        | SessionCmd::Snapshot
        | SessionCmd::Shutdown
        | SessionCmd::PrefillDraft { .. } => (Ok(()), false),
    }
}

/// One submitted prompt. `/name args` lines go to the command runner (extension and template
/// commands); a reply that failed or was cut is shown by the core as a `Shown` entry at the
/// leaf (W2), and `Session::prompt` still returns `Ok`, so the failure is detected here as
/// print mode does.
fn run_prompt(
    s: &mut Session,
    text: String,
    parts: Vec<Part>,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) -> Result<(), OxaError> {
    // `/name args` of a hook set (extension, MCP, subagent) goes to `Session::run_command`.
    // Templates and `/skill:name` are expanded by `Session::prompt`; an unknown `/name` is
    // plain prompt text, as in pi. The built-ins never get here: the UI thread runs
    // `dispatch_slash` before it sends a prompt.
    if parts.is_empty() {
        if let Expanded::Hook { name, args } = expand_input(s.inner(), &text) {
            return s.run_command(&name, &args, sink, ui);
        }
    }
    let before = s.inner().tree.leaf().to_string();
    s.prompt(text, parts, sink, ui)?;
    let tree = &s.inner().tree;
    let leaf = tree.leaf().to_string();
    if leaf != before {
        if let Some(EntryBody::Shown { why, .. }) = tree.get(&leaf).map(|e| &e.body) {
            if *why == shown_why::FAILED || *why == shown_why::CUT {
                sink.emit(&AgentEvent::Notice {
                    level: Level::Warning,
                    text: "The reply did not finish. It is shown but was not sent; ask again to retry."
                        .to_string(),
                });
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// S3: the over-cap guard of a model switch
// ---------------------------------------------------------------------------------------------

/// The choices of the over-cap dialog, in this order.
pub const OVER_CAP_CHOICES: [&str; 3] = ["Compact first", "Fork from an earlier message", "Cancel"];

/// What the user picked in the over-cap dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverCapChoice {
    CompactFirst,
    Fork,
    Cancel,
}

impl OverCapChoice {
    /// The choice behind a dialog answer (an index into [`OVER_CAP_CHOICES`]); a dismissed
    /// dialog is `Cancel`.
    pub fn from_answer(answer: Option<usize>) -> OverCapChoice {
        match answer {
            Some(0) => OverCapChoice::CompactFirst,
            Some(1) => OverCapChoice::Fork,
            _ => OverCapChoice::Cancel,
        }
    }
}

/// What a model switch does after its plan (and, over the cap, the user's choice).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchStep {
    /// The conversation fits the target: switch now.
    Switch,
    /// Compact with the current model, then switch (`OverCap::CompactFirst`).
    CompactThenSwitch,
    /// Open the fork selector; the model does not change.
    OpenFork,
    /// Nothing changes, nothing is sent.
    Nothing,
}

/// The decision of a model switch: `choice` is asked only when the plan is over the cap
/// (`None` = not asked yet; then the caller asks with [`over_cap_question`]).
pub fn switch_step(plan: &SwitchPlan, choice: Option<OverCapChoice>) -> SwitchStep {
    if !plan.over_cap {
        return SwitchStep::Switch;
    }
    match choice {
        Some(OverCapChoice::CompactFirst) => SwitchStep::CompactThenSwitch,
        Some(OverCapChoice::Fork) => SwitchStep::OpenFork,
        Some(OverCapChoice::Cancel) | None => SwitchStep::Nothing,
    }
}

/// The title of the over-cap dialog, with both numbers.
pub fn over_cap_question(plan: &SwitchPlan) -> String {
    let holds = plan
        .target_hard
        .or(plan.target_ctx)
        .map_or_else(|| "less".to_string(), |m| format!("{m} tokens"));
    format!(
        "The conversation is {} tokens; {} holds {holds}.",
        plan.context_tokens, plan.target
    )
}

/// Every interactive model switch (`/model <pattern>`, the model selector, the cycle key):
/// ask the plan first; over the cap the user picks compact first / fork / cancel. Below the
/// cap the switch happens at once; the cost notice is the core's.
fn switch_model_guarded(
    s: &mut Session,
    m: &str,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
    out: &Sender<WorkerMsg>,
) -> Result<(), OxaError> {
    let plan = s.plan_model_switch(m);
    let choice = if plan.over_cap {
        let options: Vec<String> = OVER_CAP_CHOICES.iter().map(|c| c.to_string()).collect();
        Some(OverCapChoice::from_answer(ui.select(
            &over_cap_question(&plan),
            &options,
            None,
        )))
    } else {
        None
    };
    match switch_step(&plan, choice) {
        SwitchStep::Switch => s.set_model_with(m, OverCap::Refuse, sink, ui),
        SwitchStep::CompactThenSwitch => s.set_model_with(m, OverCap::CompactFirst, sink, ui),
        SwitchStep::OpenFork => {
            let _ = out.send(WorkerMsg::Open(crate::SelectorKind::ForkPoint));
            Ok(())
        }
        SwitchStep::Nothing => Ok(()),
    }
}

fn fresh_status(s: &Session, last: &mut StatusModel) -> StatusModel {
    let state = s.state();
    let stats = s.stats();
    let next = status_from(&state, &stats, last);
    *last = next.clone();
    next
}

fn reopened(s: &Session, last: &mut StatusModel) -> WorkerMsg {
    let status = fresh_status(s, last);
    let inner = s.inner();
    let leaf = inner.tree.leaf().to_string();
    let entries: Vec<Entry> = inner.tree.branch(&leaf).into_iter().cloned().collect();
    WorkerMsg::Reopened { entries, status }
}

// ---------------------------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------------------------

/// Copy the state-derived fields of a fresh status into the UI's status. Display-only and
/// event-driven fields (activity, queues, extension status, cwd, branch) stay with the UI.
pub(crate) fn refresh_status(dst: &mut StatusModel, fresh: StatusModel) {
    if dst.model != fresh.model || fresh.context_tokens == 0 {
        dst.last_cached = None;
        dst.last_prompt = None;
    }
    dst.model = fresh.model;
    dst.thinking = fresh.thinking;
    dst.mode = fresh.mode;
    dst.session_name = fresh.session_name;
    dst.context_tokens = fresh.context_tokens;
    dst.n_ctx = fresh.n_ctx;
    dst.tok_per_s = fresh.tok_per_s;
    dst.total_in = fresh.total_in;
    dst.total_out = fresh.total_out;
    dst.auto_compact = fresh.auto_compact;
    dst.unknown_misses = dst.unknown_misses.max(fresh.unknown_misses);
    dst.tool_repairs = fresh.tool_repairs;
}

pub(crate) fn view_settings(opts: &InteractiveOpts, caps: &TermCaps) -> ViewSettings {
    let defaults = ViewSettings::default();
    ViewSettings {
        hide_thinking: opts.settings.bool("hideThinkingBlock", defaults.hide_thinking),
        // Inline images: shown when the terminal has a protocol, `terminal.showImages` (default
        // true) is on and `images.blockImages` (default false) is off.
        block_images: opts.settings.bool("images.blockImages", false)
            || !opts.settings.bool("terminal.showImages", true)
            || caps.images.is_none(),
        image_width_cells: opts.settings.u32("terminal.imageWidthCells", 60).clamp(1, 1000) as u16,
        quiet_startup: opts.settings.bool("quietStartup", defaults.quiet_startup),
        scrollbar: opts
            .settings
            .string("fullscreenScrollbar")
            .unwrap_or(defaults.scrollbar.clone()),
        wheel_lines: match opts
            .settings
            .get("fullscreenWheelScrollLines")
            .and_then(Value::as_u64)
        {
            Some(n) => n.clamp(1, 100) as u16,
            None => defaults.wheel_lines,
        },
        // pi's default for `fullscreenCopyOnSelect` is on.
        copy_on_select: opts.settings.bool("fullscreenCopyOnSelect", true),
        hyperlinks: caps.hyperlinks,
        code_block_prefix: opts.settings.string("markdown.codeBlockIndent"),
        output_pad: opts.settings.u32("outputPad", 1).min(1) as u16,
        skin: skin_of(opts),
        ..defaults
    }
}

/// The look of this session: `ui.skin`, else by profile (`local` = OXA's look), with the ASCII
/// glyph set when the locale is not UTF-8.
pub(crate) fn skin_of(opts: &InteractiveOpts) -> crate::Skin {
    let local = profile_word(&opts.env, &opts.settings).is_some();
    // `OXA_SKIN` (this run) over the `ui.skin` setting.
    let from_env = opts
        .env
        .iter()
        .find(|(k, v)| k == "OXA_SKIN" && matches!(v.trim(), "pi" | "oxa"))
        .map(|(_, v)| v.trim().to_string());
    crate::skin::resolve_skin(
        from_env.or_else(|| opts.settings.string("ui.skin")).as_deref(),
        local,
        crate::skin::env_is_utf8(&opts.env),
    )
}

/// Where the current Omarchy palette may be (read-only): from the environment snapshot's XDG
/// directories and the home directory.
fn omarchy_candidates(opts: &InteractiveOpts) -> Vec<std::path::PathBuf> {
    let dir = |key: &str| {
        opts.env
            .iter()
            .find(|(k, v)| k == key && !v.is_empty())
            .map(|(_, v)| std::path::PathBuf::from(v))
    };
    let (state, config) = (dir("XDG_STATE_HOME"), dir("XDG_CONFIG_HOME"));
    omarchy_colors_paths(&opts.dirs.home, state.as_deref(), config.as_deref())
}

/// The Omarchy theme name behind the active `oxa-omarchy` theme (for the splash), if any.
/// O-185: a watch on the desktop palette for this session (see `oxa_tui::PaletteWatch`).
pub(crate) fn palette_watch(opts: &InteractiveOpts) -> oxa_tui::PaletteWatch {
    oxa_tui::PaletteWatch::new(omarchy_candidates(opts))
}

pub(crate) fn omarchy_theme_name(opts: &InteractiveOpts) -> Option<String> {
    load_omarchy_palette(&omarchy_candidates(opts)).and_then(|(p, _)| p.name)
}

/// Resolve a theme name for this session: OXA's own themes (`oxa`, `oxa-omarchy`), else
/// [`resolve_theme`]. With OXA's look, `NO_COLOR` (or `TERM=dumb`) gives the plain theme.
/// `oxa-omarchy` without an Omarchy palette is the `system` theme.
pub(crate) fn resolve_ui_theme(
    opts: &InteractiveOpts,
    name: &str,
    caps: TermCaps,
    colors: Option<&TerminalColors>,
) -> Result<Theme, OxaError> {
    if skin_of(opts).is_oxa() && crate::skin::env_no_color(&opts.env) {
        return Ok(Theme::plain());
    }
    let dirs = vec![opts.dirs.agent_dir.join("themes")];
    match name.trim() {
        "oxa" => Ok(oxa_theme(&oxa_builtin_palette(), "oxa", caps.color)),
        "oxa-omarchy" => match load_omarchy_palette(&omarchy_candidates(opts)) {
            Some((palette, path)) => {
                let mut theme = oxa_theme(&palette, "oxa-omarchy", caps.color);
                // The palette file is the theme's source: a desktop theme switch reloads it.
                theme.source = Some(path);
                Ok(theme)
            }
            None => resolve_theme("system", &dirs, caps.color, colors),
        },
        other => resolve_theme(other, &dirs, caps.color, colors),
    }
}

/// `--theme`, then the `theme` setting, then the default: pi's `system`, or `oxa-omarchy`
/// with OXA's look.
pub(crate) fn theme_name(opts: &InteractiveOpts) -> String {
    opts.theme
        .clone()
        .or_else(|| opts.settings.string("theme"))
        .unwrap_or_else(|| {
            if skin_of(opts).is_oxa() {
                "oxa-omarchy".to_string()
            } else {
                "system".to_string()
            }
        })
}

/// The terminal colours the themes derive from (pi theme.ts `detectTerminalTheme`): the OSC
/// 10/11 answers; without a reported background, `COLORFGBG`'s light/dark guess stands in as a
/// white or black background; else nothing (the themes assume dark).
pub(crate) fn terminal_colors(
    opts: &InteractiveOpts,
    reported: Option<TerminalColors>,
) -> Option<TerminalColors> {
    let fgbg = opts
        .env
        .iter()
        .find(|(k, _)| k == "COLORFGBG")
        .map(|(_, v)| v.as_str());
    terminal_colors_from(fgbg, reported)
}

/// [`terminal_colors`] from the `COLORFGBG` value.
pub(crate) fn terminal_colors_from(
    fgbg: Option<&str>,
    reported: Option<TerminalColors>,
) -> Option<TerminalColors> {
    let mut colors = reported.unwrap_or_default();
    if colors.bg.is_none() {
        colors.bg = appearance_from_colorfgbg(fgbg).map(|a| match a {
            Appearance::Light => (255, 255, 255),
            Appearance::Dark => (0, 0, 0),
        });
    }
    (colors != TerminalColors::default()).then_some(colors)
}

pub(crate) fn load_theme(opts: &InteractiveOpts, caps: TermCaps, colors: Option<&TerminalColors>) -> Theme {
    let name = theme_name(opts);
    resolve_ui_theme(opts, &name, caps, colors)
        .or_else(|_| resolve_ui_theme(opts, "dark", caps, colors))
        .unwrap_or_else(|_| Theme::plain())
}

pub(crate) fn push_notice(vm: &mut ViewModel, level: Level, text: String) {
    vm.blocks.push(Block::Notice { level, text });
}

fn tool_index(vm: &ViewModel, call_id: &str) -> Option<usize> {
    vm.blocks
        .iter()
        .rposition(|b| matches!(b, Block::Tool(t) if t.call_id == call_id))
}

fn new_tool(call_id: &str, name: &str, args: Value, state: ToolState) -> ToolBlock {
    ToolBlock {
        call_id: call_id.to_string(),
        name: name.to_string(),
        args,
        args_raw: String::new(),
        state,
        partial: String::new(),
        result: None,
        duration_ms: None,
        expanded: false,
    }
}

fn parse_args(raw: &str) -> Value {
    serde_json::from_str::<Value>(raw).unwrap_or(Value::Null)
}

fn apply_delta(vm: &mut ViewModel, d: &MessageDelta) {
    let open = open_assistant(vm);
    match d {
        MessageDelta::TextStart { .. }
        | MessageDelta::ThinkingStart { .. }
        | MessageDelta::ThinkingEnd { .. } => {}
        MessageDelta::TextDelta { delta, .. } => {
            if let Some(i) = open {
                if let Block::Assistant { text, .. } = &mut vm.blocks[i] {
                    text.push_str(delta);
                }
            }
            vm.status.activity = Activity::Streaming;
        }
        MessageDelta::TextEnd { content, .. } => {
            if let Some(i) = open {
                if let Block::Assistant { text, .. } = &mut vm.blocks[i] {
                    *text = content.clone();
                }
            }
        }
        MessageDelta::ThinkingDelta { delta, .. } => {
            if let Some(i) = open {
                if let Block::Assistant { thinking, .. } = &mut vm.blocks[i] {
                    thinking.push_str(delta);
                }
            }
        }
        MessageDelta::ToolcallStart { id, name, .. } => {
            vm.blocks
                .push(Block::Tool(new_tool(id, name, Value::Null, ToolState::Streaming)));
            vm.status.activity = Activity::Streaming;
        }
        MessageDelta::ToolcallDelta { delta, .. } => {
            if let Some(Block::Tool(t)) = vm
                .blocks
                .iter_mut()
                .rev()
                .find(|b| matches!(b, Block::Tool(t) if t.state == ToolState::Streaming))
            {
                t.args_raw.push_str(delta);
                t.args = parse_args(&t.args_raw);
            }
        }
        MessageDelta::ToolcallEnd { call, .. } => match tool_index(vm, &call.id) {
            Some(i) => {
                if let Block::Tool(t) = &mut vm.blocks[i] {
                    t.name = call.name.clone();
                    t.args_raw = call.args_raw.clone();
                    t.args = parse_args(&call.args_raw);
                    if t.state == ToolState::Streaming {
                        t.state = ToolState::Pending;
                    }
                }
            }
            None => {
                let mut t = new_tool(
                    &call.id,
                    &call.name,
                    parse_args(&call.args_raw),
                    ToolState::Pending,
                );
                t.args_raw = call.args_raw.clone();
                vm.blocks.push(Block::Tool(t));
            }
        },
    }
}

/// Index of the open (streaming) assistant block, the last one.
fn open_assistant(vm: &ViewModel) -> Option<usize> {
    vm.blocks
        .iter()
        .rposition(|b| matches!(b, Block::Assistant { streaming: true, .. }))
}

/// `message_end` of an assistant reply: a complete reply stays an `Assistant` block; an
/// aborted or failed one becomes a `Shown` block (W2); a length cut gets a notice.
fn finish_assistant(vm: &mut ViewModel, message: &EventMessage) {
    if let Some(u) = &message.usage {
        vm.status.last_prompt = Some(u.prompt);
        vm.status.last_cached = Some(u.cached);
        vm.status.context_tokens = u.prompt + u.completion;
        vm.status.total_in += u64::from(u.prompt);
        vm.status.total_out += u64::from(u.completion);
    }
    let (old_text, old_thinking, old_model) = match open_assistant(vm) {
        Some(i) => match &vm.blocks[i] {
            Block::Assistant {
                text,
                thinking,
                model,
                ..
            } => (text.clone(), thinking.clone(), model.clone()),
            _ => (String::new(), String::new(), None),
        },
        None => (String::new(), String::new(), None),
    };
    let text = message
        .msg
        .content
        .clone()
        .filter(|t| !t.is_empty())
        .unwrap_or(old_text);
    let thinking = message
        .msg
        .reasoning
        .clone()
        .filter(|t| !t.is_empty())
        .unwrap_or(old_thinking);
    let model = message.model.clone().or(old_model);
    let block = match message.stop {
        Some(StopReason::Aborted) => Block::Shown {
            text,
            why: shown_why::ABORTED.to_string(),
        },
        Some(StopReason::Error) => {
            let text = if text.is_empty() {
                message.error_message.clone().unwrap_or_default()
            } else {
                text
            };
            Block::Shown {
                text,
                why: shown_why::FAILED.to_string(),
            }
        }
        stop => {
            if stop == Some(StopReason::Length) {
                push_notice(
                    vm,
                    Level::Warning,
                    "Reply cut: the model reached its output token limit.".to_string(),
                );
            }
            Block::Assistant {
                text,
                thinking,
                streaming: false,
                stop,
                error: message.error_message.clone(),
                model,
            }
        }
    };
    match open_assistant(vm) {
        Some(i) => vm.blocks[i] = block,
        None => vm.blocks.push(block),
    }
}

/// A reply that did not complete (`why` = aborted, failed or cut): the open assistant block
/// becomes the `Shown` block with the text the core stored (the partial text, or the error
/// when nothing had arrived), and tool calls that were still streaming in that reply are
/// dropped: they were never complete and never ran. Without an open block (a failure between
/// turns) the `Shown` block is added.
fn shown_reply(vm: &mut ViewModel, text: &str, why: &str) {
    let block = Block::Shown {
        text: text.to_string(),
        why: why.to_string(),
    };
    match open_assistant(vm) {
        Some(i) => {
            vm.blocks[i] = block;
            let mut k = i + 1;
            while k < vm.blocks.len() {
                if matches!(&vm.blocks[k], Block::Tool(t) if t.state == ToolState::Streaming) {
                    vm.blocks.remove(k);
                } else {
                    k += 1;
                }
            }
        }
        None => vm.blocks.push(block),
    }
}

/// A settled run must not leave blocks open (an aborted stream may never send `message_end`).
fn close_open_blocks(vm: &mut ViewModel) {
    for b in vm.blocks.iter_mut() {
        match b {
            Block::Assistant { streaming, .. } => *streaming = false,
            Block::Tool(t)
                if matches!(
                    t.state,
                    ToolState::Streaming | ToolState::Pending | ToolState::Running
                ) =>
            {
                t.state = ToolState::Error;
            }
            Block::Bash { running, .. } => *running = false,
            _ => {}
        }
    }
}

fn git_branch(cwd: &Path) -> Option<String> {
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        if let Ok(head) = std::fs::read_to_string(d.join(".git").join("HEAD")) {
            return head.trim().strip_prefix("ref: refs/heads/").map(str::to_string);
        }
        dir = d.parent();
    }
    None
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod preload_tests {
    use super::*;

    /// O-152 / wave H: the boot preload's progress never reaches the view model (it would set
    /// the foreground activity and turn the first prompt into steering); other events pass.
    #[test]
    fn preload_sink_drops_progress_only() {
        let (tx, rx) = mpsc::channel();
        let mut sink = PreloadSink(ChanSink(tx));
        sink.emit(&AgentEvent::Prefill {
            done: 1,
            total: 2,
            secs: 0,
        });
        sink.emit(&AgentEvent::EngineQueue {
            pos: Some(1),
            wait_s: 0,
        });
        sink.emit(&notice_event("prefix preload skipped"));
        let got: Vec<WorkerMsg> = rx.try_iter().collect();
        assert_eq!(got.len(), 1);
        assert!(matches!(&got[0], WorkerMsg::Event(AgentEvent::Notice { .. })));
    }

    #[test]
    fn worker_stop_reports_the_preload() {
        let stop = WorkerStop::new();
        assert!(!stop.is_preloading());
        stop.set_preloading(true);
        assert!(stop.clone().is_preloading());
        stop.set_preloading(false);
        assert!(!stop.is_preloading());
    }
}

#[cfg(test)]
mod profile_word_tests {
    use super::*;
    use serde_json::json;

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn settings(v: serde_json::Value) -> oxa_config::Settings {
        oxa_config::Settings::from_values(v, json!({}))
    }

    #[test]
    fn profile_word_only_for_local() {
        assert_eq!(
            profile_word(&env(&[]), &settings(json!({}))),
            Some("local".to_string()),
            "the default profile is local"
        );
        assert_eq!(
            profile_word(&env(&[]), &settings(json!({"oxa": {"profile": "pi"}}))),
            None
        );
        assert_eq!(
            profile_word(&env(&[]), &settings(json!({"oxa": {"profile": "local"}}))),
            Some("local".to_string())
        );
        assert_eq!(
            profile_word(
                &env(&[("OXA_PROFILE", "pi")]),
                &settings(json!({"oxa": {"profile": "local"}}))
            ),
            None,
            "the environment wins"
        );
        assert_eq!(
            profile_word(&env(&[("OXA_PROFILE", "local")]), &settings(json!({}))),
            Some("local".to_string())
        );
        assert_eq!(
            profile_word(
                &env(&[("OXA_PROFILE", "x")]),
                &settings(json!({"oxa": {"profile": "local"}}))
            ),
            Some("local".to_string())
        );
    }
}

#[cfg(test)]
mod terminal_colour_tests {
    use super::*;

    /// pi theme.ts detectTerminalTheme: the reported background, else COLORFGBG, else nothing
    /// (the themes assume dark).
    #[test]
    fn reported_colours_then_colorfgbg() {
        let opts = |env: &[(&str, &str)]| -> Vec<(String, String)> {
            env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
        };
        let pick = |env: Vec<(String, String)>, reported: Option<TerminalColors>| {
            let fgbg = env.iter().find(|(k, _)| k == "COLORFGBG").map(|(_, v)| v.clone());
            terminal_colors_from(fgbg.as_deref(), reported)
        };
        let dark = TerminalColors {
            fg: Some((220, 220, 220)),
            bg: Some((20, 20, 20)),
        };
        assert_eq!(
            pick(opts(&[("COLORFGBG", "0;15")]), Some(dark)),
            Some(dark),
            "the answer wins"
        );
        assert_eq!(
            pick(opts(&[("COLORFGBG", "0;15")]), None).and_then(|c| c.appearance()),
            Some(Appearance::Light)
        );
        assert_eq!(
            pick(opts(&[("COLORFGBG", "15;0")]), None).and_then(|c| c.appearance()),
            Some(Appearance::Dark)
        );
        assert_eq!(pick(opts(&[]), None), None);
    }
}

#[cfg(test)]
mod status_reset_tests {
    use super::*;

    fn stale() -> StatusModel {
        StatusModel {
            model: "a".into(),
            last_cached: Some(97),
            last_prompt: Some(100),
            tok_per_s: Some(54.4),
            context_tokens: 100,
            ..StatusModel::default()
        }
    }

    /// The TUI path of `/model`: the `ModelSelect` event, before any new turn.
    #[test]
    fn model_select_event_clears_speed_and_cache() {
        let mut vm = ViewModel {
            status: stale(),
            ..ViewModel::default()
        };
        apply_event(
            &mut vm,
            &AgentEvent::ModelSelect {
                model: "b".into(),
                previous: Some("a".into()),
            },
        );
        assert_eq!(
            (vm.status.tok_per_s, vm.status.last_cached, vm.status.last_prompt),
            (None, None, None)
        );
    }

    /// The TUI path of `/new` (`WorkerMsg::Reopened` / `Status` -> `refresh_status`).
    #[test]
    fn refresh_status_clears_cache_on_empty_session_and_keeps_it_otherwise() {
        let mut dst = stale();
        refresh_status(
            &mut dst,
            StatusModel {
                model: "a".into(),
                context_tokens: 0,
                ..StatusModel::default()
            },
        );
        assert_eq!((dst.last_cached, dst.last_prompt), (None, None));
        let mut dst = stale();
        refresh_status(
            &mut dst,
            StatusModel {
                model: "a".into(),
                context_tokens: 120,
                ..StatusModel::default()
            },
        );
        assert_eq!(dst.last_cached, Some(97));
    }
}
