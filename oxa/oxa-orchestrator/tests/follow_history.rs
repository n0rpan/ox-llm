#![cfg(target_os = "linux")]

use oxa_orchestrator::follow::History;
use serde_json::{json, Value};

// Carry #436's goal_pagination regression intent, adapting its inclusive cursor
// to #129's exclusive-after API. A page's high-water mark is not its next cursor.
#[test]
fn reconnect_cursor_does_not_skip_unread_events() {
    let mut history = History::default();
    for revision in 1..=35 {
        history.publish(json!({"revision": revision})).unwrap();
    }
    let first = history.page(0).unwrap();
    assert_eq!(first.events.len(), 16);
    assert_eq!((first.next, first.latest, first.gap), (16, 35, false));
    let second = history.page(first.next).unwrap();
    assert_eq!(second.events.first().unwrap().0, 17);
    assert_eq!((second.next, second.latest), (32, 35));
    let third = history.page(second.next).unwrap();
    assert_eq!(third.events.iter().map(|e| e.0).collect::<Vec<_>>(), [33, 34, 35]);
    let end = history.page(third.next).unwrap();
    assert!(end.events.is_empty());
    assert_eq!((end.next, end.latest, end.gap), (35, 35, false));
    // Reading one follower's page never drains another follower's events.
    assert_eq!(history.page(0).unwrap().events, first.events);
}

#[test]
fn count_retention_reports_gap_and_exact_boundary() {
    let mut history = History::default();
    for revision in 1..=130 {
        history.publish(json!({"revision": revision})).unwrap();
    }
    let stale = history.page(0).unwrap();
    assert!(stale.gap);
    assert_eq!(stale.events[0].0, 3);
    assert!(!history.page(2).unwrap().gap);
    assert!(history.page(131).is_err());
}

#[test]
fn encoded_byte_limit_evicts_independently_of_event_count() {
    let mut history = History::default();
    for _ in 0..3 {
        // JSON quotes count toward the limit, including for UTF-8 payloads.
        history.publish(Value::String("λ".repeat(128 * 1024))).unwrap();
    }
    let page = history.page(0).unwrap();
    assert!(page.gap);
    assert_eq!(page.events.len(), 1);
    assert_eq!((page.events[0].0, page.next, page.latest), (3, 3, 3));
}

#[test]
fn rejection_preserves_history_and_cursor() {
    let mut history = History::default();
    assert!(history.page(0).unwrap().events.is_empty());
    assert!(history.page(1).is_err());
    history.publish(json!({"kind":"completed"})).unwrap();
    let before = history.page(0).unwrap();
    assert!(history.publish(Value::String("x".repeat(512 * 1024))).is_err());
    let after = history.page(0).unwrap();
    assert_eq!(after.events, before.events);
    assert_eq!((after.next, after.latest), (before.next, before.latest));
    assert_eq!(history.publish(json!({"kind":"stopped"})).unwrap(), 2);
}
