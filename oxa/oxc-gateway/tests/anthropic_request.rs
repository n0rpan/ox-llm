#![cfg(feature = "upstream-anthropic-request")]
use oxc_gateway::anthropic_request::{translate, translate_with_blocks};
use serde_json::json;

#[test]
fn requests_are_native_bounded_and_do_not_forward_local_sampling_fields() {
    let payload = json!({"model":"public-alias", "stream":true, "max_tokens":1000,
        "temperature":0.8, "top_p":0.5, "n":1, "stop":["END", "STOP"],
        "messages":[{"role":"system","content":"Synthetic rules"},
            {"role":"developer","content":"More rules"},
            {"role":"user","content":"Synthetic question"}],
        "tools":[{"type":"function","function":{"name":"read","description":"Read text",
            "parameters":{"type":"object","properties":{"path":{"type":"string"}}}}}]});
    let body = translate(&payload, "synthetic-upstream", Some(16)).unwrap();
    assert_eq!(body["model"], "synthetic-upstream");
    assert_eq!(body["max_tokens"], 16);
    assert_eq!(body["stream"], true);
    assert_eq!(body["system"][0]["text"], "Synthetic rules\n\nMore rules");
    assert_eq!(
        body["tools"][0]["input_schema"]["properties"]["path"]["type"],
        "string"
    );
    assert_eq!(body["tool_choice"]["type"], "auto");
    for block in [
        &body["system"][0],
        &body["tools"][0],
        &body["messages"][0]["content"][0],
    ] {
        assert_eq!(block["cache_control"]["type"], "ephemeral");
    }
    assert_eq!(body["stop_sequences"], json!(["END", "STOP"]));
    for field in ["temperature", "top_p", "n", "reasoning_effort"] {
        assert!(body.get(field).is_none());
    }
    assert_eq!(
        payload["max_tokens"], 1000,
        "translation must not mutate admission input"
    );
}

#[test]
fn output_limits_are_positive_integers_with_one_precedence_order() {
    for field in ["max_tokens", "max_completion_tokens", "n_predict"] {
        for invalid in [
            json!(null),
            json!(false),
            json!(0),
            json!(-1),
            json!(1.5),
            json!("8"),
        ] {
            let error = translate(&json!({field:invalid}), "synthetic", Some(16)).unwrap_err();
            assert_eq!(error.param, field);
            assert!(!error.to_string().contains("synthetic"));
        }
        assert_eq!(
            translate(&json!({field:8}), "synthetic", Some(16)).unwrap()["max_tokens"],
            8
        );
    }
    assert_eq!(
        translate(
            &json!({"max_tokens":4,"max_completion_tokens":8,"n_predict":12}),
            "synthetic",
            Some(16)
        )
        .unwrap()["max_tokens"],
        4
    );
    assert_eq!(
        translate(&json!({}), "synthetic", Some(16)).unwrap()["max_tokens"],
        16
    );
    assert_eq!(
        translate(&json!({}), "synthetic", None).unwrap()["max_tokens"],
        4096
    );
    assert_eq!(
        translate(&json!([]), "synthetic", None).unwrap_err().param,
        "request"
    );
}

#[test]
fn disabled_reasoning_never_enables_adaptive_thinking() {
    for payload in [
        json!({"reasoning_effort":"none"}),
        json!({"reasoning_effort":"minimal"}),
        json!({"chat_template_kwargs":{"enable_thinking":false}}),
        json!({"reasoning_effort":"none","chat_template_kwargs":{"enable_thinking":true}}),
    ] {
        let body = translate(&payload, "synthetic", None).unwrap();
        assert!(body.get("thinking").is_none());
        assert!(body.get("output_config").is_none());
    }
    for effort in ["low", "medium", "high", "xhigh"] {
        let body = translate(&json!({"reasoning_effort":effort}), "synthetic", None).unwrap();
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["effort"], effort);
    }
    assert_eq!(
        translate(&json!({"reasoning_effort":true}), "synthetic", None)
            .unwrap_err()
            .param,
        "reasoning_effort"
    );
}

#[test]
fn tool_calls_results_and_choices_keep_their_native_relationship() {
    let payload = json!({"messages":[{"role":"user","content":"Read synthetic text"},
        {"role":"assistant","tool_calls":[{"id":"call-1","function":{"name":"read","arguments":"{\"path\":\"synthetic\"}"}}]},
        {"role":"tool","tool_call_id":"call-1","content":"Synthetic result"},
        {"role":"user","content":"Continue"}],
        "tools":[{"function":{"name":"read"}}],"tool_choice":{"type":"function","function":{"name":"read"}}});
    let body = translate(&payload, "synthetic", None).unwrap();
    assert_eq!(
        body["messages"][1]["content"][0],
        json!({"type":"tool_use","id":"call-1","name":"read","input":{"path":"synthetic"}})
    );
    assert_eq!(body["messages"][2]["role"], "user");
    assert_eq!(body["messages"][2]["content"][0]["tool_use_id"], "call-1");
    assert_eq!(body["messages"][2]["content"].as_array().unwrap().len(), 2);
    assert_eq!(body["tool_choice"], json!({"type":"tool","name":"read"}));
    for choice in [json!("required"), payload["tool_choice"].clone()] {
        let mut forced = payload.clone();
        forced["tool_choice"] = choice;
        assert_eq!(
            translate(&forced, "claude-opus-5-5", None).unwrap_err().param,
            "tool_choice"
        );
    }
    let mut no_tools = payload;
    no_tools["tool_choice"] = json!("none");
    assert!(translate(&no_tools, "synthetic", None)
        .unwrap()
        .get("tools")
        .is_none());
}

#[test]
fn tool_results_use_text_parts_and_a_nonempty_placeholder() {
    for (content, expected) in [
        (
            json!(null),
            json!([{"type":"text", "text":"[empty tool result]"}]),
        ),
        (json!(""), json!([{"type":"text", "text":"[empty tool result]"}])),
        (json!([]), json!([{"type":"text", "text":"[empty tool result]"}])),
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
    assert!(result["messages"][1]["content"][0].get("cache_control").is_none());
    assert!(payload["messages"][2].get("thinking_blocks").is_none());
}
