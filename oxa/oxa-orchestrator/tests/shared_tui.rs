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
        atomic::{AtomicBool, AtomicU64, Ordering},
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
    let sandbox: Arc<dyn Sandbox> = if std::env::var("OXA_BACKGROUND_TASKS").as_deref() == Ok("1") {
        Arc::new(oxa_sandbox::NetSandbox::new(
            Arc::new(oxa_sandbox::Bwrap {
                bwrap: "/usr/bin/bwrap".into(),
                home: std::env::var_os("HOME").unwrap().into(),
                layout: oxa_sandbox::Layout {
                    agent_dir: Some(Dirs::under(root).agent_dir),
                    ..Default::default()
                },
            }),
            true,
            true,
        ))
    } else {
        Arc::new(NoCommands)
    };
    let jobs = Arc::new(oxa_tools::JobRegistry::new());
    jobs.set_sandbox(sandbox.clone());
    let deps = Deps {
        jobs,
        engine,
        store,
        tools: vec![],
        hooks: vec![],
        policy: Box::new(Allow),
        sandbox,
        settings: Settings::default(),
        dirs: Dirs::under(root),
        cwd: work.into(),
        selection: ToolSelection::default(),
        resources: Resources::default(),
        model: None,
        thinking: None,
        mode: None,
        session_id: None,
    };
    oxa_orchestrator::background::augment(deps)
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
        reopen: Box::new(move |how, current| {
            let open = match how {
                Reopen::New => Open::New,
                Reopen::Resume(s) => Open::Resume(s),
                Reopen::Fork(s) => Open::Fork(s),
            };
            let mut config = deps(&url, &root, &work, reopened_store.clone());
            if let Some((model, thinking)) = current {
                config.model = Some(model);
                config.thinking = Some(thinking);
            }
            Session::open(config, open)
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
    let directory = Dirs::under(env.root()).agent_dir.join("control");
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
    let directory = Dirs::under(env.root()).agent_dir.join("control");
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
    let directory = Dirs::under(env.root()).agent_dir.join("control");
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
    for directory in [
        env.work().join("agent-readable-control"),
        env.root().join("outside-protected-agent-dir"),
    ] {
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
    }
    assert!(!engine.requests().iter().any(|r| r.max_tokens > 0));
    let real = env.root().join("real-agent");
    std::fs::create_dir(&real).unwrap();
    let alias = env.root().join("agent-alias");
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    let store: Arc<dyn SessionStore> = Arc::new(FsStore::new(Dirs::under(env.root()).sessions_dir));
    let mut config = deps(&engine.url(), env.root(), &env.work(), store);
    config.dirs.agent_dir = alias;
    let alias_session = Session::open(config, Open::New).unwrap();
    let directory = real.join("control");
    std::env::set_var("OXA_SESSION_CONTROL_DIRECTORY", &directory);
    let attachment = oxa_orchestrator::tui::attach(&alias_session, |_| {});
    std::env::remove_var("OXA_SESSION_CONTROL_DIRECTORY");
    assert!(attachment.is_none());
    assert!(!directory.exists());
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
    let directory = Dirs::under(env.root()).agent_dir.join("control");
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

fn model_background_task_wakes_same_session(goal_mode: bool, live: bool) {
    let env = TestEnv::new();
    let directory = Dirs::under(env.root()).agent_dir.join("control");
    let socket = directory.join("session.sock");
    std::env::set_var("OXA_SESSION_CONTROL_DIRECTORY", &directory);
    std::env::set_var("OXA_BACKGROUND_TASKS", "1");
    let interim = if goal_mode {
        r#"{"state":"done","summary":"other work finished before CI","evidence":["CI is still running"]}"#
    } else {
        "background launched; other useful work finished"
    };
    let final_text = if goal_mode {
        r#"{"state":"done","summary":"background-final","evidence":["ci-command-finished checked"]}"#
    } else {
        "background-final"
    };
    let start_args=json!({"operation":"start","command":if live {"sleep 0.1; printf ci-command-finished"}else{"sleep 1; printf ci-command-finished"},"timeout_s":3600}).to_string();
    let mut script = vec![
        ScriptTurn::tool("launch", "background_command", &start_args),
        ScriptTurn::text(interim),
    ];
    if !goal_mode {
        script.push(ScriptTurn::tool(
            "collect",
            "background_command",
            r#"{"operation":"list"}"#,
        ));
    }
    script.push(ScriptTurn::text(final_text));
    let engine = Arc::new(
        FakeServer::start(
            script,
            FakeOpts {
                hold_headers_ms: if live { 200 } else { 0 },
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
    let done = Arc::new(AtomicBool::new(false));
    let peer_done = done.clone();
    let peer_socket = socket.clone();
    let peer_engine = engine.clone();
    let expected_peer = expected.clone();
    let peer = std::thread::spawn(move || {
        let initial = wait_status(&peer_socket, |v| v["status"]["session"]["id"] == expected_peer);
        if goal_mode {
            assert_eq!(
                request(
                    &peer_socket,
                    json!({"id":"goal","method":"set_goal","revision":revision(&initial),"text":"Run CI in background, finish other work, then verify its result","budget":{"wall_ms":8000,"tokens":200000,"turns":8}})
                )["queued"],
                true
            );
            let paused = wait_status(&peer_socket, |v| {
                v["status"]["goal"]["goal"]
                    == "Run CI in background, finish other work, then verify its result"
            });
            mutation(&peer_socket, "run", "resume", revision(&paused), None);
        }
        let working = wait_status(&peer_socket, |v| {
            v["status"]["tasks"][0]["process_stopped"] == false
                && peer_engine.requests().iter().filter(|r| r.max_tokens > 0).count() >= 2
        });
        assert_ne!(working["status"]["tasks"][0]["state"], "completed");
        if goal_mode {
            let waiting = wait_status(&peer_socket, |v| state(v) == "blocked");
            assert_eq!(waiting["status"]["goal"]["reason"], "waiting_background_tasks");
            assert_eq!(
                waiting["status"]["goal"]["turns_started"], 1,
                "pending commands prevent premature DONE"
            );
        }
        let finished = wait_status(&peer_socket, |v| {
            v["status"]["tasks"][0]["process_stopped"] == true
                && v["status"]["session"]["busy"] == false
                && peer_engine.requests().iter().filter(|r| r.max_tokens > 0).count()
                    == if goal_mode { 3 } else { 4 }
                && (!goal_mode || state(v) == "done")
        });
        assert_eq!(finished["status"]["tasks"][0]["state"], "completed");
        assert_eq!(finished["status"]["tasks"][0]["exit"], 0);
        let pid = finished["status"]["tasks"][0]["process_id"].as_u64().unwrap();
        assert!(!Path::new(&format!("/proc/{pid}")).exists());
        if goal_mode {
            assert_eq!(finished["status"]["goal"]["turns_started"], 2);
            assert!(finished["status"]["goal"]["reported_tokens"].as_u64().unwrap() > 0);
        }
        let mut after = 0;
        let mut wake = false;
        let mut wake_busy = false;
        loop {
            let page = request(
                &peer_socket,
                json!({"id":"wake-events","method":"events","after":after}),
            );
            assert_eq!(page["gap"], false);
            for item in page["events"].as_array().unwrap() {
                let item = &item["event"];
                wake_busy |= item["kind"] == "background_completed" && item["session"]["busy"] == true;
                wake |= item["kind"] == "background_completed"
                    && item["reply"]["text"]
                        .as_str()
                        .is_some_and(|s| s.contains("ci-command-finished"));
            }
            after = page["next"].as_u64().unwrap();
            if after == page["latest"].as_u64().unwrap() {
                break;
            }
        }
        assert!(wake, "completion must be an observable event");
        if live {
            assert!(wake_busy, "live completion must reach the running session");
        }
        peer_done.store(true, Ordering::Release);
    });
    let mut terminal = LiveTerm {
        inner: TestTerminal::new(120, 30),
        done,
        typing: Arc::new(AtomicBool::new(false)),
        typed: Arc::new(AtomicBool::new(false)),
        deadline: Instant::now() + Duration::from_secs(12),
        quit: false,
        final_text: "background-final",
        escape: false,
        swap_when: None,
        swapped: false,
        rejected: Arc::new(AtomicBool::new(false)),
    };
    let mut options = opts(&env, &engine.url(), store);
    if !goal_mode {
        options.initial_prompt = Some("Start a background command and keep working".into());
    }
    let (code, last) = run_session(&mut terminal, session, &mut options);
    peer.join().unwrap();
    std::env::remove_var("OXA_SESSION_CONTROL_DIRECTORY");
    std::env::remove_var("OXA_BACKGROUND_TASKS");
    assert_eq!(code, 0);
    assert_eq!(last.unwrap().id, expected);
    assert!(!handle.is_busy());
    assert!(!socket.exists());
    let screen = strip_ansi(&String::from_utf8_lossy(&terminal.inner.out));
    assert!(
        screen.contains("background-final")
            && screen.contains("Background background-")
            && screen.contains("completed; exit Some(0)")
    );
    let requests = engine.requests();
    let chats: Vec<_> = requests.iter().filter(|r| r.max_tokens > 0).collect();
    assert_eq!(chats.len(), if goal_mode { 3 } else { 4 });
    assert!(chats.iter().all(|r| r.session == expected));
    assert!(chats[2].messages.iter().any(|m| m
        .content
        .as_deref()
        .is_some_and(|s| s.contains("untrusted data") && s.contains("ci-command-finished"))));
    assert!(chats.last().unwrap().messages.iter().any(|m| m
        .content
        .as_deref()
        .is_some_and(|s| s.contains("ci-command-finished"))));
}

struct DenyCommands;
impl Policy for DenyCommands {
    fn decide(&self, _: Mode, access: &Access, _: &Path) -> Decision {
        if matches!(access, Access::Exec { .. }) {
            Decision::Deny("fixture denies execution")
        } else {
            Decision::Allow
        }
    }
    fn remember(&mut self, _: &Access) {}
}
fn background_permission_boundaries(kind: &str) {
    let env = TestEnv::new();
    let directory = Dirs::under(env.root()).agent_dir.join("control");
    let socket = directory.join("session.sock");
    std::env::set_var("OXA_SESSION_CONTROL_DIRECTORY", &directory);
    std::env::set_var("OXA_BACKGROUND_TASKS", "1");
    let args = if kind == "control_hidden" {
        json!({"operation":"start","command":format!("test ! -S '{}' && printf control-hidden",socket.display()),"wake":false}).to_string()
    } else if kind == "bad_args" {
        r#"{"operation":"start","command":"printf forbidden > forbidden.txt","net":true,"wake":false}"#
            .to_owned()
    } else {
        r#"{"operation":"start","command":"printf forbidden > forbidden.txt","wake":false}"#.to_owned()
    };
    let engine = Arc::new(
        FakeServer::start(
            vec![
                ScriptTurn::tool("guarded", "background_command", &args),
                ScriptTurn::text("policy-final"),
            ],
            FakeOpts::default(),
        )
        .unwrap(),
    );
    let store: Arc<dyn SessionStore> = Arc::new(FsStore::new(Dirs::under(env.root()).sessions_dir));
    let mut config = deps(&engine.url(), env.root(), &env.work(), store.clone());
    if kind == "deny" {
        config.policy = Box::new(DenyCommands);
    }
    if kind == "no_tools" {
        config.selection.no_tools = true;
    }
    if kind == "plan" {
        config.mode = Some(Mode::Plan);
    }
    let session = Session::open(config, Open::New).unwrap();
    let handle = session.handle();
    let done = Arc::new(AtomicBool::new(false));
    let peer_done = done.clone();
    let peer_socket = socket.clone();
    let peer_engine = engine.clone();
    let plan = kind == "plan";
    let hidden = kind == "control_hidden";
    let peer = std::thread::spawn(move || {
        let status = wait_status(&peer_socket, |v| {
            v["status"]["session"]["busy"] == false
                && peer_engine.requests().iter().filter(|r| r.max_tokens > 0).count() == 2
                && (!(plan || hidden) || v["status"]["tasks"][0]["process_stopped"] == true)
        });
        if plan {
            assert_eq!(status["status"]["tasks"][0]["state"], "failed");
            assert_ne!(status["status"]["tasks"][0]["exit"], 0);
        } else if hidden {
            assert_eq!(status["status"]["tasks"][0]["state"], "completed");
            assert_eq!(status["status"]["tasks"][0]["exit"], 0);
            assert!(
                peer_socket.exists(),
                "operator socket must stay live outside the sandbox"
            );
        } else {
            assert!(status["status"]["tasks"].as_array().unwrap().is_empty());
        }
        peer_done.store(true, Ordering::Release);
    });
    let mut terminal = LiveTerm {
        inner: TestTerminal::new(120, 30),
        done,
        typing: Arc::new(AtomicBool::new(false)),
        typed: Arc::new(AtomicBool::new(false)),
        deadline: Instant::now() + Duration::from_secs(8),
        quit: false,
        final_text: "policy-final",
        escape: false,
        swap_when: None,
        swapped: false,
        rejected: Arc::new(AtomicBool::new(false)),
    };
    let mut options = opts(&env, &engine.url(), store);
    options.initial_prompt = Some("Verify command admission boundaries".into());
    let (code, _) = run_session(&mut terminal, session, &mut options);
    peer.join().unwrap();
    std::env::remove_var("OXA_SESSION_CONTROL_DIRECTORY");
    std::env::remove_var("OXA_BACKGROUND_TASKS");
    assert_eq!(code, 0);
    assert!(!handle.is_busy());
    assert!(!socket.exists());
    assert!(!env.work().join("forbidden.txt").exists());
    let requests = engine.requests();
    let chats: Vec<_> = requests.iter().filter(|r| r.max_tokens > 0).collect();
    assert_eq!(chats.len(), 2);
    if !plan && !hidden {
        assert!(chats[1].messages.iter().any(|m| m.role == oxa_proto::Role::Tool
            && m.content
                .as_deref()
                .is_some_and(|s| s.contains("denied") || s.contains("not found") || s.contains("invalid"))));
    }
}
fn background_owned_cleanup(kind: &str) {
    let env = TestEnv::new();
    let directory = Dirs::under(env.root()).agent_dir.join("control");
    let socket = directory.join("session.sock");
    std::env::set_var("OXA_SESSION_CONTROL_DIRECTORY", &directory);
    std::env::set_var("OXA_BACKGROUND_TASKS", "1");
    let settlement = kind == "settlement";
    let admission = kind == "admission";
    let goal_mode = matches!(kind, "cancel" | "deadline" | "pause" | "settlement" | "admission");
    let reply = if settlement {
        r#"{"state":"continue","summary":"task still running"}"#
    } else if goal_mode {
        r#"{"state":"blocked","reason":"waiting_background_tasks"}"#
    } else {
        "cleanup-ready"
    };
    let engine = Arc::new(
        FakeServer::start(
            vec![
                ScriptTurn::tool(
                    "long-task",
                    "background_command",
                    r#"{"operation":"start","command":"sleep 5; printf leaked > leaked.txt","timeout_s":10}"#,
                ),
                ScriptTurn::text(reply),
            ],
            FakeOpts::default(),
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
    let done = Arc::new(AtomicBool::new(false));
    let swap = Arc::new(AtomicBool::new(false));
    let pid = Arc::new(AtomicU64::new(0));
    let peer_done = done.clone();
    let peer_swap = swap.clone();
    let peer_pid = pid.clone();
    let peer_socket = socket.clone();
    let peer_engine = engine.clone();
    let expected_peer = expected.clone();
    let mode = kind.to_owned();
    let peer = std::thread::spawn(move || {
        if goal_mode {
            let status = wait_status(&peer_socket, |v| v["status"]["session"]["id"] == expected_peer);
            assert_eq!(
                request(
                    &peer_socket,
                    json!({"id":"owned-goal","method":"set_goal","revision":revision(&status),"text":"Run and await background work","budget":{"wall_ms":if mode=="deadline"{1000}else{8000},"tokens":null,"turns":if settlement || admission {1}else{4}}})
                )["queued"],
                true
            );
            let paused = wait_status(&peer_socket, |v| {
                v["status"]["goal"]["goal"] == "Run and await background work"
            });
            mutation(&peer_socket, "run", "resume", revision(&paused), None);
        }
        let working = wait_status(&peer_socket, |v| {
            (settlement || v["status"]["tasks"][0]["state"] == "running")
                && v["status"]["tasks"][0]["process_id"].as_u64().is_some()
                && v["status"]["session"]["busy"] == false
                && peer_engine.requests().iter().filter(|r| r.max_tokens > 0).count() == 2
        });
        let child = working["status"]["tasks"][0]["process_id"].as_u64().unwrap();
        peer_pid.store(child, Ordering::Release);
        if mode == "swap" {
            peer_swap.store(true, Ordering::Release);
            let new = wait_status(&peer_socket, |v| {
                v["status"]["session"]["id"]
                    .as_str()
                    .is_some_and(|s| s != expected_peer)
            });
            assert!(new["status"]["tasks"].as_array().unwrap().is_empty());
            assert!(!Path::new(&format!("/proc/{child}")).exists());
        } else if goal_mode {
            if admission {
                assert_eq!(state(&working), "blocked");
                assert_eq!(working["status"]["goal"]["reason"], "waiting_background_tasks");
                assert_eq!(working["status"]["tasks"][0]["process_stopped"], false);
                mutation(&peer_socket, "retry", "resume", revision(&working), None);
            } else if mode != "deadline" && !settlement {
                mutation(
                    &peer_socket,
                    "stop",
                    if mode == "pause" { "pause" } else { "cancel" },
                    revision(&working),
                    None,
                );
            }
            let stopped = wait_status(&peer_socket, |v| {
                state(v)
                    == if mode == "deadline" || settlement || admission {
                        "budget_exhausted"
                    } else if mode == "pause" {
                        "paused"
                    } else {
                        "canceled"
                    }
                    && v["status"]["tasks"][0]["process_stopped"] == true
            });
            assert_eq!(stopped["status"]["tasks"][0]["state"], "canceled");
            assert!(!Path::new(&format!("/proc/{child}")).exists());
            std::thread::sleep(Duration::from_millis(50));
            assert_eq!(
                peer_engine.requests().iter().filter(|r| r.max_tokens > 0).count(),
                2,
                "cancel/deadline must not trigger new inference"
            );
        }
        peer_done.store(true, Ordering::Release);
    });
    let mut terminal = LiveTerm {
        inner: TestTerminal::new(120, 30),
        done,
        typing: Arc::new(AtomicBool::new(false)),
        typed: Arc::new(AtomicBool::new(false)),
        deadline: Instant::now() + Duration::from_secs(8),
        quit: false,
        final_text: if goal_mode { "" } else { "cleanup-ready" },
        escape: false,
        swap_when: if kind == "swap" { Some(swap) } else { None },
        swapped: false,
        rejected: Arc::new(AtomicBool::new(false)),
    };
    let mut options = opts(&env, &engine.url(), store);
    if !goal_mode {
        options.initial_prompt = Some("Start owned background work".into());
    }
    let began = Instant::now();
    let (code, _) = run_session(&mut terminal, session, &mut options);
    peer.join().unwrap();
    std::env::remove_var("OXA_SESSION_CONTROL_DIRECTORY");
    std::env::remove_var("OXA_BACKGROUND_TASKS");
    assert_eq!(code, 0);
    assert!(!handle.is_busy());
    assert!(!socket.exists());
    assert!(began.elapsed() < Duration::from_secs(2));
    assert!(!Path::new(&format!("/proc/{}", pid.load(Ordering::Acquire))).exists());
    assert!(!env.work().join("leaked.txt").exists());
    assert_eq!(engine.requests().iter().filter(|r| r.max_tokens > 0).count(), 2);
}

struct WorkerDriver {
    inner: oxa_fake::ScriptedEngine,
    step: AtomicU64,
    author: Arc<FakeServer>,
    recovering: bool,
    before_retarget: std::sync::Mutex<Option<Value>>,
}
impl Engine for WorkerDriver {
    fn info(&self, target: Option<&str>) -> Result<oxa_proto::EngineInfo, OxaError> {
        self.inner.info(target)
    }
    fn count_tokens(&self, text: &str) -> Result<u32, OxaError> {
        self.inner.count_tokens(text)
    }
    fn chat(
        &self,
        r: &oxa_proto::ChatRequest,
        cancel: &CancelToken,
        on: &mut dyn FnMut(oxa_proto::StreamEvent),
    ) -> Result<oxa_proto::AssistantTurn, OxaError> {
        if r.max_tokens > 0 {
            let step = self.step.fetch_add(1, Ordering::AcqRel);
            let author = || {
                r.messages
                    .iter()
                    .rev()
                    .filter_map(|m| {
                        m.content
                            .as_deref()
                            .and_then(|s| serde_json::from_str::<Value>(s).ok())
                    })
                    .find(|v| v["profile"] == "author")
                    .expect("author admission or status")
            };
            let turn = match step {
                0 => ScriptTurn::tool(
                    "author",
                    "worker_lane",
                    r#"{"operation":"start","profile":"author","goal":"Write a synthetic change"}"#,
                ),
                1 => ScriptTurn::tool(
                    "reviewer",
                    "worker_lane",
                    r#"{"operation":"start","profile":"reviewer","goal":"Review a synthetic change"}"#,
                ),
                2 if self.recovering => ScriptTurn::text(
                    r#"{"state":"done","summary":"premature","evidence":["workers still running"]}"#,
                ),
                2 => {
                    let deadline = Instant::now() + Duration::from_secs(3);
                    while !self
                        .author
                        .requests()
                        .iter()
                        .any(|r| r.max_tokens > 0 && (!self.recovering || r.reply.is_some()))
                    {
                        assert!(Instant::now() < deadline);
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    ScriptTurn::tool(
                        "inspect-author",
                        "worker_lane",
                        &json!({"operation":"status","worker_id":author()["worker"]["id"]}).to_string(),
                    )
                }
                3 if self.recovering => ScriptTurn::tool(
                    "inspect-blocked",
                    "worker_lane",
                    &json!({"operation":"status","worker_id":author()["worker"]["id"]}).to_string(),
                ),
                4 if self.recovering => {
                    let worker = author();
                    assert_eq!(
                        worker["goal"]["state"], "blocked",
                        "worker event precedes recovery"
                    );
                    *self.before_retarget.lock().unwrap() = Some(worker["goal"].clone());
                    ScriptTurn::tool("clarify-author","worker_lane",&json!({"operation":"retarget","worker_id":worker["worker"]["id"],"revision":worker["goal"]["revision"],"goal":"Write the synthetic change with clarified-worker-goal"}).to_string())
                }
                5 if self.recovering => {
                    std::thread::sleep(Duration::from_millis(100));
                    ScriptTurn::tool(
                        "inspect-redirect",
                        "worker_lane",
                        &json!({"operation":"status","worker_id":author()["worker"]["id"]}).to_string(),
                    )
                }
                6 if self.recovering => {
                    let worker = author();
                    if worker["goal"]["state"] != "paused" {
                        self.step.store(5, Ordering::Release);
                        std::thread::sleep(Duration::from_millis(100));
                        ScriptTurn::tool(
                            "wait-redirect",
                            "worker_lane",
                            &json!({"operation":"status","worker_id":worker["worker"]["id"]}).to_string(),
                        )
                    } else {
                        ScriptTurn::tool("resume-author","worker_lane",&json!({"operation":"resume","worker_id":worker["worker"]["id"],"revision":worker["goal"]["revision"]}).to_string())
                    }
                }
                7 if self.recovering => ScriptTurn::text(
                    r#"{"state":"done","summary":"premature","evidence":["workers still running"]}"#,
                ),
                3 => {
                    let worker = author();
                    ScriptTurn::tool("steer-author","worker_lane",&json!({"operation":"steer","worker_id":worker["worker"]["id"],"revision":worker["goal"]["revision"],"text":"model-live-worker-clarification"}).to_string())
                }
                4 => ScriptTurn::text(
                    r#"{"state":"done","summary":"premature","evidence":["workers still running"]}"#,
                ),
                _ => ScriptTurn::text(
                    r#"{"state":"done","summary":"lanes-final","evidence":["worker results checked"]}"#,
                ),
            };
            self.inner.push(turn);
        }
        self.inner.chat(r, cancel, on)
    }
}

fn model_worker_lanes_wake_same_session(steering: bool, cancellation: &str, recovering: bool, gateway: bool) {
    let canceling = cancellation != "none";
    let aborting = cancellation == "escape";
    let unscoped = cancellation == "idle_unscoped";
    let idle_escape = cancellation == "idle_escape" || unscoped;
    use std::os::unix::fs::PermissionsExt;
    let env = TestEnv::new();
    let agent = Dirs::under(env.root()).agent_dir;
    std::fs::create_dir_all(&agent).unwrap();
    let directory = agent.join("control");
    let socket = directory.join("session.sock");
    std::env::set_var("OXA_SESSION_CONTROL_DIRECTORY", &directory);
    let child_script = || {
        vec![
            ScriptTurn::text(r#"{"state":"continue","summary":"first worker step"}"#),
            ScriptTurn::text(
                r#"{"state":"done","summary":"worker-finished","evidence":["two steps checked"]}"#,
            ),
        ]
    };
    let mut author_script = child_script();
    if recovering {
        author_script[0] = ScriptTurn::text(r#"{"state":"blocked","reason":"need-worker-clarification"}"#);
    }
    let author = Arc::new(
        FakeServer::start(
            author_script,
            FakeOpts {
                model: "author-model".into(),
                hold_headers_ms: if aborting || idle_escape || (gateway && canceling) {
                    1500
                } else if steering {
                    600
                } else {
                    250
                },
                ..Default::default()
            },
        )
        .unwrap(),
    );
    let reviewer = Arc::new(
        FakeServer::start(
            child_script(),
            FakeOpts {
                model: "reviewer-model".into(),
                hold_headers_ms: if aborting || idle_escape || (gateway && canceling) {
                    1500
                } else if recovering {
                    900
                } else {
                    350
                },
                ..Default::default()
            },
        )
        .unwrap(),
    );
    let profiles = agent.join("workers.json");
    let profile = |name: &str, role: &str, server: &FakeServer, model: &str| json!({"name":name,"role":if gateway {format!("{role} [gateway-test]")} else {role.to_string()},"route":if gateway {"cloud_gateway"} else {"local"},"engine":server.url().trim_start_matches("http://"),"model":model,"budget":{"wall_ms":8000,"tokens":100000,"turns":8},"turn_ms":2000,"output_bytes":1048576});
    std::fs::write(&profiles,json!({"version":if gateway {2} else {1},"verified_local_only":!gateway,"allow_cloud_workers":gateway,"parallel":2,"total_tokens":200000,"total_output_bytes":2097152,"profiles":[profile("author","fixed author role",&author,"author-model"),profile("reviewer","fixed reviewer role",&reviewer,"reviewer-model")]}).to_string()).unwrap();
    std::fs::set_permissions(&profiles, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::env::set_var("OXA_WORKER_PROFILES", &profiles);
    let final_text = r#"{"state":"done","summary":"lanes-final","evidence":["worker results checked"]}"#;
    let mut script = vec![
        ScriptTurn::tool(
            "author",
            "worker_lane",
            r#"{"operation":"start","profile":"author","goal":"Write a synthetic change"}"#,
        ),
        ScriptTurn::tool(
            "reviewer",
            "worker_lane",
            r#"{"operation":"start","profile":"reviewer","goal":"Review a synthetic change"}"#,
        ),
        ScriptTurn::text(r#"{"state":"done","summary":"premature","evidence":["workers still running"]}"#),
    ];
    for _ in 0..4 {
        script.push(ScriptTurn::text(final_text));
    }
    let engine = Arc::new(
        FakeServer::start(
            script,
            FakeOpts {
                hold_headers_ms: if aborting { 250 } else { 0 },
                ..Default::default()
            },
        )
        .unwrap(),
    );
    let store: Arc<dyn SessionStore> = Arc::new(FsStore::new(Dirs::under(env.root()).sessions_dir));
    let driver = Arc::new(WorkerDriver {
        inner: oxa_fake::ScriptedEngine::new(vec![]),
        step: AtomicU64::new(0),
        author: author.clone(),
        recovering,
        before_retarget: std::sync::Mutex::new(None),
    });
    let mut config = deps(&engine.url(), env.root(), &env.work(), store.clone());
    if steering {
        config.engine = driver.clone();
    }
    let session = Session::open(config, Open::New).unwrap();
    let expected = session.state().session_id;
    let main_handle = session.handle();
    let done = Arc::new(AtomicBool::new(false));
    let peer_done = done.clone();
    let escape_now = Arc::new(AtomicBool::new(false));
    let peer_escape = escape_now.clone();
    let peer_socket = socket.clone();
    let expected_peer = expected.clone();
    let peer_driver = driver.clone();
    let peer_author = author.clone();
    let peer_reviewer = reviewer.clone();
    let peer_agent = agent.clone();
    let peer = std::thread::spawn(move || {
        let initial = wait_status(&peer_socket, |v| v["status"]["session"]["id"] == expected_peer);
        if !unscoped {
            request(
                &peer_socket,
                json!({"id":"goal","method":"set_goal","revision":revision(&initial),"text":"Start author and reviewer, then collect both results","budget":{"wall_ms":12000,"tokens":200000,"turns":8}}),
            );
            let paused = wait_status(&peer_socket, |v| {
                v["status"]["goal"]["goal"] == "Start author and reviewer, then collect both results"
            });
            mutation(&peer_socket, "run", "resume", revision(&paused), None);
        }
        let waiting = wait_status(&peer_socket, |v| {
            state(v)
                == if unscoped {
                    "paused"
                } else if aborting {
                    "active"
                } else {
                    "blocked"
                }
                && (!(idle_escape || (gateway && canceling))
                    || (peer_author.requests().iter().any(|r| r.max_tokens > 0)
                        && peer_reviewer.requests().iter().any(|r| r.max_tokens > 0)))
                && (!canceling
                    || v["status"]["workers"].as_array().is_some_and(|jobs| {
                        jobs.len() == 2
                            && jobs
                                .iter()
                                .all(|j| j["goal"]["turns_started"].as_u64().is_some_and(|n| n >= 1))
                    }))
        });
        if !aborting && !unscoped {
            assert_eq!(waiting["status"]["goal"]["reason"], "waiting_workers");
        }
        if !unscoped {
            assert_eq!(waiting["status"]["goal"]["turns_started"], 1);
        }
        if aborting {
            main_handle.abort();
        } else if idle_escape {
            assert_eq!(waiting["status"]["session"]["busy"], false);
            assert!(!oxa_orchestrator::tui::interrupt_idle(
                "unrelated-session",
                &peer_agent
            ));
            assert!(!oxa_orchestrator::tui::interrupt_idle(
                &expected_peer,
                peer_agent.parent().unwrap()
            ));
            peer_escape.store(true, Ordering::Release);
        } else if canceling {
            mutation(&peer_socket, "cancel-workers", "cancel", revision(&waiting), None);
        }
        let finished = wait_status(&peer_socket, |v| {
            state(v)
                == if aborting {
                    "blocked"
                } else if idle_escape {
                    "paused"
                } else if canceling {
                    "canceled"
                } else {
                    "done"
                }
                && v["status"]["workers"].as_array().is_some_and(|jobs| {
                    jobs.len() == 2
                        && jobs.iter().all(|j| {
                            j["state"] == if canceling { "canceled" } else { "completed" }
                                && j["process_stopped"] == true
                        })
                })
        });
        assert_eq!(finished["status"]["session"]["id"], expected_peer);
        if idle_escape {
            for field in ["budget", "turns_started", "reported_tokens"] {
                assert_eq!(
                    finished["status"]["goal"][field],
                    waiting["status"]["goal"][field]
                );
            }
        }
        if aborting {
            assert_eq!(
                finished["status"]["goal"]["reason"],
                "token_accounting_unavailable"
            );
            assert!(finished["status"]["goal"]["reported_tokens"].is_null());
        }
        let jobs = finished["status"]["workers"].as_array().unwrap();
        assert!(jobs.iter().any(|j| j["model"] == "author-model"));
        assert!(jobs.iter().any(|j| j["model"] == "reviewer-model"));
        for job in jobs {
            if !canceling {
                let source = if job["profile"] == "author" {
                    &peer_author
                } else {
                    &peer_reviewer
                };
                let expected: u64 = source
                    .requests()
                    .iter()
                    .map(|r| {
                        let usage = r.usage.as_ref().unwrap();
                        u64::from(usage.prompt) + u64::from(usage.completion)
                    })
                    .sum();
                assert_eq!(
                    job["goal"]["reported_tokens"].as_u64().unwrap(),
                    expected,
                    "all SDK calls, including zero-output prefill, belong to the ledger"
                );
            }
            assert_eq!(job["route"], if gateway { "cloud_gateway" } else { "local" });
            if recovering && job["profile"] == "author" {
                let baseline = peer_driver.before_retarget.lock().unwrap();
                let before = baseline.as_ref().unwrap();
                assert_eq!(job["goal"]["id"], before["id"]);
                assert_eq!(job["goal"]["budget"], before["budget"]);
                assert_eq!(
                    job["goal"]["goal"],
                    "Write the synthetic change with clarified-worker-goal"
                );
                assert_eq!(job["goal"]["turns_started"], 2);
                let expected_tokens: u64 = peer_driver
                    .author
                    .requests()
                    .iter()
                    .filter(|r| r.max_tokens > 0)
                    .map(|r| {
                        let usage = r.usage.as_ref().unwrap();
                        u64::from(usage.prompt) + u64::from(usage.completion)
                    })
                    .sum();
                assert_eq!(job["goal"]["reported_tokens"].as_u64().unwrap(), expected_tokens);
                assert!(
                    job["goal"]["reported_tokens"].as_u64().unwrap()
                        > before["reported_tokens"].as_u64().unwrap()
                );
                assert!(
                    job["goal"]["elapsed_ms"].as_u64().unwrap()
                        >= before["elapsed_ms"].as_u64().unwrap() + 500
                );
            }
            let pid = job["process_id"].as_u64().unwrap();
            assert!(
                !Path::new(&format!("/proc/{pid}")).exists(),
                "worker reaped before completion"
            );
            if canceling {
                assert!(job["goal"]["turns_started"].as_u64().unwrap() <= 1);
                continue;
            }
            assert!(job["goal"]["turns_started"]
                .as_u64()
                .is_some_and(|n| (1..=2).contains(&n)));
            if steering && job["profile"] == "author" {
                assert_eq!(job["last_control"]["kind"], "control_admitted");
            }
        }
        peer_done.store(true, Ordering::Release);
    });
    let mut terminal = LiveTerm {
        inner: TestTerminal::new(160, 35),
        done,
        typing: escape_now,
        typed: Arc::new(AtomicBool::new(false)),
        deadline: Instant::now() + Duration::from_secs(18),
        quit: false,
        final_text: if aborting {
            "Worker"
        } else if canceling {
            "premature"
        } else {
            "lanes-final"
        },
        escape: idle_escape,
        swap_when: None,
        swapped: false,
        rejected: Arc::new(AtomicBool::new(false)),
    };
    let mut options = opts(&env, &engine.url(), store);
    if unscoped {
        options.initial_prompt = Some("Start an author and reviewer".into());
    }
    let (code, last) = run_session(&mut terminal, session, &mut options);
    peer.join().unwrap();
    std::env::remove_var("OXA_SESSION_CONTROL_DIRECTORY");
    std::env::remove_var("OXA_WORKER_PROFILES");
    assert_eq!(code, 0);
    assert_eq!(last.unwrap().id, expected);
    for (server, role, model) in [
        (&author, "fixed author role", "author-model"),
        (&reviewer, "fixed reviewer role", "reviewer-model"),
    ] {
        let requests = server.requests();
        let chats = requests.iter().filter(|r| r.max_tokens > 0).collect::<Vec<_>>();
        if canceling {
            if gateway || idle_escape {
                let deadline = Instant::now() + Duration::from_secs(3);
                loop {
                    let _ = server.in_flight();
                    if server
                        .timings()
                        .iter()
                        .any(|t| t.max_tokens.is_some_and(|n| n > 0) && t.client_closed)
                    {
                        break;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "cancel must close the worker inference connection"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            assert!(chats.len() <= 1, "cancel prevents follow-on inference");
            continue;
        }
        assert_eq!(chats.len(), 2, "worker performs two goal turns");
        assert!(chats.iter().all(|r| r.model == model));
        assert!(chats.iter().all(|r| r.system.contains(role)));
        assert!(chats[1]
            .messages
            .iter()
            .any(|m| m.content.as_deref().is_some_and(|text| text.contains(
                if recovering && model == "author-model" {
                    "need-worker-clarification"
                } else {
                    "first worker step"
                }
            ))));
        assert_eq!(chats[0].session, chats[1].session);
        if steering && model == "author-model" {
            assert!(chats[1]
                .messages
                .iter()
                .any(
                    |m| m.content.as_deref().is_some_and(|s| s.contains(if recovering {
                        "clarified-worker-goal"
                    } else {
                        "model-live-worker-clarification"
                    }))
                ));
        }
    }
    let requests = if steering {
        driver.inner.requests()
    } else {
        engine.requests()
    };
    let chats = requests.iter().filter(|r| r.max_tokens > 0).collect::<Vec<_>>();
    assert!(chats.iter().all(|r| r.session == expected));
    if !canceling {
        assert!(chats.last().unwrap().messages.iter().any(|m| m
            .content
            .as_deref()
            .is_some_and(|text| text.contains("worker-finished") && text.contains("untrusted data"))));
    }
    if idle_escape {
        assert_eq!(chats.len(), 3, "idle Esc prevents a parent wake/continuation");
        assert!(
            terminal.typed.load(Ordering::Acquire),
            "real TUI Esc input was delivered"
        );
    }
    if !canceling {
        let mut verified = 0;
        for directory in std::fs::read_dir(&agent).unwrap() {
            let directory = directory.unwrap();
            if !directory.file_name().to_string_lossy().starts_with("worker-") {
                continue;
            }
            for line in std::fs::read_to_string(directory.path().join("goal.jsonl"))
                .unwrap()
                .lines()
            {
                let record: Value = serde_json::from_str(line).unwrap();
                if record["event"] == "policy_verified" {
                    verified += 1;
                    assert_eq!(
                        record["details"]["usageMeter"], true,
                        "native CLI must install the engine-level source"
                    );
                    assert_eq!(
                        record["details"]["offline"], !gateway,
                        "audit must match actual worker policy"
                    );
                }
            }
        }
        assert_eq!(verified, 2);
    }
    let screen = strip_ansi(&String::from_utf8_lossy(&terminal.inner.out));
    if gateway {
        assert!(screen.contains("(cloud)"), "paid route visible in footer");
    }
    assert!(
        screen.contains("100000 tokens"),
        "worker allowance shown in footer"
    );
    assert!(
        screen.contains("author-model") && screen.contains("reviewer-model"),
        "worker models visible in TUI"
    );
}

fn offline_parent_refuses_gateway_workers_before_contact() {
    use std::os::unix::fs::PermissionsExt;
    struct RestorePolicy(bool, bool);
    impl Drop for RestorePolicy {
        fn drop(&mut self) {
            oxa_proto::set_net_policy(self.0, self.1);
        }
    }
    let _restore = RestorePolicy(oxa_proto::offline(), oxa_proto::remote_engine_allowed());
    oxa_proto::set_net_policy(true, false);
    let env = TestEnv::new();
    let agent = Dirs::under(env.root()).agent_dir;
    std::fs::create_dir_all(&agent).unwrap();
    let directory = agent.join("control");
    let socket = directory.join("session.sock");
    let gateway = FakeServer::start(vec![], FakeOpts::default()).unwrap();
    let profiles = agent.join("workers.json");
    std::fs::write(&profiles,json!({"version":2,"verified_local_only":false,"allow_cloud_workers":true,"parallel":1,"total_tokens":1000,"total_output_bytes":65536,"profiles":[{"name":"cloud-author","role":"fixed gateway role","route":"cloud_gateway","engine":gateway.url().trim_start_matches("http://"),"model":"fake-model","budget":{"wall_ms":5000,"tokens":1000,"turns":4},"turn_ms":1000,"output_bytes":65536}]}).to_string()).unwrap();
    std::fs::set_permissions(&profiles, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::env::set_var("OXA_SESSION_CONTROL_DIRECTORY", &directory);
    std::env::set_var("OXA_WORKER_PROFILES", &profiles);
    let engine = Arc::new(
        FakeServer::start(
            vec![
                ScriptTurn::tool(
                    "denied-cloud",
                    "worker_lane",
                    r#"{"operation":"start","profile":"cloud-author","goal":"must not contact gateway"}"#,
                ),
                ScriptTurn::text("offline-worker-refused"),
            ],
            FakeOpts::default(),
        )
        .unwrap(),
    );
    let store: Arc<dyn SessionStore> = Arc::new(FsStore::new(Dirs::under(env.root()).sessions_dir));
    let session = Session::open(
        deps(&engine.url(), env.root(), &env.work(), store.clone()),
        Open::New,
    )
    .unwrap();
    let done = Arc::new(AtomicBool::new(false));
    let peer_done = done.clone();
    let peer_engine = engine.clone();
    let peer = std::thread::spawn(move || {
        let status = wait_status(&socket, |v| {
            v["status"]["session"]["busy"] == false
                && peer_engine.requests().iter().filter(|r| r.max_tokens > 0).count() == 2
        });
        assert_eq!(status["status"]["workers"], json!([]));
        peer_done.store(true, Ordering::Release);
    });
    let mut terminal = LiveTerm {
        inner: TestTerminal::new(120, 30),
        done,
        typing: Arc::new(AtomicBool::new(false)),
        typed: Arc::new(AtomicBool::new(false)),
        deadline: Instant::now() + Duration::from_secs(12),
        quit: false,
        final_text: "offline-worker-refused",
        escape: false,
        swap_when: None,
        swapped: false,
        rejected: Arc::new(AtomicBool::new(false)),
    };
    let mut options = opts(&env, &engine.url(), store);
    options.initial_prompt = Some("Try the configured gateway worker".into());
    let (code, _) = run_session(&mut terminal, session, &mut options);
    peer.join().unwrap();
    assert_eq!(code, 0);
    let requests = engine.requests();
    let chats = requests.iter().filter(|r| r.max_tokens > 0).collect::<Vec<_>>();
    assert!(chats[1].messages.iter().any(|m| m
        .content
        .as_deref()
        .is_some_and(|s| s.contains("offline mode refuses cloud worker inference"))));
    assert!(
        gateway.requests().is_empty(),
        "offline refusal precedes any worker gateway generation"
    );
    assert!(
        !std::fs::read_dir(&agent).unwrap().any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("worker-")),
        "no worker audit/runtime created offline"
    );
    std::env::remove_var("OXA_SESSION_CONTROL_DIRECTORY");
    std::env::remove_var("OXA_WORKER_PROFILES");
}

fn worker_goal_compacts_with_metered_usage(limit: u64, failure: bool, canceling: bool) -> u64 {
    println!("worker compaction limit={limit} failure={failure} canceling={canceling}");
    let limited = limit > 0;
    let tokens = if limited { limit } else { 2000000 };
    use std::os::unix::fs::PermissionsExt;
    let env = TestEnv::new();
    let agent = Dirs::under(env.root()).agent_dir;
    std::fs::create_dir_all(&agent).unwrap();
    let directory = agent.join("control");
    let socket = directory.join("session.sock");
    std::env::set_var("OXA_SESSION_CONTROL_DIRECTORY", &directory);
    let continue_turn = r#"{"state":"continue","summary":"one concrete step complete"}"#;
    let done_turn = r#"{"state":"done","summary":"compaction-final","evidence":["three steps verified"]}"#;
    let mut failed_summary = ScriptTurn::text("worker-summary-marker: previous steps retained");
    if canceling {
        failed_summary.fault = Some(oxa_fake::Fault::HoldHeaders { ms: 1500 });
    }
    if failure {
        failed_summary.fault = Some(oxa_fake::Fault::Http {
            status: 400,
            body: "deliberate summary failure".into(),
        });
    }
    let worker = Arc::new(
        FakeServer::start(
            vec![
                ScriptTurn::text(continue_turn),
                ScriptTurn::text(continue_turn),
                failed_summary,
                ScriptTurn::text(done_turn),
                ScriptTurn::text("worker-summary-marker: final steps retained"),
            ],
            FakeOpts {
                model: "compact-model".into(),
                n_ctx: 50000,
                models: vec![oxa_fake::FakeModel {
                    alias: "compact-model".into(),
                    cap: 50000,
                    max_output: Some(1024),
                    caps: Some(oxa_fake::FakeCaps::rust_moe()),
                }],
                ..Default::default()
            },
        )
        .unwrap(),
    );
    let profile = agent.join("workers.json");
    std::fs::write(&profile,json!({"version":1,"verified_local_only":true,"parallel":1,"total_tokens":2000000,"total_output_bytes":4194304,"profiles":[{"name":"long-worker","role":"Retain verified context across compaction","engine":worker.url().trim_start_matches("http://"),"model":"compact-model","auto_compaction":true,"budget":{"wall_ms":12000,"tokens":tokens,"turns":8},"turn_ms":8000,"output_bytes":4194304}]}).to_string()).unwrap();
    std::fs::set_permissions(&profile, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::env::set_var("OXA_WORKER_PROFILES", &profile);
    let args=json!({"operation":"start","profile":"long-worker","goal":format!("Verify all project steps. {}","x".repeat(14000))}).to_string();
    let engine = Arc::new(
        FakeServer::start(
            vec![
                ScriptTurn::tool("long-worker", "worker_lane", &args),
                ScriptTurn::text(r#"{"state":"done","summary":"premature","evidence":["worker running"]}"#),
                ScriptTurn::text(
                    r#"{"state":"done","summary":"compaction-final","evidence":["worker result verified"]}"#,
                ),
            ],
            FakeOpts::default(),
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
    let done = Arc::new(AtomicBool::new(false));
    let peer_done = done.clone();
    let peer_worker = worker.clone();
    let peer = std::thread::spawn(move || {
        let initial = wait_status(&socket, |v| v["status"]["session"]["id"].is_string());
        request(
            &socket,
            json!({"id":"goal","method":"set_goal","revision":revision(&initial),"text":"Run a long worker goal and collect its verified result","budget":{"wall_ms":15000,"tokens":500000,"turns":8}}),
        );
        let paused = wait_status(&socket, |v| {
            v["status"]["goal"]["goal"] == "Run a long worker goal and collect its verified result"
        });
        mutation(&socket, "run", "resume", revision(&paused), None);
        let mut canceled_parent = false;
        if canceling {
            let running = wait_status(&socket, |v| {
                state(v) == "blocked" && peer_worker.requests().iter().any(|r| r.kind == "compaction")
            });
            mutation(&socket, "cancel-summary", "cancel", revision(&running), None);
            canceled_parent = true;
        }
        if failure {
            let blocked = wait_status(&socket, |v| {
                v["status"]["workers"][0]["goal"]["state"] == "blocked"
                    && v["status"]["workers"][0]["goal"]["reported_tokens"].is_null()
                    && v["status"]["session"]["busy"] == false
                    && (state(v) == "blocked" || state(v) == "done")
            });
            if blocked["status"]["workers"][0]["process_stopped"] != true {
                mutation(&socket, "cancel-unknown", "cancel", revision(&blocked), None);
                canceled_parent = true;
            }
        }
        let finished = wait_status(&socket, |v| {
            state(v) == if canceled_parent { "canceled" } else { "done" }
                && v["status"]["workers"][0]["process_stopped"] == true
        });
        let goal = &finished["status"]["workers"][0]["goal"];
        let requests = peer_worker.requests();
        let expected: u64 = requests
            .iter()
            .filter_map(|r| r.usage.as_ref())
            .map(|u| u64::from(u.prompt) + u64::from(u.completion))
            .sum();
        if failure || canceling {
            assert!(goal["state"] == "blocked" || goal["state"] == "canceled");
            assert!(goal["reported_tokens"].is_null());
        } else {
            assert_eq!(goal["state"], if limited { "budget_exhausted" } else { "done" });
        }
        assert_eq!(
            goal["turns_started"],
            if limited || failure || canceling { 2 } else { 3 }
        );
        if !limited && !failure && !canceling {
            assert_eq!(goal["reported_tokens"].as_u64().unwrap(), expected);
        }
        assert_eq!(goal["budget"], json!({"wall_ms":12000,"tokens":tokens,"turns":8}));
        let pid = finished["status"]["workers"][0]["process_id"].as_u64().unwrap();
        assert!(!Path::new(&format!("/proc/{pid}")).exists());
        peer_done.store(true, Ordering::Release);
    });
    let mut terminal = LiveTerm {
        inner: TestTerminal::new(120, 30),
        done,
        typing: Arc::new(AtomicBool::new(false)),
        typed: Arc::new(AtomicBool::new(false)),
        deadline: Instant::now() + Duration::from_secs(20),
        quit: false,
        final_text: if canceling {
            "premature"
        } else {
            "compaction-final"
        },
        escape: false,
        swap_when: None,
        swapped: false,
        rejected: Arc::new(AtomicBool::new(false)),
    };
    let mut options = opts(&env, &engine.url(), store);
    let (code, last) = run_session(&mut terminal, session, &mut options);
    peer.join().unwrap();
    assert_eq!(code, 0);
    assert_eq!(last.unwrap().id, expected);
    std::env::remove_var("OXA_SESSION_CONTROL_DIRECTORY");
    std::env::remove_var("OXA_WORKER_PROFILES");
    let requests = worker.requests();
    if canceling {
        let summary = requests.iter().find(|r| r.kind == "compaction").unwrap().seq;
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let _ = worker.in_flight();
            if worker
                .timings()
                .iter()
                .any(|t| t.seq == Some(summary) && t.client_closed)
            {
                break;
            }
            assert!(Instant::now() < deadline, "cancel must close summary inference");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            requests
                .iter()
                .filter(|r| r.kind == "chat" && r.max_tokens > 0)
                .count(),
            2
        );
    }
    assert!(
        requests.iter().any(|r| r.kind == "compaction"),
        "actual summary inference required: {:?}",
        requests
            .iter()
            .map(|r| (&r.kind, r.max_tokens, r.usage))
            .collect::<Vec<_>>()
    );
    if !limited && !failure && !canceling {
        assert!(
            requests
                .iter()
                .filter(|r| r.kind == "chat")
                .any(|r| r.messages.iter().any(|m| m
                    .content
                    .as_deref()
                    .is_some_and(|s| s.contains("worker-summary-marker")))),
            "summary must appear in a later goal turn"
        );
    }
    assert!(requests
        .iter()
        .all(|r| r.session == requests[0].session && r.model == "compact-model"));
    let root = engine.requests();
    if !limited && !failure && !canceling {
        assert!(
            root.iter()
                .rfind(|r| r.max_tokens > 0)
                .unwrap()
                .messages
                .iter()
                .any(|m| m
                    .content
                    .as_deref()
                    .is_some_and(|s| s.contains("compaction-final") && s.contains("untrusted data"))),
            "long goal text must not crowd the result out of its wake event"
        );
    }
    if failure {
        assert_eq!(
            requests.iter().filter(|r| r.kind == "compaction").count(),
            1,
            "failed summary must not silently retry"
        );
        assert_eq!(
            requests
                .iter()
                .filter(|r| r.kind == "chat" && r.max_tokens > 0)
                .count(),
            2
        );
    }
    if limited {
        assert_eq!(
            requests
                .iter()
                .filter(|r| r.kind == "chat" && r.max_tokens > 0)
                .count(),
            2,
            "summary must exhaust the cap before a third goal turn"
        );
    }
    let mut before = 0;
    for request in &requests {
        if request.kind == "compaction" {
            return before + 100;
        }
        let usage = request.usage.as_ref().unwrap();
        before += u64::from(usage.prompt) + u64::from(usage.completion);
    }
    panic!("summary request missing");
}

fn main_goal_compacts_with_metered_usage(
    limit: u64,
    failure: bool,
    canceling: bool,
    exact: bool,
) -> (u64, u64) {
    let env = TestEnv::new();
    let agent = Dirs::under(env.root()).agent_dir;
    std::fs::create_dir_all(&agent).unwrap();
    let directory = agent.join("control");
    let socket = directory.join("session.sock");
    std::env::set_var("OXA_SESSION_CONTROL_DIRECTORY", &directory);
    std::env::set_var("OXA_GOAL_AUTO_COMPACTION", "1");
    let timed = limit == u64::MAX;
    let limited = limit != 0 && !timed && !exact;
    let cap = (!timed).then_some(if limit != 0 && !timed { limit } else { 2000000 });
    let mut summary = ScriptTurn::text("main-summary-marker: retain the completed project steps");
    if failure {
        summary.fault = Some(oxa_fake::Fault::Http {
            status: 400,
            body: "unrecoverable summary failure".into(),
        });
    } else if canceling {
        summary.fault = Some(oxa_fake::Fault::HoldHeaders { ms: 1500 });
    }
    let engine = Arc::new(FakeServer::start(vec![
        ScriptTurn::text(r#"{"state":"continue","summary":"first project step checked"}"#),
        ScriptTurn::text(r#"{"state":"continue","summary":"second project step checked"}"#),
        summary,
        ScriptTurn::text(r#"{"state":"done","summary":"main-compaction-final","evidence":["all project steps checked"]}"#),
        ScriptTurn::text("main-summary-marker: final steps checked"),
    ], FakeOpts {
        model: "compact-model".into(), n_ctx: 50000,
        models: vec![oxa_fake::FakeModel { alias: "compact-model".into(), cap: 50000, max_output: Some(1024), caps: Some(oxa_fake::FakeCaps::rust_moe()) }],
        ..Default::default()
    }).unwrap());
    let store: Arc<dyn SessionStore> = Arc::new(FsStore::new(Dirs::under(env.root()).sessions_dir));
    let mut config = deps(&engine.url(), env.root(), &env.work(), store.clone());
    config.model = Some("compact-model".into());
    let session = Session::open(config, Open::New).unwrap();
    let expected = session.state().session_id;
    let peer_id = expected.clone();
    let peer_engine = engine.clone();
    let done = Arc::new(AtomicBool::new(false));
    let peer_done = done.clone();
    let peer = std::thread::spawn(move || {
        let initial = wait_status(&socket, |v| v["status"]["session"]["id"] == peer_id);
        request(
            &socket,
            json!({"id":"goal","method":"set_goal","revision":revision(&initial),"text":format!("Check all project steps. {}", "x".repeat(14000)),"budget":{"wall_ms":12000,"tokens":cap,"turns":8}}),
        );
        let paused = wait_status(&socket, |v| v["status"]["goal"]["id"] != "unassigned");
        let goal_id = paused["status"]["goal"]["id"].clone();
        mutation(&socket, "resume", "resume", revision(&paused), None);
        if canceling {
            let running = wait_status(&socket, |_| {
                peer_engine.requests().iter().any(|r| r.kind == "compaction")
            });
            mutation(&socket, "cancel-summary", "cancel", revision(&running), None);
        }
        let terminal = wait_status(&socket, |v| {
            state(v)
                == if canceling {
                    "canceled"
                } else if failure {
                    "blocked"
                } else if limited {
                    "budget_exhausted"
                } else {
                    "done"
                }
                && v["status"]["goal"]["turn_active"] == false
                && v["status"]["session"]["busy"] == false
        });
        let goal = &terminal["status"]["goal"];
        assert_eq!(goal["id"], goal_id);
        assert_eq!(goal["budget"]["tokens"], json!(cap));
        let requests = peer_engine.requests();
        let known: u64 = requests
            .iter()
            .filter_map(|r| r.usage.as_ref())
            .map(|u| u64::from(u.prompt) + u64::from(u.completion))
            .sum();
        if failure || canceling {
            assert!(
                goal["reported_tokens"].is_null(),
                "failed/aborted summary stays unknown: {goal}"
            );
            assert_eq!(goal["turns_started"], 2);
        } else {
            assert_eq!(
                goal["reported_tokens"].as_u64(),
                Some(known),
                "all SDK requests belong to the same main goal"
            );
            assert_eq!(goal["turns_started"], if limited { 2 } else { 3 });
            if exact {
                assert_eq!(Some(known), cap, "DONE at exactly the cap is valid");
            }
        }
        assert_eq!(terminal["status"]["session"]["id"], peer_id);
        // Keep the TUI and broker alive after settlement: late background work
        // and completion wakes must not issue another inference.
        let count = requests.len();
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(peer_engine.requests().len(), count);
        peer_done.store(true, Ordering::Release);
    });
    let mut terminal = LiveTerm {
        inner: TestTerminal::new(160, 35),
        done,
        typing: Arc::new(AtomicBool::new(false)),
        typed: Arc::new(AtomicBool::new(false)),
        deadline: Instant::now() + Duration::from_secs(18),
        quit: false,
        final_text: if limited || failure || canceling {
            "Goal"
        } else {
            "main-compaction-final"
        },
        escape: false,
        swap_when: None,
        swapped: false,
        rejected: Arc::new(AtomicBool::new(false)),
    };
    let mut options = opts(&env, &engine.url(), store);
    let (code, last) = run_session(&mut terminal, session, &mut options);
    peer.join().unwrap();
    std::env::remove_var("OXA_SESSION_CONTROL_DIRECTORY");
    std::env::remove_var("OXA_GOAL_AUTO_COMPACTION");
    assert_eq!(code, 0);
    assert_eq!(last.unwrap().id, expected);
    let requests = engine.requests();
    assert!(
        requests.iter().any(|r| r.kind == "compaction"),
        "actual compaction required: {:?}",
        requests
            .iter()
            .map(|r| (&r.kind, r.max_tokens, r.usage))
            .collect::<Vec<_>>()
    );
    assert!(requests
        .iter()
        .all(|r| r.session == expected && r.model == "compact-model"));
    if failure || limited || canceling {
        assert_eq!(
            requests
                .iter()
                .filter(|r| r.kind == "chat" && r.max_tokens > 0)
                .count(),
            2,
            "stop prevents a third goal turn"
        );
        assert_eq!(
            requests.iter().filter(|r| r.kind == "compaction").count(),
            1,
            "summary must not silently retry"
        );
    } else {
        assert!(
            requests
                .iter()
                .filter(|r| r.kind == "chat")
                .any(|r| r.messages.iter().any(|m| m
                    .content
                    .as_deref()
                    .is_some_and(|s| s.contains("main-summary-marker")))),
            "later chat retains summary context"
        );
    }
    if canceling {
        let seq = requests.iter().find(|r| r.kind == "compaction").unwrap().seq;
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let _ = engine.in_flight();
            if engine
                .timings()
                .iter()
                .any(|t| t.seq == Some(seq) && t.client_closed)
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "cancel must close the in-flight main summary"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    let mut before = 0;
    for request in &requests {
        if request.kind == "compaction" {
            return (
                before + 100,
                requests
                    .iter()
                    .filter_map(|r| r.usage.as_ref())
                    .map(|u| u64::from(u.prompt) + u64::from(u.completion))
                    .sum(),
            );
        }
        let usage = request.usage.as_ref().unwrap();
        before += u64::from(usage.prompt) + u64::from(usage.completion);
    }
    panic!("missing summary");
}

fn source_prefill(engine: &dyn Engine, session: &str) -> Result<oxa_proto::AssistantTurn, OxaError> {
    let prefix = oxa_proto::FrozenPrefix {
        sections: vec![],
        tools_json: "[]".into(),
        sha256: [0; 32],
    };
    let messages = vec![oxa_proto::WireMsg::user("owned background prefix")];
    engine.chat(
        &oxa_proto::ChatRequest {
            prefix: &prefix,
            messages: &messages,
            model: "fake-model",
            max_tokens: 0,
            thinking: false,
            preserve_thinking: false,
            sampling: None,
            hints: oxa_proto::Hints {
                session,
                idem_key: "",
                kind: oxa_proto::TurnKind::Prefill,
                background: true,
                snapshot_keep: false,
            },
        },
        &CancelToken::new(),
        &mut |_| {},
    )
}
fn main_goal_settles_pending_prefill(canceling: bool) {
    let env = TestEnv::new();
    let agent = Dirs::under(env.root()).agent_dir;
    std::fs::create_dir_all(&agent).unwrap();
    let directory = agent.join("control");
    let socket = directory.join("session.sock");
    std::env::set_var("OXA_SESSION_CONTROL_DIRECTORY", &directory);
    let server = Arc::new(
        FakeServer::start(
            vec![ScriptTurn::text(
                r#"{"state":"done","summary":"prefill-final","evidence":["SDK calls settled"]}"#,
            )],
            FakeOpts {
                hold_headers_ms: 250,
                hold_prefill_ms: 650,
                ..Default::default()
            },
        )
        .unwrap(),
    );
    let store: Arc<dyn SessionStore> = Arc::new(FsStore::new(Dirs::under(env.root()).sessions_dir));
    let session = Session::open(
        deps(&server.url(), env.root(), &env.work(), store.clone()),
        Open::New,
    )
    .unwrap();
    let expected = session.state().session_id;
    let peer_id = expected.clone();
    let metered_engine = session.inner().engine.clone();
    let peer_server = server.clone();
    let done = Arc::new(AtomicBool::new(false));
    let peer_done = done.clone();
    let peer = std::thread::spawn(move || {
        let initial = wait_status(&socket, |v| v["status"]["session"]["id"] == peer_id);
        request(
            &socket,
            json!({"id":"goal","method":"set_goal","revision":revision(&initial),"text":"Verify the final result after all SDK calls settle","budget":{"wall_ms":8000,"tokens":100000,"turns":4}}),
        );
        let paused = wait_status(&socket, |v| v["status"]["goal"]["id"] != "unassigned");
        mutation(&socket, "run", "resume", revision(&paused), None);
        wait_status(&socket, |_| {
            peer_server.requests().iter().any(|r| r.max_tokens > 0)
        });
        let engine = metered_engine.clone();
        let id = peer_id.clone();
        let prefill = std::thread::spawn(move || source_prefill(engine.as_ref(), &id));
        wait_status(&socket, |_| {
            peer_server.requests().iter().any(|r| r.max_tokens == 0)
        });
        let settling = wait_status(&socket, |v| {
            v["status"]["session"]["busy"] == false && v["status"]["goal"]["turn_active"] == true
        });
        assert_ne!(
            state(&settling),
            "done",
            "a pending SDK return cannot be declared known"
        );
        assert!(settling["status"]["goal"]["reported_tokens"].is_null());
        if canceling {
            mutation(&socket, "cancel-prefill", "cancel", revision(&settling), None);
        }
        let finished = wait_status(&socket, |v| {
            state(v) == if canceling { "canceled" } else { "done" }
                && v["status"]["goal"]["turn_active"] == false
        });
        let prefill = prefill.join().unwrap();
        let requests = peer_server.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(finished["status"]["goal"]["turns_started"], 1);
        if canceling {
            assert!(prefill.is_err());
            assert!(finished["status"]["goal"]["reported_tokens"].is_null());
        } else {
            prefill.unwrap();
            let tokens: u64 = requests
                .iter()
                .map(|r| {
                    let usage = r.usage.as_ref().unwrap();
                    u64::from(usage.prompt) + u64::from(usage.completion)
                })
                .sum();
            assert_eq!(
                finished["status"]["goal"]["reported_tokens"].as_u64(),
                Some(tokens)
            );
        }
        assert!(
            source_prefill(metered_engine.as_ref(), &peer_id).is_err(),
            "late prefill must be refused before SDK entry"
        );
        assert_eq!(peer_server.requests().len(), 2);
        peer_done.store(true, Ordering::Release);
    });
    let mut terminal = LiveTerm {
        inner: TestTerminal::new(160, 35),
        done,
        typing: Arc::new(AtomicBool::new(false)),
        typed: Arc::new(AtomicBool::new(false)),
        deadline: Instant::now() + Duration::from_secs(12),
        quit: false,
        final_text: "prefill-final",
        escape: false,
        swap_when: None,
        swapped: false,
        rejected: Arc::new(AtomicBool::new(false)),
    };
    let mut options = opts(&env, &server.url(), store);
    let (code, last) = run_session(&mut terminal, session, &mut options);
    peer.join().unwrap();
    std::env::remove_var("OXA_SESSION_CONTROL_DIRECTORY");
    assert_eq!(code, 0);
    assert_eq!(last.unwrap().id, expected);
    if canceling {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let _ = server.in_flight();
            if server
                .timings()
                .iter()
                .any(|t| t.max_tokens == Some(0) && t.client_closed)
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "cancel closes the pending prefill connection"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--mode") {
        if args
            .last()
            .is_some_and(|role| role.contains("fixed author role") || role.contains("fixed reviewer role"))
        {
            let gateway = args.last().unwrap().contains("[gateway-test]");
            assert_eq!(
                std::env::var("OXA_OFFLINE").as_deref(),
                Ok(if gateway { "0" } else { "1" }),
                "child environment must match its captured route"
            );
            assert_eq!(
                args.iter().any(|arg| arg == "--offline"),
                !gateway,
                "worker route controls its offline flag"
            );
        }
        std::process::exit(oxa_cli::run(
            &args,
            &oxa_config::Env::from_process(),
            oxa_cli::Stdio::real(),
        ));
    }
    if args.first().map(String::as_str) == Some("--background-admission-only") {
        background_owned_cleanup("admission");
        return;
    }
    if args.first().map(String::as_str) == Some("--background-settlement-only") {
        for kind in ["settlement", "admission"] {
            background_owned_cleanup(kind);
            println!("background {kind} revokes and reaps owned work ... ok");
        }
        return;
    }
    main_goal_settles_pending_prefill(false);
    main_goal_settles_pending_prefill(true);
    println!("main goal waits for a pending SDK prefill and cancels it without fabricating usage ... ok");
    if args.first().map(String::as_str) == Some("--main-prefill-only") {
        return;
    }
    let (main_cap, main_done_cap) = main_goal_compacts_with_metered_usage(0, false, false, false);
    main_goal_compacts_with_metered_usage(main_done_cap, false, false, true);
    main_goal_compacts_with_metered_usage(main_cap, false, false, false);
    main_goal_compacts_with_metered_usage(0, true, false, false);
    main_goal_compacts_with_metered_usage(u64::MAX, true, false, false);
    main_goal_compacts_with_metered_usage(0, false, true, false);
    println!(
        "main goal compaction meters summaries/prefill and honors caps, failure and cancellation ... ok"
    );
    if args.first().map(String::as_str) == Some("--main-compaction-only") {
        return;
    }
    if args.first().map(String::as_str) == Some("--idle-interrupt-only") {
        model_worker_lanes_wake_same_session(false, "idle_escape", false, false);
        model_worker_lanes_wake_same_session(false, "idle_escape", false, true);
        model_worker_lanes_wake_same_session(false, "idle_unscoped", false, false);
        println!("idle TUI Esc pauses the parent and closes/reaps local and gateway workers ... ok");
        return;
    }
    model_worker_lanes_wake_same_session(false, "idle_escape", false, false);
    model_worker_lanes_wake_same_session(false, "idle_escape", false, true);
    model_worker_lanes_wake_same_session(false, "idle_unscoped", false, false);
    println!("idle TUI Esc pauses the parent and closes/reaps local and gateway workers ... ok");
    let cap = worker_goal_compacts_with_metered_usage(0, false, false);
    worker_goal_compacts_with_metered_usage(cap, false, false);
    worker_goal_compacts_with_metered_usage(0, true, false);
    worker_goal_compacts_with_metered_usage(0, false, true);
    println!("cancel during summary closes inference and preserves unknown usage ... ok");
    println!("failed summary keeps usage unknown and stops goal continuation ... ok");
    println!("summary usage exhausts a worker cap before another goal turn ... ok");
    println!("worker compaction preserves context and charges summaries/prefill to the same budget ... ok");
    if args.first().map(String::as_str) == Some("--compaction-only") {
        return;
    }
    for (steering, cancellation, name) in [
        (
            false,
            "none",
            "worker roles/models retain context and wake the same TUI session",
        ),
        (
            true,
            "none",
            "model steering reaches the running worker's next inference",
        ),
        (
            false,
            "socket",
            "parent cancellation stops and reaps owned workers",
        ),
        (
            false,
            "escape",
            "Esc settlement stops workers and preserves unknown usage",
        ),
    ] {
        model_worker_lanes_wake_same_session(steering, cancellation, false, false);
        println!("{name} ... ok");
    }
    model_worker_lanes_wake_same_session(true, "none", true, false);
    model_worker_lanes_wake_same_session(false, "none", false, true);
    println!("explicit gateway profiles launch online native workers against loopback fakes ... ok");
    println!("blocked worker retarget/resume preserves context and cumulative budget ... ok");
    model_worker_lanes_wake_same_session(false, "socket", false, true);
    println!("gateway worker cancellation closes in-flight inference connections ... ok");
    offline_parent_refuses_gateway_workers_before_contact();
    println!("offline parent rejects gateway workers before launch or generation ... ok");
    if args.first().map(String::as_str) == Some("--workers-only") {
        return;
    }

    model_background_task_wakes_same_session(false, true);
    println!("background completion reaches an already running TUI turn ... ok");
    for kind in ["deny", "no_tools", "bad_args", "plan", "control_hidden"] {
        background_permission_boundaries(kind);
        println!("background tool preserves {kind} permissions ... ok");
    }
    for kind in [
        "exit",
        "swap",
        "cancel",
        "deadline",
        "pause",
        "settlement",
        "admission",
    ] {
        background_owned_cleanup(kind);
        println!("background task {kind} cancels and reaps owned work ... ok");
    }
    model_background_task_wakes_same_session(false, false);
    println!("model background command returns immediately and wakes ordinary TUI session ... ok");
    model_background_task_wakes_same_session(true, false);
    println!("background completion wakes waiting goal without a reset or false DONE ... ok");
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
