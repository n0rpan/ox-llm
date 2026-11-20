//! The finish decision: before a run or goal ends, oxa asks once for proof of an edit and once
//! about background jobs, then lets the run end with a warning and a record (never a block).
use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{
    AgentEvent, CancelToken, ExecResult, ExecSpec, HeadlessUi, OxaError, Role, Sandbox, VecSink,
};
use oxa_resources::Resources;
use oxa_session::{Goal, GoalBudget, GoalStatus, MemStore};
use oxa_tools::JobRegistry;
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

fn deps(env: &TestEnv, engine: Arc<ScriptedEngine>, oxa: serde_json::Value, jobs: Arc<JobRegistry>) -> Deps {
    let settings = json!({ "oxa": oxa });
    Deps {
        engine,
        store: Arc::new(MemStore::new()),
        tools: oxa_tools::builtin_tools_with_shell_and_jobs(oxa_tools::ShellConfig::default(), jobs.clone()),
        hooks: Vec::new(),
        policy: Box::new(ModePolicy::new()),
        sandbox: Arc::new(Direct),
        settings: Settings::from_values(settings.clone(), settings),
        dirs: Dirs::under(env.root()),
        cwd: env.work(),
        selection: ToolSelection::default(),
        resources: Resources::default(),
        model: None,
        thinking: None,
        mode: None,
        session_id: None,
        jobs,
    }
}

fn write(id: &str, path: &str) -> ScriptTurn {
    ScriptTurn::tool(id, "write", &json!({"path": path, "content": "x"}).to_string())
}

/// A `cargo test` stand-in (a shell function, so no toolchain is needed) that passes.
fn check(id: &str) -> ScriptTurn {
    ScriptTurn::tool(
        id,
        "bash",
        &json!({"command": "cargo() { echo 'test result: ok. 1 passed; 0 failed'; }; cargo test"})
            .to_string(),
    )
}

fn goal_done(id: &str) -> ScriptTurn {
    ScriptTurn::tool(
        id,
        "goal_done",
        r#"{"status":"complete","evidence":"Synthetic proof."}"#,
    )
}

struct Run {
    session: Session,
    engine: Arc<ScriptedEngine>,
    sink: VecSink,
}

fn run(oxa: serde_json::Value, turns: Vec<ScriptTurn>) -> Run {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(turns));
    let mut session = Session::open(
        deps(&env, engine.clone(), oxa, Arc::new(JobRegistry::new())),
        Open::New,
    )
    .unwrap();
    let mut sink = VecSink::default();
    session
        .prompt("go".into(), vec![], &mut sink, &mut HeadlessUi::default())
        .unwrap();
    std::mem::forget(env);
    Run {
        session,
        engine,
        sink,
    }
}

fn user_messages(engine: &ScriptedEngine, request: usize) -> usize {
    engine.requests()[request]
        .messages
        .iter()
        .filter(|m| m.role == Role::User)
        .count()
}

fn notices(sink: &VecSink, prefix: &str) -> usize {
    sink.0
        .iter()
        .filter(|e| matches!(e, AgentEvent::Notice { text, .. } if text.starts_with(prefix)))
        .count()
}

#[test]
fn edit_without_a_check_gets_one_nudge_then_ends_with_a_warning() {
    let r = run(
        json!({"envNote": false}),
        vec![
            write("c1", "a.txt"),
            ScriptTurn::text("done"),
            ScriptTurn::text("done again"),
        ],
    );
    let requests = r.engine.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(user_messages(&r.engine, 2), user_messages(&r.engine, 1) + 1);
    assert_eq!(r.session.stats().proved, Some(false));
    assert_eq!(notices(&r.sink, "Run ended without a passing check"), 1);
}

#[test]
fn edit_then_passing_check_needs_no_nudge() {
    let r = run(
        json!({"envNote": false}),
        vec![write("c1", "a.txt"), check("c2"), ScriptTurn::text("done")],
    );
    assert_eq!(r.engine.requests().len(), 3);
    assert_eq!(r.session.stats().proved, Some(true));
    assert_eq!(notices(&r.sink, "Run ended without"), 0);
}

#[test]
fn read_only_run_is_never_asked() {
    let r = run(
        json!({"envNote": false}),
        vec![ScriptTurn::text("nothing to change")],
    );
    assert_eq!(r.engine.requests().len(), 1);
    assert_eq!(r.session.stats().proved, None);
}

#[test]
fn prove_it_can_be_turned_off() {
    let r = run(
        json!({"envNote": false, "done": {"proveIt": false}}),
        vec![write("c1", "a.txt"), ScriptTurn::text("done")],
    );
    assert_eq!(r.engine.requests().len(), 2);
}

fn goal_run(turns: Vec<ScriptTurn>, job_secs: u32) -> (Run, Arc<JobRegistry>) {
    let env = TestEnv::new();
    let jobs = Arc::new(JobRegistry::new());
    jobs.set_sandbox(Arc::new(Direct));
    let id = jobs
        .spawn(ExecSpec {
            command: format!("sleep {job_secs}"),
            cwd: env.work(),
            writable: true,
            net: false,
            timeout_s: None,
            env: Vec::new(),
        })
        .unwrap();
    assert_eq!(id, "job-1");
    let engine = Arc::new(ScriptedEngine::new(turns));
    let mut session = Session::open(
        deps(
            &env,
            engine.clone(),
            json!({"envNote": false, "jobs": {"wakeOnFinish": false}}),
            jobs.clone(),
        ),
        Open::New,
    )
    .unwrap();
    let mut sink = VecSink::default();
    session
        .set_goal(
            Some(Goal::new("Finish the synthetic task.".into(), GoalBudget::default(), 1000).unwrap()),
            &mut sink,
        )
        .unwrap();
    session
        .prompt("go".into(), vec![], &mut sink, &mut HeadlessUi::default())
        .unwrap();
    std::mem::forget(env);
    (
        Run {
            session,
            engine,
            sink,
        },
        jobs,
    )
}

#[test]
fn goal_done_with_a_running_job_waits_one_turn_for_it() {
    let (r, jobs) = goal_run(
        vec![
            goal_done("d1"),
            ScriptTurn::tool("w1", "bash", &json!({"job": "job-1", "wait_s": 10}).to_string()),
            goal_done("d2"),
        ],
        1,
    );
    let requests = r.engine.requests();
    assert_eq!(requests.len(), 3);
    let nudge = requests[1]
        .messages
        .iter()
        .rev()
        .find(|m| m.role == Role::User)
        .and_then(|m| m.content.clone())
        .unwrap();
    assert!(nudge.contains("job-1"), "{nudge}");
    assert_eq!(r.session.goal().unwrap().status, GoalStatus::Done);
    assert_eq!(r.session.stats().jobs_left, 0);
    assert_eq!(notices(&r.sink, "Run ended with background jobs"), 0);
    jobs.kill_all();
}

#[test]
fn goal_done_twice_with_the_job_still_running_ends_with_a_record() {
    let (r, jobs) = goal_run(vec![goal_done("d1"), goal_done("d2")], 30);
    assert_eq!(r.engine.requests().len(), 2);
    assert_eq!(r.session.goal().unwrap().status, GoalStatus::Done);
    assert_eq!(r.session.stats().jobs_left, 1);
    assert_eq!(
        notices(&r.sink, "Run ended with background jobs still running: job-1"),
        1
    );
    jobs.kill_all();
}
