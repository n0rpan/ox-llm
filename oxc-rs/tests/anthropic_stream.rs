#![cfg(feature = "upstream-anthropic-stream")]
use oxc::anthropic_stream::{Field, Redactor, Stream, Usage};
use serde_json::{json, Value};
use std::collections::BTreeMap;

struct FixtureRedactor {
    marker: Option<String>,
    held: BTreeMap<Field, String>,
}
impl Redactor for FixtureRedactor {
    fn feed(&mut self, field: Field, text: &str) -> String {
        let Some(marker) = self.marker.as_ref() else {
            return text.to_owned();
        };
        let data = format!("{}{text}", self.held.remove(&field).unwrap_or_default())
            .replace(marker, "[redacted]");
        let hold = (1..marker.len())
            .rev()
            .find(|&size| marker.is_char_boundary(size) && data.ends_with(&marker[..size]))
            .unwrap_or(0);
        let (safe, tail) = data.split_at(data.len() - hold);
        self.held.insert(field, tail.to_owned());
        safe.to_owned()
    }
    fn finish(&mut self, field: Field) -> String {
        self.held.remove(&field).unwrap_or_default()
    }
    fn whole(&mut self, value: &str) -> String {
        self.marker
            .as_ref()
            .map_or_else(|| value.to_owned(), |m| value.replace(m, "[redacted]"))
    }
}

#[test]
fn python_stream_contracts() {
    for row in include_str!("fixtures/anthropic_streams.jsonl").lines() {
        let row: Value = serde_json::from_str(row).unwrap();
        let mut stream = Stream::with_redactor(
            "synthetic-model",
            1700000000,
            row["include_usage"] == true,
            FixtureRedactor {
                marker: row["redact_marker"].as_str().map(str::to_owned),
                held: BTreeMap::new(),
            },
        );
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
        if row["redact_marker"].is_string() {
            let assistant = serde_json::to_string(&result.assistant).unwrap();
            assert!(!assistant.contains("synthetic-redact-marker"));
            assert_eq!(
                result.assistant["content"],
                "Synthetic λ [redacted] synthetic-"
            );
            assert_eq!(
                result.assistant["reasoning_content"],
                "[redacted] synthetic-"
            );
            assert_eq!(
                result.assistant["tool_calls"][0]["function"]["name"],
                "read_[redacted]"
            );
            assert_eq!(
                result.assistant["tool_calls"][0]["function"]["arguments"],
                "{\"note\":\"[redacted]\"}"
            );
        }
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
fn repeated_signatures_replace_and_late_events_do_not_duplicate_output() {
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
    assert_eq!(completed.native_blocks[0]["signature"], "suffix");
}

#[test]
fn terminal_usage_must_supply_its_own_output_count() {
    for terminal in [
        json!({}),
        json!({"cache_read_input_tokens":3}),
        json!({"output_tokens":true}),
    ] {
        let mut stream = Stream::new("synthetic", 0, true);
        stream.event(&json!({"type":"message_start","message":{"usage":{"input_tokens":10,"output_tokens":0}}})).unwrap();
        stream.event(&json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":terminal})).unwrap();
        let chunks = stream.event(&json!({"type":"message_stop"})).unwrap();
        assert_eq!(chunks.len(), 1, "provisional usage must not be emitted");
        assert!(stream.finish().unwrap().usage.is_none());
    }
}
