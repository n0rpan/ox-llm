#![cfg(feature = "upstream-anthropic-stream")]
use oxc::anthropic_stream::{Stream, Usage};
use serde_json::{json, Value};

#[test]
fn python_stream_contracts() {
    for row in include_str!("fixtures/anthropic_streams.jsonl").lines() {
        let row: Value = serde_json::from_str(row).unwrap();
        let mut stream = Stream::new("synthetic-model", 1700000000, row["include_usage"] == true);
        let chunks: Vec<_> = row["events"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|event| stream.event(event).unwrap())
            .collect();
        assert_eq!(
            serde_json::to_vec(&chunks).unwrap(),
            serde_json::to_vec(&row["expected"]).unwrap(),
            "{}",
            row["name"]
        );
        let result = stream.finish().unwrap();
        assert_eq!(
            json!(result.native_blocks),
            row["native_blocks"],
            "{}",
            row["name"]
        );
        assert_eq!(
            result.usage.map(Usage::openai).unwrap_or(Value::Null),
            row["final_usage"],
            "{}",
            row["name"]
        );
    }
}

#[test]
fn only_complete_streams_provide_final_usage() {
    let start =
        json!({"type":"message_start", "message":{"usage":{"input_tokens":9, "output_tokens":0}}});
    let mut stream = Stream::new("synthetic", 0, true);
    stream.event(&start).unwrap();
    assert!(stream.finish().is_err());
    let mut stream = Stream::new("synthetic", 0, true);
    stream.event(&start).unwrap();
    let chunks = stream.event(&json!({"type":"message_stop"})).unwrap();
    assert_eq!(chunks.len(), 1);
    assert!(stream.finish().unwrap().usage.is_none());
    let mut stream = Stream::new("synthetic", 0, true);
    stream.event(&start).unwrap();
    assert!(stream
        .event(&json!({"type":"error", "error":{"message":"synthetic"}}))
        .is_err());
    assert!(stream.event(&json!({"type":"message_stop"})).is_err());
    assert!(stream.finish().is_err());
}

#[test]
fn malformed_and_overflowed_usage_is_untrusted() {
    for usage in [
        json!({}),
        json!({"input_tokens":1}),
        json!({"input_tokens":true, "output_tokens":0}),
        json!({"input_tokens":1.5, "output_tokens":0}),
        json!({"input_tokens":1, "output_tokens":-1}),
        json!({"input_tokens":1, "output_tokens":0, "cache_read_input_tokens":null}),
        json!({"input_tokens":u64::MAX, "output_tokens":1}),
    ] {
        assert!(Usage::from_value(&usage).is_none());
    }
    let usage = Usage::from_value(&json!({"input_tokens":10, "output_tokens":7,
        "cache_read_input_tokens":3, "cache_creation_input_tokens":2}))
    .unwrap();
    assert_eq!(
        (
            usage.input_tokens,
            usage.cache_read_input_tokens,
            usage.cache_creation_input_tokens
        ),
        (10, 3, 2)
    );
    assert_eq!(usage.openai()["prompt_tokens"], 15);
}

#[test]
fn malformed_event_order_fails_without_panicking() {
    for event in [
        json!(null),
        json!({"type":"content_block_delta"}),
        json!({"type":"message_stop"}),
        json!({"type":"message_delta"}),
    ] {
        assert!(Stream::new("synthetic", 0, false).event(&event).is_err());
    }
    for event in [
        json!({"type":"content_block_delta", "index":0, "delta":{"type":"input_json_delta", "partial_json":"{}"}}),
        json!({"type":"content_block_start", "index":true, "content_block":{"type":"text"}}),
        json!({"type":"content_block_start", "index":0, "content_block":3}),
    ] {
        let mut stream = Stream::new("synthetic", 0, false);
        stream
            .event(&json!({"type":"message_start", "message":{}}))
            .unwrap();
        assert!(stream.event(&event).is_err());
        assert!(stream.finish().is_err());
    }
}

#[test]
fn split_signatures_remain_opaque_and_late_events_do_not_duplicate_output() {
    let mut stream = Stream::new("synthetic", 0, false);
    for event in [
        json!({"type":"message_start", "message":{}}),
        json!({"type":"content_block_start", "index":0, "content_block":{"type":"thinking"}}),
        json!({"type":"content_block_delta", "index":0, "delta":{"type":"signature_delta", "signature":"opaque-"}}),
        json!({"type":"content_block_delta", "index":0, "delta":{"type":"signature_delta", "signature":"suffix"}}),
        json!({"type":"content_block_stop", "index":0}),
        json!({"type":"message_stop"}),
    ] {
        stream.event(&event).unwrap();
    }
    assert!(stream
        .event(&json!({"type":"message_stop"}))
        .unwrap()
        .is_empty());
    let completed = stream.finish().unwrap();
    assert_eq!(completed.native_blocks[0]["signature"], "opaque-suffix");
}
