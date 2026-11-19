//! G2: `/goal` through the app's session command. A set starts a run whose first request
//! carries the objective and offers the goal tools.
use super::*;
use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, ToolSelection};
use oxa_fake::{ScriptTurn, ScriptedEngine, TestEnv};
use oxa_proto::{
    Access, CancelToken, Decision, Engine, ExecResult, ExecSpec, HeadlessUi, Mode, Policy, Role, Sandbox,
    VecSink,
};
use oxa_resources::Resources;
use oxa_session::{FsStore, SessionStore};
use serde_json::json;
use std::sync::Arc;

struct NoTools;

impl Policy for NoTools {
    fn decide(&self, _: Mode, _: &Access, _: &Path) -> Decision {
        panic!("no tools in this test")
    }
    fn remember(&mut self, _: &Access) {}
}

impl Sandbox for NoTools {
    fn check(&self) -> Result<(), OxaError> {
        Ok(())
    }
    fn bwrap_args(&self, _: &ExecSpec) -> Vec<String> {
        Vec::new()
    }
    fn run(&self, _: &ExecSpec, _: &CancelToken, _: &mut dyn FnMut(&[u8])) -> ExecResult {
        panic!("no bash in this test")
    }
}

fn deps(env: &TestEnv, engine: Arc<dyn Engine>, store: Arc<dyn SessionStore>) -> Deps {
    Deps {
        engine,
        store,
        tools: Vec::new(),
        hooks: Vec::new(),
        policy: Box::new(NoTools),
        sandbox: Arc::new(NoTools),
        settings: Settings::from_values(
            json!({"retry":{"enabled":false},"oxa":{"envNote":false}}),
            json!({}),
        ),
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

#[test]
fn goal_set_starts_run_with_goal_start_prompt() {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("on it")]));
    let store: Arc<dyn SessionStore> = Arc::new(FsStore::new(env.root().join("sessions")));
    let mut session = Session::open(deps(&env, engine.clone(), store), Open::New).unwrap();
    let mut sink = VecSink::default();
    let mut ui = HeadlessUi::default();
    let objective = "Ship the parser";

    // The run may go on past the one scripted reply; only its first request is checked.
    let _ = run_goal(&mut session, objective, &mut sink, &mut ui);

    let reqs = engine.requests();
    let first = reqs.first().expect("the goal starts a request");
    let user = first
        .messages
        .iter()
        .find(|m| m.role == Role::User)
        .and_then(|m| m.content.clone())
        .unwrap_or_default();
    assert!(user.contains(objective), "{user}");
    assert!(first.tools_json.contains("\"goal_done\""), "{}", first.tools_json);
    assert!(first.tools_json.contains("\"wait\""), "{}", first.tools_json);
}
