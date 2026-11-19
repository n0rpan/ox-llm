//! P4 (wave H): offline, the session host refuses an extension / codemode
//! `fetch` to a host that is not loopback and any `exec` with network access, before the user
//! is asked (the error code proves the order: the policy would answer `denied`). One process,
//! one policy.

use oxa_compact::CompactPolicy;
use oxa_config::{Dirs, QueueMode, Settings};
use oxa_core::{CoreHost, Queue, SessionState, Shared, Stats, Toolset};
use oxa_fake::{ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{
    codes, set_net_policy, Access, Answer, Autonomy, CancelToken, Engine, ExecSpec, FrozenPrefix, HeadlessUi,
    Host, HostAction, Mode, OxaError, Policy, ThinkingLevel, Tool, Ui, Value, VecSink, YoloSwitch,
};
use oxa_resources::Resources;
use oxa_sandbox::NoSandbox;
use oxa_session::{MemStore, SessionStore, Transcript, Tree};
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};

fn tools() -> Vec<Arc<dyn Tool>> {
    let mut t = oxa_tools::builtin_tools();
    t.sort_by(|a, b| a.spec().name.cmp(&b.spec().name));
    t
}

fn state(env: &TestEnv, turns: Vec<ScriptTurn>, settings: Settings) -> (SessionState, Arc<ScriptedEngine>) {
    let eng = Arc::new(ScriptedEngine::new(turns));
    let engine: Arc<dyn Engine> = eng.clone();
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    let cwd = env.work();
    std::fs::write(cwd.join("a.txt"), "alpha\n").unwrap();
    let sref = store.create(&cwd, None, None).unwrap();
    let info = engine.info(None).unwrap();
    // A fixed prefix without the cwd: the request hashes do not depend on the temp path.
    let mut prefix = FrozenPrefix {
        sections: vec![("preamble".to_string(), "You are a test agent.".to_string())],
        tools_json: "[]".to_string(),
        sha256: [0u8; 32],
    };
    prefix.sha256 = Sha256::digest(prefix.hash_input()).into();
    let shared = Arc::new(Shared {
        queue: Mutex::new(Queue::default()),
        cancel: CancelToken::new(),
        cancel_retry: CancelToken::new(),
        wait_wake: CancelToken::new(),
        busy: std::sync::atomic::AtomicBool::new(false),
        modes: Mutex::new((QueueMode::All, QueueMode::OneAtATime)),
        dirty: std::sync::atomic::AtomicBool::new(false),
        gate: Default::default(),
        wake: Default::default(),
    });
    let tools = tools();
    let st = SessionState {
        goal: None,
        output_schema: None,
        engine,
        store,
        hooks: Vec::new(),
        policy: Box::new(ModePolicy::new()) as Box<dyn Policy>,
        sandbox: Arc::new(NoSandbox),
        settings,
        dirs: Dirs::under(env.root()),
        cwd: cwd.clone(),
        all_tools: tools.clone(),
        selection: Default::default(),
        active: Toolset {
            direct: tools,
            indirect: Vec::new(),
        },
        resources: Resources::default(),
        sref,
        tree: Tree::new(Vec::new()),
        transcript: Transcript::default(),
        prefix,
        info,
        model: "fake-model".to_string(),
        thinking: ThinkingLevel::Off,
        mode: Mode::Workspace,
        compact: CompactPolicy {
            soft: u32::MAX,
            hard: u32::MAX,
            keep_tokens: 4000,
            summary_max: 1024,
            enabled: true,
        },
        shared,
        next_turn: Vec::new(),
        last_usage: None,
        stats: Stats::default(),
        spill_dir: env.tmp(),
        tool_env: Vec::new(),
        tune: Default::default(),
        auto_compaction: true,
        auto_retry: true,
        host_rx: None,
        host_deferred: Vec::new(),
        compact_memo: Default::default(),
        jobs: oxa_core::fresh_jobs(),
    };
    (st, eng)
}

/// Run `action` through the session's host on a helper thread while this thread serves it.
fn through_host(st: &mut SessionState, action: HostAction) -> Result<Value, OxaError> {
    let (host, rx) = CoreHost::new(st.shared.clone());
    st.host_rx = Some(rx);
    let worker = std::thread::spawn(move || host.call(action));
    let mut sink = VecSink::default();
    let mut ui = HeadlessUi::default();
    while !worker.is_finished() {
        oxa_core::serve_host(st, false, &mut sink, &mut ui);
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    worker.join().unwrap()
}

#[test]
fn offline_refuses_remote_fetch_and_network_exec_before_asking() {
    let env = TestEnv::new();
    let (mut st, _eng) = state(&env, vec![ScriptTurn::text("x")], Settings::default());
    set_net_policy(true, false);
    let fetch = through_host(
        &mut st,
        HostAction::Fetch {
            url: "http://10.255.0.2:9/x".to_string(),
            method: "GET".to_string(),
            headers: Vec::new(),
            body: None,
        },
    );
    let exec = through_host(
        &mut st,
        HostAction::Exec(ExecSpec {
            command: "true".to_string(),
            cwd: env.work(),
            writable: true,
            net: true,
            timeout_s: None,
            env: Vec::new(),
        }),
    );
    set_net_policy(false, false);
    let e = fetch.unwrap_err();
    assert_eq!(e.code, codes::OFFLINE, "{e}");
    assert!(
        e.message
            .starts_with("Refused: extension fetch (10.255.0.2) needs the network"),
        "{e}"
    );
    let e = exec.unwrap_err();
    assert_eq!(e.code, codes::OFFLINE, "{e}");
    assert!(
        e.message
            .starts_with("Refused: a command with network access needs the network"),
        "{e}"
    );
}

/// #1512 S2: an extension's `exec` is classified in the directory it runs in, not in the
/// session's directory: `/…/other/victim` is outside the session's workspace but inside the
/// directory the extension gave the command, so only the first asks.
#[test]
fn extension_exec_is_classified_in_its_own_directory() {
    let env = TestEnv::new();
    let (mut st, _eng) = state(&env, vec![ScriptTurn::text("x")], Settings::default());
    st.policy = Box::new(
        ModePolicy::new()
            .with_guard(oxa_perm::PathGuard::new(
                &env.home(),
                &env.agent_dir(),
                &env.tmp(),
            ))
            .with_danger(Arc::new(oxa_perm::DangerRules::builtin()), Vec::new()),
    );
    let other = env.root().join("other");
    std::fs::create_dir_all(other.join("victim")).unwrap();
    let spec = |cwd: std::path::PathBuf| {
        HostAction::Exec(ExecSpec {
            command: format!("rm -rf {}/victim", other.display()),
            cwd,
            writable: true,
            net: false,
            timeout_s: None,
            env: Vec::new(),
        })
    };
    let e = through_host(&mut st, spec(env.work())).unwrap_err();
    assert!(
        e.message
            .starts_with("Refused: dangerous command (rm-recursive-outside: "),
        "{e}"
    );
    // This workspace has no OS sandbox, so the command cannot actually run here; what matters
    // is that the classifier does not refuse it in the directory the extension gave it.
    let e = through_host(&mut st, spec(other.clone())).unwrap_err();
    assert!(!e.message.starts_with("Refused: dangerous command"), "{e}");
}

/// A `Ui` that answers every access question with `answer` and records the questions.
struct AskUi {
    answer: Answer,
    asked: Vec<Access>,
}

impl Ui for AskUi {
    fn select(&mut self, _t: &str, _o: &[String], _ms: Option<u32>) -> Option<usize> {
        None
    }
    fn confirm(&mut self, _t: &str, _b: &str, _ms: Option<u32>) -> Option<bool> {
        None
    }
    fn input(&mut self, _t: &str, _p: &str) -> Option<String> {
        None
    }
    fn editor(&mut self, _t: &str, _p: &str) -> Option<String> {
        None
    }
    fn notify(&mut self, _text: &str, _level: oxa_proto::Level) {}
    fn set_status(&mut self, _k: &str, _t: Option<&str>) {}
    fn set_widget(&mut self, _k: &str, _l: Option<&[String]>, _at: oxa_proto::Placement) {}
    fn set_title(&mut self, _t: &str) {}
    fn set_editor_text(&mut self, _t: &str) {}
    fn ask_access(&mut self, a: &Access) -> Answer {
        self.asked.push(a.clone());
        self.answer
    }
}

fn through_host_ui(st: &mut SessionState, action: HostAction, ui: &mut AskUi) -> Result<Value, OxaError> {
    let (host, rx) = CoreHost::new(st.shared.clone());
    st.host_rx = Some(rx);
    let worker = std::thread::spawn(move || host.call(action));
    let mut sink = VecSink::default();
    while !worker.is_finished() {
        oxa_core::serve_host(st, false, &mut sink, ui);
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    worker.join().unwrap()
}

/// An extension's `exec` with a directory outside the session directory asks like a write
/// outside the project (it is mounted writable); yolo does not ask, and a directory inside the
/// session directory is unchanged. A denied ask refuses before the command runs.
#[test]
fn extension_exec_cwd_outside_the_session_asks_like_an_outside_write() {
    let env = TestEnv::new();
    let other = env.root().join("other");
    std::fs::create_dir_all(&other).unwrap();
    let exec = |cwd: std::path::PathBuf| {
        HostAction::Exec(ExecSpec {
            command: "true".to_string(),
            cwd,
            writable: true,
            net: false,
            timeout_s: None,
            env: Vec::new(),
        })
    };
    let guard = || oxa_perm::PathGuard::new(&env.home(), &env.agent_dir(), &env.tmp());

    // Standard mode: outside asks (once), inside does not.
    let (mut st, _eng) = state(&env, vec![ScriptTurn::text("x")], Settings::default());
    st.policy = Box::new(ModePolicy::new().with_guard(guard()));
    let mut ui = AskUi {
        answer: Answer::Once,
        asked: Vec::new(),
    };
    through_host_ui(&mut st, exec(env.work()), &mut ui).unwrap();
    assert!(
        ui.asked.is_empty(),
        "inside the session directory: {:?}",
        ui.asked
    );
    through_host_ui(&mut st, exec(other.clone()), &mut ui).unwrap();
    assert_eq!(ui.asked, vec![Access::Write(other.clone())]);

    // No approver (denied): refused, the command never runs.
    let mut ui = AskUi {
        answer: Answer::Deny,
        asked: Vec::new(),
    };
    let e = through_host_ui(&mut st, exec(other.clone()), &mut ui).unwrap_err();
    assert_eq!(e.code, codes::DENIED, "{e}");

    // Yolo: no question.
    let switch = YoloSwitch::new(Autonomy::Yolo);
    st.policy = Box::new(ModePolicy::new().with_guard(guard()).with_yolo(switch));
    let mut ui = AskUi {
        answer: Answer::Deny,
        asked: Vec::new(),
    };
    through_host_ui(&mut st, exec(other), &mut ui).unwrap();
    assert!(ui.asked.is_empty(), "yolo never asks: {:?}", ui.asked);
}
