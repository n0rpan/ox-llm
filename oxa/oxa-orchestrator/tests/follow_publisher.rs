#![cfg(all(target_os = "linux", feature = "a2a"))]

use oxa_orchestrator::follow::publisher::Publisher;
use serde_json::{json, Value};

#[test]
fn status_refreshes_coalesce_without_skipping_follow_events() {
    let publisher = Publisher::default();
    publisher
        .bind_session("session-one", "model", "shadow", true)
        .unwrap();
    publisher
        .tasks(json!([{"id":"task-one","state":"running"}]))
        .unwrap();
    for revision in 1..=40 {
        assert_eq!(
            publisher.publish("goal_status", json!({"revision":revision}), None, None),
            Ok(None)
        );
    }
    assert!(publisher.page(0).unwrap().events.is_empty());
    let latest = publisher.latest().unwrap();
    assert_eq!(latest["goal"]["revision"], 40);
    assert_eq!(latest["tasks"][0]["id"], "task-one");
    assert_eq!(
        publisher.publish("reply", json!({"revision":40}), Some("done"), Some("request-one")),
        Ok(Some(1))
    );
    let page = publisher.page(0).unwrap();
    assert_eq!((page.next, page.latest, page.gap), (1, 1, false));
    assert_eq!(page.events[0].1["request_id"], "request-one");
}

#[test]
fn reply_preview_is_utf8_bounded_and_clones_share_the_owner_history() {
    let publisher = Publisher::default();
    publisher
        .bind_session("session-one", "model", "shadow", false)
        .unwrap();
    let other = publisher.clone();
    let reply = "界".repeat(6000);
    other.publish("reply", Value::Null, Some(&reply), None).unwrap();
    let page = publisher.page(0).unwrap();
    let event = &page.events[0].1;
    let preview = event["reply"]["text"].as_str().unwrap();
    assert_eq!(preview.len(), 16383);
    assert!(reply.starts_with(preview));
    assert_eq!(event["reply"]["truncated"], true);
    assert_eq!(event["session"]["id"], "session-one");
    other
        .bind_session("session-two", "model", "shadow", false)
        .unwrap();
    other.publish("reply", Value::Null, Some("next"), None).unwrap();
    let page = publisher.page(0).unwrap();
    assert_eq!(page.events[0].1["session"]["id"], "session-one");
    assert_eq!(page.events[1].1["session"]["id"], "session-two");
}

#[test]
fn oversized_observation_or_event_does_not_advance_latest_or_cursor() {
    let publisher = Publisher::default();
    publisher
        .publish("reply", Value::Null, Some("accepted"), None)
        .unwrap();
    let latest = publisher.latest();
    let huge = "x".repeat(512 * 1024);
    assert!(publisher
        .publish("reply", Value::String(huge.clone()), None, None)
        .is_err());
    assert!(publisher
        .publish("reply", Value::Null, None, Some(&huge))
        .is_err());
    assert!(publisher.bind_session(&huge, "model", "shadow", false).is_err());
    assert!(publisher.tasks(Value::String(huge)).is_err());
    assert_eq!(publisher.latest(), latest);
    assert_eq!(publisher.page(0).unwrap().latest, 1);
    assert_eq!(
        publisher.publish("reply", Value::Null, Some("next"), None),
        Ok(Some(2))
    );
}
