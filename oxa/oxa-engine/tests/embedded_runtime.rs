#![cfg(feature = "in-process-gateway")]
#[allow(dead_code)]
#[path = "../../oxc-gateway/tests/common/anthropic.rs"]
mod anthropic;
#[path = "../../oxc-gateway/tests/common/skeleton.rs"]
mod skeleton;
use anthropic::{MockAnthropic, Reply};
use oxa_engine::{EmbeddedEngine, Gateway, GatewayOptions, GatewayRuntime};
use oxa_proto::{codes, CancelToken, ChatRequest, Engine, FrozenPrefix, StreamEvent, WireMsg};
use oxc_gateway::{
    catalog::{Access, Model, Provider},
    pricing::Price,
};
use serde_json::json;
use std::{
    sync::{mpsc, Arc},
    thread,
    time::Duration,
};

fn engine(fixture: &skeleton::Fixture, cap: f64) -> EmbeddedEngine {
    let model = Model::new(
        skeleton::MODEL,
        Provider::Anthropic,
        skeleton::UPSTREAM,
        fixture.config["cloud"]["models"][skeleton::MODEL]["base_url"]
            .as_str()
            .unwrap(),
        1024,
        16,
    )
    .unwrap();
    let runtime = GatewayRuntime::new(
        [(
            model,
            Price::from_model(&fixture.config["cloud"]["models"][skeleton::MODEL])
                .map(oxc_gateway::runtime::Billing::Metered),
        )],
        fixture.key_path.clone(),
        fixture.ledger_dir.clone(),
        GatewayOptions {
            access: Access::Enabled,
            daily_budget_usd: Some(cap),
            ledger_enabled: true,
            subscription: None,
            subscription_limits: None,
        },
    )
    .unwrap();
    EmbeddedEngine::from_gateway(Gateway::new(runtime).unwrap())
}
fn prefix() -> FrozenPrefix {
    FrozenPrefix {
        sections: vec![("preamble".into(), "Synthetic system prompt".into())],
        tools_json: "[]".into(),
        sha256: [0; 32],
    }
}
fn chat(
    engine: &EmbeddedEngine,
    cancel: &CancelToken,
) -> Result<oxa_proto::AssistantTurn, oxa_proto::OxaError> {
    let prefix = prefix();
    let messages = [WireMsg::user("Synthetic question")];
    engine.chat(
        &ChatRequest {
            max_tokens: 1000,
            ..ChatRequest::new(&prefix, &messages, skeleton::MODEL)
        },
        cancel,
        &mut |_| {},
    )
}
#[test]
fn actual_gateway_delivers_on_the_caller_thread_and_keeps_discovery_private() {
    let mock = MockAnthropic::start(vec![Reply::text("Synthetic λ reply", 11, 7, 3, 5)]);
    let fixture = skeleton::Fixture::new(&mock);
    let engine = engine(&fixture, 1.0);
    let info = engine.info(None).unwrap();
    assert!(info.caps.remote);
    assert_eq!(
        (info.model.as_str(), info.n_ctx, info.max_output),
        (skeleton::MODEL, 1024, 16)
    );
    assert!(mock.requests().is_empty());
    assert_eq!(std::fs::read_dir(&fixture.ledger_dir).unwrap().count(), 0);
    let caller = thread::current().id();
    let prefix = prefix();
    let messages = [WireMsg::user("Synthetic question")];
    let mut text = String::new();
    let turn = engine
        .chat(
            &ChatRequest {
                max_tokens: 1000,
                ..ChatRequest::new(&prefix, &messages, skeleton::MODEL)
            },
            &CancelToken::new(),
            &mut |event| {
                assert_eq!(thread::current().id(), caller);
                if let StreamEvent::Text(delta) = event {
                    text.push_str(&delta);
                }
            },
        )
        .unwrap();
    assert_eq!(text, "Synthetic λ reply");
    assert_eq!(turn.msg.content.as_deref(), Some(text.as_str()));
    assert_eq!((turn.usage.prompt, turn.usage.completion), (21, 5));
    assert_eq!(mock.requests()[0].body["max_tokens"], 16);
    mock.finish();
}
#[test]
fn cancellation_wakes_a_live_header_wait_and_settles_before_returning() {
    let (reply, _gate) = Reply::text("Synthetic reply", 11, 7, 3, 5).gated();
    let mock = Arc::new(MockAnthropic::start(vec![reply]));
    let fixture = skeleton::Fixture::new(&mock);
    let engine = engine(&fixture, 1.0);
    let cancel = CancelToken::new();
    let other = cancel.clone();
    let (sent, received) = mpsc::channel();
    let worker = thread::spawn(move || sent.send(chat(&engine, &cancel)).unwrap());
    mock.wait_for_requests(1);
    other.cancel();
    let result = received.recv_timeout(Duration::from_secs(2)).unwrap();
    worker.join().unwrap();
    assert_eq!(result.unwrap_err().code, codes::ABORTED);
    assert_eq!(mock.requests().len(), 1);
    assert!(std::fs::read_dir(&fixture.ledger_dir).unwrap().any(|entry| entry
        .unwrap()
        .path()
        .extension()
        .is_some_and(|extension| extension == "jsonl")));
}
#[test]
fn a_waiting_request_does_not_serialize_other_calls_on_the_same_engine() {
    let (first, release) = Reply::text("First", 11, 7, 3, 5).gated();
    let mock = MockAnthropic::start(vec![first, Reply::text("Second", 11, 7, 3, 5)]);
    let fixture = skeleton::Fixture::new(&mock);
    let engine = Arc::new(engine(&fixture, 1.0));
    let first = thread::spawn({
        let engine = engine.clone();
        move || chat(&engine, &CancelToken::new())
    });
    mock.wait_for_requests(1);
    let (sent, received) = mpsc::channel();
    let second = thread::spawn({
        let engine = engine.clone();
        move || sent.send(chat(&engine, &CancelToken::new())).unwrap()
    });
    let result = received.recv_timeout(Duration::from_secs(2));
    release.send(()).unwrap();
    first.join().unwrap().unwrap();
    second.join().unwrap();
    assert_eq!(result.unwrap().unwrap().msg.content.as_deref(), Some("Second"));
    assert_eq!(mock.requests().len(), 2);
}
#[test]
fn refusal_classes_survive_the_typed_bridge_without_provider_contact() {
    let mock = MockAnthropic::start(vec![]);
    let fixture = skeleton::Fixture::new(&mock);
    let budget_error = chat(&engine(&fixture, 1e-9), &CancelToken::new()).unwrap_err();
    assert_eq!(budget_error.code, codes::BUDGET_EXHAUSTED);
    // The native bridge names oxa's own setting: the controller's key does not exist here.
    assert_eq!(
        budget_error.hint.as_deref(),
        Some(
            "adjust oxa.cloud.daily_budget_usd in oxa user settings, or wait for the next spend day; this is not retried"
        )
    );
    std::fs::remove_file(&fixture.key_path).unwrap();
    assert_eq!(
        chat(&engine(&fixture, 1.0), &CancelToken::new())
            .unwrap_err()
            .code,
        "cloud_credentials_unavailable"
    );
    assert!(mock.requests().is_empty());
}
#[test]
fn native_thinking_tool_continuation_restores_opaque_signatures_privately() {
    let first = Reply::events(vec![
        json!({"type":"message_start","message":{"usage":{"input_tokens":11,"output_tokens":0}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Synthetic plan"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"opaque-signature"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"read-1","name":"read","input":{}}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"synthetic\"}"}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"content_block_start","index":2,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"<think>More reasoning</think>Visible answer"}}),
        json!({"type":"content_block_stop","index":2}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":5}}),
        json!({"type":"message_stop"}),
    ]);
    let mock = MockAnthropic::start(vec![first, Reply::text("Done", 11, 7, 3, 5)]);
    let fixture = skeleton::Fixture::new(&mock);
    let first_engine = engine(&fixture, 1.0);
    let mut prefix = prefix();
    prefix.tools_json =
        json!([{"type":"function","function":{"name":"read","parameters":{"type":"object"}}}]).to_string();
    let mut messages = vec![WireMsg::user("Read synthetic")];
    let mut turn = first_engine
        .chat(
            &ChatRequest::new(&prefix, &messages, skeleton::MODEL),
            &CancelToken::new(),
            &mut |_| {},
        )
        .unwrap();
    assert_eq!(turn.msg.tool_calls[0].args_raw, "{\"path\":\"synthetic\"}");
    assert_eq!(turn.msg.content.as_deref(), Some("Visible answer"));
    assert_eq!(
        turn.msg.reasoning.as_deref(),
        Some("Synthetic planMore reasoning")
    );
    let saved = turn.msg.provider_history.as_ref().unwrap().to_session_value();
    turn.msg.provider_history = Some(oxa_proto::ProviderHistory::from_session_value(&saved).unwrap());
    assert!(!format!("{:?}", turn.msg).contains("opaque-signature"));
    assert!(!String::from_utf8(turn.msg.to_json())
        .unwrap()
        .contains("opaque-signature"));
    messages.push(turn.msg);
    messages.push(WireMsg::tool_result("read-1", "Synthetic result"));
    drop(first_engine);
    let restarted = engine(&fixture, 1.0);
    restarted
        .chat(
            &ChatRequest::new(&prefix, &messages, skeleton::MODEL),
            &CancelToken::new(),
            &mut |_| {},
        )
        .unwrap();
    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1].body["messages"][1]["content"][0]["signature"],
        "opaque-signature"
    );
    assert!(requests[1].body["messages"][1]["content"][0]
        .get("cache_control")
        .is_none());
}
