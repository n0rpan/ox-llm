//! Existing TUI + external clients, one real core Session against owned loopback fakes.
use oxa_config::{Dirs, Keymap, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_engine::HttpEngine;
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
use oxa_interactive::{run_session, InteractiveOpts, Reopen};
use oxa_proto::{
    Access, CancelToken, Decision, Engine, ExecResult, ExecSpec, Mode, OxaError, Policy, Sandbox,
};
use oxa_resources::Resources;
use oxa_session::{FsStore, SessionStore};
use oxa_tui::{strip_ansi, InputEvent, Key, TermCaps, TermMode, Terminal, TestTerminal};
use serde_json::{json, Value};
use std::{
    io::Write,
    os::unix::net::UnixStream,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

struct Allow;
impl Policy for Allow {
    fn decide(&self, _: Mode, _: &Access, _: &Path) -> Decision {
        Decision::Allow
    }
    fn remember(&mut self, _: &Access) {}
}
struct NoCommands;
impl Sandbox for NoCommands {
    fn check(&self) -> Result<(), OxaError> {
        Ok(())
    }
    fn bwrap_args(&self, _: &ExecSpec) -> Vec<String> {
        vec![]
    }
    fn run(&self, _: &ExecSpec, _: &CancelToken, _: &mut dyn FnMut(&[u8])) -> ExecResult {
        panic!("no fixture command execution")
    }
}
fn deps(url: &str, root: &Path, work: &Path, store: Arc<dyn SessionStore>) -> Deps {
    let engine: Arc<dyn Engine> = Arc::new(HttpEngine::new(url, None).unwrap());
    Deps {
        engine,
        store,
        tools: vec![],
        hooks: vec![],
        policy: Box::new(Allow),
        sandbox: Arc::new(NoCommands),
        settings: Settings::default(),
        dirs: Dirs::under(root),
        cwd: work.into(),
        selection: ToolSelection::default(),
        resources: Resources::default(),
        model: None,
        thinking: None,
        mode: None,
        session_id: None,
    }
}
fn opts(env: &TestEnv, url: &str, store: Arc<dyn SessionStore>) -> InteractiveOpts {
    let root = env.root().to_path_buf();
    let work = env.work();
    let url = url.to_owned();
    let reopened_store = store.clone();
    InteractiveOpts {
        fullscreen: false,
        dirs: Dirs::under(env.root()),
        settings: Settings::default(),
        keymap: Keymap::defaults(),
        store,
        hooks: vec![],
        mcp: None,
        initial_prompt: None,
        initial_parts: vec![],
        initial_follow_ups: vec![],
        theme: None,
        notices: vec![],
        env: vec![],
        caps: Some(TermCaps::BASIC),
        reopen: Box::new(move |how| {
            let open = match how {
                Reopen::New => Open::New,
                Reopen::Resume(s) => Open::Resume(s),
                Reopen::Fork(s) => Open::Fork(s),
                Reopen::Import(_) => return Err(OxaError::new("invalid", "no fixture import")),
            };
            Session::open(deps(&url, &root, &work, reopened_store.clone()), open)
        }),
        open_picker: false,
        save_trust: None,
        gated: vec![],
        yolo: None,
    }
}
fn request(socket: &Path, value: Value) -> Value {
    let mut stream = UnixStream::connect(socket).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    writeln!(stream, "{value}").unwrap();
    serde_json::from_reader(stream).unwrap()
}
fn wait_status(socket: &Path, mut predicate: impl FnMut(&Value) -> bool) -> Value {
    let started = Instant::now();
    loop {
        if socket.exists() {
            let value = request(socket, json!({"method":"status","id":"read"}));
            if predicate(&value) {
                return value;
            }
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "status wait expired at {}",
            socket.display()
        );
        std::thread::sleep(Duration::from_millis(3));
    }
}
fn revision(status: &Value) -> u64 {
    status["status"]["goal"]["revision"].as_u64().unwrap()
}
fn state(status: &Value) -> &str {
    status["status"]["goal"]["state"].as_str().unwrap_or("")
}
fn mutation(socket: &Path, id: &str, method: &str, revision: u64, text: Option<&str>) {
    let mut value = json!({"id":id,"method":method,"revision":revision});
    if let Some(text) = text {
        value["text"] = json!(text);
    }
    let response = request(socket, value);
    assert_eq!(response["queued"], true, "{response}");
}
struct LiveTerm {
    inner: TestTerminal,
    done: Arc<AtomicBool>,
    typing: Arc<AtomicBool>,
    typed: Arc<AtomicBool>,
    deadline: Instant,
    quit: bool,
    final_text: &'static str,
    escape: bool,
    swap_when: Option<Arc<AtomicBool>>,
    swapped: bool,
    rejected: Arc<AtomicBool>,
}
impl Terminal for LiveTerm {
    fn write(&mut self, b: &[u8]) {
        self.inner.write(b)
    }
    fn flush(&mut self) {
        self.inner.flush()
    }
    fn poll(&mut self, ms: u64) -> Vec<InputEvent> {
        assert!(
            Instant::now() < self.deadline,
            "TUI deadline; screen {}",
            strip_ansi(&String::from_utf8_lossy(&self.inner.out))
        );
        if self.typing.load(Ordering::Acquire) && !self.typed.swap(true, Ordering::AcqRel) {
            if self.escape {
                return vec![InputEvent::Key(Key::press("escape"))];
            }
            let mut keys: Vec<_> = "TUI live clarification"
                .chars()
                .map(|c| InputEvent::Key(Key::char(c)))
                .collect();
            keys.push(InputEvent::Key(Key::press("enter")));
            return keys;
        }
        let screen = strip_ansi(&String::from_utf8_lossy(&self.inner.out));
        if screen.contains("Resume or cancel the goal") {
            self.rejected.store(true, Ordering::Release);
        }
        if self
            .swap_when
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Acquire))
            && screen.contains(self.final_text)
            && !self.swapped
        {
            self.swapped = true;
            let mut keys: Vec<_> = "/new".chars().map(|c| InputEvent::Key(Key::char(c))).collect();
            keys.push(InputEvent::Key(Key::press("enter")));
            return keys;
        }
        if self.done.load(Ordering::Acquire) && screen.contains(self.final_text) && !self.quit {
            self.quit = true;
            return vec![InputEvent::Key(Key::press("ctrl+d"))];
        }
        std::thread::sleep(Duration::from_millis(ms.min(3)));
        vec![]
    }
    fn size(&self) -> (u16, u16) {
        self.inner.size()
    }
    fn caps(&self) -> TermCaps {
        self.inner.caps()
    }
    fn set_mode(&mut self, mode: TermMode, on: bool) {
        self.inner.set_mode(mode, on)
    }
    fn suspend(&mut self) {
        self.inner.suspend()
    }
    fn resume(&mut self) {
        self.inner.resume()
    }
}
fn shared_tui_and_external_steering() {
    let env = TestEnv::new();
    let engine=Arc::new(FakeServer::start(vec![
        ScriptTurn::text("shared-visible-first"),
        ScriptTurn::text(r#"{"state":"continue","summary":"both clarifications reviewed"}"#),
        ScriptTurn::text(r#"{"state":"continue","summary":"clarifications verified; final review remains"}"#),
        ScriptTurn::text(r#"{"state":"done","summary":"shared-complete-final","evidence":["same conversation checked"]}"#),
    ],FakeOpts{hold_headers_ms:200,..FakeOpts::default()}).unwrap());
    let store: Arc<dyn SessionStore> = Arc::new(FsStore::new(Dirs::under(env.root()).sessions_dir));
    let session = Session::open(
        deps(&engine.url(), env.root(), &env.work(), store.clone()),
        Open::New,
    )
    .unwrap();
    let session_id = session.state().session_id;
    let handle = session.handle();
    let directory = env.root().join("control");
    let socket = directory.join("session.sock");
    std::env::set_var("OXA_SESSION_CONTROL_DIRECTORY", &directory);
    let done = Arc::new(AtomicBool::new(false));
    let typing = Arc::new(AtomicBool::new(false));
    let typed = Arc::new(AtomicBool::new(false));
    let peer_done = done.clone();
    let peer_typing = typing.clone();
    let peer_typed = typed.clone();
    let peer_engine = engine.clone();
    let peer_socket = socket.clone();
    let expected = session_id.clone();
    let peer = std::thread::spawn(move || {
        let status = wait_status(&peer_socket, |v| v["status"]["session"]["id"] == expected);
        let rev = revision(&status);
        let response = request(
            &peer_socket,
            json!({"method":"set_goal","id":"set","revision":rev,"text":"Review the shared session and verify both clarifications","budget":{"wall_ms":10_000,"tokens":null,"turns":8}}),
        );
        assert_eq!(response["queued"], true);
        let paused = wait_status(&peer_socket, |v| {
            state(v) == "paused"
                && v["status"]["goal"]["goal"] == "Review the shared session and verify both clarifications"
        });
        mutation(&peer_socket, "resume", "resume", revision(&paused), None);
        let running = wait_status(&peer_socket, |v| {
            v["status"]["goal"]["turn_active"] == true
                && peer_engine.requests().iter().any(|r| r.max_tokens > 0)
        });
        peer_typing.store(true, Ordering::Release);
        mutation(
            &peer_socket,
            "steer",
            "steer",
            revision(&running),
            Some("External live clarification"),
        );
        let finished = wait_status(&peer_socket, |v| state(v) == "done");
        assert_eq!(finished["status"]["session"]["id"], expected);
        assert_eq!(finished["status"]["goal"]["turns_started"], 2);
        assert!(finished["status"]["goal"]["reported_tokens"].as_u64().unwrap() > 0);
        assert!(peer_typed.load(Ordering::Acquire));
        let mut after = 0;
        let mut text = String::new();
        let mut admitted = false;
        loop {
            let page = request(
                &peer_socket,
                json!({"method":"events","id":"history","after":after}),
            );
            assert_eq!(page["gap"], false);
            for event in page["events"].as_array().unwrap() {
                let event = &event["event"];
                if event["request_id"] == "steer" && event["kind"] == "control_admitted" {
                    admitted = true;
                }
                if let Some(reply) = event["reply"]["text"].as_str() {
                    text.push_str(reply);
                }
            }
            after = page["next"].as_u64().unwrap();
            if after == page["latest"].as_u64().unwrap() {
                break;
            }
        }
        assert!(
            admitted && text.contains("shared-visible-first") && text.contains("shared-complete-final"),
            "{text}"
        );
        peer_done.store(true, Ordering::Release);
    });
    let mut terminal = LiveTerm {
        inner: TestTerminal::new(120, 30),
        done,
        typing,
        typed,
        deadline: Instant::now() + Duration::from_secs(15),
        quit: false,
        final_text: "shared-complete-final",
        escape: false,
        swap_when: None,
        swapped: false,
        rejected: Arc::new(AtomicBool::new(false)),
    };
    let (code, last) = run_session(&mut terminal, session, &mut opts(&env, &engine.url(), store));
    peer.join().unwrap();
    std::env::remove_var("OXA_SESSION_CONTROL_DIRECTORY");
    assert_eq!(code, 0);
    assert_eq!(last.unwrap().id, session_id);
    assert!(!handle.is_busy());
    assert!(!socket.exists());
    let screen = strip_ansi(&String::from_utf8_lossy(&terminal.inner.out));
    assert!(
        screen.contains("shared-visible-first")
            && screen.contains("shared-complete-final")
            && screen.contains("Goal Done"),
        "{screen}"
    );
    let requests = engine.requests();
    let chats: Vec<_> = requests.iter().filter(|r| r.max_tokens > 0).collect();
    assert_eq!(chats.len(), 4);
    assert!(chats.iter().all(|r| r.session == session_id));
    for clarification in [
        "TUI live clarification",
        "External live clarification",
        "shared-visible-first",
    ] {
        assert!(
            chats
                .last()
                .unwrap()
                .messages
                .iter()
                .any(|m| m.content.as_deref().is_some_and(|s| s.contains(clarification))),
            "missing {clarification}"
        );
    }
}
fn pause_resume_redirect_and_rebind(escape: bool) {
    let env = TestEnv::new();
    let engine = Arc::new(
        FakeServer::start(
            vec![
                ScriptTurn::text("unfinished paused request"),
                ScriptTurn::text(
                    r#"{"state":"done","summary":"pause-resume-final","evidence":["revised goal verified"]}"#,
                ),
            ],
            FakeOpts {
                hold_headers_ms: 250,
                ..FakeOpts::default()
            },
        )
        .unwrap(),
    );
    let store: Arc<dyn SessionStore> = Arc::new(FsStore::new(Dirs::under(env.root()).sessions_dir));
    let session = Session::open(
        deps(&engine.url(), env.root(), &env.work(), store.clone()),
        Open::New,
    )
    .unwrap();
    let session_id = session.state().session_id;
    let expected = session_id.clone();
    let handle = session.handle();
    let directory = env.root().join("control");
    let socket = directory.join("session.sock");
    std::env::set_var("OXA_SESSION_CONTROL_DIRECTORY", &directory);
    let done = Arc::new(AtomicBool::new(false));
    let typing = Arc::new(AtomicBool::new(false));
    let typed = Arc::new(AtomicBool::new(false));
    let swap = Arc::new(AtomicBool::new(false));
    let rejected = Arc::new(AtomicBool::new(false));
    let peer_rejected = rejected.clone();
    let peer_done = done.clone();
    let peer_typing = typing.clone();
    let peer_swap = swap.clone();
    let peer_socket = socket.clone();
    let peer_engine = engine.clone();
    let peer = std::thread::spawn(move || {
        let status = wait_status(&peer_socket, |v| v["status"]["session"]["id"] == expected);
        assert_eq!(
            request(
                &peer_socket,
                json!({"method":"set_goal","id":"initial","revision":revision(&status),"text":"initial paused goal","budget":{"wall_ms":6000,"tokens":null,"turns":5}})
            )["queued"],
            true
        );
        let paused = wait_status(&peer_socket, |v| {
            v["status"]["goal"]["goal"] == "initial paused goal"
        });
        mutation(&peer_socket, "start", "resume", revision(&paused), None);
        let running = wait_status(&peer_socket, |v| {
            v["status"]["goal"]["turn_active"] == true
                && peer_engine.requests().iter().any(|r| r.max_tokens > 0)
        });
        if escape {
            peer_typing.store(true, Ordering::Release);
        } else {
            mutation(&peer_socket, "pause", "pause", revision(&running), None);
        }
        let paused = wait_status(&peer_socket, |v| {
            state(v) == "paused"
                && v["status"]["goal"]["turns_started"] == 1
                && v["status"]["goal"]["turn_active"] == false
        });
        assert!(paused["status"]["goal"]["reported_tokens"].is_null());
        if !escape {
            peer_typing.store(true, Ordering::Release);
            let began = Instant::now();
            while !peer_rejected.load(Ordering::Acquire) {
                assert!(
                    began.elapsed() < Duration::from_secs(3),
                    "paused TUI prompt must be visibly rejected"
                );
                std::thread::sleep(Duration::from_millis(3));
            }
        }
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(
            peer_engine.requests().iter().filter(|r| r.max_tokens > 0).count(),
            1
        );
        mutation(
            &peer_socket,
            "retarget",
            "set_goal",
            revision(&paused),
            Some("operator revised paused goal"),
        );
        let updated = wait_status(&peer_socket, |v| {
            v["status"]["goal"]["goal"] == "operator revised paused goal"
        });
        assert_eq!(updated["status"]["goal"]["turns_started"], 1);
        assert_eq!(updated["status"]["goal"]["budget"]["wall_ms"], 6000);
        assert!(
            updated["status"]["goal"]["elapsed_ms"].as_u64().unwrap()
                >= paused["status"]["goal"]["elapsed_ms"].as_u64().unwrap()
        );
        mutation(&peer_socket, "continue", "resume", revision(&updated), None);
        let finished = wait_status(&peer_socket, |v| state(v) == "done");
        assert_eq!(finished["status"]["goal"]["turns_started"], 2);
        assert!(finished["status"]["goal"]["reported_tokens"].is_null());
        let old_revision = revision(&finished);
        peer_swap.store(true, Ordering::Release);
        let rebound = wait_status(&peer_socket, |v| {
            v["status"]["session"]["id"]
                .as_str()
                .is_some_and(|s| s != expected)
        });
        assert!(revision(&rebound) > old_revision);
        assert_eq!(rebound["status"]["goal"]["reason"], "no_goal");
        mutation(
            &peer_socket,
            "stale",
            "set_goal",
            old_revision,
            Some("must not start on new session"),
        );
        let began = Instant::now();
        loop {
            let events = request(
                &peer_socket,
                json!({"method":"events","id":"stale-result","after":0}),
            );
            if events["events"].as_array().unwrap().iter().any(|event| {
                event["event"]["request_id"] == "stale" && event["event"]["kind"] == "control_rejected"
            }) {
                break;
            }
            // The relevant event can be on a later page.
            let mut after = events["next"].as_u64().unwrap();
            let mut found = false;
            while after < events["latest"].as_u64().unwrap() {
                let page = request(
                    &peer_socket,
                    json!({"method":"events","id":"stale-page","after":after}),
                );
                found |= page["events"].as_array().unwrap().iter().any(|event| {
                    event["event"]["request_id"] == "stale" && event["event"]["kind"] == "control_rejected"
                });
                after = page["next"].as_u64().unwrap();
            }
            if found {
                break;
            }
            assert!(began.elapsed() < Duration::from_secs(3));
            std::thread::sleep(Duration::from_millis(3));
        }
        peer_done.store(true, Ordering::Release);
    });
    let mut terminal = LiveTerm {
        inner: TestTerminal::new(120, 30),
        done,
        typing,
        typed,
        deadline: Instant::now() + Duration::from_secs(15),
        quit: false,
        final_text: "pause-resume-final",
        escape,
        swap_when: Some(swap),
        swapped: false,
        rejected,
    };
    let (code, last) = run_session(&mut terminal, session, &mut opts(&env, &engine.url(), store));
    peer.join().unwrap();
    std::env::remove_var("OXA_SESSION_CONTROL_DIRECTORY");
    assert_eq!(code, 0);
    assert_ne!(last.unwrap().id, session_id);
    assert!(!handle.is_busy());
    assert!(!socket.exists());
    let screen = strip_ansi(&String::from_utf8_lossy(&terminal.inner.out));
    assert!(
        screen.contains("Goal Paused")
            && screen.contains("pause-resume-final")
            && screen.contains("New session started")
    );
    let requests = engine.requests();
    let chats: Vec<_> = requests.iter().filter(|r| r.max_tokens > 0).collect();
    assert_eq!(chats.len(), 2);
    assert!(chats.iter().all(|r| r.session == session_id));
    assert!(chats[1].messages.iter().any(|m| m
        .content
        .as_deref()
        .is_some_and(|s| s.contains("operator revised paused goal"))));
}
fn bounded_goal_or_tui_exit(kind: &str) {
    let env = TestEnv::new();
    let engine = Arc::new(
        FakeServer::start(
            vec![ScriptTurn::text(
                r#"{"state":"done","summary":"must not evade stop","evidence":["late result"]}"#,
            )],
            FakeOpts {
                hold_headers_ms: if kind == "tokens" { 30 } else { 1000 },
                ..FakeOpts::default()
            },
        )
        .unwrap(),
    );
    let store: Arc<dyn SessionStore> = Arc::new(FsStore::new(Dirs::under(env.root()).sessions_dir));
    let session = Session::open(
        deps(&engine.url(), env.root(), &env.work(), store.clone()),
        Open::New,
    )
    .unwrap();
    let handle = session.handle();
    let directory = env.root().join("control");
    let socket = directory.join("session.sock");
    std::env::set_var("OXA_SESSION_CONTROL_DIRECTORY", &directory);
    let done = Arc::new(AtomicBool::new(false));
    let peer_done = done.clone();
    let peer_engine = engine.clone();
    let peer_socket = socket.clone();
    let kind = kind.to_owned();
    let expected = kind.clone();
    let peer = std::thread::spawn(move || {
        let status = wait_status(&peer_socket, |v| v["status"]["session"]["id"].is_string());
        let budget = json!({"wall_ms":if kind=="time"{300}else{5000},"tokens":if kind=="tokens"{json!(1)}else{Value::Null},"turns":4});
        assert_eq!(
            request(
                &peer_socket,
                json!({"method":"set_goal","id":"bounded","revision":revision(&status),"text":"bounded session goal","budget":budget})
            )["queued"],
            true
        );
        let paused = wait_status(&peer_socket, |v| {
            v["status"]["goal"]["goal"] == "bounded session goal"
        });
        mutation(&peer_socket, "go", "resume", revision(&paused), None);
        if kind != "tokens" {
            let running = wait_status(&peer_socket, |v| {
                v["status"]["goal"]["turn_active"] == true
                    && peer_engine.requests().iter().any(|r| r.max_tokens > 0)
            });
            if kind == "cancel" {
                mutation(&peer_socket, "stop", "cancel", revision(&running), None);
            }
        }
        if kind != "exit" {
            let stopped = wait_status(&peer_socket, |v| {
                state(v)
                    == if kind == "cancel" {
                        "canceled"
                    } else {
                        "budget_exhausted"
                    }
                    && v["status"]["goal"]["turn_active"] == false
            });
            assert_ne!(state(&stopped), "done");
            assert_eq!(stopped["status"]["goal"]["turns_started"], 1);
            std::thread::sleep(Duration::from_millis(40));
            assert_eq!(
                peer_engine.requests().iter().filter(|r| r.max_tokens > 0).count(),
                1,
                "no invisible follow-on work after stop"
            );
        }
        peer_done.store(true, Ordering::Release);
    });
    let mut terminal = LiveTerm {
        inner: TestTerminal::new(120, 30),
        done,
        typing: Arc::new(AtomicBool::new(false)),
        typed: Arc::new(AtomicBool::new(false)),
        deadline: Instant::now() + Duration::from_secs(10),
        quit: false,
        final_text: "",
        escape: false,
        swap_when: None,
        swapped: false,
        rejected: Arc::new(AtomicBool::new(false)),
    };
    let began = Instant::now();
    let (code, _) = run_session(&mut terminal, session, &mut opts(&env, &engine.url(), store));
    peer.join().unwrap();
    std::env::remove_var("OXA_SESSION_CONTROL_DIRECTORY");
    assert_eq!(code, 0);
    assert!(!handle.is_busy());
    assert!(!socket.exists());
    assert!(
        began.elapsed() < Duration::from_millis(900),
        "{expected} should stop the actual running turn, not wait for its one-second reply"
    );
}

fn attachment_rejects_agent_workspace() {
    let env = TestEnv::new();
    let engine = FakeServer::start(vec![], FakeOpts::default()).unwrap();
    let store: Arc<dyn SessionStore> = Arc::new(FsStore::new(Dirs::under(env.root()).sessions_dir));
    let session = Session::open(deps(&engine.url(), env.root(), &env.work(), store), Open::New).unwrap();
    let directory = env.work().join("agent-readable-control");
    std::env::set_var("OXA_SESSION_CONTROL_DIRECTORY", &directory);
    let failed = Arc::new(AtomicBool::new(false));
    let notified = failed.clone();
    let attachment = oxa_orchestrator::tui::attach(&session, move |event| {
        if let oxa_proto::AgentEvent::Notice {
            level: oxa_proto::Level::Error,
            ..
        } = event
        {
            notified.store(true, Ordering::Release);
        }
    });
    std::env::remove_var("OXA_SESSION_CONTROL_DIRECTORY");
    assert!(attachment.is_none());
    assert!(failed.load(Ordering::Acquire));
    assert!(!directory.exists());
    assert!(!engine.requests().iter().any(|r| r.max_tokens > 0));
}

fn ordinary_tui_turn_advances_attachment_revision() {
    let env = TestEnv::new();
    let engine = Arc::new(
        FakeServer::start(
            vec![
                ScriptTurn::text("ordinary-visible-first"),
                ScriptTurn::text("ordinary-final"),
            ],
            FakeOpts {
                hold_headers_ms: 200,
                ..FakeOpts::default()
            },
        )
        .unwrap(),
    );
    let store: Arc<dyn SessionStore> = Arc::new(FsStore::new(Dirs::under(env.root()).sessions_dir));
    let session = Session::open(
        deps(&engine.url(), env.root(), &env.work(), store.clone()),
        Open::New,
    )
    .unwrap();
    let expected = session.state().session_id;
    let handle = session.handle();
    let directory = env.root().join("control");
    let socket = directory.join("session.sock");
    std::env::set_var("OXA_SESSION_CONTROL_DIRECTORY", &directory);
    let done = Arc::new(AtomicBool::new(false));
    let typing = Arc::new(AtomicBool::new(false));
    let typed = Arc::new(AtomicBool::new(false));
    let peer_done = done.clone();
    let peer_typing = typing.clone();
    let peer_socket = socket.clone();
    let peer_engine = engine.clone();
    let peer = std::thread::spawn(move || {
        let initial = wait_status(&peer_socket, |v| v["status"]["session"]["id"] == expected);
        let old_revision = revision(&initial);
        peer_typing.store(true, Ordering::Release);
        let running = wait_status(&peer_socket, |v| {
            v["status"]["session"]["busy"] == true && peer_engine.requests().iter().any(|r| r.max_tokens > 0)
        });
        assert!(revision(&running) > old_revision);
        mutation(
            &peer_socket,
            "stale-ordinary",
            "steer",
            old_revision,
            Some("must not reach a later TUI turn"),
        );
        mutation(
            &peer_socket,
            "live-ordinary",
            "steer",
            revision(&running),
            Some("ordinary external clarification"),
        );
        let final_status = wait_status(&peer_socket, |v| {
            v["status"]["session"]["busy"] == false
                && peer_engine.requests().iter().filter(|r| r.max_tokens > 0).count() == 2
        });
        assert!(revision(&final_status) > revision(&running));
        assert_eq!(final_status["status"]["goal"]["reason"], "no_goal");
        let mut after = 0;
        let mut rejected = false;
        let mut admitted = false;
        loop {
            let page = request(
                &peer_socket,
                json!({"id":"ordinary-events","method":"events","after":after}),
            );
            assert_eq!(page["gap"], false);
            for item in page["events"].as_array().unwrap() {
                let item = &item["event"];
                rejected |= item["request_id"] == "stale-ordinary" && item["kind"] == "control_rejected";
                admitted |= item["request_id"] == "live-ordinary" && item["kind"] == "control_admitted";
            }
            after = page["next"].as_u64().unwrap();
            if after == page["latest"].as_u64().unwrap() {
                break;
            }
        }
        assert!(rejected && admitted);
        peer_done.store(true, Ordering::Release);
    });
    let mut terminal = LiveTerm {
        inner: TestTerminal::new(120, 30),
        done,
        typing,
        typed,
        deadline: Instant::now() + Duration::from_secs(10),
        quit: false,
        final_text: "ordinary-final",
        escape: false,
        swap_when: None,
        swapped: false,
        rejected: Arc::new(AtomicBool::new(false)),
    };
    let (code, _) = run_session(&mut terminal, session, &mut opts(&env, &engine.url(), store));
    peer.join().unwrap();
    std::env::remove_var("OXA_SESSION_CONTROL_DIRECTORY");
    assert_eq!(code, 0);
    assert!(!handle.is_busy());
    assert!(!socket.exists());
    let requests = engine.requests();
    let chats: Vec<_> = requests.iter().filter(|r| r.max_tokens > 0).collect();
    assert_eq!(chats.len(), 2);
    assert!(chats[1].messages.iter().any(|m| m
        .content
        .as_deref()
        .is_some_and(|s| s.contains("ordinary external clarification"))));
    assert!(!chats[1].messages.iter().any(|m| m
        .content
        .as_deref()
        .is_some_and(|s| s.contains("must not reach a later TUI turn"))));
}

fn main() {
    ordinary_tui_turn_advances_attachment_revision();
    println!("ordinary TUI turns and external controls share revision history ... ok");
    attachment_rejects_agent_workspace();
    println!("attachment authority stays outside the agent workspace ... ok");
    shared_tui_and_external_steering();
    println!("existing TUI and external steering share one core session ... ok");
    pause_resume_redirect_and_rebind(false);
    println!("external pause/resume and TUI session swap retain honest accounting ... ok");
    pause_resume_redirect_and_rebind(true);
    println!("TUI Esc pauses autonomous goal progression until explicit resume ... ok");
    for kind in ["time", "tokens", "cancel", "exit"] {
        bounded_goal_or_tui_exit(kind);
        println!("shared TUI goal {kind} stops owned work without follow-on inference ... ok");
    }
}
