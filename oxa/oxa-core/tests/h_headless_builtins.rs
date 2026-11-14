//! Wave H (lane run): built-in slash commands outside the interactive mode
//! (`oxa_core::run_builtin`) and `/reload` re-creating code-loaded hook sets (EXT-007).
//! Isolation: every file lives under `oxa_fake::TestEnv`.

use oxa_config::{Dirs, Settings};
use oxa_core::{run_builtin, Deps, Open, Session, ToolSelection};
use oxa_fake::{ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{
    codes, CommandSpec, HeadlessUi, HookPoint, HookReply, Hooks, Level, OxaError, Tool, Ui, Value, VecSink,
};
use oxa_resources::Resources;
use oxa_sandbox::NoSandbox;
use oxa_session::MemStore;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

fn open(env: &TestEnv, hooks: Vec<Arc<dyn Hooks>>) -> Session {
    let deps = Deps {
        engine: Arc::new(ScriptedEngine::new(Vec::new())),
        store: Arc::new(MemStore::new()),
        tools: Vec::new(),
        hooks,
        policy: Box::new(ModePolicy::new()),
        sandbox: Arc::new(NoSandbox),
        settings: Settings::default(),
        dirs: Dirs::under(env.root()),
        cwd: env.work(),
        selection: ToolSelection::default(),
        resources: Resources::default(),
        model: None,
        thinking: None,
        mode: None,
        session_id: None,
    };
    Session::open(deps, Open::New).expect("open")
}

fn run(s: &mut Session, name: &str, args: &str) -> Result<Vec<String>, OxaError> {
    run_builtin(s, name, args, &mut VecSink::default(), &mut HeadlessUi::default())
}

#[test]
fn engine_status_names_the_model() {
    let env = TestEnv::new();
    let mut s = open(&env, Vec::new());
    let lines = run(&mut s, "engine", "").expect("status");
    let model = s.inner().model.clone();
    assert!(lines.iter().any(|l| *l == format!("Model: {model}")), "{lines:?}");
    assert!(
        lines.iter().any(|l| l.starts_with("Context window: ")),
        "{lines:?}"
    );
    let e = run(&mut s, "engine", "nonsense").expect_err("usage");
    assert_eq!(e.code, codes::USAGE);
}

#[test]
fn omitted_commands_are_unsupported_and_selectors_need_the_tui() {
    let env = TestEnv::new();
    let mut s = open(&env, Vec::new());
    for name in ["share", "login", "logout"] {
        let e = run(&mut s, name, "").expect_err(name);
        assert_eq!(e.code, codes::UNSUPPORTED_IN_OXA, "{name}");
    }
    let e = run(&mut s, "tree", "").expect_err("tree");
    assert_eq!(e.code, codes::INVALID);
    assert_eq!(e.message, "/tree is available in interactive mode");
}

#[test]
fn trust_saves_the_decision() {
    let env = TestEnv::new();
    let mut s = open(&env, Vec::new());
    let lines = run(&mut s, "trust", "").expect("trust");
    assert!(lines[0].starts_with("This project is trusted"), "{lines:?}");
    let file = Dirs::under(env.root()).trust_file();
    let saved = std::fs::read_to_string(&file).expect("trust.json");
    let cwd = std::fs::canonicalize(env.work()).unwrap();
    assert!(saved.contains(&*cwd.to_string_lossy()), "{saved}");
    assert!(saved.contains("true"), "{saved}");
    run(&mut s, "trust", "no").expect("no");
    let saved = std::fs::read_to_string(&file).expect("trust.json");
    assert!(saved.contains("false"), "{saved}");
    assert_eq!(
        run(&mut s, "trust", "maybe").expect_err("usage").code,
        codes::USAGE
    );
}

#[test]
fn bug_writes_a_private_local_bundle() {
    use std::os::unix::fs::PermissionsExt;
    let env = TestEnv::new();
    let mut s = open(&env, Vec::new());
    let lines = run(&mut s, "bug", "it broke").expect("bug");
    let path = lines[0]
        .strip_prefix("Bug report written to ")
        .and_then(|r| r.split(" (").next())
        .expect("path in the line");
    let text = std::fs::read_to_string(path).expect("bundle");
    assert!(text.starts_with("oxa bug report (local file; nothing was uploaded)\n"));
    assert!(text.contains("description: it broke\n"));
    let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}

#[test]
fn simple_setters_run_headless() {
    let env = TestEnv::new();
    let mut s = open(&env, Vec::new());
    run(&mut s, "name", "demo").expect("name");
    assert_eq!(s.state().session_name.as_deref(), Some("demo"));
    run(&mut s, "mode", "plan").expect("mode");
    assert_eq!(s.state().mode, oxa_proto::Mode::Plan);
    run(&mut s, "thinking", "high").expect("thinking");
    assert_eq!(s.state().thinking, oxa_proto::ThinkingLevel::High);
    assert!(run(&mut s, "session", "").expect("session")[1].starts_with("Session id: "));
}

/// Records reloads and the session_start / session_shutdown it sees.
#[derive(Default)]
struct Reloadable {
    seen: Mutex<Vec<String>>,
}

impl Hooks for Reloadable {
    fn fire(&self, p: HookPoint, payload: &Value, _ui: &mut dyn Ui) -> HookReply {
        if matches!(p, HookPoint::SessionStart | HookPoint::SessionShutdown) {
            let reason = payload["reason"].as_str().unwrap_or("-");
            self.seen.lock().unwrap().push(format!("{}:{reason}", p.as_str()));
        }
        HookReply::Continue
    }
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        Vec::new()
    }
    fn commands(&self) -> Vec<CommandSpec> {
        Vec::new()
    }
    fn run_command(&self, _name: &str, _args: &str, _ui: &mut dyn Ui) -> Result<(), OxaError> {
        Ok(())
    }
    fn prompt_sections(&self) -> Vec<(String, String)> {
        Vec::new()
    }
    fn reload(&self, _paths: &[PathBuf]) {
        self.seen.lock().unwrap().push("reload".into());
    }
}

/// EXT-007: `/reload` fires session_shutdown, re-creates the hook set, then session_start
/// (reason `reload`).
#[test]
fn reload_recreates_code_loaded_hook_sets() {
    let env = TestEnv::new();
    let hooks = Arc::new(Reloadable::default());
    let mut s = open(&env, vec![hooks.clone()]);
    hooks.seen.lock().unwrap().clear();
    let mut ui = HeadlessUi::default();
    run_builtin(&mut s, "reload", "", &mut VecSink::default(), &mut ui).expect("reload");
    assert_eq!(
        *hooks.seen.lock().unwrap(),
        ["session_shutdown:-", "reload", "session_start:reload"]
    );
    assert!(ui.notes.iter().all(|(l, _)| *l != Level::Error), "{:?}", ui.notes);
}
