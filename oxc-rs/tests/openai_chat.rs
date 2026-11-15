#![cfg(feature = "upstream-openai-chat")]
use oxc::openai_chat::{request, response, Stream, Usage};
use serde_json::{json, Value};

#[test]
fn python_request_contracts() {
    for row in include_str!("fixtures/openai_chat_requests.jsonl").lines() {
        let row: Value = serde_json::from_str(row).unwrap();
        let result = request(&row["input"], row["model"].as_str().unwrap(), Some(1024));
        if let Some(expected) = row.get("error") {
            let error = result.unwrap_err();
            assert_eq!(
                json!({"status":error.status,"code":error.code,"param":error.param}),
                *expected,
                "{}",
                row["name"]
            );
        } else {
            assert_eq!(
                serde_json::to_vec(&result.unwrap()).unwrap(),
                serde_json::to_vec(&row["expected"]).unwrap(),
                "{}",
                row["name"]
            );
        }
    }
}

#[test]
fn python_passthrough_stream_and_usage_extraction() {
    for row in include_str!("fixtures/openai_chat_streams.jsonl").lines() {
        let row: Value = serde_json::from_str(row).unwrap();
        let (reply, usage) = response(&row["response"], "synthetic-model").unwrap();
        assert_eq!(reply, row["expected_response"]);
        let mut stream = Stream::new("synthetic-model");
        let chunks: Vec<_> = row["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| stream.event(e).unwrap())
            .collect();
        assert_eq!(json!(chunks), row["expected"]);
        assert_eq!(
            serde_json::to_vec(&chunks).unwrap(),
            serde_json::to_vec(&row["expected"]).unwrap()
        );
        stream.done().unwrap();
        let completed = stream.finish().unwrap();
        assert_eq!(completed.usage, usage);
        let usage = completed.usage.unwrap();
        assert_eq!(
            json!({"prompt":usage.prompt_tokens,"completion":usage.completion_tokens,
            "cached":usage.cache_read_tokens,"cache_written":usage.cache_write_tokens,
            "finish":completed.finish_reason}),
            row["meter"]
        );
        assert_eq!(usage.uncached_input_tokens(), 10);
    }
}

fn chunk(delta: Value, finish: Value) -> Value {
    json!({"id":"synthetic","object":"chat.completion.chunk","created":0,"model":"o3",
        "choices":[{"index":0,"delta":delta,"finish_reason":finish}]})
}

#[test]
fn malformed_usage_is_never_trusted() {
    let good = json!({"prompt_tokens":15,"completion_tokens":7});
    for usage in [
        json!({}),
        json!({"prompt_tokens":1}),
        json!({"prompt_tokens":true,"completion_tokens":1}),
        json!({"prompt_tokens":1,"completion_tokens":-1}),
        json!({"prompt_tokens":u64::MAX,"completion_tokens":1}),
        json!({"prompt_tokens":1,"completion_tokens":1.5}),
        json!({"prompt_tokens_details":{"cached_tokens":true},"prompt_tokens":15,"completion_tokens":7}),
        json!({"prompt_tokens_details":{"cached_tokens":16},"prompt_tokens":15,"completion_tokens":7}),
        json!({"prompt_tokens_details":{"cached_tokens":10,"cache_write_tokens":6},"prompt_tokens":15,"completion_tokens":7}),
        json!({"completion_tokens_details":{"reasoning_tokens":8},"prompt_tokens":15,"completion_tokens":7}),
        json!({"prompt_tokens_details":[],"prompt_tokens":15,"completion_tokens":7}),
    ] {
        assert!(Usage::from_response(&json!({"usage":usage})).is_none());
    }
    assert_eq!(
        Usage::from_response(&json!({"usage":good}))
            .unwrap()
            .uncached_input_tokens(),
        15
    );
}

#[test]
fn streams_require_finish_and_done_and_discard_provisional_usage() {
    assert!(Stream::new("synthetic").finish().is_err());
    let mut stream = Stream::new("synthetic");
    assert!(stream.done().is_err());
    assert!(stream.finish().is_err());
    let mut stream = Stream::new("synthetic");
    stream.event(&chunk(json!({}), json!("stop"))).unwrap();
    assert!(stream.finish().is_err());
    let mut stream = Stream::new("synthetic");
    let mut first = chunk(json!({"content":"Synthetic"}), Value::Null);
    first["usage"] = json!({"prompt_tokens":1,"completion_tokens":0});
    stream.event(&first).unwrap();
    stream.event(&chunk(json!({}), json!("stop"))).unwrap();
    stream.done().unwrap();
    assert!(stream.finish().unwrap().usage.is_none());
}

#[test]
fn malformed_stream_shapes_have_fixed_errors_and_stay_failed() {
    for value in [
        json!(null),
        json!({"error":{"message":"synthetic upstream error"}}),
        chunk(json!({"content":3}), Value::Null),
        chunk(json!({"tool_calls":[1]}), Value::Null),
        chunk(
            json!({"tool_calls":[{"index":0,"function":{"arguments":[]}}]}),
            Value::Null,
        ),
        chunk(json!({"role":"user"}), Value::Null),
    ] {
        let mut stream = Stream::new("synthetic");
        let error = stream.event(&value).unwrap_err();
        assert_eq!(error.status, 502);
        assert_eq!(error.code, "cloud_upstream_error");
        assert!(stream.event(&chunk(json!({}), json!("stop"))).is_err());
        assert!(stream.finish().is_err());
    }
}

#[test]
fn request_shape_errors_are_typed_before_any_transport() {
    for (payload, param) in [
        (json!({"messages":null}), "messages"),
        (
            json!({"messages":[{"role":"assistant","tool_calls":{}}]}),
            "messages",
        ),
        (
            json!({"messages":[{"role":"tool","content":"Synthetic"}]}),
            "messages",
        ),
        (
            json!({"tools":[{"type":"function","function":{"name":"read","parameters":[]}}]}),
            "tools",
        ),
        (
            json!({"tool_choice":{"type":"function","function":{}}}),
            "tool_choice",
        ),
    ] {
        let error = request(&payload, "o3", Some(1024)).unwrap_err();
        assert_eq!((error.status, error.param), (400, param));
    }
}

#[test]
fn usage_only_frames_before_finish_or_before_later_output_are_not_final() {
    let usage = json!({"id":"synthetic", "object":"chat.completion.chunk", "model":"o3", "created":0,
        "choices":[], "usage":{"prompt_tokens":1,"completion_tokens":1}});
    let mut early = Stream::new("synthetic");
    early.event(&usage).unwrap();
    early.event(&chunk(json!({}), json!("stop"))).unwrap();
    early.done().unwrap();
    assert!(early.finish().unwrap().usage.is_none());
    let mut stale = Stream::new("synthetic");
    stale.event(&chunk(json!({}), json!("stop"))).unwrap();
    stale.event(&usage).unwrap();
    stale
        .event(&chunk(json!({"content":"later output"}), Value::Null))
        .unwrap();
    stale.event(&chunk(json!({}), json!("stop"))).unwrap();
    stale.done().unwrap();
    assert!(stale.finish().unwrap().usage.is_none());
}
