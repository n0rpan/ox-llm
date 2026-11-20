#![cfg(all(feature = "anthropic-client", feature = "ledger"))]
use oxc_gateway::model_rules::{Replay, Route, Rules, Thinking, TokenField};
use oxc_gateway::opencode;
use serde_json::{json, Value};

fn model(id: &str) -> opencode::Model {
    let found = json!({"data":[{"id":id, "canonical_id":"synthetic/canonical", "limit":{"context":262144,"output":200000},
        "api":{"chat-completions":{"base_url":"https://example.invalid/inference/go/openai/v1"}},
        "cost":{"input":0.1},"capabilities":{"tools":true},"modalities":{"input":["text"],"output":["text"]}}]});
    opencode::models(&found, "https://example.invalid/inference/go")
        .unwrap()
        .remove(0)
}
fn tool_turn() -> Value {
    json!([
        {"role":"user","content":"go"},
        {"role":"assistant","content":null,"tool_calls":[{"id":"c1","type":"function","function":{"name":"read","arguments":"{}"}}]},
        {"role":"tool","tool_call_id":"c1","content":"ok"},
        {"role":"assistant","content":"done"}
    ])
}

#[test]
fn glm_5_3_writes_the_keep_thinking_switch_by_model_id_not_url() {
    let m = model("glm-5.3");
    let on = m
        .request_with(
            &json!({"messages":tool_turn(),"reasoning_effort":"high"}),
            &Rules::default(),
        )
        .unwrap();
    assert_eq!(on["thinking"], json!({"type":"enabled","clear_thinking":false}));
    let off = m
        .request_with(
            &json!({"messages":[],"reasoning_effort":"none"}),
            &Rules::default(),
        )
        .unwrap();
    assert_eq!(off["thinking"], json!({"type":"disabled"}));
    let unset = m
        .request_with(&json!({"messages":[]}), &Rules::default())
        .unwrap();
    assert!(unset.get("thinking").is_none());
    // A thinking field the caller set is kept.
    let own = m
        .request_with(
            &json!({"messages":[],"reasoning_effort":"high","thinking":{"type":"enabled"}}),
            &Rules::default(),
        )
        .unwrap();
    assert_eq!(own["thinking"], json!({"type":"enabled"}));
    // Output is capped by the table's max output when it is below the route's.
    let capped = m
        .request_with(&json!({"messages":[],"max_tokens":300000}), &Rules::default())
        .unwrap();
    assert_eq!(capped["max_tokens"], 128000);
    // Replay of GLM reasoning is unchanged by default (the A/B is a later step).
    let kept = json!([{"role":"assistant","reasoning_content":"r","content":"x"}]);
    let body = m
        .request_with(&json!({"messages":kept}), &Rules::default())
        .unwrap();
    assert_eq!(body["messages"], kept);
}

#[test]
fn kimi_k3_and_deepseek_v4_replay_reasoning_on_tool_call_turns() {
    for (id, thinking) in [("kimi-k3", None), ("DeepSeek-V4.1-Flash", Some("enabled"))] {
        let m = model(id);
        let body = m
            .request_with(
                &json!({"messages":tool_turn(),"reasoning_effort":"high"}),
                &Rules::default(),
            )
            .unwrap();
        assert_eq!(body["messages"][1]["reasoning_content"], "", "{id}");
        assert!(body["messages"][3].get("reasoning_content").is_none(), "{id}");
        assert_eq!(
            body.get("thinking").map(|t| t["type"].clone()),
            thinking.map(Value::from)
        );
        let kept = json!([{"role":"assistant","reasoning_content":"r","tool_calls":[{"id":"c"}]}]);
        let body = m
            .request_with(&json!({"messages":kept}), &Rules::default())
            .unwrap();
        assert_eq!(body["messages"], kept, "{id}");
    }
}

#[test]
fn an_unlisted_model_keeps_the_plain_request() {
    let m = model("synthetic-flash");
    let payload = json!({"messages":tool_turn(),"reasoning_effort":"high","max_tokens":9000});
    let with_rules = m.request_with(&payload, &Rules::default()).unwrap();
    assert_eq!(with_rules, m.request(&payload).unwrap());
    assert!(with_rules.get("thinking").is_none());
    assert_eq!(with_rules["messages"], payload["messages"]);
    assert_eq!(with_rules["max_tokens"], 9000);
}

#[test]
fn owner_overrides_win_field_by_field_and_bad_ones_are_refused() {
    let rules = Rules::from_overrides(&json!({
        "glm-5.3*": {"replay":"strip","token_field":"max_completion_tokens","max_output":4096},
        "my-proxy/*": {"thinking":"object","route":"cloud"}
    }))
    .unwrap();
    let glm = rules.resolve(Route::Cloud, "opencode/GLM-5.3");
    assert_eq!(glm.replay, Replay::Strip);
    assert_eq!(glm.token_field, TokenField::MaxCompletionTokens);
    assert_eq!(glm.max_output, Some(4096));
    // Fields the owner left out keep the built-in value.
    assert_eq!(glm.thinking, Thinking::ObjectKeep);
    assert_eq!(glm.context_window, Some(1_000_000));
    assert_eq!(
        rules.resolve(Route::Cloud, "my-proxy/anything").thinking,
        Thinking::Object
    );
    assert_eq!(
        rules.resolve(Route::Local, "my-proxy/anything").thinking,
        Thinking::Template
    );

    let body = model("glm-5.3")
        .request_with(
            &json!({"messages":[{"role":"assistant","reasoning_content":"r","content":"x"}],"max_tokens":9000}),
            &rules,
        )
        .unwrap();
    assert!(body["messages"][0].get("reasoning_content").is_none());
    assert_eq!(body["max_completion_tokens"], 4096);
    assert!(body.get("max_tokens").is_none());

    for bad in [
        json!([]),
        json!({"x":1}),
        json!({"":{}}),
        json!({"x":{"unknown":1}}),
        json!({"x":{"replay":"sometimes"}}),
        json!({"x":{"max_output":0}}),
        json!({"x":{"context_window":-5}}),
        json!({"x":{"route":"moon"}}),
    ] {
        assert!(Rules::from_overrides(&bad).is_err(), "{bad}");
    }
    assert!(Rules::from_overrides(&json!({})).is_ok());
}

#[test]
fn local_aliases_have_rows_that_never_rewrite_a_cloud_body_or_name_cloud_quirks() {
    let rules = Rules::default();
    for alias in ["qwen3.6-27b", "gemma-4-12b", "some-local-alias"] {
        let rule = rules.resolve(Route::Local, alias);
        assert_eq!(rule.thinking, Thinking::Template, "{alias}");
        assert_eq!(rule.replay, Replay::AsIs, "{alias}");
        assert_eq!(rule.token_field, TokenField::MaxTokens, "{alias}");
        assert_eq!(rule.max_output, None, "{alias}");
    }
    // A cloud id is not a local row, and a local family is not a cloud row.
    assert_eq!(rules.resolve(Route::Local, "glm-5.3").context_window, None);
    assert_eq!(
        rules.resolve(Route::Cloud, "qwen3.6-27b").thinking,
        Thinking::Effort
    );
    // An owner can still pin a local alias.
    let own = Rules::from_overrides(&json!({"qwen3.6-27b":{"route":"local","replay":"strip"}})).unwrap();
    assert_eq!(own.resolve(Route::Local, "qwen3.6-27b").replay, Replay::Strip);
    assert_eq!(own.resolve(Route::Local, "qwen3.6-35b").replay, Replay::AsIs);
}
