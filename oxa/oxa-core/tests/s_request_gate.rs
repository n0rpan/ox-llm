//! S1 (ENG-001, ENG-002): one request at a time per instance, and the user's own input cancels
//! the instance's background request (threshold summary, idle prefill) but never a foreground
//! one. Every wait on another thread has a hard timeout.

use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, RunEnd, Session, ToolSelection};
use oxa_fake::{ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{
    codes, AgentEvent, AssistantTurn, CancelToken, ChatRequest, CommandSpec, Engine, EngineInfo, HeadlessUi,
    HookPoint, HookReply, Hooks, Host, HostAction, OxaError, Role, StreamEvent, Tool, Ui, Value, VecSink,
};
use oxa_resources::Resources;
use oxa_sandbox::NoSandbox;
use oxa_session::{MemStore, SessionStore};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The longest any held request waits before it fails on its own (a hang guard).
const HARD: Duration = Duration::from_secs(10);

/// The scripted engine, plus holds: the first `hold_bg` background requests wait until their
/// cancel token fires; a foreground chat request waits `hold_fg_ms` (ending early only when
/// its own token fires). Each hold is announced on `started`.
struct Holding {
    inner: ScriptedEngine,
    hold_bg: AtomicUsize,
    hold_fg_ms: AtomicU64,
    started: Mutex<Sender<String>>,
    /// `(kind, background, outcome)` per request.
    log: Mutex<Vec<(String, bool, &'static str)>>,
}

impl Holding {
    fn new(turns: Vec<ScriptTurn>) -> (Arc<Holding>, Receiver<String>) {
        let (tx, rx) = channel();
        (
            Arc::new(Holding {
                inner: ScriptedEngine::new(turns),
                hold_bg: AtomicUsize::new(0),
                hold_fg_ms: AtomicU64::new(0),
                started: Mutex::new(tx),
                log: Mutex::new(Vec::new()),
            }),
            rx,
        )
    }
    fn log(&self) -> Vec<(String, bool, &'static str)> {
        self.log.lock().unwrap().clone()
    }
    fn note(&self, kind: &str, bg: bool, outcome: &'static str) {
        self.log.lock().unwrap().push((kind.to_string(), bg, outcome));
    }
}

impl Engine for Holding {
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
        let kind = r.hints.kind.as_str();
        let bg = r.hints.background;
        let hold_bg = bg && {
            let left = self.hold_bg.load(Ordering::SeqCst);
            left > 0
                && self
                    .hold_bg
                    .compare_exchange(left, left - 1, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
        };
        if hold_bg {
            let _ = self.started.lock().unwrap().send(kind.to_string());
            let end = Instant::now() + HARD;
            while Instant::now() < end {
                if cancel.is_cancelled() {
                    self.note(kind, bg, "cancelled");
                    return Err(OxaError::new(codes::ABORTED, "aborted"));
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            self.note(kind, bg, "timeout");
            return Err(OxaError::new(codes::INTERNAL, "held request was never cancelled"));
        }
        let fg_ms = self.hold_fg_ms.load(Ordering::SeqCst);
        if !bg && kind == "chat" && fg_ms > 0 {
            self.hold_fg_ms.store(0, Ordering::SeqCst);
            let _ = self.started.lock().unwrap().send(kind.to_string());
            let end = Instant::now() + Duration::from_millis(fg_ms);
            while Instant::now() < end {
                if cancel.is_cancelled() {
                    self.note(kind, bg, "cancelled");
                    return Err(OxaError::new(codes::ABORTED, "aborted"));
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        let res = self.inner.chat(r, cancel, on);
        self.note(kind, bg, if res.is_ok() { "ok" } else { "error" });
        res
    }
}

fn open(env: &TestEnv, engine: Arc<dyn Engine>, hooks: Vec<Arc<dyn Hooks>>, settings: &str) -> Session {
    let dirs = Dirs::under(env.root());
    let path = dirs.settings_file();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, settings).unwrap();
    let settings = Settings::load(&dirs, &env.work(), false);
    open_with_settings(env, engine, hooks, settings)
}

fn open_with_settings(
    env: &TestEnv,
    engine: Arc<dyn Engine>,
    hooks: Vec<Arc<dyn Hooks>>,
    settings: Settings,
) -> Session {
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    Session::open(
        Deps {
            engine,
            store,
            tools: Vec::new(),
            hooks,
            policy: Box::new(ModePolicy::new()),
            sandbox: Arc::new(NoSandbox),
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
        },
        Open::New,
    )
    .expect("open")
}

/// Every run ends above the soft limit: a threshold compaction after each run.
const ALWAYS_COMPACT: &str =
    r#"{"compaction": {"keepRecentTokens": 1}, "oxa": {"compaction": {"softTokens": 10}}}"#;

fn user_texts(s: &Session) -> Vec<String> {
    s.inner()
        .transcript
        .wire()
        .iter()
        .filter(|m| m.role == Role::User)
        .filter_map(|m| m.content.clone())
        .collect()
}

/// ENG-002: a follow-up typed while the threshold summary runs in the background cancels it
/// within a bounded time; the queued message then runs before `prompt` returns, and the
/// summary is decided again (and succeeds) after that run.
#[test]
fn follow_up_cancels_background_summary_and_runs() {
    let env = TestEnv::new();
    let (eng, started) = Holding::new(vec![
        ScriptTurn::text("first reply"),
        ScriptTurn::text("queued reply"),
        ScriptTurn::text("SUMMARY"),
    ]);
    eng.hold_bg.store(1, Ordering::SeqCst);
    let mut s = open(&env, eng.clone(), Vec::new(), ALWAYS_COMPACT);
    let handle = s.handle();
    let typist = std::thread::spawn(move || {
        let kind = started.recv_timeout(HARD).expect("the summary request is held");
        let at = Instant::now();
        handle.follow_up("typed meanwhile".to_string());
        (kind, at)
    });
    let mut sink = VecSink::default();
    s.prompt(
        "first".to_string(),
        Vec::new(),
        &mut sink,
        &mut HeadlessUi::default(),
    )
    .expect("prompt");
    let (kind, _) = typist.join().expect("typist");
    assert_eq!(kind, "compaction");

    let log = eng.log();
    let kinds: Vec<(&str, bool, &str)> = log.iter().map(|(k, b, o)| (k.as_str(), *b, *o)).collect();
    let cancelled = kinds
        .iter()
        .position(|e| *e == ("compaction", true, "cancelled"))
        .expect("the held summary was cancelled, not timed out");
    let queued_run = kinds
        .iter()
        .rposition(|e| *e == ("chat", false, "ok"))
        .expect("the queued message ran");
    let retried = kinds
        .iter()
        .rposition(|e| *e == ("compaction", true, "ok"))
        .expect("the summary is retried");
    assert!(cancelled < queued_run && queued_run < retried, "{kinds:?}");
    assert!(sink.0.iter().any(|e| matches!(
        e,
        AgentEvent::CompactionEnd {
            aborted: true,
            result: None,
            ..
        }
    )));
    assert!(
        eng.inner.requests().iter().any(|r| r.kind == "chat"
            && r.messages.last().and_then(|m| m.content.as_deref()) == Some("typed meanwhile")),
        "the typed message was sent as its own run"
    );
    assert_eq!(s.stats().compactions, 1);
    assert_eq!(s.handle().queued(), (Vec::new(), Vec::new()));
}

/// ENG-002: steering during a FOREGROUND request does not cancel it: the reply completes and
/// is stored, and the steering message is delivered after it.
#[test]
fn steer_does_not_cancel_a_foreground_request() {
    let env = TestEnv::new();
    let (eng, started) = Holding::new(vec![
        ScriptTurn::text("full reply"),
        ScriptTurn::text("after steer"),
    ]);
    eng.hold_fg_ms.store(300, Ordering::SeqCst);
    let mut s = open(
        &env,
        eng.clone(),
        Vec::new(),
        r#"{"compaction": {"enabled": false}}"#,
    );
    let handle = s.handle();
    let typist = std::thread::spawn(move || {
        started.recv_timeout(HARD).expect("the chat request is held");
        handle.steer("steered".to_string());
    });
    let mut sink = VecSink::default();
    s.prompt(
        "go".to_string(),
        Vec::new(),
        &mut sink,
        &mut HeadlessUi::default(),
    )
    .expect("prompt");
    typist.join().expect("typist");
    let log = eng.log();
    assert!(log.iter().all(|(_, _, o)| *o != "cancelled"), "{log:?}");
    let replies: Vec<String> = s
        .inner()
        .transcript
        .wire()
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .filter_map(|m| m.content.clone())
        .collect();
    assert_eq!(replies, vec!["full reply", "after steer"]);
    assert!(user_texts(&s).contains(&"steered".to_string()));
}

/// Abort during a background request cancels it too (`abort` sets both tokens).
#[test]
fn abort_cancels_background_summary() {
    let env = TestEnv::new();
    let (eng, started) = Holding::new(vec![ScriptTurn::text("first reply")]);
    eng.hold_bg.store(1, Ordering::SeqCst);
    let mut s = open(&env, eng.clone(), Vec::new(), ALWAYS_COMPACT);
    let handle = s.handle();
    let stopper = std::thread::spawn(move || {
        started.recv_timeout(HARD).expect("held");
        handle.abort();
    });
    let mut sink = VecSink::default();
    s.prompt(
        "first".to_string(),
        Vec::new(),
        &mut sink,
        &mut HeadlessUi::default(),
    )
    .expect("prompt");
    stopper.join().expect("stopper");
    assert!(eng
        .log()
        .iter()
        .any(|(k, _, o)| k == "compaction" && *o == "cancelled"));
    assert!(sink
        .0
        .iter()
        .any(|e| matches!(e, AgentEvent::AgentSettled { aborted: true })));
}

/// `Session::run_queued` runs input that waits in the queue when the session is idle (the front
/// end calls it after a navigation or `/compact`), and is a no-op when nothing is queued.
#[test]
fn run_queued_runs_waiting_input() {
    let env = TestEnv::new();
    let (eng, _started) = Holding::new(vec![ScriptTurn::text("answer")]);
    let mut s = open(
        &env,
        eng.clone(),
        Vec::new(),
        r#"{"compaction": {"enabled": false}}"#,
    );
    let mut sink = VecSink::default();
    let mut ui = HeadlessUi::default();
    assert_eq!(s.run_queued(&mut sink, &mut ui).expect("idle"), None);
    s.handle().follow_up("waiting".to_string());
    assert_eq!(
        s.run_queued(&mut sink, &mut ui).expect("run"),
        Some(RunEnd::Stopped)
    );
    assert_eq!(user_texts(&s), vec!["waiting"]);
    assert!(sink
        .0
        .iter()
        .any(|e| matches!(e, AgentEvent::AgentSettled { aborted: false })));
}

/// A hook set that asks the host to park the session (a model request) from inside a request's
/// `context` hook: the re-entrant request is refused by the gate.
#[derive(Default)]
struct Reentrant {
    host: Mutex<Option<Arc<dyn Host>>>,
    answers: Mutex<Vec<Result<Value, OxaError>>>,
}

impl Hooks for Reentrant {
    fn fire(&self, p: HookPoint, _payload: &Value, _ui: &mut dyn Ui) -> HookReply {
        if p == HookPoint::Context && self.answers.lock().unwrap().is_empty() {
            let host = self.host.lock().unwrap().clone().expect("attached");
            let answer = host.call(HostAction::ParkSession);
            self.answers.lock().unwrap().push(answer);
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
        Err(OxaError::new(codes::NOT_FOUND, name))
    }
    fn prompt_sections(&self) -> Vec<(String, String)> {
        Vec::new()
    }
    fn attach(&self, host: Arc<dyn Host>) {
        *self.host.lock().unwrap() = Some(host);
    }
}

/// ENG-001: a second request while one is in flight on this instance is refused with `busy`
/// and never sent; the first request goes on normally.
#[test]
fn gate_refuses_a_reentrant_request() {
    let env = TestEnv::new();
    let (eng, _started) = Holding::new(vec![ScriptTurn::text("reply")]);
    let hook = Arc::new(Reentrant::default());
    let mut s = open(
        &env,
        eng.clone(),
        vec![hook.clone()],
        r#"{"compaction": {"enabled": false}}"#,
    );
    let mut sink = VecSink::default();
    s.prompt(
        "go".to_string(),
        Vec::new(),
        &mut sink,
        &mut HeadlessUi::default(),
    )
    .expect("prompt");
    let answers = hook.answers.lock().unwrap().clone();
    assert_eq!(answers.len(), 1);
    let err = answers[0].clone().expect_err("refused");
    assert_eq!(err.code, codes::BUSY);
    let log = eng.log();
    assert_eq!(
        log.iter().map(|(k, _, o)| (k.as_str(), *o)).collect::<Vec<_>>(),
        vec![("chat", "ok")],
        "the park was never sent"
    );
}

/// L1: with `oxa.liveSettings` on, a settings edit made after the session opened is applied by
/// a queued run (`run_queued`), which does not pass through `run_from_wire`; with the setting
/// off (opt-out, `false`) the edit waits for `/reload`.
#[test]
fn queued_run_applies_changed_settings_only_when_live_settings_is_on() {
    for (live, expect) in [(true, "light-theme"), (false, "dark")] {
        let env = TestEnv::new();
        let (eng, _started) = Holding::new(vec![ScriptTurn::text("answer")]);
        let first = format!(r#"{{"theme": "dark", "oxa": {{"liveSettings": {live}}}}}"#);
        let mut s = open(&env, eng.clone(), Vec::new(), &first);
        let path = Dirs::under(env.root()).settings_file();
        std::fs::write(
            path,
            format!(r#"{{"theme": "light-theme", "oxa": {{"liveSettings": {live}}}}}"#),
        )
        .unwrap();
        s.handle().follow_up("waiting".to_string());
        let mut sink = VecSink::default();
        let mut ui = HeadlessUi::default();
        assert_eq!(
            s.run_queued(&mut sink, &mut ui).expect("run"),
            Some(RunEnd::Stopped)
        );
        assert_eq!(
            s.inner().settings.get("theme").and_then(Value::as_str),
            Some(expect),
            "live = {live}"
        );
    }
}

#[test]
fn queued_run_preserves_settings_supplied_by_the_session_caller() {
    let env = TestEnv::new();
    let (engine, _started) = Holding::new(vec![ScriptTurn::text("answer")]);
    let settings = Settings::from_values(serde_json::json!({"theme": "dark"}), Value::Null);
    let mut session = open_with_settings(&env, engine, Vec::new(), settings);
    let path = Dirs::under(env.root()).settings_file();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, r#"{"theme": "light-theme"}"#).unwrap();
    session.handle().follow_up("waiting".to_string());
    let mut sink = VecSink::default();
    let mut ui = HeadlessUi::default();
    assert_eq!(
        session.run_queued(&mut sink, &mut ui).unwrap(),
        Some(RunEnd::Stopped)
    );
    assert_eq!(session.inner().settings.string("theme").as_deref(), Some("dark"));
}

#[test]
fn follow_up_started_during_settle_uses_live_settings() {
    struct QueueFollowUp {
        handle: oxa_core::Handle,
        path: Option<std::path::PathBuf>,
        events: Vec<AgentEvent>,
    }
    impl oxa_proto::EventSink for QueueFollowUp {
        fn emit(&mut self, event: &AgentEvent) {
            self.events.push(event.clone());
            if matches!(
                event,
                AgentEvent::AgentEnd {
                    will_retry: false,
                    ..
                }
            ) {
                if let Some(path) = self.path.take() {
                    std::fs::write(path, r#"{"theme":"light-theme"}"#).unwrap();
                    self.handle.follow_up("waiting".to_string());
                }
            }
        }
    }

    let env = TestEnv::new();
    let (engine, _started) = Holding::new(vec![ScriptTurn::text("first"), ScriptTurn::text("next")]);
    let mut session = open(&env, engine.clone(), Vec::new(), r#"{"theme":"dark"}"#);
    let mut sink = QueueFollowUp {
        handle: session.handle(),
        path: Some(Dirs::under(env.root()).settings_file()),
        events: Vec::new(),
    };
    session
        .prompt(
            "first".to_string(),
            Vec::new(),
            &mut sink,
            &mut HeadlessUi::default(),
        )
        .unwrap();
    assert_eq!(
        engine.log().iter().filter(|(kind, _, _)| kind == "chat").count(),
        2
    );
    let mut starts = 0;
    for event in sink.events {
        match event {
            AgentEvent::AgentStart => starts += 1,
            AgentEvent::Notice { text, .. } if text == "settings changed: theme" => {
                assert_eq!(starts, 1, "refresh must precede the follow-up");
                return;
            }
            _ => {}
        }
    }
    panic!("the follow-up did not observe the edit");
}
