use oxa_fake::TestEnv;
use oxa_proto::{Entry, EntryBody, WireMsg};
use oxa_session::{FsStore, Goal, GoalBudget, GoalStatus, SessionStore, Transcript};
fn entry(id: &str, parent: Option<&str>, body: EntryBody) -> Entry {
    Entry {
        id: id.into(),
        parent: parent.map(str::to_owned),
        ts_ms: 1000,
        body,
    }
}
fn objective() -> Goal {
    Goal::new(
        "Review synthetic work until evidence supports completion.".into(),
        GoalBudget::default(),
        1000,
    )
    .unwrap()
}

#[test]
fn goal_survives_store_restart_and_does_not_change_the_transcript() {
    let env = TestEnv::new();
    let root = env.root().join("sessions");
    let store = FsStore::new(root.clone());
    let session = store.create(&env.work(), None, None).unwrap();
    let message = entry(
        "00000001",
        None,
        EntryBody::Msg {
            wire: WireMsg::user("Synthetic request"),
            parts: vec![],
            tool: None,
        },
    );
    store.append(&session, &message).unwrap();
    let before = Transcript::project(std::slice::from_ref(&message));
    let mut goal = objective();
    goal.charge(Some(41), 1250, true).unwrap();
    store
        .append(&session, &entry("00000002", Some("00000001"), goal.body()))
        .unwrap();
    drop(store);
    let loaded = FsStore::new(root).load(&session).unwrap();
    assert_eq!(Goal::latest(&loaded).unwrap(), Some(goal));
    assert_eq!(Transcript::project(&loaded).wire(), before.wire());
}
#[test]
fn every_stop_and_resume_preserves_original_budget_and_accounting() {
    let mut goal = objective();
    goal.budget = GoalBudget {
        max_tokens: Some(50),
        max_time_ms: Some(5000),
        max_turns: Some(4),
    };
    goal.charge(Some(20), 1000, true).unwrap();
    assert!(goal.stopped(true));
    goal.pause();
    goal.resume().unwrap();
    assert_eq!((goal.tokens_used, goal.elapsed_ms, goal.turns), (20, 1000, 1));
    assert!(goal.stopped(false));
    assert!(!goal.stopped(false));
    assert_eq!(goal.status, GoalStatus::Paused);
    goal.resume().unwrap();
    assert_eq!(goal.idle_stops, 0);
    goal.charge(Some(30), 1000, true).unwrap();
    assert!(!goal.stopped(true));
    assert_eq!(goal.status, GoalStatus::Blocked);
    assert!(goal.resume().is_err());
    for (budget, tokens, time) in [
        (
            GoalBudget {
                max_time_ms: Some(1),
                ..GoalBudget::default()
            },
            Some(0),
            1,
        ),
        (
            GoalBudget {
                max_turns: Some(1),
                ..GoalBudget::default()
            },
            Some(0),
            0,
        ),
        (
            GoalBudget {
                max_tokens: Some(10),
                ..GoalBudget::default()
            },
            None,
            0,
        ),
    ] {
        let mut goal = Goal::new("Synthetic objective".into(), budget, 0).unwrap();
        goal.charge(tokens, time, true).unwrap();
        assert!(!goal.stopped(true));
        assert_eq!(goal.status, GoalStatus::Blocked);
    }
}
#[test]
fn terminal_evidence_is_required_and_new_snapshots_or_clear_replace_the_goal() {
    for status in [GoalStatus::Done, GoalStatus::Blocked] {
        let mut goal = objective();
        assert!(goal.finish(status, " ".into()).is_err());
        goal.finish(status, "Synthetic verification completed.".into())
            .unwrap();
        assert!(!goal.stopped(true));
        assert_eq!(Goal::decode(&goal.value()).unwrap(), goal);
        let entries = [
            entry("00000001", None, objective().body()),
            entry("00000002", Some("00000001"), goal.body()),
        ];
        assert_eq!(Goal::latest(&entries).unwrap(), Some(goal));
    }
    assert!(Goal::latest(&[
        entry("00000001", None, objective().body()),
        entry("00000002", Some("00000001"), Goal::clear_body())
    ])
    .unwrap()
    .is_none());
}
#[test]
fn malformed_or_unknown_goal_metadata_is_an_error_not_a_fresh_budget() {
    for key in [
        "tokens_used",
        "elapsed_ms",
        "turns",
        "unknown_requests",
        "started_ms",
    ] {
        let mut v = objective().value();
        v[key] = (-1).into();
        assert!(Goal::decode(&v).is_err());
    }
    let mut v = objective().value();
    v["version"] = 2.into();
    assert!(Goal::decode(&v).is_err());
    let mut v = objective().value();
    v["budget"]["max_tokens"] = 0.into();
    assert!(Goal::decode(&v).is_err());
    let mut v = objective().value();
    v["status"] = "done".into();
    assert!(Goal::decode(&v).is_err());
    assert!(Goal::latest(&[entry(
        "00000001",
        None,
        EntryBody::Custom {
            custom_type: oxa_session::GOAL_ENTRY.into(),
            data: v
        }
    )])
    .is_err());
    assert!(Goal::new(" ".into(), GoalBudget::default(), 0).is_err());
}
