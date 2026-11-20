#![cfg(target_os = "linux")]

use oxa_orchestrator::follow::publisher::Publisher;
use serde_json::{json, Value};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

struct MeteredAllocator;

thread_local! {
    static LARGEST: Cell<Option<usize>> = const { Cell::new(None) };
}

fn record_allocation(size: usize) {
    let _ = LARGEST.try_with(|slot| {
        if let Some(largest) = slot.get() {
            slot.set(Some(largest.max(size)));
        }
    });
}

// SAFETY: Allocation operations delegate to System with the caller's pointers and layouts.
unsafe impl GlobalAlloc for MeteredAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record_allocation(layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        record_allocation(size);
        unsafe { System.realloc(ptr, layout, size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: MeteredAllocator = MeteredAllocator;

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

#[test]
fn rejecting_oversized_summaries_does_not_allocate_a_full_payload_copy() {
    let publisher = Publisher::default();
    let oversized = "x".repeat(2 * 1024 * 1024);
    let goal = Value::String(oversized.clone());
    let tasks = Value::String(oversized.clone());
    LARGEST.set(Some(0));
    let rejected = [
        publisher.publish("reply", goal, None, None).is_err(),
        publisher
            .publish("reply", Value::Null, None, Some(&oversized))
            .is_err(),
        publisher
            .bind_session(&oversized, "model", "shadow", false)
            .is_err(),
        publisher.tasks(tasks).is_err(),
    ];
    let largest = LARGEST.take().unwrap();
    assert_eq!(rejected, [true; 4]);
    assert!(
        largest <= 512 * 1024,
        "oversized rejection allocated a {largest}-byte buffer"
    );
}
