//! #1540: a `bash` command that polls for CI gets the native `wait` call in its result; the
//! session counts such calls (`ci_polls`).
use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{CancelToken, ExecResult, ExecSpec, HeadlessUi, OxaError, Role, Sandbox, VecSink};
use oxa_resources::Resources;
use oxa_session::MemStore;
use serde_json::json;
use std::sync::Arc;

/// Runs the command directly in the work folder (the real sandbox needs bwrap).
struct Direct;
impl Sandbox for Direct {
    fn check(&self) -> Result<(), OxaError> {
        Ok(())
    }
    fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
        Vec::new()
    }
    fn run(&self, s: &ExecSpec, c: &CancelToken, live: &mut dyn FnMut(&[u8])) -> ExecResult {
        oxa_sandbox::run_unsandboxed(s, c, live)
    }
}

/// One bash call, then a final reply. Returns the tool result text the model got back and the
/// session's `ci_polls`.
fn run(command: &str) -> (String, u32) {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(vec![
        ScriptTurn::tool("b1", "bash", &json!({"command": command}).to_string()),
        ScriptTurn::text("done"),
    ]));
    let deps = Deps {
        engine: engine.clone(),
        store: Arc::new(MemStore::new()),
        tools: oxa_tools::builtin_tools(),
        hooks: Vec::new(),
        policy: Box::new(ModePolicy::new()),
        sandbox: Arc::new(Direct),
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
    let mut session = Session::open(deps, Open::New).unwrap();
    session
        .prompt(
            "go".into(),
            vec![],
            &mut VecSink::default(),
            &mut HeadlessUi::default(),
        )
        .unwrap();
    let requests = engine.requests();
    let result = requests[1]
        .messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Tool)
        .and_then(|m| m.content.clone())
        .unwrap_or_default();
    (result, session.stats().ci_polls)
}

#[test]
fn a_polling_loop_gets_the_wait_call_and_is_counted() {
    let (result, polls) =
        run("gh() { echo ok; }; for i in 1 2; do gh run list --workflow lane; sleep 0; done");
    assert!(result.contains(r#""kind":"ci""#), "{result}");
    assert_eq!(polls, 1);
}

#[test]
fn a_single_log_read_gets_no_hint() {
    let (result, polls) = run("gh() { echo ok; }; gh run view 123 --log-failed");
    assert!(!result.contains(r#""kind":"ci""#), "{result}");
    assert_eq!(polls, 0);
}
