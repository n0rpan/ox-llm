#![cfg(feature = "upstream-anthropic-stream")]
use oxc_gateway::anthropic_stream::{Stream, Usage};
use serde_json::json;

#[test]
fn only_complete_streams_provide_final_usage() {
    let start = json!({"type":"message_start", "message":{"usage":{"input_tokens":9, "output_tokens":0}}});
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
fn split_signatures_are_complete_and_late_events_do_not_duplicate_output() {
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
    assert!(stream.event(&json!({"type":"message_stop"})).unwrap().is_empty());
    let completed = stream.finish().unwrap();
    assert_eq!(completed.native_blocks[0]["signature"], "opaque-suffix");
}

#[test]
fn terminal_usage_must_supply_its_own_output_count() {
    for terminal in [
        json!({}),
        json!({"cache_read_input_tokens":3}),
        json!({"output_tokens":true}),
    ] {
        let mut stream = Stream::new("synthetic", 0, true);
        stream
            .event(&json!({"type":"message_start","message":{"usage":{"input_tokens":10,"output_tokens":0}}}))
            .unwrap();
        stream
            .event(&json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":terminal}))
            .unwrap();
        let chunks = stream.event(&json!({"type":"message_stop"})).unwrap();
        assert_eq!(chunks.len(), 1, "provisional usage must not be emitted");
        assert!(stream.finish().unwrap().usage.is_none());
    }
}

#[test]
fn final_usage_retains_billing_categories_even_when_public_usage_is_off() {
    for include_usage in [false, true] {
        for (native, expected) in [
            ("end_turn", "stop"),
            ("stop_sequence", "stop"),
            ("tool_use", "tool_calls"),
            ("max_tokens", "length"),
            ("model_context_window_exceeded", "length"),
            ("refusal", "content_filter"),
        ] {
            let mut stream = Stream::new("public-alias", 42, include_usage);
            stream.event(&json!({"type":"message_start","message":{"usage":{
                "input_tokens":11,"cache_read_input_tokens":7,"cache_creation_input_tokens":3,"output_tokens":0}}})).unwrap();
            stream.event(&json!({"type":"message_delta","delta":{"stop_reason":native},"usage":{"output_tokens":5}})).unwrap();
            let chunks = stream.event(&json!({"type":"message_stop"})).unwrap();
            assert_eq!(chunks[0]["model"], "public-alias");
            assert_eq!(chunks[0]["choices"][0]["finish_reason"], expected);
            assert_eq!(chunks.len(), if include_usage { 2 } else { 1 });
            let done = stream.finish().unwrap();
            assert_eq!(done.finish_reason, expected);
            let usage = done.usage.unwrap();
            assert_eq!(
                (
                    usage.input_tokens,
                    usage.cache_read_input_tokens,
                    usage.cache_creation_input_tokens,
                    usage.output_tokens
                ),
                (11, 7, 3, 5)
            );
            assert_eq!(usage.openai()["prompt_tokens"], 21);
            assert_eq!(usage.openai()["total_tokens"], 26);
            if include_usage {
                assert_eq!(chunks[1]["usage"], usage.openai());
                assert!(chunks[1]["choices"].as_array().unwrap().is_empty());
            }
        }
    }
}

#[cfg(feature = "secrets")]
#[test]
fn public_chunks_and_completion_redact_fields_while_native_replay_stays_opaque() {
    use oxc_gateway::{
        anthropic_stream::{Field, Redactor},
        redaction, secrets,
    };
    use std::{collections::BTreeMap, fs, os::unix::fs::PermissionsExt};
    struct SafeFields {
        key: secrets::Key,
        fields: BTreeMap<Field, redaction::Stream>,
    }
    impl Redactor for SafeFields {
        fn feed(&mut self, field: Field, text: &str) -> String {
            String::from_utf8(
                self.fields
                    .entry(field)
                    .or_insert_with(|| redaction::Stream::new(&self.key))
                    .feed(text.as_bytes()),
            )
            .unwrap()
        }
        fn finish(&mut self, field: Field) -> String {
            self.fields
                .remove(&field)
                .map(|mut stream| String::from_utf8(stream.finish()).unwrap())
                .unwrap_or_default()
        }
        fn whole(&mut self, text: &str) -> String {
            let mut value = json!(text);
            redaction::value(&mut value, &self.key);
            value.as_str().unwrap().to_owned()
        }
    }
    let dir = std::env::temp_dir().join(format!("gateway-native-redaction-{}", std::process::id()));
    fs::create_dir(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.join("synthetic.json");
    fs::write(&path, br#"{"anthropic":"synthetic-marker"}"#).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let key = secrets::read(&path, "anthropic").unwrap();
    fs::remove_dir_all(&dir).unwrap();
    let mut stream = Stream::with_redactor(
        "alias",
        42,
        true,
        SafeFields {
            key,
            fields: BTreeMap::new(),
        },
    );
    let mut chunks = Vec::new();
    for event in [
        json!({"type":"message_start","message":{}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"λ synthetic-"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"marker tail"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"opaque-signature"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":"Hello synthetic-"}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"marker synthetic-"}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"content_block_start","index":3,"content_block":{"type":"tool_use","id":"id-synthetic-marker","name":"read-synthetic-marker"}}),
        json!({"type":"content_block_start","index":7,"content_block":{"type":"tool_use","id":"second","name":"list","input":{"path":"synthetic-marker"}}}),
        json!({"type":"content_block_delta","index":3,"delta":{"type":"input_json_delta","partial_json":"{\"note\":\"synthetic-"}}),
        json!({"type":"content_block_delta","index":3,"delta":{"type":"input_json_delta","partial_json":"marker\"}"}}),
        json!({"type":"content_block_stop","index":3}),
        json!({"type":"content_block_stop","index":7}),
        json!({"type":"content_block_start","index":9,"content_block":{"type":"tool_use","id":"third","name":"empty","input":{}}}),
        json!({"type":"content_block_stop","index":9}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"}}),
        json!({"type":"message_stop"}),
    ] {
        chunks.extend(stream.event(&event).unwrap());
    }
    let done = stream.finish().unwrap();
    assert!(!serde_json::to_string(&chunks)
        .unwrap()
        .contains("synthetic-marker"));
    assert!(!done.assistant.to_string().contains("synthetic-marker"));
    assert!(!done.assistant.to_string().contains("opaque-signature"));
    assert!(!serde_json::to_string(&chunks)
        .unwrap()
        .contains("opaque-signature"));
    assert_eq!(done.assistant["content"], "Hello [redacted] synthetic-");
    assert_eq!(done.assistant["reasoning_content"], "λ [redacted] tail");
    assert_eq!(done.assistant["tool_calls"][0]["id"], "id-[redacted]");
    assert_eq!(
        done.assistant["tool_calls"][0]["function"]["name"],
        "read-[redacted]"
    );
    assert_eq!(
        done.assistant["tool_calls"][0]["function"]["arguments"],
        "{\"note\":\"[redacted]\"}"
    );
    assert_eq!(
        done.assistant["tool_calls"][1]["function"]["arguments"],
        "{\"path\":\"[redacted]\"}"
    );
    assert_eq!(done.assistant["tool_calls"][2]["function"]["arguments"], "{}");
    let mut text = String::new();
    let mut arguments = [String::new(), String::new(), String::new()];
    for chunk in &chunks {
        let delta = &chunk["choices"][0]["delta"];
        text.push_str(delta["content"].as_str().unwrap_or_default());
        for call in delta["tool_calls"].as_array().into_iter().flatten() {
            arguments[call["index"].as_u64().unwrap() as usize]
                .push_str(call["function"]["arguments"].as_str().unwrap_or_default());
        }
    }
    assert_eq!(json!(text), done.assistant["content"]);
    for (slot, argument) in arguments.iter().enumerate() {
        assert_eq!(
            json!(argument),
            done.assistant["tool_calls"][slot]["function"]["arguments"]
        );
    }
    assert_eq!(done.native_blocks[0]["signature"], "opaque-signature");
    assert_eq!(done.native_blocks[2]["input"]["note"], "synthetic-marker");
    assert_eq!(done.native_blocks[3]["input"]["path"], "synthetic-marker");
    assert_eq!(done.native_blocks[4]["input"], json!({}));
}

#[test]
fn zero_argument_tools_have_complete_json_in_chunks_and_completion() {
    for explicit_input in [false, true] {
        for fragment in [None, Some(""), Some("{}"), Some(" {} ")] {
            let mut stream = Stream::new("alias", 0, false);
            stream
                .event(&json!({"type":"message_start","message":{}}))
                .unwrap();
            let mut block = json!({"type":"tool_use","id":"call_1","name":"empty"});
            if explicit_input {
                block["input"] = json!({});
            }
            let mut chunks = stream
                .event(&json!({"type":"content_block_start","index":0,"content_block":block}))
                .unwrap();
            if let Some(fragment) = fragment {
                chunks.extend(
                    stream
                        .event(&json!({"type":"content_block_delta","index":0,
                    "delta":{"type":"input_json_delta","partial_json":fragment}}))
                        .unwrap(),
                );
            }
            chunks.extend(
                stream
                    .event(&json!({"type":"content_block_stop","index":0}))
                    .unwrap(),
            );
            chunks.extend(stream.event(&json!({"type":"message_stop"})).unwrap());
            let arguments: String = chunks
                .iter()
                .filter_map(|chunk| {
                    chunk["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"].as_str()
                })
                .collect();
            let expected = fragment.filter(|s| !s.is_empty()).unwrap_or("{}");
            assert_eq!(
                arguments, expected,
                "input present: {explicit_input}, fragment: {fragment:?}"
            );
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&arguments).unwrap(),
                json!({})
            );
            let done = stream.finish().unwrap();
            assert_eq!(
                done.assistant["tool_calls"][0]["function"]["arguments"],
                arguments
            );
            assert_eq!(done.native_blocks[0]["input"], json!({}));
        }
    }
}

#[test]
fn initial_text_and_thinking_are_not_lost() {
    for (kind, field, delta, wire) in [
        ("text", "text", "text_delta", "content"),
        ("thinking", "thinking", "thinking_delta", "reasoning_content"),
    ] {
        let mut stream = Stream::new("alias", 0, false);
        let mut chunks = Vec::new();
        for event in [
            json!({"type":"message_start","message":{}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":kind,field:"λ prefix"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":delta,field:" tail"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_stop"}),
        ] {
            chunks.extend(stream.event(&event).unwrap());
        }
        let text: String = chunks
            .iter()
            .filter_map(|chunk| chunk["choices"][0]["delta"][wire].as_str())
            .collect();
        assert_eq!(text, "λ prefix tail");
        let done = stream.finish().unwrap();
        assert_eq!(done.assistant[wire], text);
        assert_eq!(done.native_blocks[0][field], text);
    }
}

#[test]
fn initial_tool_arguments_are_complete_and_not_extended_as_partial_json() {
    let input = json!({"path":"λ"});
    let event = json!({"type":"content_block_start","index":0,
        "content_block":{"type":"tool_use","id":"call_1","name":"read","input":input}});
    let mut stream = Stream::new("alias", 0, false);
    for event in [
        json!({"type":"message_start","message":{}}),
        event.clone(),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_stop"}),
    ] {
        stream.event(&event).unwrap();
    }
    let done = stream.finish().unwrap();
    assert_eq!(done.native_blocks[0]["input"], input);
    let args = done.assistant["tool_calls"][0]["function"]["arguments"]
        .as_str()
        .unwrap();
    assert_eq!(serde_json::from_str::<serde_json::Value>(args).unwrap(), input);
    let mut stream = Stream::new("alias", 0, false);
    stream
        .event(&json!({"type":"message_start","message":{}}))
        .unwrap();
    stream.event(&event).unwrap();
    assert!(stream
        .event(&json!({"type":"content_block_delta","index":0,
        "delta":{"type":"input_json_delta","partial_json":"{}"}}))
        .is_err());
    assert!(stream.finish().is_err());
}
