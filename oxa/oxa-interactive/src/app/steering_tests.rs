//! Force the two queue/command completion windows without timing sleeps.
use super::*;
use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, ToolSelection};
use oxa_engine::HttpEngine;
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
use oxa_proto::{
    Access, CancelToken, Decision, Engine, ExecResult, ExecSpec, HeadlessUi, Mode, Policy, Sandbox, VecSink,
};
use oxa_resources::Resources;
use oxa_session::{MemStore, SessionStore};
use oxa_tui::{InputEvent, Key, TestTerminal};
use serde_json::json;
use std::sync::{Arc, Mutex};

struct NoActions;
impl Policy for NoActions {
    fn decide(&self, _: Mode, _: &Access, _: &Path) -> Decision {
        panic!("no tools in this regression")
    }
    fn remember(&mut self, _: &Access) {}
}
impl Sandbox for NoActions {
    fn check(&self) -> Result<(), OxaError> {
        Ok(())
    }
    fn bwrap_args(&self, _: &ExecSpec) -> Vec<String> {
        Vec::new()
    }
    fn run(&self, _: &ExecSpec, _: &CancelToken, _: &mut dyn FnMut(&[u8])) -> ExecResult {
        panic!("no bash")
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Window {
    AfterEmptyTake,
    BeforeDoneObserved,
    DoneObserved,
}
const GATE_TIMEOUT: Duration = Duration::from_secs(30);

/// Test-only scheduling gate at the existing post-empty-drain progress observation.
/// It never calls back into the session and adds no production hook or event.
#[derive(Debug)]
struct EmptyTakeGate(Mutex<Option<(Sender<()>, Receiver<()>)>>);
impl oxa_core::ProgressObserver for EmptyTakeGate {
    fn notify(&self, _: &oxa_session::SessionRef, progress: oxa_core::Progress<'_>) {
        if matches!(progress, oxa_core::Progress::Settled(_)) {
            let pause = self.0.lock().unwrap().take();
            if let Some((reached, release)) = pause {
                let _ = reached.send(());
                let _ = release.recv_timeout(GATE_TIMEOUT);
            }
        }
    }
}

fn deps(env: &TestEnv, engine: Arc<dyn Engine>, store: Arc<dyn SessionStore>, settings: Settings) -> Deps {
    Deps {
        engine,
        store,
        settings,
        tools: Vec::new(),
        hooks: Vec::new(),
        policy: Box::new(NoActions),
        sandbox: Arc::new(NoActions),
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

fn gated_prompt(window: Window, instructions: Option<String>, follow_up: bool, rebind: bool) {
    let env = TestEnv::new();
    let server = FakeServer::start(
        ["one", "two", "summary text", "three"]
            .into_iter()
            .map(ScriptTurn::text)
            .collect(),
        FakeOpts::default(),
    )
    .unwrap();
    let engine: Arc<dyn Engine> = Arc::new(HttpEngine::new(&server.url(), None).unwrap());
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    let settings = Settings::from_values(
        json!({"compaction":{"keepRecentTokens":1},"oxa":{"envNote":false}}),
        json!({}),
    );
    let mut session = Session::open(
        deps(&env, engine.clone(), store.clone(), settings.clone()),
        Open::Ephemeral,
    )
    .unwrap();
    let mut ui = HeadlessUi::default();
    for text in ["first", "second"] {
        session
            .prompt(text.to_string(), Vec::new(), &mut VecSink::default(), &mut ui)
            .unwrap();
    }
    let control = session.handle();
    let vm = ViewModel {
        status: status_from(
            &session.state(),
            &session.stats(),
            &StatusModel {
                cwd: env.work().display().to_string(),
                ..Default::default()
            },
        ),
        ..Default::default()
    };
    let info = session_info(&session);
    let mut opts = InteractiveOpts {
        fullscreen: true,
        dirs: Dirs::under(env.root()),
        settings: settings.clone(),
        keymap: Keymap::defaults(),
        store: store.clone(),
        hooks: Vec::new(),
        mcp: None,
        initial_prompt: None,
        initial_parts: Vec::new(),
        initial_follow_ups: Vec::new(),
        theme: None,
        notices: Vec::new(),
        env: Vec::new(),
        caps: Some(TermCaps::BASIC),
        reopen: Box::new(|_, _| panic!("must not reopen")),
        open_picker: false,
        save_trust: None,
        gated: Vec::new(),
        yolo: None,
    };
    let mut terminal = TestTerminal::new(100, 30);
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let (out_tx, out_rx) = mpsc::channel();
    let (ui_tx, _ui_rx) = mpsc::channel();
    let mut lp = Loop::new(&mut terminal, &mut opts, control.clone(), cmd_tx, vm);
    lp.worker_msg(WorkerMsg::Session(info));
    let (reached_tx, reached_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let worker_out = out_tx.clone();
    let worker_ui = ui_tx.clone();
    let worker = thread::spawn(move || {
        let mut release = Some(release_rx);
        if window == Window::AfterEmptyTake {
            session.set_progress_observer(Arc::new(EmptyTakeGate(Mutex::new(Some((
                reached_tx.clone(),
                release.take().unwrap(),
            ))))));
        }
        let mut sink = ChanSink(worker_out.clone());
        let (result, _) = execute(
            &mut session,
            SessionCmd::Compact { instructions },
            &mut sink,
            &mut HeadlessUi::default(),
            &worker_ui,
            &worker_out,
        );
        result.unwrap();
        worker_out.send(WorkerMsg::Done(Ok(()))).unwrap();
        if window != Window::AfterEmptyTake {
            reached_tx.send(()).unwrap();
            release
                .take()
                .unwrap()
                .recv_timeout(GATE_TIMEOUT)
                .expect("release the Done gate");
        }
        session
    });
    reached_rx
        .recv_timeout(GATE_TIMEOUT)
        .expect("reach the requested command boundary");
    let mut observed = ViewModel::default();
    let mut held_done = Vec::new();
    for message in out_rx.try_iter() {
        if let WorkerMsg::Event(event) = &message {
            apply_event(&mut observed, event);
        }
        if matches!(message, WorkerMsg::Done(_)) && window == Window::BeforeDoneObserved {
            held_done.push(message);
        } else {
            lp.worker_msg(message);
        }
    }
    assert_eq!(control.is_busy(), window == Window::AfterEmptyTake);
    if window == Window::BeforeDoneObserved {
        assert_eq!(held_done.len(), 1);
        assert_ne!(
            observed.status.activity,
            Activity::Idle,
            "the UI must still hold real prefill activity"
        );
    }
    for c in "third".chars() {
        lp.input(&mut terminal, InputEvent::Key(Key::char(c)));
    }
    lp.input(
        &mut terminal,
        InputEvent::Key(Key::press(if follow_up { "alt+enter" } else { "enter" })),
    );
    let commands: Vec<_> = cmd_rx.try_iter().collect();
    release_tx.send(()).unwrap();
    let mut session = worker.join().unwrap();
    assert_eq!(
        commands.len(),
        1,
        "admitted input must retain one worker consumer after the final empty take"
    );
    assert_eq!(server.requests().iter().filter(|r| r.max_tokens > 0).count(), 3);
    if rebind {
        let mut replacement = Session::open(deps(&env, engine, store, settings), Open::Ephemeral).unwrap();
        replacement.handle().steer("replacement-only".into());
        lp.worker_msg(WorkerMsg::Rebind(replacement.handle()));
        let (reply, rx) = mpsc::channel();
        lp.ui_req(UiRequest::EditorText { reply });
        assert_eq!(rx.recv_timeout(GATE_TIMEOUT).unwrap(), "third");
        for command in commands {
            execute(
                &mut replacement,
                command,
                &mut VecSink::default(),
                &mut ui,
                &ui_tx,
                &out_tx,
            )
            .0
            .unwrap();
        }
        assert_eq!(replacement.handle().queued().0, ["replacement-only"]);
        assert_eq!(
            server.requests().iter().filter(|r| r.max_tokens > 0).count(),
            3,
            "a stale wake cannot run another destination"
        );
        return;
    }
    let mut reply = VecSink::default();
    for command in commands {
        execute(&mut session, command, &mut reply, &mut ui, &ui_tx, &out_tx)
            .0
            .unwrap();
    }
    let requests: Vec<_> = server
        .requests()
        .into_iter()
        .filter(|r| r.max_tokens > 0)
        .collect();
    assert_eq!(requests.len(), 4, "exactly one next chat request");
    assert!(requests[3]
        .messages
        .iter()
        .any(|m| m.role == Role::User && m.content.as_deref() == Some("third")));
    assert_eq!(
        session.handle().queued(),
        (Vec::new(), Vec::new()),
        "no stranded input"
    );
    let mut rendered = ViewModel::default();
    for event in reply.0 {
        apply_event(&mut rendered, &event);
    }
    assert!(
        rendered
            .blocks
            .iter()
            .any(|b| matches!(b, Block::Assistant { text, .. } if text == "three")),
        "next reply is shown"
    );
}

#[test]
fn steering_after_empty_take_has_a_worker_consumer() {
    gated_prompt(Window::AfterEmptyTake, None, false, false);
}
#[test]
fn steering_before_done_is_observed_has_a_worker_consumer() {
    gated_prompt(
        Window::BeforeDoneObserved,
        Some("Focus on code".into()),
        false,
        false,
    );
}
#[test]
fn command_completed_before_input_starts_an_ordinary_prompt() {
    gated_prompt(Window::DoneObserved, None, false, false);
}
#[test]
fn follow_up_after_empty_take_has_a_worker_consumer() {
    gated_prompt(Window::AfterEmptyTake, None, true, false);
}
#[test]
fn stale_steering_wake_preserves_rebound_destination() {
    gated_prompt(Window::BeforeDoneObserved, None, false, true);
}
