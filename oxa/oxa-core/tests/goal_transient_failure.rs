//! R1: a pursuing goal survives a transient failed request. It notes the failure, waits without
//! a model request (`oxa.goal.failureBackoffSeconds`, doubling, no attempt cap) and sends the same
//! request again. Auth errors, aborts and an owner time budget still stop the goal with the reason.
use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Handle, Open, RunOutcome, Session, ToolSelection};
use oxa_fake::{Fault, ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{codes, AgentEvent, EventSink, HeadlessUi, VecSink};
use oxa_resources::Resources;
use oxa_sandbox::NoSandbox;
use oxa_session::{Goal, GoalBudget, GoalStatus, MemStore, SessionStore};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// `backoff` sets `oxa.goal.failureBackoffSeconds`; the default applies when it is `None`.
/// Agent-level retries are off, so a scripted fault is the request's final answer.
fn deps(
    env: &TestEnv,
    store: Arc<dyn SessionStore>,
    engine: Arc<ScriptedEngine>,
    backoff: Option<u64>,
) -> Deps {
    let goal = match backoff {
        Some(seconds) => serde_json::json!({ "failureBackoffSeconds": seconds }),
        None => serde_json::json!({}),
    };
    Deps {
        engine,
        store,
        tools: Vec::new(),
        hooks: Vec::new(),
        policy: Box::new(ModePolicy::new()),
        sandbox: Arc::new(NoSandbox),
        settings: Settings::from_values(
            serde_json::json!({"oxa": {"envNote": false, "goal": goal}, "retry": {"enabled": false}}),
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

fn busy() -> ScriptTurn {
    ScriptTurn::fault(Fault::Http {
        status: 503,
        body: "queue full".into(),
    })
}

fn finish() -> ScriptTurn {
    ScriptTurn::tool(
        "finish",
        "goal_done",
        r#"{"status":"complete","evidence":"Synthetic proof."}"#,
    )
}

fn waits(sink: &VecSink) -> Vec<String> {
    sink.0
        .iter()
        .filter_map(|event| match event {
            AgentEvent::Notice { text, .. } if text.starts_with("Goal waiting") => Some(text.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn goal_run_continues_after_transient_failure() {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(vec![busy(), finish()]));
    let store = Arc::new(MemStore::new());
    let mut session = Session::open(deps(&env, store, engine.clone(), Some(1)), Open::New).unwrap();
    let mut sink = VecSink::default();
    session
        .set_goal(Some(objective(GoalBudget::default())), &mut sink)
        .unwrap();
    session
        .prompt("Start".into(), vec![], &mut sink, &mut HeadlessUi::default())
        .unwrap();
    assert_eq!(session.stats().outcome, RunOutcome::Done);
    assert_eq!(session.goal().unwrap().status, GoalStatus::Done);
    // The failed request is sent again once, after the one-second wait.
    assert_eq!(engine.requests().len(), 2);
    assert_eq!(waits(&sink), vec!["Goal waiting 1 s after a transient request failure (engine_busy); no request is sent meanwhile, then the goal continues.".to_string()]);
}

#[test]
fn goal_failure_backoff_doubles_per_consecutive_failure() {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(vec![busy(), busy(), finish()]));
    let store = Arc::new(MemStore::new());
    let mut session = Session::open(deps(&env, store, engine.clone(), Some(1)), Open::New).unwrap();
    let mut sink = VecSink::default();
    session
        .set_goal(Some(objective(GoalBudget::default())), &mut sink)
        .unwrap();
    session
        .prompt("Start".into(), vec![], &mut sink, &mut HeadlessUi::default())
        .unwrap();
    assert_eq!(session.stats().outcome, RunOutcome::Done);
    assert_eq!(engine.requests().len(), 3);
    let notes = waits(&sink);
    assert_eq!(notes.len(), 2);
    assert!(notes[0].starts_with("Goal waiting 1 s "), "{notes:?}");
    assert!(notes[1].starts_with("Goal waiting 2 s "), "{notes:?}");
}

/// Aborts the run from the sink as soon as the wait is announced.
struct AbortOnWait {
    handle: Handle,
    fired: bool,
}

impl EventSink for AbortOnWait {
    fn emit(&mut self, event: &AgentEvent) {
        if let AgentEvent::Notice { text, .. } = event {
            if !self.fired && text.starts_with("Goal waiting") {
                self.fired = true;
                self.handle.abort();
            }
        }
    }
}

#[test]
fn goal_failure_backoff_makes_no_request_and_is_cancellable() {
    let env = TestEnv::new();
    // The default wait is 60 s: finishing well before that shows the abort ended it.
    let engine = Arc::new(ScriptedEngine::new(vec![busy()]));
    let store = Arc::new(MemStore::new());
    let mut session = Session::open(deps(&env, store, engine.clone(), None), Open::New).unwrap();
    let mut sink = AbortOnWait {
        handle: session.handle(),
        fired: false,
    };
    session
        .set_goal(Some(objective(GoalBudget::default())), &mut VecSink::default())
        .unwrap();
    let started = Instant::now();
    session
        .prompt("Start".into(), vec![], &mut sink, &mut HeadlessUi::default())
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "the abort ends the wait"
    );
    assert_eq!(session.stats().outcome, RunOutcome::Interrupted);
    assert_eq!(engine.requests().len(), 1, "no request is sent during the wait");
    assert_eq!(session.goal().unwrap().status, GoalStatus::Pursuing);
}

#[test]
fn auth_failure_stops_goal_with_reason() {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(vec![ScriptTurn::fault(Fault::Http {
        status: 401,
        body: "invalid credentials".into(),
    })]));
    let store = Arc::new(MemStore::new());
    let mut session = Session::open(deps(&env, store.clone(), engine.clone(), Some(1)), Open::New).unwrap();
    let mut sink = VecSink::default();
    session
        .set_goal(Some(objective(GoalBudget::default())), &mut sink)
        .unwrap();
    session
        .prompt("Start".into(), vec![], &mut sink, &mut HeadlessUi::default())
        .unwrap();
    assert_eq!(session.stats().outcome, RunOutcome::Failed(codes::ENGINE_HTTP));
    assert_eq!(engine.requests().len(), 1, "an auth failure is not sent again");
    assert!(waits(&sink).is_empty());
    let persisted = format!("{:?}", store.load(&session.inner().sref).unwrap());
    assert!(
        persisted.contains("HTTP 401"),
        "the reason is kept in the session"
    );
}

#[test]
fn run_without_goal_still_ends_on_failure() {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(vec![busy()]));
    let store = Arc::new(MemStore::new());
    let mut session = Session::open(deps(&env, store, engine.clone(), Some(1)), Open::New).unwrap();
    let mut sink = VecSink::default();
    session
        .prompt("Start".into(), vec![], &mut sink, &mut HeadlessUi::default())
        .unwrap();
    assert_eq!(session.stats().outcome, RunOutcome::Failed(codes::ENGINE_BUSY));
    assert_eq!(engine.requests().len(), 1);
    assert!(waits(&sink).is_empty());
}

#[test]
fn owner_time_budget_that_ends_during_the_wait_stops_the_goal() {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(vec![busy()]));
    let store = Arc::new(MemStore::new());
    let mut session = Session::open(deps(&env, store, engine.clone(), Some(60)), Open::New).unwrap();
    let mut sink = VecSink::default();
    let budget = GoalBudget {
        max_time_ms: Some(1_000),
        ..GoalBudget::default()
    };
    session.set_goal(Some(objective(budget)), &mut sink).unwrap();
    session
        .prompt("Start".into(), vec![], &mut sink, &mut HeadlessUi::default())
        .unwrap();
    assert_eq!(session.stats().outcome, RunOutcome::Failed(codes::ENGINE_BUSY));
    assert_eq!(
        engine.requests().len(),
        1,
        "a wait the budget cannot cover is not taken"
    );
    assert!(waits(&sink).is_empty());
}

#[test]
fn backoff_zero_opts_out() {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(vec![busy()]));
    let store = Arc::new(MemStore::new());
    let mut session = Session::open(deps(&env, store, engine.clone(), Some(0)), Open::New).unwrap();
    let mut sink = VecSink::default();
    session
        .set_goal(Some(objective(GoalBudget::default())), &mut sink)
        .unwrap();
    session
        .prompt("Start".into(), vec![], &mut sink, &mut HeadlessUi::default())
        .unwrap();
    assert_eq!(session.stats().outcome, RunOutcome::Failed(codes::ENGINE_BUSY));
    assert_eq!(engine.requests().len(), 1);
}
