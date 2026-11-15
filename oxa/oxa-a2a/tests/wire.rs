use oxa_a2a::sse::SseReader;
use oxa_a2a::wire::*;
use serde_json::json;
use std::io::{BufReader, Cursor};

#[test]
fn task_and_direct_message_are_distinct_wire_results() {
    let task = json!({"kind":"task","id":"t1","contextId":"c1","status":{"state":"working"}});
    let event: Event = serde_json::from_value(task.clone()).unwrap();
    assert_eq!(serde_json::to_value(&event).unwrap(), task);
    assert!(event.belongs_to("t1", "c1"));
    assert!(!event.belongs_to("t2", "c1"));
    let direct =
        json!({"kind":"message","messageId":"m1","role":"agent","parts":[{"kind":"text","text":"done"}]});
    let event: Event = serde_json::from_value(direct.clone()).unwrap();
    assert!(matches!(event, Event::Message(_)));
    assert_eq!(serde_json::to_value(event).unwrap(), direct);
}

#[test]
fn stream_final_is_not_task_terminal() {
    let event: Event = serde_json::from_value(json!({"kind":"status-update","taskId":"t","contextId":"c","status":{"state":"input-required"},"final":true})).unwrap();
    let Event::Status(status) = event else { panic!() };
    assert!(status.final_event);
    assert!(!status.status.state.is_terminal());
    for state in [
        TaskState::Completed,
        TaskState::Failed,
        TaskState::Canceled,
        TaskState::Rejected,
    ] {
        assert!(state.is_terminal());
    }
    assert!(!TaskState::Unknown.is_terminal());
}

#[test]
fn invalid_discriminants_and_file_parts_are_rejected() {
    for value in [
        json!({"kind":"task","id":"t","contextId":"c","status":{"state":"invented"}}),
        json!({"kind":"message","messageId":"m","role":"system","parts":[]}),
        json!({"kind":"message","messageId":"m","role":"agent","parts":[{"kind":"file","file":{"uri":"file:///private"}}]}),
        json!({"id":"t","contextId":"c","status":{"state":"working"}}),
    ] {
        assert!(serde_json::from_value::<Event>(value).is_err());
    }
}

#[test]
fn response_envelope_checks_version_id_and_exclusive_result() {
    let id = RequestId::Text("r1".into());
    assert_eq!(
        response_result(json!({"jsonrpc":"2.0","id":"r1","result":42}), &id).unwrap(),
        json!(42)
    );
    for value in [
        json!({"jsonrpc":"2.0","id":"other","result":42}),
        json!({"jsonrpc":"1.0","id":"r1","result":42}),
        json!({"jsonrpc":"2.0","id":"r1","result":42,"error":null}),
        json!({"jsonrpc":"2.0","id":"r1"}),
    ] {
        assert!(response_result(value, &id).is_err());
    }
    let error = response_result(
        json!({"jsonrpc":"2.0","id":"r1","error":{"code":-32001,"message":"Task not found"}}),
        &id,
    )
    .unwrap_err();
    assert_eq!(error.code, -32001);
}

#[test]
fn card_cannot_change_registration_or_require_unknown_extension() {
    let mut card: AgentCard = serde_json::from_value(json!({"protocolVersion":"0.3.0","name":"mock","description":"mock","version":"1","url":"http://127.0.0.1:1234/","capabilities":{},"defaultInputModes":["text/plain"],"defaultOutputModes":["text/plain"],"skills":[]})).unwrap();
    assert!(card.validate_for("http://127.0.0.1:1234/").is_ok());
    assert!(card.validate_for("http://127.0.0.1:4321/").is_err());
    card.capabilities.extensions.push(Extension {
        uri: "urn:unknown".into(),
        required: true,
    });
    assert!(card.validate_for(&card.url).is_err());
}

#[test]
fn sse_handles_every_buffer_boundary_and_multiline_utf8() {
    let input = "\u{feff}: keepalive\r\ndata: héllo\r\ndata: world\r\n\r\ndata: next\n\n";
    for capacity in 1..=input.len() {
        let mut reader = SseReader::new(BufReader::with_capacity(capacity, Cursor::new(input)), 128, 512);
        assert_eq!(reader.next_data().unwrap().as_deref(), Some("héllo\nworld"));
        assert_eq!(reader.next_data().unwrap().as_deref(), Some("next"));
        assert_eq!(reader.next_data().unwrap(), None);
    }
}

#[test]
fn sse_limits_include_comments_and_cannot_be_bypassed_after_error() {
    for input in [":123456789\n\n", "data: 123456789\n\n", "data:x\ndata:y\n\n"] {
        let mut reader = SseReader::new(Cursor::new(input), 10, 100);
        assert!(reader.next_data().is_err());
        assert!(reader.next_data().is_err());
    }
    let mut reader = SseReader::new(Cursor::new("data:a\n\ndata:b\n\n"), 10, 10);
    assert_eq!(reader.next_data().unwrap().as_deref(), Some("a"));
    assert!(reader.next_data().is_err());
    let mut reader = SseReader::new(Cursor::new("data:uncommitted"), 64, 64);
    assert_eq!(reader.next_data().unwrap(), None);
}
