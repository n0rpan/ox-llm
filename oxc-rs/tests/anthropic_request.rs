#![cfg(feature = "upstream-anthropic-request")]
use oxc::anthropic_request::{translate, translate_with_blocks};
use serde_json::{json, Value};

/// Without native-controller the comparison is byte-exact. That feature turns on
/// serde_json `preserve_order`, so only then are keys sorted on both sides.
fn sorted(v: &Value) -> Value {
    if cfg!(not(feature = "native-controller")) {
        return v.clone();
    }
    match v {
        Value::Object(m) => {
            let mut keys: Vec<_> = m.keys().collect();
            keys.sort();
            Value::Object(
                keys.into_iter()
                    .map(|k| (k.clone(), sorted(&m[k])))
                    .collect(),
            )
        }
        Value::Array(a) => Value::Array(a.iter().map(sorted).collect()),
        other => other.clone(),
    }
}

#[test]
fn python_mock_upstream_contracts() {
    for row in include_str!("fixtures/anthropic_requests.jsonl").lines() {
        let row: Value = serde_json::from_str(row).unwrap();
        let result = translate(
            &row["input"],
            row["item"]["upstream_model"].as_str().unwrap(),
            row["item"]["max_output"].as_u64(),
        );
        if let Some(param) = row["error_param"].as_str() {
            assert_eq!(result.unwrap_err().param, param, "{}", row["name"]);
        } else {
            assert_eq!(
                serde_json::to_vec(&sorted(&result.unwrap())).unwrap(),
                serde_json::to_vec(&sorted(&row["expected"])).unwrap(),
                "{}",
                row["name"]
            );
        }
    }
}

#[test]
fn tool_results_use_text_parts_and_a_nonempty_placeholder() {
    for (content, expected) in [
        (
            json!(null),
            json!([{"type":"text", "text":"[empty tool result]"}]),
        ),
        (
            json!(""),
            json!([{"type":"text", "text":"[empty tool result]"}]),
        ),
        (
            json!([]),
            json!([{"type":"text", "text":"[empty tool result]"}]),
        ),
        (
            json!([{"type":"text", "text":""}]),
            json!([{"type":"text", "text":"[empty tool result]"}]),
        ),
        (
            json!([{"type":"text", "text":"A"}, {"type":"text", "text":"λ"}]),
            json!([{"type":"text", "text":"A"}, {"type":"text", "text":"λ"}]),
        ),
    ] {
        let result = translate(
            &json!({"messages":[{"role":"tool", "tool_call_id":"call_1",
            "content":content}]}),
            "synthetic",
            None,
        )
        .unwrap();
        assert_eq!(result["messages"][0]["content"][0]["content"], expected);
        assert_eq!(
            result["messages"][0]["content"][0]["cache_control"],
            json!({"type":"ephemeral"})
        );
    }
}

#[test]
fn restore_uses_original_index_and_preserves_opaque_blocks() {
    let blocks = vec![json!({"type":"thinking", "thinking":"Synthetic", "signature":"opaque"})];
    let payload = json!({"messages":[{"role":"system", "content":"Synthetic"},
        {"role":"user", "content":"Q"}, {"role":"assistant", "content":"visible"}]});
    let mut indices = Vec::new();
    let result = translate_with_blocks(&payload, "synthetic", None, |index| {
        indices.push(index);
        Some(blocks.clone())
    })
    .unwrap();
    assert_eq!(indices, vec![2]);
    assert_eq!(result["messages"][1]["content"], json!(blocks));
    assert!(result["messages"][1]["content"][0]
        .get("cache_control")
        .is_none());
    assert!(payload["messages"][2].get("thinking_blocks").is_none());
}
