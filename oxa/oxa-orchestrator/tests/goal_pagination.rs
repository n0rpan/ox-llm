#![cfg(feature = "interactive-control")]

use oxa_orchestrator::goals::{Budget, Goal};

#[test]
fn paginated_events_do_not_skip_unread_goal_changes() {
    let mut goal = Goal::new(
        "project".into(),
        "Verify the selected project".into(),
        Budget {
            wall_ms: 60_000,
            tokens: Some(100),
            turns: 4,
        },
    )
    .unwrap();
    goal.pause(goal.snapshot().revision).unwrap();
    goal.resume(goal.snapshot().revision).unwrap();
    let first = goal.events(0, 1);
    assert_eq!(first.events.len(), 1);
    assert_eq!(first.next_sequence, 1);
    let second = goal.events(first.next_sequence, 1);
    assert_eq!(second.events.len(), 1);
    assert_eq!(second.events[0].sequence, 1);
    let third = goal.events(second.next_sequence, 1);
    assert_eq!(third.events.len(), 1);
    assert_eq!(third.events[0].sequence, 2);
    assert!(goal.events(third.next_sequence, 1).events.is_empty());
}
