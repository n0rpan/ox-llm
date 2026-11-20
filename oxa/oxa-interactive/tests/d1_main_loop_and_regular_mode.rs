//! D1 acceptance tests: Main loop and regular mode.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_config::{Dirs, Keymap, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{ScriptedEngine, TestEnv};
use oxa_interactive::{
    apply_event, blocks_from_entries, exit_lines, frames, run_loop, status_from, transcript_lines, Activity,
    Block, InteractiveOpts, RegularPrinter, SessionCmd, StatusModel, ToolState, UiRequest, ViewModel,
    ViewSettings, WorkerMsg,
};
use oxa_proto::{
    codes, Access, AgentEvent, AssistantTurn, CancelToken, ChatRequest, Decision, Engine, EngineInfo, Entry,
    EntryBody, EventMessage, ExecResult, ExecSpec, Level, Mode, OxaError, Part, Policy, Sandbox, StopReason,
    StreamEvent, ToolOutput, WireMsg,
};
use oxa_resources::Resources;
use oxa_session::{MemStore, SessionStore};
use oxa_tui::{visible_width, InputEvent, Key, TermCaps, TestTerminal, Theme};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------------------------
// Harness

/// Test doubles for the permission policy and the sandbox (the crates that implement them are
/// not dependencies of this crate). No tool runs in these tests, so neither is ever called.
struct AllowAll;

impl Policy for AllowAll {
    fn decide(&self, _mode: Mode, _a: &Access, _cwd: &Path) -> Decision {
        Decision::Allow
    }
    fn remember(&mut self, _a: &Access) {}
}

struct NoBash;

impl Sandbox for NoBash {
    fn check(&self) -> Result<(), OxaError> {
        Ok(())
    }
    fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
        Vec::new()
    }
    fn run(&self, _s: &ExecSpec, _c: &CancelToken, _live: &mut dyn FnMut(&[u8])) -> ExecResult {
        ExecResult {
            output: Vec::new(),
            exit: Some(0),
            timed_out: false,
            unsandboxed: false,
        }
    }
}

/// A session with no turns, in the test's own directories.
fn session(env: &TestEnv) -> Session {
    session_with(env, Vec::new())
}

/// A session on the scripted engine with these turns.
fn session_with(env: &TestEnv, turns: Vec<oxa_fake::ScriptTurn>) -> Session {
    session_open(env, turns, Arc::new(MemStore::new()), Open::Ephemeral)
}

fn session_open(
    env: &TestEnv,
    turns: Vec<oxa_fake::ScriptTurn>,
    store: Arc<dyn SessionStore>,
    open: Open,
) -> Session {
    session_deps(
        env,
        Arc::new(ScriptedEngine::new(turns)) as Arc<dyn Engine>,
        store,
        open,
    )
}

/// A session on an engine of the test's choosing, in the test's own directories.
fn session_deps(env: &TestEnv, engine: Arc<dyn Engine>, store: Arc<dyn SessionStore>, open: Open) -> Session {
    let deps = Deps {
        engine,
        store,
        tools: Vec::new(),
        hooks: Vec::new(),
        policy: Box::new(AllowAll),
        sandbox: Arc::new(NoBash),
        settings: Settings::default(),
        dirs: Dirs::under(env.root()),
        cwd: env.work(),
        selection: ToolSelection::default(),
        resources: Resources::default(),
        model: None,
        thinking: None,
        mode: None,
        session_id: None,
        jobs: oxa_core::fresh_jobs(),
    };
    Session::open(deps, open).expect("open the test session")
}

/// An engine whose first `chat` blocks until the test releases it (or drops the sender): the
/// session stays claimed (`Handle::is_busy`) while the test drives the loop against a run that
/// is unwinding. Later requests pass through to the scripted turns.
struct HeldEngine {
    inner: ScriptedEngine,
    /// Taken by the first `chat`; dropping it (a failing test) releases the run too.
    release: Mutex<Option<Receiver<()>>>,
    /// Signalled as the first `chat` starts, so the test knows the run is in the engine.
    entered: Sender<()>,
}

impl HeldEngine {
    fn new(turns: Vec<oxa_fake::ScriptTurn>, release: Receiver<()>, entered: Sender<()>) -> HeldEngine {
        HeldEngine {
            inner: ScriptedEngine::new(turns),
            release: Mutex::new(Some(release)),
            entered,
        }
    }
}

impl Engine for HeldEngine {
    fn info(&self, target: Option<&str>) -> Result<EngineInfo, OxaError> {
        self.inner.info(target)
    }
    fn count_tokens(&self, text: &str) -> Result<u32, OxaError> {
        self.inner.count_tokens(text)
    }
    fn chat(
        &self,
        r: &ChatRequest,
        cancel: &CancelToken,
        on: &mut dyn FnMut(StreamEvent),
    ) -> Result<AssistantTurn, OxaError> {
        if let Some(release) = self.release.lock().unwrap().take() {
            let _ = self.entered.send(());
            let _ = release.recv();
        }
        self.inner.chat(r, cancel, on)
    }
}

fn interactive_opts(env: &TestEnv) -> InteractiveOpts {
    InteractiveOpts {
        fullscreen: false,
        dirs: Dirs::under(env.root()),
        settings: Settings::default(),
        keymap: Keymap::defaults(),
        store: Arc::new(MemStore::new()) as Arc<dyn SessionStore>,
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
        reopen: Box::new(|_, _| Err(OxaError::new(codes::INVALID, "no session switch in tests"))),
        open_picker: false,
        save_trust: None,
        gated: Vec::new(),
        engine_status: None,
        yolo: None,
        os_sandbox: false,
    }
}

fn key(id: &str) -> InputEvent {
    InputEvent::Key(Key::press(id))
}

fn ch(c: char) -> InputEvent {
    InputEvent::Key(Key::char(c))
}

/// What a drive produced: the exit code and the commands the loop sent to the worker.
struct Driven {
    code: i32,
    cmds: Vec<SessionCmd>,
}

/// Run the loop with scripted input batches (one batch per poll) and scripted worker messages
/// queued before it starts. Every script must end with Ctrl+D on an empty editor.
fn drive(session: &Session, env: &TestEnv, batches: Vec<Vec<InputEvent>>, worker: Vec<WorkerMsg>) -> Driven {
    drive_handle(session.handle(), env, batches, worker)
}

/// [`drive`] on a handle alone, for a session that another thread is running.
fn drive_handle(
    handle: oxa_core::Handle,
    env: &TestEnv,
    batches: Vec<Vec<InputEvent>>,
    worker: Vec<WorkerMsg>,
) -> Driven {
    let mut term = TestTerminal::new(80, 24);
    for b in batches {
        term.push_input(b);
    }
    let (cmd_tx, cmd_rx): (Sender<SessionCmd>, Receiver<SessionCmd>) = mpsc::channel();
    let (msg_tx, msg_rx) = mpsc::channel::<WorkerMsg>();
    let (_ui_tx, ui_rx) = mpsc::channel::<UiRequest>();
    for m in worker {
        msg_tx.send(m).expect("queue a worker message");
    }
    let mut opts = interactive_opts(env);
    let code = run_loop(
        &mut term,
        &mut opts,
        handle,
        cmd_tx,
        msg_rx,
        ui_rx,
        ViewModel::default(),
    )
    .expect("the loop ends with Ctrl+D");
    let cmds = cmd_rx.try_iter().collect();
    Driven { code, cmds }
}

fn assistant_msg(text: &str, stop: Option<StopReason>) -> EventMessage {
    EventMessage {
        msg: WireMsg::assistant(text),
        ts_ms: 0,
        stop,
        usage: None,
        model: Some("fake-model".to_string()),
        tool_name: None,
        is_error: false,
        error_message: None,
    }
}

fn user_msg(text: &str) -> EventMessage {
    EventMessage {
        msg: WireMsg::user(text),
        ts_ms: 0,
        stop: None,
        usage: None,
        model: None,
        tool_name: None,
        is_error: false,
        error_message: None,
    }
}

/// A short conversation: one user message and one finished reply.
fn conversation() -> Vec<AgentEvent> {
    vec![
        AgentEvent::AgentStart,
        AgentEvent::MessageStart {
            message: user_msg("hi there"),
        },
        AgentEvent::MessageStart {
            message: assistant_msg("", None),
        },
        AgentEvent::MessageUpdate {
            usage: Default::default(),
            delta: oxa_proto::MessageDelta::TextDelta {
                index: 0,
                delta: "Hello".to_string(),
            },
        },
        AgentEvent::MessageEnd {
            message: assistant_msg("Hello", Some(StopReason::Stop)),
        },
        AgentEvent::AgentSettled { aborted: false },
    ]
}

// ---------------------------------------------------------------------------------------------
// Acceptance

/// A dirty frame must not sit behind the 250ms idle input wait (#791). Check the
/// actual loop's requested waits rather than a machine-dependent wall-clock budget.
#[test]
fn first_prompt_renders_without_idle_wait_then_sleeps_when_clean() {
    struct StartupTerminal {
        term: TestTerminal,
        waits: Vec<u64>,
    }
    impl oxa_tui::Terminal for StartupTerminal {
        fn write(&mut self, bytes: &[u8]) {
            self.term.write(bytes);
        }
        fn flush(&mut self) {
            self.term.flush();
        }
        fn size(&self) -> (u16, u16) {
            self.term.size()
        }
        fn caps(&self) -> TermCaps {
            self.term.caps()
        }
        fn set_mode(&mut self, mode: oxa_tui::TermMode, on: bool) {
            self.term.set_mode(mode, on);
        }
        fn suspend(&mut self) {
            self.term.suspend();
        }
        fn resume(&mut self) {
            self.term.resume();
        }
        fn poll(&mut self, ms: u64) -> Vec<InputEvent> {
            self.waits.push(ms);
            match self.waits.len() {
                1 => Vec::new(), // No key or capability reply to wake startup.
                2 => {
                    let output = String::from_utf8_lossy(&self.term.out);
                    assert!(output.contains("READY"), "prompt drawn before idle wait");
                    vec![key("ctrl+d")]
                }
                _ => panic!("the empty editor should exit on Ctrl+D"),
            }
        }
    }

    for fullscreen in [false, true] {
        let env = TestEnv::new();
        let session = session(&env);
        let mut opts = interactive_opts(&env);
        opts.fullscreen = fullscreen;
        let mut term = StartupTerminal {
            term: TestTerminal::new(120, 36),
            waits: Vec::new(),
        };
        let (cmd_tx, _cmd_rx) = mpsc::channel();
        let (_msg_tx, msg_rx) = mpsc::channel();
        let (_ui_tx, ui_rx) = mpsc::channel();
        assert_eq!(
            run_loop(
                &mut term,
                &mut opts,
                session.handle(),
                cmd_tx,
                msg_rx,
                ui_rx,
                ViewModel::default(),
            )
            .unwrap(),
            0
        );
        assert_eq!(term.waits, [0, 250], "fullscreen={fullscreen}");
    }
}

/// snapshot
#[test]
fn event_list_to_frames_80() {
    let lines = frames(&conversation(), 80, &Theme::plain(), &ViewSettings::default());
    assert!(
        lines.iter().any(|l| l.contains("hi there")),
        "user text: {lines:?}"
    );
    assert!(lines.iter().any(|l| l.contains("Hello")), "reply text: {lines:?}");
    assert!(
        lines.iter().all(|l| visible_width(l) <= 80),
        "no line wider than 80: {lines:?}"
    );
}

/// snapshot
#[test]
fn event_list_to_frames_120() {
    let lines = frames(&conversation(), 120, &Theme::plain(), &ViewSettings::default());
    assert!(
        lines.iter().any(|l| l.contains("hi there")),
        "user text: {lines:?}"
    );
    assert!(lines.iter().any(|l| l.contains("Hello")), "reply text: {lines:?}");
    assert!(
        lines.iter().all(|l| visible_width(l) <= 120),
        "no line wider than 120: {lines:?}"
    );
}

/// no reprint
#[test]
fn regular_mode_prints_finished_blocks_once() {
    let theme = Theme::plain();
    let settings = ViewSettings::default();
    let mut printer = RegularPrinter::new();
    let mut vm = ViewModel::default();
    let mut committed: Vec<String> = Vec::new();

    apply_event(&mut vm, &AgentEvent::AgentStart);
    apply_event(
        &mut vm,
        &AgentEvent::MessageStart {
            message: user_msg("first question"),
        },
    );
    apply_event(
        &mut vm,
        &AgentEvent::MessageStart {
            message: assistant_msg("", None),
        },
    );
    apply_event(
        &mut vm,
        &AgentEvent::MessageUpdate {
            usage: Default::default(),
            delta: oxa_proto::MessageDelta::TextDelta {
                index: 0,
                delta: "partial".to_string(),
            },
        },
    );
    let step = printer.step(&vm, 80, &theme, &settings);
    committed.extend(step.commit);
    assert!(
        step.live.iter().any(|l| l.contains("partial")),
        "the unfinished reply is live"
    );

    // Nothing changed: nothing is printed again.
    let again = printer.step(&vm, 80, &theme, &settings);
    assert!(again.commit.is_empty(), "no reprint: {:?}", again.commit);

    apply_event(
        &mut vm,
        &AgentEvent::MessageEnd {
            message: assistant_msg("final answer", Some(StopReason::Stop)),
        },
    );
    apply_event(&mut vm, &AgentEvent::AgentSettled { aborted: false });
    let step = printer.step(&vm, 80, &theme, &settings);
    committed.extend(step.commit);

    apply_event(
        &mut vm,
        &AgentEvent::MessageStart {
            message: user_msg("second question"),
        },
    );
    let step = printer.step(&vm, 80, &theme, &settings);
    committed.extend(step.commit);

    let all = committed.join("\n");
    assert_eq!(
        all.matches("first question").count(),
        1,
        "the first message is printed once: {all}"
    );
    assert_eq!(
        all.matches("final answer").count(),
        1,
        "the reply is printed once: {all}"
    );
    assert_eq!(
        all.matches("partial").count(),
        0,
        "the unfinished text is never committed: {all}"
    );
    // A user block is finished as soon as it exists, so it is committed at once, once.
    assert_eq!(
        all.matches("second question").count(),
        1,
        "a user block is committed once: {all}"
    );

    // The printer renders only the blocks it has not printed yet; what it committed plus the
    // live part is still exactly the whole transcript (separators included).
    apply_event(
        &mut vm,
        &AgentEvent::MessageStart {
            message: assistant_msg("", None),
        },
    );
    apply_event(
        &mut vm,
        &AgentEvent::MessageUpdate {
            usage: Default::default(),
            delta: oxa_proto::MessageDelta::TextDelta {
                index: 0,
                delta: "live tail".to_string(),
            },
        },
    );
    let step = printer.step(&vm, 80, &theme, &settings);
    committed.extend(step.commit);
    let mut printed = committed.clone();
    printed.extend(step.live);
    assert_eq!(
        printed,
        transcript_lines(&vm, 80, &theme, &settings).0,
        "commits + live = the transcript"
    );
}

/// resume hint line
#[test]
fn exit_output() {
    // pi interactive-mode.ts: one line, `To resume this session:` dimmed, then the command.
    let lines = exit_lines(Some(Path::new("/tmp/x/session.jsonl")), "0123abcd");
    assert_eq!(
        lines,
        vec!["\x1b[2mTo resume this session:\x1b[22m oxa --session 0123abcd".to_string()]
    );
    assert!(
        exit_lines(None, "").is_empty(),
        "an ephemeral session prints nothing"
    );
}

/// while running
#[test]
fn enter_steers_alt_enter_follows_up() {
    let env = TestEnv::new();
    let s = session(&env);
    let d = drive(
        &s,
        &env,
        vec![
            vec![ch('x'), key("enter")],
            vec![ch('y'), key("alt+enter")],
            vec![key("ctrl+d")],
        ],
        vec![WorkerMsg::Event(AgentEvent::AgentStart)],
    );
    assert_eq!(d.code, 0);
    let (steering, follow_up) = s.handle().queued();
    assert_eq!(
        steering,
        vec!["x".to_string()],
        "Enter steers while a run is active"
    );
    assert_eq!(follow_up, vec!["y".to_string()], "Alt+Enter queues a follow-up");
    assert!(
        matches!(d.cmds.as_slice(), [SessionCmd::QueuedInput(handle)] if handle.same_instance(&s.handle())),
        "steering and follow-up share one bound wake, never a new prompt"
    );
}

/// editor refilled
#[test]
fn escape_aborts_and_restores_queue() {
    let env = TestEnv::new();
    let s = session(&env);
    s.handle().steer("q1".to_string());
    let d = drive(
        &s,
        &env,
        vec![vec![key("escape")], vec![key("enter")], vec![key("ctrl+d")]],
        vec![WorkerMsg::Event(AgentEvent::AgentStart)],
    );
    assert_eq!(d.code, 0);
    // Escape restores queued text; explicit Enter recovers after the canceled run.
    let (steering, follow_up) = s.handle().queued();
    assert!(
        steering.is_empty(),
        "recovered text must not steer the canceled run"
    );
    assert!(follow_up.is_empty());
    assert!(
        matches!(d.cmds.as_slice(), [SessionCmd::RecoveryPrompt { handle, text, parts }]
            if handle.same_instance(&s.handle()) && text == "q1" && parts.is_empty()),
        "restored input becomes exactly one recovery prompt on its original handle"
    );
}

#[test]
fn input_before_agent_start_is_consumed_steers_the_claimed_run() {
    let env = TestEnv::new();
    let (release_tx, release_rx) = mpsc::channel();
    let (entered_tx, entered_rx) = mpsc::channel();
    let mut s = session_deps(
        &env,
        Arc::new(HeldEngine::new(
            vec![oxa_fake::ScriptTurn::text("done")],
            release_rx,
            entered_tx,
        )),
        Arc::new(MemStore::new()),
        Open::Ephemeral,
    );
    let handle = s.handle();
    let run = std::thread::spawn(move || {
        s.prompt(
            "first".into(),
            Vec::new(),
            &mut oxa_proto::VecSink(Vec::new()),
            &mut oxa_proto::HeadlessUi::default(),
        )
    });
    entered_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("foreground request entered");
    assert!(handle.is_busy());
    // The core has started, while the front end has not consumed AgentStart yet.
    let d = drive_handle(
        handle.clone(),
        &env,
        vec![vec![ch('x'), key("enter")], vec![key("ctrl+d")]],
        Vec::new(),
    );
    let queued = handle.queued();
    handle.abort();
    drop(release_tx);
    let _ = run.join().expect("run thread");
    assert_eq!(queued.0, vec!["x".to_string()]);
    assert!(
        matches!(d.cmds.as_slice(), [SessionCmd::QueuedInput(owner)] if owner.same_instance(&handle)),
        "a claimed foreground request must not be mistaken for an idle prompt"
    );
}

#[test]
fn dropped_failure_does_not_reactivate_the_busy_settlement_claim() {
    struct HeldFailure {
        entered: Sender<()>,
        release: Receiver<()>,
    }
    impl oxa_proto::EventSink for HeldFailure {
        fn emit(&mut self, event: &AgentEvent) {
            if matches!(event, AgentEvent::EntryAppended { entry }
                if matches!(&entry.body, EntryBody::Shown { why, .. } if *why == oxa_proto::shown_why::FAILED))
            {
                let _ = self.entered.send(());
                let _ = self.release.recv();
            }
        }
    }
    let env = TestEnv::new();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let mut s = session_deps(
        &env,
        Arc::new(ScriptedEngine::new(vec![oxa_fake::ScriptTurn::fault(
            oxa_fake::Fault::Http {
                status: 400,
                body: "synthetic failure".into(),
            },
        )])),
        Arc::new(MemStore::new()),
        Open::Ephemeral,
    );
    let handle = s.handle();
    let run = std::thread::spawn(move || {
        s.prompt(
            "first".into(),
            Vec::new(),
            &mut HeldFailure {
                entered: entered_tx,
                release: release_rx,
            },
            &mut oxa_proto::HeadlessUi::default(),
        )
    });
    entered_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("failed reply persisted");
    assert!(
        handle.is_busy(),
        "hold the original operation through its terminal event"
    );
    assert!(
        !handle.is_aborting(),
        "this is a failure, not the cancellation-token guard"
    );
    let d = drive_handle(
        handle.clone(),
        &env,
        vec![vec![ch('x'), key("enter")], vec![key("ctrl+d")]],
        Vec::new(),
    );
    let queued = handle.queued();
    drop(release_tx);
    let _ = run.join().expect("run thread");
    assert_eq!(queued, (Vec::new(), Vec::new()));
    assert!(
        matches!(d.cmds.as_slice(), [SessionCmd::Prompt { text, .. }] if text == "x"),
        "the busy settlement claim must not take new input as steering"
    );
}

#[test]
fn escape_after_claim_release_keeps_input_out_of_the_stale_run_queue() {
    let env = TestEnv::new();
    let s = session(&env);
    assert!(!s.handle().is_busy());
    let d = drive(
        &s,
        &env,
        vec![
            vec![key("escape")],
            vec![ch('x'), key("enter")],
            vec![key("ctrl+d")],
        ],
        vec![WorkerMsg::Event(AgentEvent::AgentStart)],
    );
    assert_eq!(s.handle().queued(), (Vec::new(), Vec::new()));
    assert!(
        matches!(d.cmds.as_slice(), [SessionCmd::RecoveryPrompt { handle, text, .. }]
        if handle.same_instance(&s.handle()) && text == "x"),
        "Escape must retain the explicit recovery boundary until the next input"
    );
}

/// #906: Enter while an aborted run is still unwinding starts a new run. The aborted run takes
/// no further tool batch, so a steering message would sit in a queue nothing drains (the flaky
/// `oxa-cli::i699_interrupt::tool_pty_ctrl_c_stops_tool_without_exiting` caught it: the reply to
/// the message typed after `Command aborted` never came).
#[test]
fn enter_while_the_run_is_aborting_starts_a_new_run() {
    let env = TestEnv::new();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let (entered_tx, entered_rx) = mpsc::channel::<()>();
    let mut s = session_deps(
        &env,
        Arc::new(HeldEngine::new(
            vec![oxa_fake::ScriptTurn::text("recovered")],
            release_rx,
            entered_tx,
        )),
        Arc::new(MemStore::new()),
        Open::Ephemeral,
    );
    let handle = s.handle();
    let run = std::thread::spawn(move || {
        let mut sink = oxa_proto::VecSink(Vec::new());
        let mut ui = oxa_proto::HeadlessUi::default();
        s.prompt("first".to_string(), Vec::new(), &mut sink, &mut ui)
    });
    entered_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the run reaches the engine");
    handle.abort();
    assert!(
        handle.is_aborting(),
        "Ctrl+C cancels the run that holds the session"
    );
    let d = drive_handle(
        handle.clone(),
        &env,
        vec![vec![ch('x'), key("enter")], vec![key("ctrl+d")]],
        vec![WorkerMsg::Event(AgentEvent::AgentStart)],
    );
    assert_eq!(d.code, 0);
    let (steering, follow_up) = handle.queued();
    assert!(
        steering.is_empty() && follow_up.is_empty(),
        "nothing waits in a queue the aborted run will never drain: {steering:?} {follow_up:?}"
    );
    match d.cmds.as_slice() {
        [SessionCmd::Prompt { text, .. }] => assert_eq!(text.as_str(), "x"),
        _ => panic!("the input must start a run, not steer"),
    }
    drop(release_tx);
    run.join().expect("the run ends").expect("the run settles");
}

/// ! and !!
#[test]
fn bang_commands_dispatch() {
    let env = TestEnv::new();
    let s = session(&env);
    let d = drive(
        &s,
        &env,
        vec![
            vec![ch('!'), ch('l'), ch('s'), key("enter")],
            vec![ch('!'), ch('!'), ch('p'), ch('w'), ch('d'), key("enter")],
            vec![key("ctrl+d")],
        ],
        Vec::new(),
    );
    assert_eq!(d.code, 0);
    assert_eq!(d.cmds.len(), 2, "two commands sent");
    match &d.cmds[0] {
        SessionCmd::UserBash { command, excluded } => {
            assert_eq!(command, "ls");
            assert!(!excluded, "! is sent to the model");
        }
        _ => panic!("! must become UserBash"),
    }
    match &d.cmds[1] {
        SessionCmd::UserBash { command, excluded } => {
            assert_eq!(command, "pwd");
            assert!(excluded, "!! is excluded from the model");
        }
        _ => panic!("!! must become UserBash"),
    }
}

// ---------------------------------------------------------------------------------------------
// Reducer and view model (no rendering, no terminal)

/// The events of one real run: a `Session` on the scripted engine, the core's own sequence.
fn real_run(env: &TestEnv, turns: Vec<oxa_fake::ScriptTurn>, prompt: &str) -> Vec<AgentEvent> {
    let mut s = session_with(env, turns);
    let mut sink = oxa_proto::VecSink(Vec::new());
    let mut ui = oxa_proto::HeadlessUi::default();
    s.prompt(prompt.to_string(), Vec::new(), &mut sink, &mut ui)
        .expect("the run settles");
    sink.0
}

fn reduce(events: &[AgentEvent]) -> ViewModel {
    let mut vm = ViewModel::default();
    for e in events {
        apply_event(&mut vm, e);
    }
    vm
}

/// A stream cut after the first bytes: the core stores `Shown { why: cut }` with the partial
/// text, sends NO `message_end` for it and resends the request (#456). The UI shows the cut
/// block, then the replayed reply as the one assistant reply.
#[test]
fn cut_stream_becomes_shown_block_from_the_real_event_sequence() {
    let env = TestEnv::new();
    let turn = oxa_fake::ScriptTurn {
        chunk: 4,
        fault: Some(oxa_fake::Fault::DropAt { bytes: 12 }),
        ..oxa_fake::ScriptTurn::text("half a sentence that never ends")
    };
    let events = real_run(
        &env,
        vec![turn, oxa_fake::ScriptTurn::text("the whole answer")],
        "go",
    );
    let ends: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::MessageEnd { message } if message.msg.role == oxa_proto::Role::Assistant => {
                message.msg.content.as_deref()
            }
            _ => None,
        })
        .collect();
    assert_eq!(ends, vec!["the whole answer"], "only the replay ends as a reply");
    let stored = events.iter().find_map(|e| match e {
        AgentEvent::EntryAppended { entry } => match &entry.body {
            EntryBody::Shown { text, why } => Some((text.clone(), *why)),
            _ => None,
        },
        _ => None,
    });
    let (text, why) = stored.expect("the core stored a Shown entry");
    assert_eq!(why, "cut");

    let vm = reduce(&events);
    assert_eq!(vm.status.activity, Activity::Idle);
    let shown: Vec<_> = vm
        .blocks
        .iter()
        .filter_map(|b| match b {
            Block::Shown { text, why } => Some((text.clone(), why.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(shown.len(), 1, "{:?}", vm.blocks);
    assert_eq!(shown[0].0, text, "the block shows what the core stored");
    assert!(!text.is_empty() && "half a sentence that never ends".starts_with(text.as_str()));
    assert_eq!(shown[0].1, "cut");
    let assistants: Vec<_> = vm
        .blocks
        .iter()
        .filter(|b| matches!(b, Block::Assistant { .. }))
        .collect();
    assert_eq!(
        assistants.len(),
        1,
        "the cut partial is not an assistant reply: {:?}",
        vm.blocks
    );
    assert!(
        matches!(vm.blocks.last(), Some(Block::Assistant { .. })),
        "{:?}",
        vm.blocks
    );
}

/// An engine error before any byte: `Shown { why: failed }` carrying the error text.
#[test]
fn engine_error_becomes_shown_block_from_the_real_event_sequence() {
    let env = TestEnv::new();
    let turn = oxa_fake::ScriptTurn::fault(oxa_fake::Fault::Http {
        status: 400,
        body: "bad request: nope".to_string(),
    });
    let events = real_run(&env, vec![turn], "go");
    assert!(
        !events.iter().any(|e| matches!(e, AgentEvent::MessageEnd { message } if message.msg.role == oxa_proto::Role::Assistant)),
        "the core sends no message_end for a failed request"
    );
    let vm = reduce(&events);
    assert_eq!(vm.status.activity, Activity::Idle);
    assert!(
        !vm.blocks.iter().any(|b| matches!(b, Block::Assistant { .. })),
        "{:?}",
        vm.blocks
    );
    match vm.blocks.last() {
        Some(Block::Shown { text, why }) => {
            assert_eq!(why, "failed");
            assert!(!text.is_empty(), "the error is shown");
        }
        other => panic!("expected a Shown block, got {other:?}"),
    }
}

#[test]
fn length_cut_is_shown_with_notice() {
    let mut vm = ViewModel::default();
    apply_event(
        &mut vm,
        &AgentEvent::MessageStart {
            message: assistant_msg("", None),
        },
    );
    apply_event(
        &mut vm,
        &AgentEvent::MessageEnd {
            message: assistant_msg("cut off", Some(StopReason::Length)),
        },
    );
    match vm.blocks.first() {
        Some(Block::Assistant { stop, streaming, .. }) => {
            assert_eq!(*stop, Some(StopReason::Length));
            assert!(!streaming);
        }
        other => panic!("expected the assistant block, got {other:?}"),
    }
    assert!(
        vm.blocks.iter().any(|b| matches!(
            b,
            Block::Notice {
                level: Level::Warning,
                ..
            }
        )),
        "a warning notice names the cut"
    );
}

#[test]
fn length_cut_with_thinking_on_names_the_thinking_budget() {
    let notice = |level: oxa_proto::ThinkingLevel| {
        let mut vm = ViewModel::default();
        vm.status.thinking = Some(level);
        apply_event(
            &mut vm,
            &AgentEvent::MessageStart {
                message: assistant_msg("", None),
            },
        );
        apply_event(
            &mut vm,
            &AgentEvent::MessageEnd {
                message: assistant_msg("cut off", Some(StopReason::Length)),
            },
        );
        vm.blocks
            .iter()
            .find_map(|b| match b {
                Block::Notice { text, .. } => Some(text.clone()),
                _ => None,
            })
            .expect("a notice")
    };
    let on = notice(oxa_proto::ThinkingLevel::Medium);
    assert!(on.contains("thinking") && on.contains("/thinking off"), "{on}");
    let off = notice(oxa_proto::ThinkingLevel::Off);
    assert!(!off.contains("thinking"), "{off}");
}

#[test]
fn tool_lifecycle_moves_one_block() {
    let mut vm = ViewModel::default();
    apply_event(
        &mut vm,
        &AgentEvent::ToolExecutionStart {
            tool_call_id: "c1".to_string(),
            tool_name: "bash".to_string(),
            args: serde_json::json!({"command": "ls"}),
        },
    );
    apply_event(
        &mut vm,
        &AgentEvent::ToolExecutionEnd {
            tool_call_id: "c1".to_string(),
            tool_name: "bash".to_string(),
            result: ToolOutput::ok("a\nb"),
            duration_ms: Some(12),
        },
    );
    assert_eq!(vm.blocks.len(), 1, "start and end share one block");
    match &vm.blocks[0] {
        Block::Tool(t) => {
            assert_eq!(t.state, ToolState::Done);
            assert_eq!(t.duration_ms, Some(12));
            assert!(t.result.is_some());
        }
        other => panic!("expected a tool block, got {other:?}"),
    }
    // An unknown id adds a block and never panics.
    apply_event(
        &mut vm,
        &AgentEvent::ToolExecutionEnd {
            tool_call_id: "zz".to_string(),
            tool_name: "read".to_string(),
            result: ToolOutput::error("nope"),
            duration_ms: None,
        },
    );
    assert_eq!(vm.blocks.len(), 2);
}

#[test]
fn settle_sets_idle_and_queue_update_replaces_queues() {
    let mut vm = ViewModel::default();
    apply_event(&mut vm, &AgentEvent::AgentStart);
    assert_eq!(vm.status.activity, Activity::Thinking);
    apply_event(
        &mut vm,
        &AgentEvent::QueueUpdate {
            steering: vec!["s".into()],
            follow_up: vec!["f".into()],
        },
    );
    assert_eq!(vm.status.steering, vec!["s".to_string()]);
    assert_eq!(vm.status.follow_up, vec!["f".to_string()]);
    apply_event(&mut vm, &AgentEvent::AgentSettled { aborted: true });
    assert_eq!(vm.status.activity, Activity::Idle);
}

#[test]
fn cache_miss_unknown_is_counted() {
    let mut vm = ViewModel::default();
    apply_event(
        &mut vm,
        &AgentEvent::CacheMiss {
            expected: 10,
            cached: 2,
            cause: "unknown",
        },
    );
    apply_event(
        &mut vm,
        &AgentEvent::CacheMiss {
            expected: 10,
            cached: 2,
            cause: "epoch",
        },
    );
    assert_eq!(vm.status.unknown_misses, 1);
    assert_eq!(vm.blocks.len(), 2, "each miss is a notice");
}

#[test]
fn blocks_from_entries_attach_results_to_calls() {
    let entries = vec![
        Entry {
            id: "00000001".into(),
            parent: None,
            ts_ms: 0,
            body: EntryBody::Msg {
                wire: WireMsg::user("list files"),
                parts: vec![Part::Text("list files".into())],
                tool: None,
            },
        },
        Entry {
            id: "00000002".into(),
            parent: Some("00000001".into()),
            ts_ms: 0,
            body: EntryBody::Msg {
                wire: WireMsg {
                    images: Vec::new(),
                    role: oxa_proto::Role::Assistant,
                    content: None,
                    reasoning: None,
                    tool_calls: vec![oxa_proto::ToolCall {
                        id: "call_1".into(),
                        name: "bash".into(),
                        args_raw: "{\"command\":\"ls\"}".into(),
                    }],
                    provider_history: None,
                    tool_call_id: None,
                },
                parts: Vec::new(),
                tool: None,
            },
        },
        Entry {
            id: "00000003".into(),
            parent: Some("00000002".into()),
            ts_ms: 0,
            body: EntryBody::Msg {
                wire: WireMsg::tool_result("call_1", "a\nb"),
                parts: Vec::new(),
                tool: None,
            },
        },
        Entry {
            id: "00000004".into(),
            parent: Some("00000003".into()),
            ts_ms: 0,
            body: EntryBody::ModelChange("other".into()),
        },
    ];
    let blocks = blocks_from_entries(&entries);
    assert_eq!(
        blocks.len(),
        3,
        "user, assistant, tool; the model change gives no block: {blocks:?}"
    );
    match &blocks[2] {
        Block::Tool(t) => {
            assert_eq!(t.name, "bash");
            assert_eq!(t.state, ToolState::Done);
            assert_eq!(t.result.as_ref().map(|r| r.text.as_str()), Some("a\nb"));
        }
        other => panic!("expected the tool block, got {other:?}"),
    }
}

#[test]
fn blocks_from_entries_keep_error_flag_and_ui_of_results() {
    let ui = serde_json::json!({ "exit": 2 });
    let entry = |id: &str, parent: Option<&str>, body| Entry {
        id: id.into(),
        parent: parent.map(Into::into),
        ts_ms: 0,
        body,
    };
    let call = WireMsg {
        images: Vec::new(),
        role: oxa_proto::Role::Assistant,
        content: None,
        reasoning: None,
        tool_calls: vec![oxa_proto::ToolCall {
            id: "call_1".into(),
            name: "bash".into(),
            args_raw: "{}".into(),
        }],
        provider_history: None,
        tool_call_id: None,
    };
    let entries = vec![
        entry(
            "00000001",
            None,
            EntryBody::Msg {
                wire: call,
                parts: Vec::new(),
                tool: None,
            },
        ),
        entry(
            "00000002",
            Some("00000001"),
            EntryBody::Msg {
                wire: WireMsg::tool_result("call_1", "boom"),
                parts: Vec::new(),
                tool: Some(oxa_proto::ToolMeta {
                    is_error: true,
                    ui: ui.clone(),
                }),
            },
        ),
    ];
    match &blocks_from_entries(&entries)[1] {
        Block::Tool(t) => {
            let r = t.result.as_ref().expect("result");
            assert!(r.is_error, "a failed call stays failed after resume");
            assert_eq!(r.ui, ui);
        }
        other => panic!("expected the tool block, got {other:?}"),
    }
}

#[test]
fn status_from_keeps_display_only_fields() {
    let env = TestEnv::new();
    let s = session(&env);
    let state = s.state();
    let stats = s.stats();
    let prev = StatusModel {
        cwd: "/work".into(),
        git_branch: Some("main".into()),
        working_message: Some("Reading".into()),
        ..StatusModel::default()
    };
    let next = status_from(&state, &stats, &prev);
    assert_eq!(next.cwd, "/work");
    assert_eq!(next.git_branch.as_deref(), Some("main"));
    assert_eq!(next.working_message.as_deref(), Some("Reading"));
    assert_eq!(next.model, state.model);
    assert!(!next.model_remote, "a local session is shown as local");
    let mut remote_state = state.clone();
    remote_state.model_remote = true;
    remote_state.model_provider = Some("openrouter");
    let remote = status_from(&remote_state, &stats, &prev);
    assert!(remote.model_remote);
    assert_eq!(remote.model_provider, Some("openrouter"));
}

#[test]
fn status_from_clears_speed_and_cache_on_model_switch_and_new_session() {
    let env = TestEnv::new();
    let s = session(&env);
    let state = s.state();
    let mut stats = s.stats();
    stats.last_tok_s = 54.4;
    stats.context_tokens = 0;
    let prev = StatusModel {
        model: "other-model".into(),
        last_cached: Some(86),
        last_prompt: Some(100),
        tok_per_s: Some(54.4),
        ..StatusModel::default()
    };
    let next = status_from(&state, &stats, &prev);
    assert_eq!(next.tok_per_s, None, "speed cleared on model switch");
    assert_eq!((next.last_cached, next.last_prompt), (None, None));
    // same model, empty session (/new): cache cleared
    let prev = StatusModel {
        model: state.model.clone(),
        last_cached: Some(86),
        last_prompt: Some(100),
        ..StatusModel::default()
    };
    let next = status_from(&state, &stats, &prev);
    assert_eq!((next.last_cached, next.last_prompt), (None, None));
}

#[test]
fn transcript_lines_index_each_block() {
    let mut vm = ViewModel::default();
    for e in conversation() {
        apply_event(&mut vm, &e);
    }
    let (lines, starts) = transcript_lines(&vm, 80, &Theme::plain(), &ViewSettings::default());
    assert_eq!(starts.len(), vm.blocks.len());
    assert!(
        starts.windows(2).all(|w| w[0] <= w[1]),
        "segment starts are ordered"
    );
    assert!(starts.iter().all(|&i| i <= lines.len()));
}

#[test]
fn quit_with_ctrl_c_twice_on_empty_editor() {
    let env = TestEnv::new();
    let s = session(&env);
    let d = drive(
        &s,
        &env,
        vec![vec![key("ctrl+c")], vec![key("ctrl+c")]],
        Vec::new(),
    );
    assert_eq!(d.code, 0);
    assert!(d.cmds.is_empty());
}

/// Issue #344: Alt+Enter on an idle prompt inserts a newline instead of sending.
#[test]
fn alt_enter_idle_inserts_newline() {
    let env = TestEnv::new();
    let s = session(&env);
    let d = drive(
        &s,
        &env,
        vec![
            vec![ch('a'), key("alt+enter"), ch('b'), key("enter")],
            vec![key("ctrl+d")],
        ],
        vec![],
    );
    let prompts: Vec<String> = d
        .cmds
        .iter()
        .filter_map(|c| match c {
            SessionCmd::Prompt { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(prompts, vec!["a\nb".to_string()]);
}

/// Deliver worker/UI messages between polls, like a run finishing after focus-out.
struct NotificationTerminal {
    term: TestTerminal,
    worker: Sender<WorkerMsg>,
    ui: Sender<UiRequest>,
    steps: std::collections::VecDeque<(Vec<WorkerMsg>, Vec<UiRequest>)>,
}

impl oxa_tui::Terminal for NotificationTerminal {
    fn write(&mut self, bytes: &[u8]) {
        self.term.write(bytes);
    }
    fn flush(&mut self) {
        self.term.flush();
    }
    fn size(&self) -> (u16, u16) {
        self.term.size()
    }
    fn caps(&self) -> TermCaps {
        self.term.caps()
    }
    fn set_mode(&mut self, mode: oxa_tui::TermMode, on: bool) {
        self.term.set_mode(mode, on);
    }
    fn suspend(&mut self) {
        self.term.suspend();
    }
    fn resume(&mut self) {
        self.term.resume();
    }
    fn poll(&mut self, ms: u64) -> Vec<InputEvent> {
        if let Some((events, requests)) = self.steps.pop_front() {
            for event in events {
                self.worker.send(event).unwrap();
            }
            for request in requests {
                self.ui.send(request).unwrap();
            }
        }
        self.term.poll(ms)
    }
}

fn notification_output(
    policy: Option<&str>,
    fullscreen: bool,
    focus: Vec<InputEvent>,
) -> (String, TestTerminal) {
    let env = TestEnv::new();
    let session = session(&env);
    let mut opts = interactive_opts(&env);
    opts.fullscreen = fullscreen;
    if let Some(policy) = policy {
        opts.settings = Settings::from_values(
            serde_json::json!({"ui": {"notify": policy}}),
            serde_json::json!({}),
        );
    }
    let (cmd_tx, _cmd_rx) = mpsc::channel();
    let (msg_tx, msg_rx) = mpsc::channel();
    let (ui_tx, ui_rx) = mpsc::channel();
    let (reply, _answer) = mpsc::channel();
    let mut term = TestTerminal::new(80, 24);
    term.push_input(focus);
    term.push_input(vec![]);
    term.push_input(vec![]);
    term.push_input(vec![key("escape")]);
    term.push_input(vec![key("ctrl+d")]);
    let mut term = NotificationTerminal {
        term,
        worker: msg_tx,
        ui: ui_tx,
        steps: [
            (vec![WorkerMsg::Event(AgentEvent::AgentStart)], vec![]),
            (
                vec![
                    WorkerMsg::Event(AgentEvent::AgentSettled { aborted: false }),
                    WorkerMsg::Done(Ok(())),
                ],
                vec![],
            ),
            (
                vec![],
                vec![UiRequest::AskAccess {
                    access: Access::Write(env.work().join("file")),
                    reply,
                }],
            ),
        ]
        .into(),
    };
    assert_eq!(
        run_loop(
            &mut term,
            &mut opts,
            session.handle(),
            cmd_tx,
            msg_rx,
            ui_rx,
            ViewModel::default()
        )
        .unwrap(),
        0
    );
    (term.term.take_output(), term.term)
}

#[test]
fn terminal_notifications_follow_focus_and_policy_in_both_layouts() {
    for fullscreen in [false, true] {
        for (policy, focus, expected) in [
            (None, vec![], 0),
            (None, vec![InputEvent::FocusLost], 2),
            (None, vec![InputEvent::FocusLost, InputEvent::FocusGained], 0),
            (Some("off"), vec![InputEvent::FocusLost], 0),
            (Some("always"), vec![], 2),
        ] {
            let (out, term) = notification_output(policy, fullscreen, focus);
            assert_eq!(
                out.matches("\x1b]9;").count(),
                expected,
                "policy={policy:?}, fullscreen={fullscreen}"
            );
            if expected != 0 {
                assert!(out.contains("\x1b]9;oxa: Run finished\x07"));
                assert!(out.contains("\x1b]9;oxa: Permission required\x07"));
            }
            if fullscreen {
                assert_eq!(term.modes.first(), Some(&(oxa_tui::TermMode::AltScreen, true)));
            }
            assert!(term.modes.contains(&(oxa_tui::TermMode::FocusReporting, true)));
            assert!(!term.mode_on(oxa_tui::TermMode::FocusReporting));
        }
    }
}

#[test]
fn terminal_notifications_only_on_new_edges_not_focus_out_or_done() {
    let env = TestEnv::new();
    let session = session(&env);
    let mut opts = interactive_opts(&env);
    let (cmd_tx, _cmd_rx) = mpsc::channel();
    let (msg_tx, msg_rx) = mpsc::channel();
    let (ui_tx, ui_rx) = mpsc::channel();
    let mut term = TestTerminal::new(80, 24);
    // Settle while focused, then lose focus: the old completion must not replay.
    term.push_input(vec![]);
    term.push_input(vec![]);
    term.push_input(vec![InputEvent::FocusLost]);
    term.push_input(vec![]);
    term.push_input(vec![]);
    term.push_input(vec![key("ctrl+d")]);
    let mut term = NotificationTerminal {
        term,
        worker: msg_tx,
        ui: ui_tx,
        steps: [
            (
                vec![WorkerMsg::Event(AgentEvent::AgentSettled { aborted: false })],
                vec![],
            ),
            (vec![], vec![]),
            (vec![WorkerMsg::Done(Ok(()))], vec![]),
            (
                vec![
                    WorkerMsg::Event(AgentEvent::AgentSettled { aborted: true }),
                    WorkerMsg::Done(Ok(())),
                ],
                vec![],
            ),
        ]
        .into(),
    };
    assert_eq!(
        run_loop(
            &mut term,
            &mut opts,
            session.handle(),
            cmd_tx,
            msg_rx,
            ui_rx,
            ViewModel::default()
        )
        .unwrap(),
        0
    );
    let out = term.term.take_output();
    assert_eq!(out.matches("\x1b]9;").count(), 1);
    // The OSC terminator alone is not a bell on terminals which ignore OSC 9.
    assert!(out.contains("\x1b]9;oxa: Run finished\x07\x07"));
}

/// No keyboard input until both the startup backlog and a live file notification ran.
struct InboxTerminal {
    term: TestTerminal,
    agent: std::path::PathBuf,
    id: String,
    sent_live: bool,
    start: std::time::Instant,
}

impl oxa_tui::Terminal for InboxTerminal {
    fn write(&mut self, bytes: &[u8]) {
        self.term.out.extend_from_slice(bytes);
    }
    fn flush(&mut self) {}
    fn size(&self) -> (u16, u16) {
        (100, 30)
    }
    fn caps(&self) -> TermCaps {
        TermCaps::BASIC
    }
    fn set_mode(&mut self, mode: oxa_tui::TermMode, on: bool) {
        self.term.modes.push((mode, on));
    }
    fn suspend(&mut self) {}
    fn resume(&mut self) {}
    fn poll(&mut self, _: u64) -> Vec<InputEvent> {
        let out = String::from_utf8_lossy(&self.term.out);
        if out.contains("inbox-second-reply") || self.start.elapsed() > std::time::Duration::from_secs(3) {
            return vec![key("ctrl+d")];
        }
        if !self.sent_live && out.contains("inbox-first-reply") {
            self.sent_live = true;
            oxa_session::send_inbox(&self.agent, &self.id, "live follow-up").unwrap();
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
        Vec::new()
    }
}

#[test]
fn inbox_wakes_the_real_tui_worker_when_idle() {
    let env = TestEnv::new();
    let s = session_with(
        &env,
        vec![
            oxa_fake::ScriptTurn::text("inbox-first-reply"),
            oxa_fake::ScriptTurn::text("inbox-second-reply"),
        ],
    );
    let id = s.state().session_id;
    let mut opts = interactive_opts(&env);
    oxa_session::send_inbox(&opts.dirs.agent_dir, &id, "startup follow-up").unwrap();
    let mut term = InboxTerminal {
        term: TestTerminal::new(100, 30),
        agent: opts.dirs.agent_dir.clone(),
        id,
        sent_live: false,
        start: std::time::Instant::now(),
    };
    let (code, _) = oxa_interactive::run_session(&mut term, s, &mut opts);
    assert_eq!(code, 0);
    let out = String::from_utf8_lossy(&term.term.out);
    assert!(
        out.contains("inbox-first-reply"),
        "startup inbox was not delivered: {out}"
    );
    assert!(
        out.contains("inbox-second-reply"),
        "live inbox did not wake idle TUI: {out}"
    );
}

/// Keep the UI on the old session until its watcher has admitted a message.
struct StaleInboxTerminal {
    cmds: Receiver<SessionCmd>,
    destination: String,
    reopen: Option<(Sender<WorkerMsg>, oxa_core::Handle)>,
}

impl oxa_tui::Terminal for StaleInboxTerminal {
    fn write(&mut self, _: &[u8]) {}
    fn flush(&mut self) {}
    fn size(&self) -> (u16, u16) {
        (80, 24)
    }
    fn caps(&self) -> TermCaps {
        TermCaps::BASIC
    }
    fn set_mode(&mut self, _: oxa_tui::TermMode, _: bool) {}
    fn suspend(&mut self) {}
    fn resume(&mut self) {}
    fn poll(&mut self, _: u64) -> Vec<InputEvent> {
        assert!(matches!(
            self.cmds.recv_timeout(std::time::Duration::from_secs(3)),
            Ok(SessionCmd::Inbox { session_id }) if session_id == self.destination
        ));
        if let Some((messages, handle)) = self.reopen.take() {
            // Reopen creates a fresh Session/Shared, unlike core Session::switch.
            messages.send(WorkerMsg::Rebind(handle)).unwrap();
            messages
                .send(WorkerMsg::Session(oxa_interactive::SessionInfo {
                    id: self.destination.clone(),
                    ..Default::default()
                }))
                .unwrap();
            Vec::new()
        } else {
            vec![key("ctrl+d")]
        }
    }
}

#[test]
fn stale_tui_inbox_callback_keeps_messages_with_original_session_after_clone_or_fork() {
    for fork in [false, true] {
        let env = TestEnv::new();
        let mut s = session_with(&env, vec![oxa_fake::ScriptTurn::text("initial reply")]);
        s.prompt(
            "initial turn".into(),
            Vec::new(),
            &mut oxa_proto::VecSink::default(),
            &mut oxa_proto::HeadlessUi::default(),
        )
        .unwrap();
        let original = s.inner().sref.clone();
        let handle = s.handle();
        if fork {
            let user = s
                .inner()
                .tree
                .entries()
                .iter()
                .find(
                    |e| matches!(&e.body, EntryBody::Msg { wire, .. } if wire.role == oxa_proto::Role::User),
                )
                .unwrap()
                .id
                .clone();
            s.fork(&user).unwrap();
        } else {
            s.clone_session().unwrap();
        }
        assert_ne!(s.state().session_id, original.id);

        // The worker has changed identity but the UI has not yet processed its Session
        // notification. Its old watcher still owns this backlog and the shared handle.
        let mut opts = interactive_opts(&env);
        oxa_session::send_inbox(&opts.dirs.agent_dir, &original.id, "old destination only").unwrap();
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (msg_tx, msg_rx) = mpsc::channel();
        let (_ui_tx, ui_rx) = mpsc::channel();
        msg_tx
            .send(WorkerMsg::Session(oxa_interactive::SessionInfo {
                id: original.id.clone(),
                ..Default::default()
            }))
            .unwrap();
        let mut resumed = session_open(
            &env,
            vec![oxa_fake::ScriptTurn::text("resumed reply")],
            s.inner().store.clone(),
            Open::Resume(original.clone()),
        );
        let resumed_handle = resumed.handle();
        let mut term = StaleInboxTerminal {
            cmds: cmd_rx,
            destination: original.id.clone(),
            reopen: Some((msg_tx, resumed_handle.clone())),
        };
        assert_eq!(
            run_loop(
                &mut term,
                &mut opts,
                handle.clone(),
                cmd_tx,
                msg_rx,
                ui_rx,
                ViewModel::default()
            )
            .unwrap(),
            0
        );
        assert!(
            handle.queued().1.is_empty(),
            "old inbox leaked across session transition"
        );
        assert_eq!(resumed_handle.queued().1, ["old destination only"]);
        resumed
            .run_queued(
                &mut oxa_proto::VecSink::default(),
                &mut oxa_proto::HeadlessUi::default(),
            )
            .unwrap();
        assert!(resumed_handle.queued().1.is_empty());
        assert!(resumed.inner().tree.entries().iter().any(|entry| {
            matches!(&entry.body, EntryBody::Msg { wire, .. }
                if wire.role == oxa_proto::Role::User && wire.content.as_deref() == Some("old destination only"))
        }));
    }
}

#[test]
fn regular_renderer_refreshes_live_policy_after_load_and_unload() {
    struct PolicyTerminal {
        inner: TestTerminal,
        messages: Sender<WorkerMsg>,
        phase: usize,
        frames: Vec<String>,
    }
    impl oxa_tui::Terminal for PolicyTerminal {
        fn write(&mut self, b: &[u8]) {
            self.inner.write(b);
        }
        fn flush(&mut self) {
            self.inner.flush();
        }
        fn size(&self) -> (u16, u16) {
            self.inner.size()
        }
        fn caps(&self) -> TermCaps {
            self.inner.caps()
        }
        fn set_mode(&mut self, m: oxa_tui::TermMode, b: bool) {
            self.inner.set_mode(m, b);
        }
        fn suspend(&mut self) {
            self.inner.suspend();
        }
        fn resume(&mut self) {
            self.inner.resume();
        }
        fn poll(&mut self, _: u64) -> Vec<InputEvent> {
            self.frames.push(self.inner.take_output());
            let policy = match self.phase {
                0 => Some("scope"),
                1 => Some("inherited"),
                2 => None,
                // Worker messages drain before poll, then the frame is drawn after
                // poll. Capture the unload frame on the following iteration.
                3 => {
                    self.phase += 1;
                    return Vec::new();
                }
                _ => return vec![key("ctrl+d")],
            };
            self.messages
                .send(WorkerMsg::Session(oxa_interactive::SessionInfo {
                    id: "synthetic".into(),
                    engine_url: "native-local".into(),
                    memory_policy: policy,
                    ..Default::default()
                }))
                .unwrap();
            self.phase += 1;
            Vec::new()
        }
    }
    let env = TestEnv::new();
    let s = session(&env);
    let (cmd, _commands) = mpsc::channel();
    let (messages, receiver) = mpsc::channel();
    let (_ui, requests) = mpsc::channel();
    let mut term = PolicyTerminal {
        inner: TestTerminal::new(100, 24),
        messages,
        phase: 0,
        frames: Vec::new(),
    };
    assert_eq!(
        run_loop(
            &mut term,
            &mut interactive_opts(&env),
            s.handle(),
            cmd,
            receiver,
            requests,
            ViewModel::default()
        )
        .unwrap(),
        0
    );
    assert!(!term.frames[1].contains("Memory: scope"));
    assert!(
        term.frames[2].contains("Memory: scope"),
        "regular live load: {:?}",
        term.frames[2]
    );
    assert!(
        term.frames[3].contains("Memory: inherited"),
        "regular live switch: {:?}",
        term.frames[3]
    );
    assert!(!term.frames[4].contains("Memory: scope") && !term.frames[4].contains("Memory: inherited"));
    assert!(
        term.frames[4].contains("\x1b[J"),
        "unload clears the removed live row through the end of the region"
    );
}
