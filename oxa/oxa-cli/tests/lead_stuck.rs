use oxa_cli::lead::cards::stuck;
use oxa_cli::lead::stuck::{derive, MergeState, Signal, Thresholds, WorkerState};

const MINUTE_MS: u64 = 60_000;

fn worker(task: &str) -> WorkerState {
    WorkerState {
        task: task.into(),
        publish_pending: false,
        auth_failed: false,
        idle_with_work_left: false,
    }
}

fn quiet_merge() -> MergeState {
    MergeState {
        green_prs: 0,
        since_last_merge_ms: None,
    }
}

#[test]
fn publish_pending_is_stuck() {
    let mut pending = worker("task-a");
    pending.publish_pending = true;
    let signals = derive(&[pending], quiet_merge(), Thresholds::default());
    assert_eq!(
        signals,
        vec![Signal::PublishPending {
            task: "task-a".into()
        }]
    );
    assert!(derive(&[worker("task-a")], quiet_merge(), Thresholds::default()).is_empty());
}

#[test]
fn auth_failure_is_stuck() {
    let mut refused = worker("task-b");
    refused.auth_failed = true;
    let signals = derive(&[refused], quiet_merge(), Thresholds::default());
    assert_eq!(
        signals,
        vec![Signal::AuthFailure {
            task: "task-b".into()
        }]
    );
}

#[test]
fn idle_with_work_left_is_stuck() {
    let mut idle = worker("task-c");
    idle.idle_with_work_left = true;
    let signals = derive(&[idle], quiet_merge(), Thresholds::default());
    assert_eq!(
        signals,
        vec![Signal::IdleWithWork {
            task: "task-c".into()
        }]
    );
}

#[test]
fn green_prs_without_merge_for_60_min_is_stuck() {
    let green_two = |since| MergeState {
        green_prs: 2,
        since_last_merge_ms: Some(since),
    };
    let at_limit = derive(&[], green_two(60 * MINUTE_MS), Thresholds::default());
    assert_eq!(
        at_limit,
        vec![Signal::MergeStalled {
            green_prs: 2,
            minutes: 60
        }]
    );
    assert!(derive(&[], green_two(59 * MINUTE_MS), Thresholds::default()).is_empty());

    let no_green = MergeState {
        green_prs: 0,
        since_last_merge_ms: Some(10 * 60 * MINUTE_MS),
    };
    assert!(derive(&[], no_green, Thresholds::default()).is_empty());

    let unknown_merge = MergeState {
        green_prs: 2,
        since_last_merge_ms: None,
    };
    assert!(derive(&[], unknown_merge, Thresholds::default()).is_empty());
}

#[test]
fn thresholds_come_from_settings() {
    assert_eq!(Thresholds::from_settings(None), Thresholds::default());
    assert_eq!(Thresholds::default().merge_stall_minutes, 60);

    let thirty = Thresholds::from_settings(Some(30));
    let merge = MergeState {
        green_prs: 1,
        since_last_merge_ms: Some(30 * MINUTE_MS),
    };
    assert_eq!(
        derive(&[], merge, thirty),
        vec![Signal::MergeStalled {
            green_prs: 1,
            minutes: 30
        }]
    );
    assert!(derive(&[], merge, Thresholds::default()).is_empty());

    let disabled = Thresholds::from_settings(Some(0));
    let ancient = MergeState {
        green_prs: 5,
        since_last_merge_ms: Some(u64::MAX),
    };
    assert!(derive(&[], ancient, disabled).is_empty());
}

#[test]
fn stuck_card_is_bounded_and_escaped() {
    assert_eq!(stuck(&[]), "Stuck: none in this observation");
    let mut signals: Vec<Signal> = (0..9)
        .map(|n| Signal::PublishPending {
            task: format!("task-{n}\u{202e}"),
        })
        .collect();
    signals.push(Signal::MergeStalled {
        green_prs: 3,
        minutes: 60,
    });
    let card = stuck(&signals);
    assert!(card.lines().count() <= 6, "{card}");
    assert!(card.starts_with("Stuck: 10 signal(s)"), "{card}");
    assert!(card.contains("6 more stuck signals"), "{card}");
    assert!(!card.contains('\u{202e}'), "{card}");
}
