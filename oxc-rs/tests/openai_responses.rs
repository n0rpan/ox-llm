#![cfg(feature = "upstream-openai-responses")]
use oxc::openai_responses::{request, response, HistoryStore, Stream};
use serde_json::{json, Value};

#[test]
fn python_requests_and_continuations() {
    for row in include_str!("fixtures/openai_responses_requests.jsonl").lines() {
        let row: Value = serde_json::from_str(row).unwrap();
        let mut store = HistoryStore::default();
        if let Some(remember) = row.get("remember") {
            store.remember(
                "synthetic-scope",
                "gpt-6-sol",
                remember["history"].as_array().unwrap(),
                &remember["assistant"],
                remember["output"].as_array().unwrap(),
                0,
            );
        }
        let result = request(
            &row["input"],
            "gpt-6-sol",
            Some(1024),
            "synthetic-scope",
            &mut store,
            0,
        );
        if let Some(error) = row.get("error") {
            let result = result.unwrap_err();
            assert_eq!(
                json!({"status":result.status, "code":result.code, "param":result.param}),
                *error,
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
fn python_terminal_responses_and_stream_chunks() {
    for row in include_str!("fixtures/openai_responses_streams.jsonl").lines() {
        let row: Value = serde_json::from_str(row).unwrap();
        let mut stream = Stream::new("synthetic-model", 1700000000);
        let chunks: Vec<_> = row["events"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|e| stream.event(e).unwrap())
            .collect();
        assert_eq!(
            serde_json::to_vec(&chunks).unwrap(),
            serde_json::to_vec(&row["expected"]).unwrap()
        );
        assert!(stream
            .event(&json!({"type":"response.completed"}))
            .unwrap()
            .is_empty());
        assert_eq!(stream.finish().unwrap(), row["terminal"]);
        assert_eq!(
            response(&row["terminal"], "synthetic-model", 1700000000).unwrap(),
            row["reply"]
        );
    }
}

fn step(n: usize) -> (Value, Vec<Value>, Value) {
    let id = format!("call_{n}");
    (
        json!({"role":"assistant", "content":null, "tool_calls":[{"id":id, "type":"function",
        "function":{"name":"read", "arguments":"{\"x\":1}"}}]}),
        vec![
            json!({"type":"reasoning", "encrypted_content":format!("synthetic-{n}"), "summary":[]}),
            json!({"type":"function_call", "call_id":id, "name":"read", "arguments":"{\"x\":1}", "status":"completed"}),
        ],
        json!({"role":"tool", "tool_call_id":id, "content":"Synthetic result"}),
    )
}

#[test]
fn long_loop_replays_evicted_closed_steps_and_retains_latest_reasoning() {
    let mut store = HistoryStore::with_limits(2, 4096, 1800);
    let mut history = vec![json!({"role":"user", "content":"Synthetic"})];
    for n in 0..140 {
        let (assistant, output, tool) = step(n);
        store.remember("scope", "gpt-6-sol", &history, &assistant, &output, 0);
        history.extend([assistant, tool]);
    }
    assert_eq!(store.len(), 2);
    assert!(store.byte_len() <= 4096);
    let out = request(
        &json!({"messages":history}),
        "gpt-6-sol",
        Some(1024),
        "scope",
        &mut store,
        0,
    )
    .unwrap();
    let input = out["input"].as_array().unwrap();
    assert_eq!(
        input
            .iter()
            .filter(|p| p["type"] == "function_call")
            .count(),
        140
    );
    assert_eq!(
        input
            .iter()
            .filter(|p| p["type"] == "function_call_output")
            .count(),
        140
    );
    assert_eq!(input.iter().filter(|p| p["type"] == "reasoning").count(), 2);
    assert!(input
        .iter()
        .any(|p| p["encrypted_content"] == "synthetic-139"));
}

#[test]
fn stores_are_bounded_expiring_and_partitioned_by_exact_prefix_scope_and_model() {
    let mut store = HistoryStore::with_limits(2, 4096, 5);
    let mut history = vec![json!({"role":"user", "content":"Synthetic"})];
    let (assistant, output, tool) = step(1);
    store.remember("scope", "gpt-6-sol", &history, &assistant, &output, 0);
    history.extend([assistant.clone(), tool]);
    for (scope, model, now) in [
        ("other", "gpt-6-sol", 0),
        ("scope", "gpt-6-luna", 0),
        ("scope", "gpt-6-sol", 6),
    ] {
        assert_eq!(
            request(
                &json!({"messages":history}),
                model,
                Some(1024),
                scope,
                &mut store,
                now
            )
            .unwrap_err()
            .status,
            409
        );
    }
    store.remember("scope", "gpt-6-sol", &history[..1], &assistant, &output, 7);
    let mut changed = history.clone();
    changed[0]["content"] = json!("different prefix");
    assert!(request(
        &json!({"messages":changed}),
        "gpt-6-sol",
        Some(1024),
        "scope",
        &mut store,
        7
    )
    .is_err());
    // Text arrays, argument whitespace and object ordering retain canonical identity.
    let mut equivalent = history.clone();
    equivalent[0]["content"] = json!([{"type":"text", "text":"Synthetic"}]);
    equivalent[1]["tool_calls"][0]["function"]["arguments"] = json!("{ \"x\" : 1 }");
    assert!(request(
        &json!({"messages":equivalent}),
        "gpt-6-sol",
        Some(1024),
        "scope",
        &mut store,
        7
    )
    .is_ok());
    let mut tiny = HistoryStore::with_limits(100, 1, 5);
    tiny.remember("scope", "gpt-6-sol", &[], &assistant, &output, 0);
    assert!(tiny.is_empty());
    assert_eq!(tiny.byte_len(), 0);
}

#[test]
fn unclosed_steps_never_use_historical_replay() {
    let (assistant, _, _) = step(1);
    let messages = json!([{"role":"user", "content":"Synthetic"}, assistant, {"role":"user", "content":"next"}]);
    assert_eq!(
        request(
            &json!({"messages":messages}),
            "gpt-6-sol",
            Some(100),
            "scope",
            &mut HistoryStore::default(),
            0
        )
        .unwrap_err()
        .status,
        409
    );
}

#[test]
fn stream_failures_never_produce_a_terminal_record() {
    assert!(Stream::new("synthetic", 0).finish().is_err());
    for event in [
        json!(null),
        json!({"type":"response.failed"}),
        json!({"type":"error"}),
        json!({"type":"response.function_call_arguments.delta", "output_index":1, "delta":"{}"}),
        json!({"type":"response.output_text.delta", "delta":true}),
        json!({"type":"response.completed", "response":{"status":"failed", "output":[]}}),
    ] {
        let mut stream = Stream::new("synthetic", 0);
        assert!(stream.event(&event).is_err());
        assert!(stream.event(&json!({"type":"response.created"})).is_err());
        assert!(stream.finish().is_err());
    }
}

#[test]
fn terminal_state_is_remembered_only_when_completed_and_scoped_to_visible_history() {
    let history = vec![
        json!({"role":"user", "content":[{"type":"image_url", "image_url":{
        "url":"https://example.invalid/a.png", "detail":"low"}}]}),
    ];
    let (assistant, output, tool) = step(1);
    let mut terminal = json!({"status":"incomplete", "output":output});
    let mut store = HistoryStore::default();
    store
        .remember_completed("scope", "gpt-6-sol", &history, &terminal, &assistant, 0)
        .unwrap();
    assert!(store.is_empty());
    terminal["status"] = json!("completed");
    store
        .remember_completed("scope", "gpt-6-sol", &history, &terminal, &assistant, 0)
        .unwrap();
    assert_eq!(store.len(), 1);
    let mut continued = history.clone();
    continued.extend([assistant, tool]);
    assert!(request(
        &json!({"messages":continued}),
        "gpt-6-sol",
        Some(100),
        "scope",
        &mut store,
        0
    )
    .is_ok());
    continued[0]["content"][0]["image_url"]["url"] = json!("https://example.invalid/b.png");
    assert!(request(
        &json!({"messages":continued}),
        "gpt-6-sol",
        Some(100),
        "scope",
        &mut store,
        0
    )
    .is_err());
    terminal["status"] = json!("failed");
    assert!(store
        .remember_completed("scope", "gpt-6-sol", &history, &terminal, &json!({}), 0)
        .is_err());
}

#[test]
fn malformed_usage_and_unsupported_models_fail_closed() {
    use oxc::openai_responses::usage;
    for value in [
        json!({}),
        json!({"usage":{"input_tokens":1,"output_tokens":true}}),
        json!({"usage":{"input_tokens":-1,"output_tokens":1}}),
        json!({"usage":{"input_tokens":u64::MAX,"output_tokens":1}}),
    ] {
        assert!(usage(&value).is_none());
    }
    let mut store = HistoryStore::default();
    assert_eq!(
        request(
            &json!({}),
            "unreviewed-model",
            Some(100),
            "scope",
            &mut store,
            0
        )
        .unwrap_err()
        .code,
        "unsupported_model"
    );
    for model in ["gpt-6-astra", "gpt-6.1-sol"] {
        assert_eq!(
            request(
                &json!({"reasoning_effort":"none"}),
                model,
                Some(100),
                "scope",
                &mut store,
                0
            )
            .unwrap()["reasoning"]["effort"],
            "low"
        );
    }
}
