use oxa_config::{Dirs, Settings};
use oxa_core::{goal::GoalControl, Deps, Open, RunOutcome, Session, ToolSelection};
use oxa_fake::{ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{HeadlessUi, ToolCall, VecSink};
use oxa_resources::Resources;
use oxa_sandbox::NoSandbox;
use oxa_session::{FsStore, Goal, GoalBudget, GoalStatus, MemStore, SessionStore};
use std::sync::Arc;
fn deps(env: &TestEnv, store: Arc<dyn SessionStore>, engine: Arc<ScriptedEngine>) -> Deps {
    Deps {
        engine,
        store,
        tools: Vec::new(),
        hooks: Vec::new(),
        policy: Box::new(ModePolicy::new()),
        sandbox: Arc::new(NoSandbox),
        // Exact prompt-byte fixtures select note-free queues/RPC/replay.
        settings: Settings::from_values(
            serde_json::json!({"oxa": {"envNote": false}}),
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
    Goal::new("Review synthetic work and wait for more.".into(), budget, 1000).unwrap()
}

#[test]
fn native_goal_control_rejects_stale_requests_and_persists_terminal_cancel() {
    let env = TestEnv::new();
    let root = env.root().join("sessions");
    let store = Arc::new(FsStore::new(root.clone()));
    let engine = Arc::new(ScriptedEngine::new(vec![]));
    let mut session = Session::open(deps(&env, store, engine.clone()), Open::New).unwrap();
    let mut sink = VecSink::default();
    let mut ui = HeadlessUi::default();
    let saved = session.inner().sref.clone();
    assert_eq!(session.goal_revision(), 0);

    let mut goal = objective(GoalBudget {
        max_tokens: Some(100),
        ..GoalBudget::default()
    });
    goal.charge(Some(17), 20, true).unwrap();
    session.set_goal(Some(goal.clone()), &mut sink).unwrap();
    let original = session.goal_revision();

    let paused = session
        .control_goal(&saved, original, GoalControl::Pause, &mut sink)
        .unwrap();
    assert!(paused > original);
    assert_eq!(session.goal().unwrap().status, GoalStatus::Paused);
    assert_eq!(
        session
            .control_goal(&saved, original, GoalControl::Cancel, &mut sink)
            .unwrap_err()
            .code,
        "goal_revision_conflict"
    );
    assert_eq!(session.goal_revision(), paused);
    assert_eq!(session.goal().unwrap().status, GoalStatus::Paused);

    let resumed = session
        .control_goal(&saved, paused, GoalControl::Resume, &mut sink)
        .unwrap();
    assert!(resumed > paused);
    assert_eq!(session.goal().unwrap(), goal);
    let canceled = session
        .control_goal(&saved, resumed, GoalControl::Cancel, &mut sink)
        .unwrap();
    assert!(canceled > resumed);
    goal.status = GoalStatus::Canceled;
    assert_eq!(session.goal(), Some(goal.clone()));
    session.shutdown(&mut ui);
    drop(session);

    let store = Arc::new(FsStore::new(root));
    let mut session = Session::open(deps(&env, store, engine), Open::Resume(saved.clone())).unwrap();
    assert_eq!(session.goal(), Some(goal));
    assert_eq!(session.goal_revision(), canceled);
    for control in [GoalControl::Pause, GoalControl::Resume, GoalControl::Cancel] {
        assert!(session
            .control_goal(&saved, canceled, control, &mut sink)
            .is_err());
    }
    assert_eq!(session.goal_revision(), canceled);

    session.switch(Open::New, &mut sink, &mut ui).unwrap();
    let new_id = session.state().session_id;
    assert_ne!(saved.id, new_id);
    session
        .set_goal(Some(objective(GoalBudget::default())), &mut sink)
        .unwrap();
    let new_revision = session.goal_revision();
    assert_eq!(
        session
            .control_goal(&saved, new_revision, GoalControl::Cancel, &mut sink)
            .unwrap_err()
            .code,
        "goal_revision_conflict"
    );
    assert_eq!(session.goal_revision(), new_revision);
    assert_eq!(session.goal().unwrap().status, GoalStatus::Pursuing);
    session.shutdown(&mut ui);
}

#[test]
fn a_control_bound_to_a_different_file_rejects_an_equal_id_and_revision() {
    use oxa_core::goal::{SteeringBinding, SteeringLease};

    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(vec![]));
    let open = |name| {
        let mut deps = deps(
            &env,
            Arc::new(FsStore::new(env.root().join(name))),
            engine.clone(),
        );
        deps.session_id = Some("shared-id".into());
        Session::open(deps, Open::New).unwrap()
    };
    let mut sink = VecSink::default();
    let mut ui = HeadlessUi::default();
    let goal = objective(GoalBudget::default());
    let mut owner = open("original");
    owner.set_goal(Some(goal.clone()), &mut sink).unwrap();
    let observation = owner.goal_observation();
    let original = observation.session.clone();
    let revision = observation.revision;
    assert_eq!(observation.status, Some(GoalStatus::Pursuing));
    let lease = SteeringLease {
        holder: "fixture/worker-a".into(),
        generation: (1, 1),
    };
    let steer = SteeringBinding {
        session: original.clone(),
        revision,
        lease: Some(lease.clone()),
    };
    steer.validate(owner.inner(), Some(&lease)).unwrap();
    for changed in [
        SteeringLease {
            holder: "fixture/worker-b".into(),
            ..lease.clone()
        },
        SteeringLease {
            generation: (1, 2),
            ..lease.clone()
        },
    ] {
        assert_eq!(
            steer.validate(owner.inner(), Some(&changed)).unwrap_err().code,
            "goal_steer_stale"
        );
    }
    assert_eq!(
        steer.validate(owner.inner(), None).unwrap_err().code,
        "goal_steer_stale"
    );

    let mut other = open("second");
    other.set_goal(Some(goal.clone()), &mut sink).unwrap();
    let target = other.inner().sref.clone();
    assert_ne!(original.path, target.path);
    assert_eq!(original.id, target.id);
    assert_eq!(other.goal_revision(), revision);
    other.shutdown(&mut ui);
    drop(other);

    owner
        .switch(Open::Resume(target.clone()), &mut sink, &mut ui)
        .unwrap();
    let observation = owner.goal_observation();
    assert_eq!(observation.session, &target);
    assert_ne!(observation.session, &original);
    assert_eq!(observation.revision, revision);
    assert_eq!(observation.status, Some(GoalStatus::Pursuing));
    assert_eq!(
        steer.validate(owner.inner(), Some(&lease)).unwrap_err().code,
        "goal_steer_stale"
    );
    let result = owner.control_goal(&original, revision, GoalControl::Cancel, &mut sink);
    assert_eq!(result.unwrap_err().code, "goal_revision_conflict");
    assert_eq!(owner.goal_revision(), revision);
    assert_eq!(owner.goal(), Some(goal));
    owner.shutdown(&mut ui);
}

#[test]
fn ordinary_prefix_unchanged_and_two_idle_turns_stall_with_one_fixed_continuation() {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(vec![
        ScriptTurn::text("Idle"),
        ScriptTurn::text("Still idle"),
    ]));
    let store = Arc::new(MemStore::new());
    let mut session = Session::open(deps(&env, store.clone(), engine.clone()), Open::New).unwrap();
    let ordinary = session.inner().prefix.clone();
    let mut sink = VecSink::default();
    session
        .set_goal(Some(objective(GoalBudget::default())), &mut sink)
        .unwrap();
    let initial_revision = session.goal_revision();
    assert!(session.inner().active.get("goal_done").is_some());
    assert!(session.inner().active.get("wait").is_some());
    session
        .prompt(
            "Start the stored goal".into(),
            vec![],
            &mut sink,
            &mut HeadlessUi::default(),
        )
        .unwrap();
    assert_eq!(session.stats().outcome, RunOutcome::Stalled);
    assert!(session.goal_revision() > initial_revision);
    let goal = session.goal().unwrap();
    assert_eq!(goal.status, GoalStatus::Paused);
    assert_eq!(goal.turns, 2);
    assert!(goal.tokens_used > 0);
    let messages = session.inner().transcript.wire();
    assert_eq!(
        messages
            .iter()
            .filter(|m| m.content.as_deref() == Some(oxa_core::prompt::GOAL_CONTINUE))
            .count(),
        1
    );
    let sref = session.inner().sref.clone();
    session.shutdown(&mut HeadlessUi::default());
    drop(session);
    let mut resumed = Session::open(deps(&env, store, engine), Open::Resume(sref)).unwrap();
    assert_eq!(resumed.goal(), Some(goal));
    resumed.set_goal(None, &mut sink).unwrap();
    assert_eq!(resumed.inner().prefix, ordinary);
    assert!(resumed.inner().active.get("wait").is_none());
}
#[test]
fn done_closes_all_calls_without_running_later_calls_or_requesting_another_reply() {
    for (status, expected) in [
        ("complete", RunOutcome::Done),
        ("blocked", RunOutcome::Blocked("goal_blocked")),
    ] {
        let env = TestEnv::new();
        let mut turn = ScriptTurn::tool(
            "finish",
            "goal_done",
            &format!(r#"{{"status":"{status}","evidence":"Synthetic proof."}}"#),
        );
        turn.tool_calls.push(ToolCall {
            id: "later".into(),
            name: "wait".into(),
            args_raw: r#"{"seconds":3600,"reason":"Must never run"}"#.into(),
        });
        let engine = Arc::new(ScriptedEngine::new(vec![turn]));
        let store = Arc::new(MemStore::new());
        let mut session = Session::open(deps(&env, store.clone(), engine), Open::New).unwrap();
        let mut sink = VecSink::default();
        session
            .set_goal(Some(objective(GoalBudget::default())), &mut sink)
            .unwrap();
        session
            .prompt("Start".into(), vec![], &mut sink, &mut HeadlessUi::default())
            .unwrap();
        assert_eq!(session.stats().outcome, expected);
        let goal = session.goal().unwrap();
        assert_eq!(goal.evidence.as_deref(), Some("Synthetic proof."));
        let wire = session.inner().transcript.wire();
        assert!(wire.iter().any(|m| m.tool_call_id.as_deref() == Some("later")
            && m.content.as_deref().is_some_and(|s| s.contains("not executed"))));
        assert_eq!(
            Goal::latest(&store.load(&session.inner().sref).unwrap()).unwrap(),
            Some(goal)
        );
    }
}
#[test]
fn spent_token_or_turn_budget_can_cancel_but_cannot_resume() {
    for budget in [
        GoalBudget {
            max_tokens: Some(1),
            ..GoalBudget::default()
        },
        GoalBudget {
            max_turns: Some(1),
            ..GoalBudget::default()
        },
    ] {
        let env = TestEnv::new();
        let engine = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("Progress")]));
        let root = env.root().join("sessions");
        let store = Arc::new(FsStore::new(root.clone()));
        let mut session = Session::open(deps(&env, store, engine.clone()), Open::New).unwrap();
        let mut sink = VecSink::default();
        let mut ui = HeadlessUi::default();
        session.set_goal(Some(objective(budget)), &mut sink).unwrap();
        session
            .prompt("Start".into(), vec![], &mut sink, &mut ui)
            .unwrap();
        assert_eq!(session.stats().outcome, RunOutcome::Blocked("goal_blocked"));
        let mut expected = session.goal().unwrap();
        assert_eq!(expected.status, GoalStatus::Blocked);
        assert_eq!(expected.turns, 1);
        assert!(expected.exhausted());

        let bound = session.inner().sref.clone();
        let revision = session.goal_revision();
        assert!(session
            .control_goal(&bound, revision, GoalControl::Resume, &mut sink)
            .is_err());
        assert_eq!(session.goal_revision(), revision);
        let canceled_revision = session
            .control_goal(&bound, revision, GoalControl::Cancel, &mut sink)
            .expect("an exhausted goal can still be canceled");
        assert!(canceled_revision > revision);
        expected.status = GoalStatus::Canceled;
        assert_eq!(session.goal(), Some(expected.clone()));
        session.shutdown(&mut ui);
        drop(session);

        let store = Arc::new(FsStore::new(root));
        let mut reopened = Session::open(deps(&env, store, engine), Open::Resume(bound)).unwrap();
        assert_eq!(reopened.goal(), Some(expected));
        assert_eq!(reopened.goal_revision(), canceled_revision);
        reopened.shutdown(&mut ui);
    }
}

#[test]
fn repeated_completed_waits_are_work_not_stalls_and_charge_wall_time() {
    let env = TestEnv::new();
    let mut turns: Vec<_> = (0..7)
        .map(|i| {
            ScriptTurn::tool(
                &format!("wait{i}"),
                "wait",
                r#"{"seconds":1,"reason":"Await synthetic work"}"#,
            )
        })
        .collect();
    turns.push(ScriptTurn::tool(
        "done",
        "goal_done",
        r#"{"status":"complete","evidence":"All waits completed."}"#,
    ));
    let engine = Arc::new(ScriptedEngine::new(turns));
    let mut session = Session::open(deps(&env, Arc::new(MemStore::new()), engine), Open::New).unwrap();
    let mut sink = VecSink::default();
    session
        .set_goal(Some(objective(GoalBudget::default())), &mut sink)
        .unwrap();
    session
        .prompt("Start".into(), vec![], &mut sink, &mut HeadlessUi::default())
        .unwrap();
    assert_eq!(session.stats().outcome, RunOutcome::Done);
    let goal = session.goal().unwrap();
    assert_eq!(goal.turns, 8);
    assert!(goal.elapsed_ms >= 7000);
}

#[test]
fn wait_is_cancellable_and_elapsed_budget_caps_it() {
    use oxa_proto::{AgentEvent, EventSink};
    struct AbortWait(oxa_core::Handle);
    impl EventSink for AbortWait {
        fn emit(&mut self, event: &AgentEvent) {
            if matches!(event, AgentEvent::ToolExecutionUpdate { tool_name, .. } if tool_name == "wait") {
                self.0.abort();
            }
        }
    }
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(vec![ScriptTurn::tool(
        "wait",
        "wait",
        r#"{"seconds":3600,"reason":"Cancellation test"}"#,
    )]));
    let mut session = Session::open(deps(&env, Arc::new(MemStore::new()), engine), Open::New).unwrap();
    session
        .set_goal(Some(objective(GoalBudget::default())), &mut VecSink::default())
        .unwrap();
    let start = std::time::Instant::now();
    session
        .prompt(
            "Start".into(),
            vec![],
            &mut AbortWait(session.handle()),
            &mut HeadlessUi::default(),
        )
        .unwrap();
    assert_eq!(session.stats().outcome, RunOutcome::Interrupted);
    assert!(start.elapsed() < std::time::Duration::from_secs(2));
    assert_eq!(session.goal().unwrap().status, GoalStatus::Pursuing);

    let engine = Arc::new(ScriptedEngine::new(vec![ScriptTurn::tool(
        "wait",
        "wait",
        r#"{"seconds":3600,"reason":"Budget test"}"#,
    )]));
    let mut session = Session::open(deps(&env, Arc::new(MemStore::new()), engine), Open::New).unwrap();
    session
        .set_goal(
            Some(objective(GoalBudget {
                max_time_ms: Some(100),
                ..GoalBudget::default()
            })),
            &mut VecSink::default(),
        )
        .unwrap();
    session
        .prompt(
            "Start".into(),
            vec![],
            &mut VecSink::default(),
            &mut HeadlessUi::default(),
        )
        .unwrap();
    assert_eq!(session.stats().outcome, RunOutcome::Blocked("goal_blocked"));
    assert!(session.goal().unwrap().elapsed_ms >= 100);
}

#[test]
fn goal_done_cannot_bypass_project_done_checks() {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(vec![
        ScriptTurn::tool(
            "edit",
            "write",
            r#"{"path":"result.txt","content":"Synthetic result"}"#,
        ),
        ScriptTurn::tool(
            "done",
            "goal_done",
            r#"{"status":"complete","evidence":"Model claims completion."}"#,
        ),
    ]));
    let mut dependencies = deps(&env, Arc::new(MemStore::new()), engine);
    dependencies.tools = oxa_tools::builtin_tools();
    dependencies.settings = Settings::from_values(
        serde_json::json!({"done":["exit 1"], "oxa":{"envNote":false}}),
        serde_json::json!({}),
    );
    let mut session = Session::open(dependencies, Open::New).unwrap();
    let mut sink = VecSink::default();
    session
        .set_goal(Some(objective(GoalBudget::default())), &mut sink)
        .unwrap();
    session
        .prompt("Start".into(), vec![], &mut sink, &mut HeadlessUi::default())
        .unwrap();
    assert_eq!(session.stats().outcome, RunOutcome::Failed("done_checks_failed"));
    assert_eq!(session.goal().unwrap().status, GoalStatus::Blocked);
}

#[test]
fn budget_exhausted_by_wait_still_runs_checks_for_earlier_edits() {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(vec![
        ScriptTurn::tool(
            "edit",
            "write",
            r#"{"path":"result.txt","content":"Synthetic result"}"#,
        ),
        ScriptTurn::tool(
            "wait",
            "wait",
            r#"{"seconds":1,"reason":"Spend remaining synthetic budget"}"#,
        ),
    ]));
    let mut dependencies = deps(&env, Arc::new(MemStore::new()), engine);
    dependencies.tools = oxa_tools::builtin_tools();
    dependencies.settings = Settings::from_values(
        serde_json::json!({"done":["exit 1"], "oxa":{"envNote":false}}),
        serde_json::json!({}),
    );
    let mut session = Session::open(dependencies, Open::New).unwrap();
    let mut sink = VecSink::default();
    session
        .set_goal(
            Some(objective(GoalBudget {
                max_time_ms: Some(100),
                ..GoalBudget::default()
            })),
            &mut sink,
        )
        .unwrap();
    session
        .prompt("Start".into(), vec![], &mut sink, &mut HeadlessUi::default())
        .unwrap();
    assert_eq!(session.stats().outcome, RunOutcome::Failed("done_checks_failed"));
    assert!(session.stats().done_failed);
    assert_eq!(session.goal().unwrap().status, GoalStatus::Blocked);
}

#[test]
fn goal_completion_obeys_the_existing_final_answer_schema_repair() {
    use oxa_core::output_schema::OutputSchema;
    for (initial, repair, accepted) in [
        (Some(r#"{"done":true}"#), None, true),
        (None, Some(r#"{"done":true}"#), true),
        (None, Some("still invalid"), false),
    ] {
        let env = TestEnv::new();
        let mut complete = ScriptTurn::tool(
            "done",
            "goal_done",
            r#"{"status":"complete","evidence":"Synthetic completion proof."}"#,
        );
        complete.text = initial.map(str::to_owned);
        let mut script = vec![complete];
        if let Some(text) = repair {
            script.push(ScriptTurn::text(text));
        }
        let engine = Arc::new(ScriptedEngine::new(script));
        let store = Arc::new(MemStore::new());
        let mut session = Session::open(deps(&env, store.clone(), engine.clone()), Open::New).unwrap();
        session.set_output_schema(Some(OutputSchema::new(serde_json::json!({"type":"object","required":["done"],"properties":{"done":{"enum":[true]}}})).unwrap()));
        let mut sink = VecSink::default();
        session
            .set_goal(Some(objective(GoalBudget::default())), &mut sink)
            .unwrap();
        let result = session.prompt("Start".into(), vec![], &mut sink, &mut HeadlessUi::default());
        assert_eq!(
            engine.requests().iter().filter(|r| r.max_tokens != 0).count(),
            if repair.is_some() { 2 } else { 1 }
        );
        if accepted {
            result.unwrap();
            assert_eq!(session.stats().outcome, RunOutcome::Done);
            assert_eq!(session.goal().unwrap().status, GoalStatus::Done);
            assert_eq!(
                session.goal().unwrap().evidence.as_deref(),
                Some("Synthetic completion proof.")
            );
        } else {
            assert_eq!(result.unwrap_err().code, oxa_proto::codes::SCHEMA_MISMATCH);
            assert_ne!(session.goal().unwrap().status, GoalStatus::Done);
        }
        assert_eq!(
            Goal::latest(&store.load(&session.inner().sref).unwrap()).unwrap(),
            session.goal()
        );
    }
}

#[test]
fn a_durable_pending_steer_requires_the_live_lease_after_reopening() {
    use oxa_session::{LeadGoalState, OpenGoalAsk, SteeringLease};

    let env = TestEnv::new();
    let root = env.root().join("sessions");
    let engine = Arc::new(ScriptedEngine::new(vec![]));
    let store = Arc::new(FsStore::new(root.clone()));
    let mut owner = Session::open(deps(&env, store, engine.clone()), Open::New).unwrap();
    let mut sink = VecSink::default();
    let session = owner.inner().sref.clone();
    owner
        .set_goal(Some(objective(GoalBudget::default())), &mut sink)
        .unwrap();
    let revision = owner.goal_revision();
    let lease = SteeringLease {
        holder: "fixture".into(),
        generation: (0, 1),
    };
    let staged = owner
        .stage_lead_steer(
            &session,
            revision,
            "Synthetic confirmed steer.".into(),
            Some(lease),
            &mut sink,
        )
        .unwrap();
    let lead = LeadGoalState {
        open_asks: vec![OpenGoalAsk {
            id: "ask-1".into(),
            revision: 2,
            question: "Which synthetic result should be used?".into(),
        }],
        ..LeadGoalState::default()
    };
    assert_eq!(
        owner
            .update_lead_goal(&session, revision, lead.clone(), &mut sink)
            .unwrap_err()
            .code,
        "goal_revision_conflict"
    );
    owner
        .update_lead_goal(&session, staged, lead.clone(), &mut sink)
        .unwrap();
    drop(owner);

    let mut owner = Session::open(
        deps(&env, Arc::new(FsStore::new(root)), engine.clone()),
        Open::Resume(session),
    )
    .unwrap();
    assert!(owner.goal().unwrap().lead.unwrap().pending_steer.is_some());
    // No authority is available yet, so preserve the pending steer and refuse to run.
    let mut ui = HeadlessUi::default();
    let error = owner
        .prompt("continue".into(), vec![], &mut sink, &mut ui)
        .unwrap_err();
    assert_eq!(error.code, "goal_steer_stale");
    assert!(owner.goal().unwrap().lead.unwrap().pending_steer.is_some());

    owner.bind_lead_steering_authority(Arc::new(|_, operation| {
        operation(Some(&SteeringLease {
            holder: "fixture".into(),
            generation: (0, 2),
        }));
    }));
    let error = owner
        .prompt("continue".into(), vec![], &mut sink, &mut ui)
        .unwrap_err();
    assert_eq!(error.code, "goal_steer_stale");
    assert_eq!(owner.goal().unwrap().lead, Some(lead));
    assert!(engine.requests().is_empty());
}

#[test]
fn a_queued_turn_delivers_durably_staged_input_before_its_first_request() {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(vec![
        ScriptTurn::text("first"),
        ScriptTurn::text("second"),
    ]));
    let mut session =
        Session::open(deps(&env, Arc::new(MemStore::new()), engine.clone()), Open::New).unwrap();
    let mut sink = VecSink::default();
    let mut ui = HeadlessUi::default();
    session
        .set_goal(Some(objective(GoalBudget::default())), &mut sink)
        .unwrap();
    let bound = session.inner().sref.clone();
    session
        .stage_lead_steer(
            &bound,
            session.goal_revision(),
            "Synthetic durable steer.".into(),
            None,
            &mut sink,
        )
        .unwrap();
    session.handle().follow_up("Synthetic follow-up.".into());
    session.run_queued(&mut sink, &mut ui).unwrap();
    let requests = engine.requests();
    assert!(requests[0]
        .messages
        .iter()
        .any(|message| message.content.as_deref() == Some("Synthetic durable steer.")));
    assert!(session.goal().unwrap().lead.unwrap().pending_steer.is_none());
}
