//! K3 acceptance tests: headless print and JSON runs read their plain session inbox.
//! A persisted session with a pursuing goal takes `oxa send` follow-ups for the run; a run
//! without a goal does not watch the inbox, and an oversized inbox file is refused and kept.
//! Isolation: `oxa_fake::TestEnv` provides every file and directory; the real home is untouched.

use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{ScriptTurn, ScriptedEngine, TestEnv};
use oxa_proto::{
    codes, Access, AssistantTurn, CancelToken, ChatRequest, Decision, Engine, EngineInfo, ExecResult,
    ExecSpec, Mode, NullSink, OxaError, Part, Policy, Sandbox, StreamEvent,
};
use oxa_rpc::run_json_outcome_with_inbox;
use oxa_session::{send_inbox, FsStore, Goal, GoalBudget, SessionStore};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(10);

/// A policy for sessions without tools: every access is refused.
struct DenyAll;

impl Policy for DenyAll {
    fn decide(&self, _mode: Mode, _a: &Access, _cwd: &Path) -> Decision {
        Decision::Deny("no tools in this test")
    }
    fn remember(&mut self, _a: &Access) {}
}

/// A sandbox that is never reached: the sessions here run no tools.
struct NoBwrap;

impl Sandbox for NoBwrap {
    fn check(&self) -> Result<(), OxaError> {
        Err(OxaError::new(codes::NO_SANDBOX, "no sandbox in this test"))
    }
    fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
        Vec::new()
    }
    fn run(&self, _s: &ExecSpec, _c: &CancelToken, _live: &mut dyn FnMut(&[u8])) -> ExecResult {
        ExecResult::default()
    }
}

/// A scripted engine whose first model request waits until the test releases it.
struct HeldFirst {
    inner: Arc<ScriptedEngine>,
    hold: Mutex<Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>>,
}

impl Engine for HeldFirst {
    fn info(&self, target: Option<&str>) -> Result<EngineInfo, OxaError> {
        self.inner.info(target)
    }
    fn count_tokens(&self, text: &str) -> Result<u32, OxaError> {
        self.inner.count_tokens(text)
    }
    fn chat(
        &self,
        request: &ChatRequest,
        cancel: &CancelToken,
        on: &mut dyn FnMut(StreamEvent),
    ) -> Result<AssistantTurn, OxaError> {
        if let Some((entered, release)) = self.hold.lock().unwrap().take() {
            entered.send(()).unwrap();
            release.recv_timeout(WAIT).unwrap();
        }
        self.inner.chat(request, cancel, on)
    }
}

/// The test's side of a held first request.
struct Hold {
    entered: mpsc::Receiver<()>,
    release: mpsc::Sender<()>,
}

/// Three text replies behind a held first request: the test's recorder, the engine and the hold.
fn held() -> (Arc<ScriptedEngine>, Arc<dyn Engine>, Hold) {
    let inner = Arc::new(ScriptedEngine::new(vec![
        ScriptTurn::text("first answer"),
        ScriptTurn::text("second answer"),
        ScriptTurn::text("third answer"),
    ]));
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let engine: Arc<dyn Engine> = Arc::new(HeldFirst {
        inner: inner.clone(),
        hold: Mutex::new(Some((entered_tx, release_rx))),
    });
    let hold = Hold {
        entered: entered_rx,
        release: release_tx,
    };
    (inner, engine, hold)
}

/// A persisted session over `engine`, stored under the test's own agent directory.
fn open(env: &TestEnv, engine: Arc<dyn Engine>) -> Session {
    let store: Arc<dyn SessionStore> = Arc::new(FsStore::new(env.agent_dir().join("sessions")));
    let deps = Deps {
        engine,
        store,
        tools: Vec::new(),
        hooks: Vec::new(),
        policy: Box::new(DenyAll),
        sandbox: Arc::new(NoBwrap),
        settings: Settings::from_values(
            serde_json::json!({"oxa": {"envNote": false}}),
            serde_json::json!({}),
        ),
        dirs: Dirs::under(env.root()),
        cwd: env.work(),
        selection: ToolSelection::default(),
        resources: Default::default(),
        model: None,
        thinking: None,
        mode: None,
        session_id: None,
        jobs: oxa_core::fresh_jobs(),
    };
    Session::open(deps, Open::New).expect("the session opens over the scripted engine")
}

/// The agent directory whose inbox the run watches.
fn agent_dir(env: &TestEnv) -> PathBuf {
    Dirs::under(env.root()).agent_dir
}

/// Give the session a pursuing goal, which also persists it.
fn pursue(session: &mut Session) {
    let goal = Goal::new("Synthetic objective.".into(), GoalBudget::default(), 1000).unwrap();
    session.set_goal(Some(goal), &mut NullSink).unwrap();
}

fn prompt(text: &str) -> Vec<(String, Vec<Part>)> {
    vec![(text.to_string(), Vec::new())]
}

/// Runs one JSON prompt on `session` in a background thread, watching the inbox.
fn run_in_background(mut session: Session, agent: PathBuf) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let _ = run_json_outcome_with_inbox(&mut session, prompt("start"), &mut out, &mut err, &agent, true);
    })
}

/// Polls `ready` until it holds, or fails the test after `WAIT`.
fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !ready() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(ready(), "timed out waiting for {what}");
}

/// Whether any request the engine received carries `text` in its messages.
fn received(engine: &ScriptedEngine, text: &str) -> bool {
    engine
        .requests()
        .iter()
        .any(|r| format!("{:?}", r.messages).contains(text))
}

/// The number of `.msg` files still queued in a session's inbox.
fn queued(agent: &Path, id: &str) -> usize {
    fs::read_dir(agent.join("inbox").join(id))
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|e| e.path().extension().is_some_and(|x| x == "msg"))
                .count()
        })
        .unwrap_or(0)
}

#[test]
fn json_goal_run_receives_oxa_send_follow_up() {
    let env = TestEnv::new();
    let (inner, engine, hold) = held();
    let mut session = open(&env, engine);
    pursue(&mut session);
    let id = session.state().session_id;
    let handle = session.handle();
    let agent = agent_dir(&env);
    let run = run_in_background(session, agent.clone());
    hold.entered
        .recv_timeout(WAIT)
        .expect("the run reaches its first model request");
    send_inbox(&agent, &id, "follow-up from oxa send").unwrap();
    wait_until("the watcher to admit the message", || handle.has_inbox_input());
    hold.release.send(()).unwrap();
    run.join().expect("the run finishes");
    assert!(
        received(&inner, "follow-up from oxa send"),
        "the queued inbox message reaches the model as a follow-up"
    );
    assert_eq!(
        queued(&agent, &id),
        0,
        "an admitted message is removed from the inbox"
    );
}

#[test]
fn json_run_without_goal_does_not_watch_inbox() {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("ok")]));
    let mut session = open(&env, engine.clone());
    let id = session.state().session_id;
    let agent = agent_dir(&env);
    send_inbox(&agent, &id, "left for later").unwrap();
    let mut out = Vec::new();
    let mut err = Vec::new();
    run_json_outcome_with_inbox(&mut session, prompt("hi"), &mut out, &mut err, &agent, true)
        .expect("the run completes");
    assert!(
        !received(&engine, "left for later"),
        "no watcher without a pursuing goal"
    );
    assert_eq!(queued(&agent, &id), 1, "the message stays in the inbox");
}

#[test]
fn oversized_inbox_message_is_refused_and_file_kept() {
    let env = TestEnv::new();
    let (inner, engine, hold) = held();
    let mut session = open(&env, engine);
    pursue(&mut session);
    let id = session.state().session_id;
    let handle = session.handle();
    let agent = agent_dir(&env);
    // `send_inbox` refuses more than 256 KiB, so the oversized file is written directly.
    let dir = agent.join("inbox").join(&id);
    fs::create_dir_all(&dir).unwrap();
    let big = dir.join("00000000000000000000000000000001-oversized.msg");
    fs::write(&big, "x".repeat(256 * 1024 + 1)).unwrap();
    let run = run_in_background(session, agent.clone());
    hold.entered
        .recv_timeout(WAIT)
        .expect("the run reaches its first model request");
    send_inbox(&agent, &id, "small follow-up").unwrap();
    wait_until("the watcher to admit the small message", || {
        handle.has_inbox_input()
    });
    hold.release.send(()).unwrap();
    run.join().expect("the run finishes");
    assert!(
        received(&inner, "small follow-up"),
        "the normal message beside the oversized one is delivered"
    );
    assert!(
        !received(&inner, &"x".repeat(1000)),
        "the oversized message is never delivered"
    );
    assert!(big.exists(), "the oversized file is kept on disk");
    assert_eq!(queued(&agent, &id), 1, "only the oversized file is still queued");
}
