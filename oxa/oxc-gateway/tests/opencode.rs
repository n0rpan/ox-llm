#![cfg(all(feature = "anthropic-client", feature = "ledger"))]
use oxc_gateway::opencode::{self, Error, Refusal, SessionHint, Usage};
use serde_json::{json, Value};

fn discovery() -> Value {
    json!({"data":[{"id":"synthetic-flash", "canonical_id":"synthetic/canonical", "limit":{"context":131072,"output":8192},
        "api":{"chat-completions":{"base_url":"https://example.invalid/inference/go/openai/v1", "compatibility":{"reasoning_field":"reasoning_content"}},
               "future-shape":{"base_url":"https://example.invalid/inference/go/future","unknown_hint":true}},
        "cost":{"input":0.1,"cache_read":0.01,"multipliers":{"night":0.5}},
        "capabilities":{"tools":true},"modalities":{"input":["text"],"output":["text"]}}]})
}
fn usage() -> Value {
    json!({"prompt_tokens":100,"completion_tokens":25,"cost":0,"opencode":{"billing_source":"go","allowance":0.00012,"list_cost":0.00015,
        "tokens":{"input":20,"output":25,"reasoning":5,"cache_read":80,"cache_write_5m":0,"cache_write_1h":0}}})
}
#[test]
fn discovery_keeps_shapes_hints_limits_and_prices_and_routes_by_shape() {
    let models = opencode::models(&discovery(), "https://example.invalid/inference/go").unwrap();
    let model = &models[0];
    assert_eq!((model.context_window, model.max_output), (131072, 8192));
    assert_eq!(model.api["future-shape"].hints["unknown_hint"], true);
    assert_eq!(
        model.api["chat-completions"].hints["compatibility"]["reasoning_field"],
        "reasoning_content"
    );
    assert_eq!(model.cost["multipliers"]["night"], 0.5);
    assert_eq!(
        model.chat_endpoint().unwrap().as_str(),
        "https://example.invalid/inference/go/openai/v1/chat/completions"
    );
    let mut no_chat = discovery();
    no_chat["data"][0]["api"]
        .as_object_mut()
        .unwrap()
        .remove("chat-completions");
    let model = opencode::models(&no_chat, "https://example.invalid/inference/go")
        .unwrap()
        .remove(0);
    assert_eq!(model.chat_endpoint(), Err(Error::UnsupportedShape));
}
#[test]
fn discovered_routes_cannot_send_credentials_to_other_origins_or_balances() {
    for url in [
        "https://other.invalid/inference/go/openai/v1",
        "https://example.invalid/inference/paid/v1",
        "https://example.invalid/inference/go-elsewhere/v1",
        "http://example.invalid/inference/go/v1",
        "https://user:synthetic@example.invalid/inference/go/v1",
        "https://example.invalid/inference/go/v1?key=synthetic",
        "https://example.invalid/inference/go/v1#synthetic",
        "https://example.invalid/inference/go/../paid/v1",
    ] {
        let mut value = discovery();
        value["data"][0]["api"]["chat-completions"]["base_url"] = url.into();
        assert!(
            opencode::models(&value, "https://example.invalid/inference/go").is_err(),
            "{url}"
        );
    }
    for base in [
        "http://127.0.0.1:19876/inference/go",
        "http://[::1]:19876/inference/go",
    ] {
        let mut value = discovery();
        value["data"][0]["api"] = json!({"chat-completions":{"base_url":format!("{base}/openai/v1")}});
        assert!(opencode::models(&value, base).is_ok());
    }
}
#[test]
fn malformed_catalog_is_never_a_partial_success_or_handwritten_fallback() {
    let mut duplicate = discovery();
    let item = duplicate["data"][0].clone();
    duplicate["data"].as_array_mut().unwrap().push(item);
    assert_eq!(
        opencode::models(&duplicate, "https://example.invalid/inference/go"),
        Err(Error::Discovery)
    );
    for (pointer, replacement) in [
        ("/data/0/limit/context", json!(0)),
        ("/data/0/limit/output", json!(131073)),
        ("/data/0/id", json!("")),
        ("/data/0/api", json!(["chat-completions"])),
        ("/data/0/api/chat-completions/base_url", Value::Null),
    ] {
        let mut value = discovery();
        *value.pointer_mut(pointer).unwrap() = replacement;
        assert!(
            opencode::models(&value, "https://example.invalid/inference/go").is_err(),
            "{pointer}"
        );
    }
}
#[test]
fn chat_payload_preserves_tools_reasoning_and_history_but_uses_discovered_route_and_limits() {
    let model = opencode::models(&discovery(), "https://example.invalid/inference/go")
        .unwrap()
        .remove(0);
    let payload = json!({"model":"untrusted-other", "messages":[{"role":"assistant","reasoning_content":"synthetic thought","tool_calls":[{"id":"c1","type":"function","function":{"name":"read","arguments":"{}"}}]},
        {"role":"tool","tool_call_id":"c1","content":"synthetic result"}], "tools":[{"type":"function","function":{"name":"read","parameters":{"type":"object"}}}],
        "max_tokens":9000,"provider":{"allow_fallbacks":true},"stream":false,"cache_prompt":true,"slot_id":7});
    let body = model.request(&payload).unwrap();
    assert_eq!(body["model"], "synthetic-flash");
    assert_eq!(body["max_tokens"], 8192);
    assert_eq!(body["messages"], payload["messages"]);
    assert_eq!(body["tools"], payload["tools"]);
    assert_eq!(body["stream_options"]["include_usage"], true);
    assert!(body.get("provider").is_none());
    assert!(body.get("slot_id").is_none());
    assert!(body.get("cache_prompt").is_none());
    for limit in [json!(0), json!(-1), json!(0.5), Value::Null] {
        assert!(model.request(&json!({"messages":[],"max_tokens":limit})).is_err());
    }
}
#[test]
fn subscription_usage_keeps_token_categories_and_exact_allowance_without_paid_fallback() {
    let parsed = Usage::parse(&usage()).unwrap();
    assert_eq!((parsed.prompt, parsed.completion), (100, 25));
    assert_eq!(parsed.tokens["cache_read"], 80);
    assert_eq!(parsed.tokens["reasoning"], 5);
    assert_eq!(parsed.allowance, json!(0.00012));
    assert_eq!(parsed.list_cost, json!(0.00015));
    for (pointer, replacement) in [
        ("/cost", json!(0.001)),
        ("/opencode/billing_source", json!("paid")),
        ("/opencode/allowance", json!(-1)),
        ("/opencode/list_cost", Value::Null),
        ("/opencode/tokens/reasoning", json!(-1)),
    ] {
        let mut value = usage();
        *value.pointer_mut(pointer).unwrap() = replacement;
        assert_eq!(Usage::parse(&value), Err(Error::Usage));
    }
    let mut tiny_paid = usage();
    tiny_paid["cost"] = serde_json::from_str("1e-999").unwrap();
    assert_eq!(Usage::parse(&tiny_paid), Err(Error::Usage));
    let mut huge = usage();
    huge["opencode"]["allowance"] = serde_json::from_str("1e999999999").unwrap();
    assert_eq!(Usage::parse(&huge), Err(Error::Usage));
}
#[test]
fn headers_are_agent_specific_stable_and_carry_only_the_supplied_private_digest() {
    let one = SessionHint::from_hmac([0x12; 32]);
    let two = SessionHint::from_hmac([0x13; 32]);
    let headers = one.headers();
    assert_eq!(headers, one.headers());
    assert_ne!(headers, two.headers());
    assert!(headers["user-agent"].to_str().unwrap().starts_with("oxa/"));
    assert_eq!(headers["x-opencode-session"].to_str().unwrap(), "12".repeat(32));
    assert!(headers["x-opencode-session"].is_sensitive());
    assert!(!format!("{headers:?}").contains(&"12".repeat(32)));
}
#[test]
fn refusals_are_typed_and_never_return_provider_messages_or_workspace_urls() {
    let body = json!({"error":{"type":"invalid_request_error", "message":"Model is hosted outside Europe and the United States; see https://example.invalid/workspaces/synthetic"}});
    assert_eq!(opencode::refusal(400, &body), Refusal::RegionBlocked);
    assert_eq!(opencode::refusal(429, &body), Refusal::LimitReached);
    assert_eq!(opencode::refusal(401, &body), Refusal::Unauthenticated);
    assert_eq!(opencode::refusal(403, &body), Refusal::Forbidden);
    assert!(!format!("{:?}", opencode::refusal(400, &body)).contains("workspaces"));
    assert_eq!(opencode::refusal(400, &json!({})), Refusal::Other(400));
}
