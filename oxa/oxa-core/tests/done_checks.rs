//! Issue #104: the `done` setting. oxa runs the project's done commands itself when a run
//! that edited files is about to finish. Scripted engine, isolated `TestEnv`, real sandbox-less
//! shell in the work folder.

use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{AgentEvent, CancelToken, EventSink, ExecResult, ExecSpec, HeadlessUi, OxaError, Sandbox};
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

#[derive(Default)]
struct Rec(Vec<AgentEvent>);
impl EventSink for Rec {
    fn emit(&mut self, e: &AgentEvent) {
        self.0.push(e.clone());
    }
}
impl Rec {
    fn checks(&self) -> Vec<String> {
        self.0
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Notice { text, .. } if text.starts_with("done check:") => Some(text.clone()),
                _ => None,
            })
            .collect()
    }
}

fn run(env: &TestEnv, done: serde_json::Value, turns: Vec<ScriptTurn>) -> (Rec, Arc<ScriptedEngine>) {
    run_with(env, done, turns, Arc::new(Direct))
}

fn run_with(
    env: &TestEnv,
    done: serde_json::Value,
    turns: Vec<ScriptTurn>,
    sandbox: Arc<dyn Sandbox>,
) -> (Rec, Arc<ScriptedEngine>) {
    let engine = Arc::new(ScriptedEngine::new(turns));
    let settings = if done.is_null() {
        Settings::default()
    } else {
        Settings::from_values(json!({ "done": done }), json!({}))
    };
    let deps = Deps {
        engine: engine.clone(),
        store: Arc::new(MemStore::new()),
        tools: oxa_tools::builtin_tools(),
        hooks: Vec::new(),
        policy: Box::new(ModePolicy::new()),
        sandbox,
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
    };
    let mut s = Session::open(deps, Open::New).expect("open");
    let mut rec = Rec::default();
    s.prompt("go".into(), Vec::new(), &mut rec, &mut HeadlessUi::default())
        .expect("prompt");
    (rec, engine)
}

fn write(id: &str, path: &str) -> ScriptTurn {
    ScriptTurn::tool(id, "write", &json!({"path": path, "content": "x"}).to_string())
}

const CHECK: &str = "test -f fixed.txt || { echo MISSING-FIXED; exit 3; }";

#[test]
fn failing_check_feeds_model_then_passes() {
    let env = TestEnv::new();
    let (rec, engine) = run(
        &env,
        json!([CHECK]),
        vec![
            write("c1", "a.txt"),
            ScriptTurn::text("done"),
            write("c2", "fixed.txt"),
            ScriptTurn::text("really done"),
        ],
    );
    let checks = rec.checks();
    assert_eq!(checks.len(), 2, "{checks:?}");
    assert!(
        checks[0].contains("FAILED (exit 3)") && checks[0].contains("MISSING-FIXED"),
        "{checks:?}"
    );
    assert!(checks[1].ends_with("ok"), "{checks:?}");
    let reqs = engine.requests();
    assert_eq!(reqs.len(), 4);
    let last_user = reqs[2]
        .messages
        .iter()
        .rev()
        .find(|m| m.role == oxa_proto::Role::User)
        .unwrap();
    assert!(last_user.content.as_ref().unwrap().contains("MISSING-FIXED"));
    assert_eq!(engine.remaining(), 0);
}

#[test]
fn passing_check_adds_no_round() {
    let env = TestEnv::new();
    let (rec, engine) = run(
        &env,
        json!(["true"]),
        vec![write("c1", "a.txt"), ScriptTurn::text("done")],
    );
    assert_eq!(rec.checks(), vec!["done check: true ok".to_string()]);
    assert_eq!(engine.requests().len(), 2);
}

#[test]
fn no_edits_no_checks() {
    let env = TestEnv::new();
    let (rec, engine) = run(&env, json!(["exit 1"]), vec![ScriptTurn::text("nothing to do")]);
    assert!(rec.checks().is_empty());
    assert_eq!(engine.requests().len(), 1);
}

#[test]
fn off_by_default() {
    let env = TestEnv::new();
    let (rec, engine) = run(
        &env,
        serde_json::Value::Null,
        vec![write("c1", "a.txt"), ScriptTurn::text("done")],
    );
    assert!(rec.checks().is_empty());
    assert_eq!(engine.requests().len(), 2);
}

#[test]
fn failing_forever_stops_after_two_rounds() {
    let env = TestEnv::new();
    let (rec, engine) = run(
        &env,
        json!(["exit 1"]),
        vec![
            write("c1", "a.txt"),
            ScriptTurn::text("done"),
            write("c2", "b.txt"),
            ScriptTurn::text("done"),
            write("c3", "c.txt"),
            ScriptTurn::text("done"),
            ScriptTurn::text("must not be requested"),
        ],
    );
    assert_eq!(rec.checks().len(), 3, "{:?}", rec.checks());
    assert_eq!(engine.requests().len(), 6);
    assert_eq!(engine.remaining(), 1);
}

#[test]
fn failed_check_without_new_edit_is_not_rerun() {
    let env = TestEnv::new();
    let (rec, engine) = run(
        &env,
        json!(["exit 1"]),
        vec![
            write("c1", "a.txt"),
            ScriptTurn::text("done"),
            ScriptTurn::text("cannot fix"),
        ],
    );
    assert_eq!(rec.checks().len(), 1);
    assert_eq!(engine.requests().len(), 3);
}

/// Records every command and cancels the token while the first one runs.
struct CancelDuringFirst(std::sync::Mutex<Vec<String>>);
impl Sandbox for CancelDuringFirst {
    fn check(&self) -> Result<(), OxaError> {
        Ok(())
    }
    fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
        Vec::new()
    }
    fn run(&self, s: &ExecSpec, c: &CancelToken, _live: &mut dyn FnMut(&[u8])) -> ExecResult {
        self.0.lock().unwrap().push(s.command.clone());
        c.cancel();
        ExecResult {
            exit: None,
            ..Default::default()
        }
    }
}

#[test]
fn cancel_during_first_check_stops_done_phase() {
    let env = TestEnv::new();
    let spy = Arc::new(CancelDuringFirst(Default::default()));
    let (rec, engine) = run_with(
        &env,
        json!(["echo first", "echo second"]),
        vec![
            write("c1", "a.txt"),
            ScriptTurn::text("done"),
            ScriptTurn::text("must not be requested"),
        ],
        spy.clone(),
    );
    let ran = spy.0.lock().unwrap().clone();
    assert_eq!(ran.len(), 1, "{ran:?}");
    assert!(ran[0].contains("first"), "{ran:?}");
    assert!(rec.checks().is_empty(), "{:?}", rec.checks());
    assert_eq!(engine.requests().len(), 2);
    assert_eq!(engine.remaining(), 1);
}
