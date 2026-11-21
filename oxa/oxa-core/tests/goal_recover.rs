//! A goal never gets stuck in a state it cannot leave: any run that fails leaves it paused with
//! a reason and `Resume` continues it in the same session; a reply without a tool call is "no
//! work right now" (a nudge, then oxa waits itself) instead of a request loop.
use oxa_config::{Dirs, Settings};
use oxa_core::goal::GoalControl;
use oxa_core::{Deps, Handle, Open, RunOutcome, Session, ToolSelection};
use oxa_fake::{Fault, ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{AgentEvent, EventSink, HeadlessUi, Role, VecSink};
use oxa_resources::Resources;
use oxa_sandbox::NoSandbox;
use oxa_session::{Goal, GoalBudget, GoalStatus, MemStore, SessionStore};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// `goal` is the `oxa.goal` settings object. Agent-level retries are off.
fn deps(
    env: &TestEnv,
    store: Arc<dyn SessionStore>,
    engine: Arc<ScriptedEngine>,
    goal: serde_json::Value,
) -> Deps {
    Deps {
        engine,
        store,
        tools: Vec::new(),
        hooks: Vec::new(),
        policy: Box::new(ModePolicy::new()),
        sandbox: Arc::new(NoSandbox),
        settings: Settings::from_values(
            // A goal with no budget of its own pauses on idle by default (`oxa.onExceed.goalIdle`);
            // these tests are about the goals that only warn.
            serde_json::json!({
                "oxa": {"envNote": false, "goal": goal, "onExceed": {"goalIdle": "warn"}},
                "retry": {"enabled": false}
            }),
            serde_json::json!({}),
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

fn objective(budget: GoalBudget) -> Goal {
    Goal::new("Finish the synthetic task.".into(), budget, 1000).unwrap()
}

fn finish() -> ScriptTurn {
    ScriptTurn::tool(
        "finish",
        "goal_done",
        r#"{"status":"complete","evidence":"Synthetic proof."}"#,
    )
}

fn bad_gateway() -> ScriptTurn {
    ScriptTurn::fault(Fault::Http {
        status: 502,
        body: "bad gateway".into(),
    })
}

fn last_user(engine: &ScriptedEngine, request: usize) -> String {
    engine.requests()[request]
        .messages
        .iter()
        .rev()
        .find(|m| m.role == Role::User)
        .and_then(|m| m.content.clone())
        .unwrap_or_default()
}

fn notices(sink: &VecSink, prefix: &str) -> usize {
    sink.0
        .iter()
        .filter(|e| matches!(e, AgentEvent::Notice { text, .. } if text.starts_with(prefix)))
        .count()
}

#[test]
fn a_failed_run_records_why_and_resume_continues_in_the_same_session() {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(vec![bad_gateway(), finish()]));
    let store = Arc::new(MemStore::new());
    let mut session = Session::open(
        deps(
            &env,
            store.clone(),
            engine.clone(),
            serde_json::json!({"failureBackoffSeconds": 0}),
        ),
        Open::New,
    )
    .unwrap();
    let mut sink = VecSink::default();
    session
        .set_goal(Some(objective(GoalBudget::default())), &mut sink)
        .unwrap();
    let id = session.state().session_id;
    session
        .prompt("Start".into(), vec![], &mut sink, &mut HeadlessUi::default())
        .unwrap();
    assert!(matches!(session.stats().outcome, RunOutcome::Failed(_)));
    // The goal stays pursuing and the run's failure is recorded, code only.
    assert_eq!(session.goal().unwrap().status, GoalStatus::Pursuing);
    assert_eq!(notices(&sink, "Goal: the run failed ("), 1);

    let bound = session.inner().sref.clone();
    let revision = session.goal_revision();
    session
        .control_goal(&bound, revision, GoalControl::Resume, &mut sink)
        .unwrap();
    assert_eq!(session.goal().unwrap().status, GoalStatus::Pursuing);
    session
        .prompt(
            oxa_core::prompt::GOAL_CONTINUE.into(),
            vec![],
            &mut sink,
            &mut HeadlessUi::default(),
        )
        .unwrap();
    assert_eq!(session.goal().unwrap().status, GoalStatus::Done);
    assert_eq!(session.state().session_id, id);
    assert_eq!(store.list(None).len(), 1);
}

#[test]
fn resume_works_from_pursuing_and_is_refused_for_done_canceled_and_exhausted_goals() {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(vec![]));
    let mut session = Session::open(
        deps(&env, Arc::new(MemStore::new()), engine, serde_json::json!({})),
        Open::New,
    )
    .unwrap();
    let mut sink = VecSink::default();
    let bound = session.inner().sref.clone();

    session
        .set_goal(Some(objective(GoalBudget::default())), &mut sink)
        .unwrap();
    let revision = session.goal_revision();
    session
        .control_goal(&bound, revision, GoalControl::Resume, &mut sink)
        .expect("a pursuing goal with no run can be resumed");

    let mut done = objective(GoalBudget::default());
    done.finish(GoalStatus::Done, "Synthetic proof.".into()).unwrap();
    session.set_goal(Some(done), &mut sink).unwrap();
    let revision = session.goal_revision();
    assert!(session
        .control_goal(&bound, revision, GoalControl::Resume, &mut sink)
        .is_err());

    let mut canceled = objective(GoalBudget::default());
    canceled.cancel().unwrap();
    session.set_goal(Some(canceled), &mut sink).unwrap();
    let revision = session.goal_revision();
    assert!(session
        .control_goal(&bound, revision, GoalControl::Resume, &mut sink)
        .is_err());

    let mut exhausted = objective(GoalBudget {
        max_turns: Some(1),
        ..GoalBudget::default()
    });
    exhausted.turns = 1;
    exhausted.pause();
    session.set_goal(Some(exhausted), &mut sink).unwrap();
    let revision = session.goal_revision();
    assert_eq!(
        session
            .control_goal(&bound, revision, GoalControl::Resume, &mut sink)
            .unwrap_err()
            .code,
        "goal_budget_exhausted"
    );
}

#[test]
fn a_schema_answer_is_recorded_and_the_goal_goes_on_to_wait_and_finish() {
    let env = TestEnv::new();
    let answer = r#"{"ok":"yes"}"#;
    let engine = Arc::new(ScriptedEngine::new(vec![
        ScriptTurn::text(answer),
        ScriptTurn::tool("w1", "wait", r#"{"seconds":1,"reason":"Nothing to do yet"}"#),
        finish(),
        ScriptTurn::text(answer),
    ]));
    let mut session = Session::open(
        deps(
            &env,
            Arc::new(MemStore::new()),
            engine.clone(),
            serde_json::json!({}),
        ),
        Open::New,
    )
    .unwrap();
    session.set_output_schema(Some(
        oxa_core::output_schema::OutputSchema::new(serde_json::json!({
            "type": "object",
            "properties": {"ok": {"type": "string"}},
            "required": ["ok"]
        }))
        .unwrap(),
    ));
    let mut sink = VecSink::default();
    session
        .set_goal(Some(objective(GoalBudget::default())), &mut sink)
        .unwrap();
    session
        .prompt("Start".into(), vec![], &mut sink, &mut HeadlessUi::default())
        .unwrap();
    assert!(
        engine.requests()[1].messages.iter().any(|m| m.role == Role::User
            && m.content.as_deref() == Some(oxa_core::prompt::GOAL_CONTINUE_AFTER_ANSWER)),
        "the answer is followed by the after-answer continuation"
    );
    assert_eq!(session.goal().unwrap().status, GoalStatus::Done);
    assert_eq!(session.stats().outcome, RunOutcome::Done);
}

#[test]
fn an_idle_goal_is_nudged_once_then_waits_between_requests() {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(vec![
        ScriptTurn::text("Idle"),
        ScriptTurn::text("Still idle"),
        ScriptTurn::text("Idle again"),
        finish(),
    ]));
    let mut session = Session::open(
        deps(
            &env,
            Arc::new(MemStore::new()),
            engine.clone(),
            serde_json::json!({"idleBackoffSeconds": 1}),
        ),
        Open::New,
    )
    .unwrap();
    let mut sink = VecSink::default();
    session
        .set_goal(Some(objective(GoalBudget::default())), &mut sink)
        .unwrap();
    let started = Instant::now();
    session
        .prompt("Start".into(), vec![], &mut sink, &mut HeadlessUi::default())
        .unwrap();
    assert!(
        started.elapsed() >= Duration::from_secs(1),
        "the idle wait is real"
    );
    assert_eq!(engine.requests().len(), 4);
    assert_eq!(last_user(&engine, 1), oxa_core::prompt::GOAL_CONTINUE);
    assert_eq!(last_user(&engine, 2), oxa_core::prompt::GOAL_IDLE_NUDGE);
    assert_eq!(last_user(&engine, 3), oxa_core::prompt::GOAL_CONTINUE);
    assert_eq!(notices(&sink, "Goal idle:"), 1);
    assert_eq!(session.goal().unwrap().status, GoalStatus::Done);
}

/// Queues a message as soon as the idle wait is announced.
struct SteerOnIdle {
    handle: Handle,
    sink: VecSink,
    fired: bool,
}

impl EventSink for SteerOnIdle {
    fn emit(&mut self, event: &AgentEvent) {
        if let AgentEvent::Notice { text, .. } = event {
            if !self.fired && text.starts_with("Goal idle:") {
                self.fired = true;
                self.handle.follow_up("hello from the owner".into());
            }
        }
        self.sink.emit(event);
    }
}

#[test]
fn a_message_queued_during_the_idle_wait_ends_it() {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(vec![
        ScriptTurn::text("Idle"),
        ScriptTurn::text("Still idle"),
        ScriptTurn::text("Idle again"),
        finish(),
    ]));
    let mut session = Session::open(
        deps(
            &env,
            Arc::new(MemStore::new()),
            engine.clone(),
            serde_json::json!({"idleBackoffSeconds": 30}),
        ),
        Open::New,
    )
    .unwrap();
    session
        .set_goal(Some(objective(GoalBudget::default())), &mut VecSink::default())
        .unwrap();
    let mut sink = SteerOnIdle {
        handle: session.handle(),
        sink: VecSink::default(),
        fired: false,
    };
    let started = Instant::now();
    session
        .prompt("Start".into(), vec![], &mut sink, &mut HeadlessUi::default())
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "the queued message ends the wait"
    );
    assert_eq!(last_user(&engine, 3), "hello from the owner");
}

#[test]
fn no_idle_wait_when_the_owner_turns_it_off_and_idle_pause_still_pauses() {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(vec![
        ScriptTurn::text("a"),
        ScriptTurn::text("b"),
        ScriptTurn::text("c"),
        finish(),
    ]));
    let mut session = Session::open(
        deps(
            &env,
            Arc::new(MemStore::new()),
            engine.clone(),
            serde_json::json!({"idleBackoffSeconds": 0}),
        ),
        Open::New,
    )
    .unwrap();
    let mut sink = VecSink::default();
    session
        .set_goal(Some(objective(GoalBudget::default())), &mut sink)
        .unwrap();
    let started = Instant::now();
    session
        .prompt("Start".into(), vec![], &mut sink, &mut HeadlessUi::default())
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(notices(&sink, "Goal idle:"), 0);
    assert_eq!(session.goal().unwrap().status, GoalStatus::Done);

    let engine = Arc::new(ScriptedEngine::new(vec![
        ScriptTurn::text("a"),
        ScriptTurn::text("b"),
    ]));
    let mut session = Session::open(
        deps(
            &env,
            Arc::new(MemStore::new()),
            engine,
            serde_json::json!({"idlePause": true}),
        ),
        Open::New,
    )
    .unwrap();
    let mut sink = VecSink::default();
    session
        .set_goal(Some(objective(GoalBudget::default())), &mut sink)
        .unwrap();
    session
        .prompt("Start".into(), vec![], &mut sink, &mut HeadlessUi::default())
        .unwrap();
    assert_eq!(session.goal().unwrap().status, GoalStatus::Paused);
}
