//! Post-wave-E fixes in interactive mode (lane L4): the worker's idle tick serves the host and
//! runs triggered turns (item 3), extension shortcuts reach `Session::run_shortcut` (item 3),
//! the worker's own stop flag (item 6), the `queued` activity (item 7) and the model-switch
//! over-cap decision (item 8). In-process: a scripted engine and a native hook set; every
//! scenario that involves the worker thread runs under a hard time limit.

use oxa_config::{Dirs, Keymap, Settings};
use oxa_core::{Deps, Open, Session, SwitchPlan, ToolSelection};
use oxa_fake::{ScriptTurn, ScriptedEngine, TestEnv};
use oxa_interactive::{
    activity_text, apply_event, over_cap_question, queued_text, run_loop, switch_step, worker_loop_with,
    Activity, InteractiveOpts, OverCapChoice, Reopen, ReopenFn, SessionCmd, SessionInfo, SwitchStep,
    UiRequest, ViewModel, WorkerMsg, WorkerStop, OVER_CAP_CHOICES,
};
use oxa_proto::{
    shown_why, AgentEvent, CommandSpec, Delivery, Entry, EntryBody, HookPoint, HookReply, Hooks, Host,
    HostAction, KeyId, OxaError, ShortcutSpec, ThinkingLevel, Tool, Ui, Value,
};
use oxa_resources::Resources;
use oxa_session::{MemStore, SessionStore};
use oxa_tui::{InputEvent, Key, TermCaps, TestTerminal};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct AllowAll;

impl oxa_proto::Policy for AllowAll {
    fn decide(
        &self,
        _mode: oxa_proto::Mode,
        _a: &oxa_proto::Access,
        _cwd: &std::path::Path,
    ) -> oxa_proto::Decision {
        oxa_proto::Decision::Allow
    }
    fn remember(&mut self, _a: &oxa_proto::Access) {}
}

struct NoBash;

impl oxa_proto::Sandbox for NoBash {
    fn check(&self) -> Result<(), OxaError> {
        Ok(())
    }
    fn bwrap_args(&self, _s: &oxa_proto::ExecSpec) -> Vec<String> {
        Vec::new()
    }
    fn run(
        &self,
        _s: &oxa_proto::ExecSpec,
        _c: &oxa_proto::CancelToken,
        _live: &mut dyn FnMut(&[u8]),
    ) -> oxa_proto::ExecResult {
        oxa_proto::ExecResult {
            output: Vec::new(),
            exit: Some(0),
            timed_out: false,
        }
    }
}

fn deps(
    env: &TestEnv,
    store: Arc<dyn SessionStore>,
    engine: Arc<ScriptedEngine>,
    hooks: Vec<Arc<dyn Hooks>>,
) -> Deps {
    deps_with_settings(env, store, engine, hooks, Settings::default())
}

fn deps_with_settings(
    env: &TestEnv,
    store: Arc<dyn SessionStore>,
    engine: Arc<ScriptedEngine>,
    hooks: Vec<Arc<dyn Hooks>>,
    settings: Settings,
) -> Deps {
    Deps {
        engine,
        store,
        tools: Vec::new(),
        hooks,
        policy: Box::new(AllowAll),
        sandbox: Arc::new(NoBash),
        settings,
        dirs: Dirs::under(env.root()),
        cwd: env.work(),
        selection: ToolSelection::default(),
        resources: Resources::default(),
        model: None,
        thinking: None,
        mode: None,
        session_id: None,
        jobs: oxa_core::fresh_jobs(),
    }
}

/// Run `f` on its own thread; fail if it has not finished within `secs`.
fn bounded(secs: u64, f: impl FnOnce() + Send + 'static) {
    let (tx, rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        f();
        let _ = tx.send(());
    });
    match rx.recv_timeout(Duration::from_secs(secs)) {
        Ok(()) => worker.join().expect("scenario thread"),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            if let Err(p) = worker.join() {
                std::panic::resume_unwind(p);
            }
        }
        Err(mpsc::RecvTimeoutError::Timeout) => panic!("the scenario did not finish in {secs} s (hang)"),
    }
}

fn trigger(text: &str) -> HostAction {
    HostAction::SendMessage {
        custom_type: "test".into(),
        content: text.into(),
        display: true,
        details: Value::Null,
        deliver: Delivery::FollowUp,
        trigger_turn: true,
    }
}

/// A hook set that keeps its host, binds `ctrl+alt+y` and can block at `agent_start`.
#[derive(Default)]
struct TestHooks {
    host: Mutex<Option<Arc<dyn Host>>>,
    shortcut_runs: AtomicUsize,
    /// `agent_start`: signal here, then wait until `release` is set.
    started: Mutex<Option<Sender<()>>>,
    release: Arc<AtomicBool>,
    hold: bool,
}

const EXT_KEY: &str = "ctrl+alt+y";

impl Hooks for TestHooks {
    fn fire(&self, p: HookPoint, _payload: &Value, _ui: &mut dyn Ui) -> HookReply {
        if p == HookPoint::AgentStart && self.hold {
            if let Some(tx) = self.started.lock().unwrap().as_ref() {
                let _ = tx.send(());
            }
            while !self.release.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        HookReply::Continue
    }
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        Vec::new()
    }
    fn commands(&self) -> Vec<CommandSpec> {
        Vec::new()
    }
    fn run_command(&self, name: &str, _args: &str, _ui: &mut dyn Ui) -> Result<(), OxaError> {
        Err(OxaError::new(oxa_proto::codes::NOT_FOUND, name))
    }
    fn prompt_sections(&self) -> Vec<(String, String)> {
        Vec::new()
    }
    fn attach(&self, host: Arc<dyn Host>) {
        *self.host.lock().unwrap() = Some(host);
    }
    fn shortcuts(&self) -> Vec<ShortcutSpec> {
        vec![ShortcutSpec {
            key: KeyId::parse(EXT_KEY).expect("key"),
            description: "test".into(),
        }]
    }
    fn run_shortcut(&self, _key: &KeyId, _ui: &mut dyn Ui) -> Result<(), OxaError> {
        self.shortcut_runs.fetch_add(1, Ordering::SeqCst);
        // The usual pattern: the shortcut sends a message that starts a turn.
        if let Some(h) = self.host.lock().unwrap().clone() {
            h.call(trigger("from the shortcut"))?;
        }
        Ok(())
    }
}

struct Worker {
    cmds: Sender<SessionCmd>,
    msgs: Receiver<WorkerMsg>,
    _ui: Receiver<UiRequest>,
    done: Receiver<Option<SessionInfo>>,
    stop: WorkerStop,
}

fn no_reopen() -> ReopenFn {
    Box::new(|_, _| Err(OxaError::new("invalid", "no reopen in this test")))
}

fn spawn_worker(session: Session, reopen: ReopenFn) -> Worker {
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let (msg_tx, msg_rx) = mpsc::channel();
    let (ui_tx, ui_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let stop = WorkerStop::new();
    let worker_stop = stop.clone();
    std::thread::spawn(move || {
        let last = worker_loop_with(session, reopen, cmd_rx, msg_tx, ui_tx, worker_stop);
        let _ = done_tx.send(last);
    });
    Worker {
        cmds: cmd_tx,
        msgs: msg_rx,
        _ui: ui_rx,
        done: done_rx,
        stop,
    }
}

/// Collect worker messages until `until` holds for an event (or 10 s pass).
fn events_until(w: &Worker, until: impl Fn(&AgentEvent) -> bool) -> Vec<AgentEvent> {
    let end = Instant::now() + Duration::from_secs(10);
    let mut out = Vec::new();
    while Instant::now() < end {
        if let Ok(WorkerMsg::Event(e)) = w.msgs.recv_timeout(Duration::from_millis(50)) {
            let stop = until(&e);
            out.push(e);
            if stop {
                return out;
            }
        }
    }
    panic!("the expected event did not arrive; got {} events", out.len());
}

/// Drain messages until a `Done` arrives, then return the `Status` that follows it (the worker
/// sends `Done`, then a fresh `Status`, after every command).
fn status_after_done(w: &Worker) -> oxa_interactive::StatusModel {
    let end = Instant::now() + Duration::from_secs(10);
    let mut done = false;
    while Instant::now() < end {
        match w.msgs.recv_timeout(Duration::from_millis(50)) {
            Ok(WorkerMsg::Done(_)) => done = true,
            Ok(WorkerMsg::Status(s)) if done => return s,
            Ok(_) => {}
            Err(_) => {}
        }
    }
    panic!("no Status after Done arrived");
}

/// Item 3: a turn an extension triggers while the session is idle runs on the worker's idle
/// tick, and its events reach the UI like a prompt's.
#[test]
fn idle_tick_runs_a_triggered_turn() {
    bounded(30, || {
        let env = TestEnv::new();
        let hooks = Arc::new(TestHooks::default());
        let engine = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("pong")]));
        let session = Session::open(
            deps(
                &env,
                Arc::new(MemStore::new()),
                engine.clone(),
                vec![hooks.clone()],
            ),
            Open::New,
        )
        .expect("open");
        let w = spawn_worker(session, no_reopen());
        let host = hooks.host.lock().unwrap().clone().expect("attached");
        host.call(trigger("ping")).expect("accepted");
        let events = events_until(&w, |e| matches!(e, AgentEvent::AgentSettled { .. }));
        assert!(events.iter().any(|e| matches!(e, AgentEvent::AgentStart)));
        assert!(events.iter().any(|e| matches!(
            e,
            AgentEvent::EntryAppended { entry: Entry { body: EntryBody::CustomMessage { content, .. }, .. } }
                if content == "ping"
        )));
        // O-152: the idle tick may also send the boot preload (a `prefill`); one chat turn.
        let turns: Vec<_> = engine
            .requests()
            .into_iter()
            .filter(|r| r.kind != "prefill")
            .collect();
        assert_eq!(turns.len(), 1);
        let _ = w.cmds.send(SessionCmd::Shutdown);
        w.done
            .recv_timeout(Duration::from_secs(10))
            .expect("worker ended");
    });
}

/// Item 3: `SessionCmd::Shortcut` runs the extension's handler through `Session::run_shortcut`;
/// a turn it triggers runs right after it. The worker announces the bound keys.
#[test]
fn shortcut_command_runs_the_handler_and_its_turn() {
    bounded(30, || {
        let env = TestEnv::new();
        let hooks = Arc::new(TestHooks::default());
        let engine = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("done")]));
        let session = Session::open(
            deps(
                &env,
                Arc::new(MemStore::new()),
                engine.clone(),
                vec![hooks.clone()],
            ),
            Open::New,
        )
        .expect("open");
        let w = spawn_worker(session, no_reopen());
        match w.msgs.recv_timeout(Duration::from_secs(10)) {
            Ok(WorkerMsg::Session(info)) => {
                assert_eq!(info.shortcuts, vec![KeyId::parse(EXT_KEY).unwrap()]);
            }
            _ => panic!("the worker announces its session first"),
        }
        w.cmds
            .send(SessionCmd::Shortcut(KeyId::parse(EXT_KEY).unwrap()))
            .unwrap();
        events_until(&w, |e| matches!(e, AgentEvent::AgentSettled { .. }));
        assert_eq!(hooks.shortcut_runs.load(Ordering::SeqCst), 1);
        // O-152: the idle tick may also send the boot preload (a `prefill`); one chat turn.
        let turns: Vec<_> = engine
            .requests()
            .into_iter()
            .filter(|r| r.kind != "prefill")
            .collect();
        assert_eq!(turns.len(), 1);
        let _ = w.cmds.send(SessionCmd::Shutdown);
        w.done
            .recv_timeout(Duration::from_secs(10))
            .expect("worker ended");
    });
}

fn opts(env: &TestEnv) -> InteractiveOpts {
    InteractiveOpts {
        fullscreen: false,
        dirs: Dirs::under(env.root()),
        settings: Settings::default(),
        keymap: Keymap::defaults(),
        store: Arc::new(MemStore::new()),
        hooks: Vec::new(),
        mcp: None,
        initial_prompt: None,
        initial_parts: Vec::new(),
        initial_follow_ups: Vec::new(),
        theme: None,
        theme_paths: Vec::new(),
        notices: Vec::new(),
        env: Vec::new(),
        caps: Some(TermCaps::BASIC),
        reopen: no_reopen(),
        open_picker: false,
        save_trust: None,
        gated: Vec::new(),
        yolo: None,
    }
}

/// Item 3, key path: a key an extension bound and no built-in binding has goes to the worker
/// as `SessionCmd::Shortcut`; a key with a built-in binding keeps it (built-ins win).
#[test]
fn extension_key_goes_to_the_worker_unless_built_in() {
    let env = TestEnv::new();
    let keymap = Keymap::defaults();
    let free = KeyId::parse(EXT_KEY).unwrap();
    let taken = KeyId::parse("ctrl+t").unwrap();
    assert!(
        keymap.actions_for(&free).is_empty(),
        "the test key must be unbound"
    );
    assert!(
        !keymap.actions_for(&taken).is_empty(),
        "the clashing key must be bound"
    );

    let session = Session::open(
        deps(
            &env,
            Arc::new(MemStore::new()),
            Arc::new(ScriptedEngine::new(Vec::new())),
            Vec::new(),
        ),
        Open::New,
    )
    .expect("open");
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let (msg_tx, msg_rx) = mpsc::channel();
    let (_ui_tx, ui_rx) = mpsc::channel();
    msg_tx
        .send(WorkerMsg::Session(SessionInfo {
            shortcuts: vec![free.clone(), taken.clone()],
            ..SessionInfo::default()
        }))
        .unwrap();
    let mut term = TestTerminal::new(80, 24);
    term.push_input(vec![
        InputEvent::Key(Key::press(EXT_KEY)),
        InputEvent::Key(Key::press("ctrl+t")),
        InputEvent::Key(Key::press("ctrl+d")),
    ]);
    let mut o = opts(&env);
    let code = run_loop(
        &mut term,
        &mut o,
        session.handle(),
        cmd_tx,
        msg_rx,
        ui_rx,
        ViewModel::default(),
    )
    .expect("loop");
    assert_eq!(code, 0);
    let shortcuts: Vec<KeyId> = cmd_rx
        .try_iter()
        .filter_map(|c| match c {
            SessionCmd::Shortcut(k) => Some(k),
            _ => None,
        })
        .collect();
    assert_eq!(shortcuts, vec![free]);
}

/// Item 6: quit while a session swap is in flight. The new session is entered after the stop
/// flag was set: nothing runs on it (the prompt queued behind the swap never starts) and the
/// worker ends with it.
#[test]
fn stop_during_a_swap_aborts_the_session_entered_after_it() {
    bounded(30, || {
        let env = TestEnv::new();
        let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
        let first = Session::open(
            deps(
                &env,
                store.clone(),
                Arc::new(ScriptedEngine::new(Vec::new())),
                Vec::new(),
            ),
            Open::New,
        )
        .expect("open");
        let old_id = first.state().session_id;
        let engine2 = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("NEVER")]));
        let (entered_tx, entered_rx) = mpsc::channel::<()>();
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        let env2 = TestEnv::new();
        let (e2, s2) = (engine2.clone(), store.clone());
        let reopen: ReopenFn = Box::new(move |_, _| {
            let _ = entered_tx.send(());
            let _ = gate_rx.recv();
            Session::open(deps(&env2, s2.clone(), e2.clone(), Vec::new()), Open::New)
        });
        let w = spawn_worker(first, reopen);
        w.cmds.send(SessionCmd::Reopen(Reopen::New)).unwrap();
        w.cmds
            .send(SessionCmd::Prompt {
                text: "queued behind the swap".into(),
                parts: Vec::new(),
            })
            .unwrap();
        entered_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the swap began");
        // The quit arrives while the swap is in flight. No `Shutdown` command is sent: the flag
        // alone must stop the worker.
        w.stop.stop();
        gate_tx.send(()).unwrap();
        let last = w
            .done
            .recv_timeout(Duration::from_secs(10))
            .expect("the worker stopped")
            .expect("it holds the new session");
        assert_ne!(last.id, old_id, "the swap completed");
        assert!(engine2.requests().is_empty(), "nothing ran on the new session");
    });
}

/// Hooks that ask for a new session at `agent_end` and may cancel `session_before_switch`.
#[derive(Default)]
struct SwapHooks {
    host: Mutex<Option<Arc<dyn Host>>>,
    cancel: bool,
    before_switch: AtomicUsize,
}

impl Hooks for SwapHooks {
    fn fire(&self, p: HookPoint, _payload: &Value, _ui: &mut dyn Ui) -> HookReply {
        match p {
            HookPoint::AgentEnd => {
                if let Some(h) = self.host.lock().unwrap().clone() {
                    let _ = h.call(HostAction::NewSession);
                }
            }
            HookPoint::SessionBeforeSwitch => {
                self.before_switch.fetch_add(1, Ordering::SeqCst);
                if self.cancel {
                    return HookReply::Cancel("not now".into());
                }
            }
            _ => {}
        }
        HookReply::Continue
    }
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        Vec::new()
    }
    fn commands(&self) -> Vec<CommandSpec> {
        Vec::new()
    }
    fn run_command(&self, name: &str, _args: &str, _ui: &mut dyn Ui) -> Result<(), OxaError> {
        Err(OxaError::new(oxa_proto::codes::NOT_FOUND, name))
    }
    fn prompt_sections(&self) -> Vec<(String, String)> {
        Vec::new()
    }
    fn attach(&self, host: Arc<dyn Host>) {
        *self.host.lock().unwrap() = Some(host);
    }
}

/// Run one prompt whose `agent_end` handler asks for a new session; returns the worker
/// messages seen until the worker settles, the hooks and the original session id.
fn swap_scenario(cancel: bool) -> (Vec<WorkerMsg>, Arc<SwapHooks>, String, Worker) {
    let env = TestEnv::new();
    let hooks = Arc::new(SwapHooks {
        cancel,
        ..SwapHooks::default()
    });
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    let engine = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("ok")]));
    let first =
        Session::open(deps(&env, store.clone(), engine, vec![hooks.clone()]), Open::New).expect("open");
    let old_id = first.state().session_id.to_string();
    let env2 = TestEnv::new();
    let (h2, s2) = (hooks.clone(), store.clone());
    let reopen: ReopenFn = Box::new(move |_, _| {
        let engine = Arc::new(ScriptedEngine::new(Vec::new()));
        Session::open(deps(&env2, s2.clone(), engine, vec![h2.clone()]), Open::New)
    });
    let w = spawn_worker(first, reopen);
    w.cmds
        .send(SessionCmd::Prompt {
            text: "hi".into(),
            parts: Vec::new(),
        })
        .unwrap();
    let mut seen = Vec::new();
    let end = Instant::now() + Duration::from_secs(5);
    while Instant::now() < end {
        if let Ok(m) = w.msgs.recv_timeout(Duration::from_millis(50)) {
            seen.push(m);
        }
    }
    (seen, hooks, old_id, w)
}

/// An extension's `newSession` in the TUI reopens the session, after the before-switch hook.
#[test]
fn tui_applies_an_extension_new_session_through_the_before_switch_hook() {
    bounded(30, || {
        let (seen, hooks, old_id, w) = swap_scenario(false);
        w.stop.stop();
        let last = w.done.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
        assert_ne!(last.id.to_string(), old_id, "the session was swapped");
        assert!(hooks.before_switch.load(Ordering::SeqCst) >= 1);
        assert!(seen.iter().any(|m| matches!(m, WorkerMsg::Rebind(_))));
    });
}

/// A cancel from `session_before_switch` keeps the session, and the refusal does not send a
/// stray `Done` (no command is running).
#[test]
fn tui_extension_swap_cancelled_by_the_hook_keeps_the_session_without_a_stray_done() {
    bounded(30, || {
        let (seen, hooks, old_id, w) = swap_scenario(true);
        w.stop.stop();
        let last = w.done.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
        assert_eq!(last.id.to_string(), old_id, "cancelled: still the old session");
        assert!(hooks.before_switch.load(Ordering::SeqCst) >= 1);
        let dones = seen.iter().filter(|m| matches!(m, WorkerMsg::Done(_))).count();
        assert_eq!(dones, 1, "only the prompt's own Done");
    });
}

/// Item 6: the stop flag aborts the run in progress (here a triggered turn on the idle tick)
/// without any handle from the UI side.
#[test]
fn stop_aborts_the_run_in_progress() {
    bounded(30, || {
        let env = TestEnv::new();
        let (started_tx, started_rx) = mpsc::channel();
        let hooks = Arc::new(TestHooks {
            hold: true,
            started: Mutex::new(Some(started_tx)),
            ..TestHooks::default()
        });
        let engine = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("NEVER")]));
        let session = Session::open(
            deps(
                &env,
                Arc::new(MemStore::new()),
                engine.clone(),
                vec![hooks.clone()],
            ),
            Open::New,
        )
        .expect("open");
        let w = spawn_worker(session, no_reopen());
        let host = hooks.host.lock().unwrap().clone().expect("attached");
        host.call(trigger("go")).expect("accepted");
        started_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the run started");
        w.stop.stop();
        hooks.release.store(true, Ordering::SeqCst);
        let events = events_until(&w, |e| matches!(e, AgentEvent::AgentSettled { .. }));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::AgentSettled { aborted: true })),
            "the run was aborted"
        );
        assert!(!events.iter().any(|e| matches!(
            e,
            AgentEvent::MessageEnd { message } if message.msg.content.as_deref() == Some("NEVER")
        )));
        w.done
            .recv_timeout(Duration::from_secs(10))
            .expect("the worker stopped");
    });
}

/// Item 7: `engine_queue` shows as `queued #<pos>` / `queued <wait_s>s` and is cleared by the
/// first data event, by the end of the request (a shown abort) and by settlement.
#[test]
fn engine_queue_activity_is_shown_and_cleared() {
    let mut vm = ViewModel::default();
    apply_event(&mut vm, &AgentEvent::AgentStart);
    apply_event(
        &mut vm,
        &AgentEvent::EngineQueue {
            pos: Some(3),
            wait_s: 4,
        },
    );
    assert_eq!(
        vm.status.activity,
        Activity::Queued {
            pos: Some(3),
            wait_s: 4
        }
    );
    assert_eq!(activity_text(&vm.status).as_deref(), Some("queued #3"));
    assert_eq!(queued_text(&vm.status.activity).as_deref(), Some("queued #3"));
    apply_event(
        &mut vm,
        &AgentEvent::EngineQueue {
            pos: None,
            wait_s: 12,
        },
    );
    assert_eq!(activity_text(&vm.status).as_deref(), Some("queued 12s"));
    // The first prefill event ends the wait.
    apply_event(
        &mut vm,
        &AgentEvent::Prefill {
            done: 10,
            total: 100,
            secs: 1,
        },
    );
    assert!(matches!(vm.status.activity, Activity::Prefill { .. }));
    assert_eq!(
        vm.status.context_tokens, 100,
        "CTX counts the prompt during prefill"
    );
    assert_eq!(queued_text(&vm.status.activity), None);

    // An abort while queued: the shown entry ends the wait, settlement makes it idle.
    apply_event(
        &mut vm,
        &AgentEvent::EngineQueue {
            pos: Some(1),
            wait_s: 0,
        },
    );
    let shown = Entry {
        id: "e1".into(),
        parent: None,
        ts_ms: 0,
        body: EntryBody::Shown {
            text: String::new(),
            why: shown_why::ABORTED,
        },
    };
    apply_event(&mut vm, &AgentEvent::EntryAppended { entry: shown });
    assert!(!matches!(vm.status.activity, Activity::Queued { .. }));
    apply_event(
        &mut vm,
        &AgentEvent::EngineQueue {
            pos: Some(1),
            wait_s: 0,
        },
    );
    apply_event(&mut vm, &AgentEvent::AgentSettled { aborted: true });
    assert_eq!(vm.status.activity, Activity::Idle);
    // Not an error: no notice block was added (the only block is the shown abort).
    assert!(!vm
        .blocks
        .iter()
        .any(|b| matches!(b, oxa_interactive::Block::Notice { .. })));
}

fn plan(over_cap: bool) -> SwitchPlan {
    SwitchPlan {
        target: "small".into(),
        context_tokens: 50_000,
        target_ctx: Some(32_768),
        target_hard: Some(30_000),
        over_cap,
        cold_tokens: 50_000,
        est_secs: 430,
    }
}

/// Item 8: the over-cap decision as a pure function of the plan and the user's choice.
#[test]
fn model_switch_over_cap_decision() {
    assert_eq!(switch_step(&plan(false), None), SwitchStep::Switch);
    assert_eq!(
        switch_step(&plan(false), Some(OverCapChoice::Cancel)),
        SwitchStep::Switch,
        "no question below the cap"
    );
    let over = plan(true);
    assert_eq!(
        switch_step(&over, None),
        SwitchStep::Nothing,
        "never switch unasked"
    );
    assert_eq!(
        switch_step(&over, Some(OverCapChoice::CompactFirst)),
        SwitchStep::CompactThenSwitch
    );
    assert_eq!(
        switch_step(&over, Some(OverCapChoice::Fork)),
        SwitchStep::OpenFork
    );
    assert_eq!(
        switch_step(&over, Some(OverCapChoice::Cancel)),
        SwitchStep::Nothing
    );
    assert_eq!(OverCapChoice::from_answer(Some(0)), OverCapChoice::CompactFirst);
    assert_eq!(OverCapChoice::from_answer(Some(1)), OverCapChoice::Fork);
    assert_eq!(OverCapChoice::from_answer(Some(2)), OverCapChoice::Cancel);
    assert_eq!(
        OverCapChoice::from_answer(None),
        OverCapChoice::Cancel,
        "dismissed = cancel"
    );
    assert_eq!(
        OVER_CAP_CHOICES,
        ["Compact first", "Fork from an earlier message", "Cancel"]
    );
    let q = over_cap_question(&over);
    assert!(
        q.contains("50000") && q.contains("small") && q.contains("30000"),
        "{q}"
    );
}

/// S1 / ENG-002: input that waits in the queues when a command that is not a prompt ends
/// (it was typed during the command and cancelled its background request) is run by the
/// worker right after that command (`Session::run_queued`), not left until the next prompt.
#[test]
fn queued_input_runs_after_a_compact_command() {
    bounded(30, || {
        let env = TestEnv::new();
        let engine = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("pong")]));
        let session = Session::open(
            deps(&env, Arc::new(MemStore::new()), engine.clone(), Vec::new()),
            Open::New,
        )
        .expect("open");
        let handle = session.handle();
        handle.follow_up("typed meanwhile".to_string());
        let w = spawn_worker(session, no_reopen());
        // Nothing to summarise: the compaction itself sends no request.
        let _ = w.cmds.send(SessionCmd::Compact { instructions: None });
        let events = events_until(&w, |e| matches!(e, AgentEvent::AgentSettled { .. }));
        assert!(events.iter().any(|e| matches!(e, AgentEvent::AgentStart)));
        assert!(events.iter().any(|e| matches!(
            e,
            AgentEvent::EntryAppended { entry: Entry { body: EntryBody::Msg { wire, .. }, .. } }
                if wire.content.as_deref() == Some("typed meanwhile")
        )));
        assert_eq!(engine.requests().len(), 1, "the queued message was sent once");
        assert!(handle.queued().0.is_empty() && handle.queued().1.is_empty());
        let _ = w.cmds.send(SessionCmd::Shutdown);
        w.done
            .recv_timeout(Duration::from_secs(10))
            .expect("worker ended");
    });
}

/// #75: the status the worker sends after a `/compact` (the `Status` refresh that follows the
/// `CompactionEnd` event) keeps the post-compaction estimate, not 0. The `CompactionEnd` event
/// sets the estimate, but a later `Status` refresh used to overwrite it with
/// `stats.context_tokens` (0 after a rebase).
#[test]
fn status_after_compact_keeps_the_estimate() {
    bounded(30, || {
        let env = TestEnv::new();
        let engine = Arc::new(ScriptedEngine::new(vec![
            ScriptTurn::text("pong"),
            ScriptTurn::text("SUM"),
        ]));
        // A tiny keep budget forces the manual compaction to actually cut the transcript.
        let settings = Settings::from_values(
            serde_json::json!({"compaction": {"keepRecentTokens": 1}}),
            serde_json::json!({}),
        );
        let session = Session::open(
            deps_with_settings(
                &env,
                Arc::new(MemStore::new()),
                engine.clone(),
                Vec::new(),
                settings,
            ),
            Open::New,
        )
        .expect("open");
        let w = spawn_worker(session, no_reopen());
        // Build some context, then compact it.
        let _ = w.cmds.send(SessionCmd::Prompt {
            text: "hello".into(),
            parts: Vec::new(),
        });
        events_until(&w, |e| matches!(e, AgentEvent::AgentSettled { .. }));
        // Drain the prompt's own Done + Status so the next Status is the compaction's.
        let _ = status_after_done(&w);
        let _ = w.cmds.send(SessionCmd::Compact { instructions: None });
        // The worker sends Done, then a fresh Status; the Status must carry the estimate.
        let status = status_after_done(&w);
        assert!(
            status.context_tokens > 0,
            "the post-compaction status must keep the estimate, got {}",
            status.context_tokens
        );
        let _ = w.cmds.send(SessionCmd::Shutdown);
        w.done
            .recv_timeout(Duration::from_secs(10))
            .expect("worker ended");
    });
}

/// A `/new` after a `/model` (and `/thinking`) switch must keep the switched model and
/// thinking level, not fall back to the startup `--model` choice. The worker passes the model
/// and thinking of the session it leaves to the reopen closure (which the dispatcher uses for
/// `Open::New`); the status bar of the new session shows them.
#[test]
fn new_session_keeps_the_switched_model_and_thinking() {
    bounded(30, || {
        let env = TestEnv::new();
        let engine = Arc::new(ScriptedEngine::new(Vec::new()));
        let mut startup = deps(&env, Arc::new(MemStore::new()), engine.clone(), Vec::new());
        startup.model = Some("startup-model".to_string());
        startup.thinking = Some(ThinkingLevel::Off);
        let session = Session::open(startup, Open::New).expect("open");
        assert_eq!(session.state().model, "startup-model");

        // Mirrors the dispatcher's reopen closure: `/new` carries the current model and
        // thinking level over; the other modes restore the target session's own.
        let env2 = TestEnv::new();
        let engine2 = engine.clone();
        let reopen: ReopenFn = Box::new(move |_, carry| {
            let mut deps = deps(&env2, Arc::new(MemStore::new()), engine2.clone(), Vec::new());
            deps.model = Some("startup-model".to_string());
            deps.thinking = Some(ThinkingLevel::Off);
            if let Some((model, thinking)) = carry {
                deps.model = Some(model);
                deps.thinking = Some(thinking);
            }
            Session::open(deps, Open::New)
        });
        let w = spawn_worker(session, reopen);

        // `/model switched-model` then `/thinking high` on the running session.
        w.cmds
            .send(SessionCmd::SetModel("switched-model".to_string()))
            .unwrap();
        w.cmds.send(SessionCmd::SetThinking(ThinkingLevel::High)).unwrap();

        // `/new`: the new session's status bar must show the switched model and level.
        w.cmds.send(SessionCmd::Reopen(Reopen::New)).unwrap();
        let end = Instant::now() + Duration::from_secs(10);
        let mut model = String::new();
        let mut thinking = None;
        while Instant::now() < end {
            match w.msgs.recv_timeout(Duration::from_millis(50)) {
                Ok(WorkerMsg::Reopened { status, .. }) => {
                    model = status.model.clone();
                    thinking = status.thinking;
                    break;
                }
                Ok(_) => {}
                Err(_) => {}
            }
        }
        assert_eq!(
            model, "switched-model",
            "the new session kept the `/model` switch"
        );
        assert_eq!(
            thinking,
            Some(ThinkingLevel::High),
            "the new session kept the `/thinking` switch"
        );

        let _ = w.cmds.send(SessionCmd::Shutdown);
        w.done
            .recv_timeout(Duration::from_secs(10))
            .expect("worker ended");
    });
}
