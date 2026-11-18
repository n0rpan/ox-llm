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
fn goal_revisions_follow_saved_goal_records_including_clear_and_replacement() {
    let goal = objective();
    let mut entries = vec![entry("00000001", None, goal.body())];
    let initial = Goal::revision(&entries);
    entries.push(entry(
        "00000002",
        Some("00000001"),
        EntryBody::Msg {
            wire: WireMsg::user("Synthetic request"),
            parts: vec![],
            tool: None,
        },
    ));
    assert_eq!(Goal::revision(&entries), initial);
    entries.push(entry("00000003", Some("00000002"), Goal::clear_body()));
    let cleared = Goal::revision(&entries);
    assert!(cleared > initial);
    assert!(Goal::latest(&entries).unwrap().is_none());
    entries.push(entry("00000004", Some("00000003"), goal.body()));
    assert!(Goal::revision(&entries) > cleared);
    assert_eq!(Goal::latest(&entries).unwrap(), Some(goal));
}

#[test]
fn steering_revision_crosses_only_accounting_records() {
    let mut goal = objective();
    let legacy = EntryBody::Custom {
        custom_type: oxa_session::GOAL_ENTRY.into(),
        data: goal.value(),
    };
    let mut entries = vec![entry("00000001", None, legacy)];
    assert_eq!(Goal::latest(&entries).unwrap(), Some(goal.clone()));
    let original = Goal::revision(&entries);
    goal.charge(Some(15), 250, true).unwrap();
    entries.push(entry(
        "00000002",
        Some("00000001"),
        goal.checkpoint_body(&entries),
    ));
    let counted = Goal::revision(&entries);
    assert!(counted > original);
    assert!(Goal::allows_steering(&entries, original));
    assert_eq!(Goal::latest(&entries).unwrap(), Some(goal.clone()));

    let mut replaced = goal.clone();
    replaced.objective = "Verify the replacement objective.".into();
    entries.push(entry("00000003", Some("00000002"), replaced.body()));
    entries.push(entry("00000004", Some("00000003"), goal.body()));
    assert!(!Goal::allows_steering(&entries, counted));
    let restored = Goal::revision(&entries);
    assert!(Goal::allows_steering(&entries, restored));

    goal.cancel().unwrap();
    entries.push(entry(
        "00000005",
        Some("00000004"),
        goal.checkpoint_body(&entries),
    ));
    assert!(!Goal::allows_steering(&entries, restored));
    assert!(!Goal::allows_steering(&entries, Goal::revision(&entries)));
}

#[test]
fn canceled_goal_round_trips_and_cannot_resume() {
    let mut goal = objective();
    goal.pause();
    goal.cancel().unwrap();
    let mut restored = Goal::decode(&goal.value()).unwrap();
    assert_eq!(restored.status, GoalStatus::Canceled);
    assert!(restored.resume().is_err());
    assert_eq!(restored, goal);
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

#[test]
fn lead_state_survives_compaction_and_stays_private_to_its_session() {
    use oxa_proto::Value;
    use oxa_session::{HeldGoalLease, LeadGoalState, OpenGoalAsk, PendingGoalSteer, SteeringLease};

    let env = TestEnv::new();
    let root = env.root().join("sessions");
    let store = FsStore::new(root.clone());
    let session = store.create(&env.work(), None, None).unwrap();
    let mut goal = objective();
    let lease = SteeringLease {
        holder: "fixture".into(),
        generation: (7, 2),
    };
    goal.lead = Some(LeadGoalState {
        // Existing standing-store fixtures retain 39 concurrently open asks.
        open_asks: (1..=39)
            .map(|id| OpenGoalAsk {
                id: format!("ask-{id}"),
                revision: 2,
                question: format!("Which synthetic result should be used for item {id}?"),
            })
            .collect(),
        standing_refs: vec!["rule-1".into(), "decision-2".into()],
        held_leases: vec![HeldGoalLease {
            task: "task-1".into(),
            lease: lease.clone(),
            expires_at: 60,
        }],
        pending_steer: Some(PendingGoalSteer {
            entry_id: "abcdef02".into(),
            revision: 1,
            text: "Keep the synthetic result.\nThen check the next item.".into(),
            lease: Some(lease),
        }),
    });
    let checkpoint = entry("00000001", None, goal.body());
    let mut public = Vec::new();
    oxa_session::encode_visible_entry(&checkpoint, &mut public);
    let public: Value = serde_json::from_slice(&public).unwrap();
    assert!(public["data"].get("lead").is_none());

    let kept = ["First synthetic turn.", "Second synthetic turn."];
    let mut entries = vec![checkpoint];
    for (index, message) in kept.iter().enumerate() {
        let id = format!("{:08x}", index + 2);
        let parent = entries.last().unwrap().id.clone();
        entries.push(entry(
            &id,
            Some(&parent),
            EntryBody::Msg {
                wire: WireMsg::user(*message),
                parts: vec![],
                tool: None,
            },
        ));
    }
    store.append_many(&session, &entries).unwrap();
    store
        .append(
            &session,
            &entry(
                "00000004",
                Some("00000003"),
                EntryBody::Compaction {
                    summary: "Synthetic summary.".into(),
                    first_kept: "00000003".into(),
                    tokens_before: 500,
                    details: Value::Null,
                    from_hook: false,
                },
            ),
        )
        .unwrap();
    store.sync(&session).unwrap();
    drop(store);

    let reopened = FsStore::new(root);
    let entries = reopened.load(&session).unwrap();
    assert_eq!(Goal::latest(&entries).unwrap(), Some(goal.clone()));
    assert_eq!(Goal::revision(&entries), 1);
    assert_eq!(
        Transcript::project(&entries).wire(),
        &[WireMsg::user("Synthetic summary."), WireMsg::user(kept[1])]
    );
    let fork = oxa_session::fork_file(&reopened, &session, &env.work(), None).unwrap();
    goal.lead = None;
    assert_eq!(Goal::latest(&reopened.load(&fork).unwrap()).unwrap(), Some(goal));
}
