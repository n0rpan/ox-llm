//! The finish decision: before a run or goal ends, oxa asks once for proof of an edit, once
//! about background jobs and once about a tool the task names for its acceptance that never ran,
//! then lets the run end with a warning and a record (never a block).
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

fn notices(sink: &VecSink, prefix: &str) -> usize {
    sink.0
        .iter()
        .filter(|e| matches!(e, AgentEvent::Notice { text, .. } if text.starts_with(prefix)))
        .count()
}

#[test]
fn edit_without_a_check_gets_one_nudge_then_ends_with_a_warning() {
    let r = run(
        json!({"envNote": false, "done": {"proveIt": true}}),
        vec![
            write("c1", "a.txt"),
            ScriptTurn::text("done"),
            ScriptTurn::text("done again"),
        ],
    );
    let requests = r.engine.requests();
    assert_eq!(requests.len(), 3);
    // The extra turn is a user message after the first final reply.
    assert_eq!(requests[1].messages.last().map(|m| m.role), Some(Role::Tool));
    assert_eq!(requests[2].messages.last().map(|m| m.role), Some(Role::User));
    assert_eq!(r.session.stats().proved, Some(false));
    assert_eq!(notices(&r.sink, "Run ended without a passing check"), 1);
}

#[test]
fn edit_then_passing_check_needs_no_nudge() {
    let r = run(
        json!({"envNote": false, "done": {"proveIt": true}}),
        vec![write("c1", "a.txt"), check("c2"), ScriptTurn::text("done")],
    );
    assert_eq!(r.engine.requests().len(), 3);
    assert_eq!(r.session.stats().proved, Some(true));
    assert_eq!(notices(&r.sink, "Run ended without"), 0);
}

#[test]
fn read_only_run_is_never_asked() {
    let r = run(
        json!({"envNote": false, "done": {"proveIt": true}}),
        vec![ScriptTurn::text("nothing to change")],
    );
    assert_eq!(r.engine.requests().len(), 1);
    assert_eq!(r.session.stats().proved, None);
}

#[test]
fn prove_it_is_off_by_default_and_can_be_turned_off() {
    let r = run(
        json!({"envNote": false}),
        vec![write("c1", "a.txt"), ScriptTurn::text("done")],
    );
    assert_eq!(r.engine.requests().len(), 2);
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

/// A session that starts its run with `task` and follows `turns` (T6 tests below).
fn toolchain_run(task: &str, turns: Vec<ScriptTurn>) -> Run {
    toolchain_run_with(json!({"envNote": false}), task, turns)
}

fn toolchain_run_with(oxa: serde_json::Value, task: &str, turns: Vec<ScriptTurn>) -> Run {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(turns));
    let mut session = Session::open(
        deps(&env, engine.clone(), oxa, Arc::new(JobRegistry::new())),
        Open::New,
    )
    .unwrap();
    let mut sink = VecSink::default();
    session
        .prompt(task.into(), vec![], &mut sink, &mut HeadlessUi::default())
        .unwrap();
    std::mem::forget(env);
    Run {
        session,
        engine,
        sink,
    }
}

fn last_user(request: &oxa_fake::Recorded) -> String {
    request
        .messages
        .iter()
        .rev()
        .find(|m| m.role == Role::User)
        .and_then(|m| m.content.clone())
        .unwrap_or_default()
}

/// The turns that asked about the task's own acceptance tool.
fn toolchain_questions(requests: &[oxa_fake::Recorded]) -> Vec<String> {
    requests
        .iter()
        .map(last_user)
        .filter(|text| text.starts_with("This task's acceptance names"))
        .collect()
}

/// The task names the tool its acceptance runs through, and the run checks with a stand-in
/// instead. One question shows the failed call, and the record keeps the tool.
#[test]
fn a_task_tool_that_never_ran_is_asked_once_and_recorded() {
    let r = toolchain_run(
        "Make check.py pass. The grader applies the pattern with Python 3.12.",
        vec![
            write("w1", "check.py"),
            ScriptTurn::tool(
                "b1",
                "bash",
                &json!({"command": "command -v python3.12"}).to_string(),
            ),
            check("c1"),
            ScriptTurn::text("done"),
            ScriptTurn::text("done, the substitute equivalent passed"),
        ],
    );
    let questions = toolchain_questions(&r.engine.requests());
    assert_eq!(questions.len(), 1, "{questions:?}");
    assert!(questions[0].contains("names python3.12"), "{}", questions[0]);
    assert!(
        questions[0].contains("command -v python3.12"),
        "the attempt is shown: {}",
        questions[0]
    );
    assert_eq!(r.session.stats().verifier.as_deref(), Some("python3.12"));
    assert_eq!(
        notices(&r.sink, "Run ended without running the tool this task names"),
        1
    );
}

/// The task's tool runs as the check itself: the run has its evidence, so nothing is asked.
#[test]
fn running_the_task_tool_as_the_check_ends_the_run_without_a_question() {
    let r = toolchain_run(
        "Make check.py pass. The grader applies the pattern with Python 3.12.",
        vec![
            write("w1", "check.py"),
            ScriptTurn::tool(
                "b1",
                "bash",
                &json!({"command": "python3() { echo 'test result: ok. 1 passed; 0 failed'; }; python3 -m pytest"})
                    .to_string(),
            ),
            ScriptTurn::text("done"),
        ],
    );
    assert!(toolchain_questions(&r.engine.requests()).is_empty());
    assert_eq!(r.session.stats().verifier, None);
    assert_eq!(notices(&r.sink, "Run ended without running the tool"), 0);
}

#[test]
fn a_task_that_names_no_tool_is_never_asked() {
    let r = toolchain_run(
        "Explain how the cache works and fix the typo. Then run the project's own tests.",
        vec![write("w1", "a.txt"), ScriptTurn::text("done")],
    );
    assert_eq!(r.engine.requests().len(), 2);
    assert_eq!(r.session.stats().verifier, None);
    assert_eq!(notices(&r.sink, "Run ended without running the tool"), 0);
}

#[test]
fn a_goal_pursuit_is_no_task_text_for_the_tool_check() {
    // The goal text names python3 and sits in the conversation; what opens a pursuit is the
    // model's own work, so no finish question comes from it.
    let (r, jobs) = goal_run(
        vec![
            write("w1", "a.txt"),
            ScriptTurn::text("one"),
            ScriptTurn::text("two"),
        ],
        30,
    );
    assert_eq!(r.session.stats().verifier, None);
    assert_eq!(notices(&r.sink, "Run ended without running the tool"), 0);
    jobs.kill_all();
}

/// `oxa.done.proveIt` on: the task's own tool never ran, so its question stands in for the edit
/// proof and is asked once (T6).
#[test]
fn with_prove_it_the_unrun_task_tool_is_asked_once() {
    let r = toolchain_run_with(
        json!({"envNote": false, "done": {"proveIt": true}}),
        "Make check.py pass. The grader applies the pattern with Python 3.12.",
        vec![
            write("w1", "check.py"),
            ScriptTurn::tool(
                "b1",
                "bash",
                &json!({"command": "command -v python3.12"}).to_string(),
            ),
            check("c1"),
            ScriptTurn::text("done"),
            ScriptTurn::text("done, the substitute equivalent passed"),
        ],
    );
    let questions = toolchain_questions(&r.engine.requests());
    assert_eq!(questions.len(), 1, "{questions:?}");
    assert!(questions[0].contains("names python3.12"), "{}", questions[0]);
    // The passing check proves the edit; the question stands in for the proof, so 2 turns.
    assert_eq!(r.session.stats().proved, Some(true));
    assert_eq!(r.session.stats().verifier.as_deref(), Some("python3.12"));
}
