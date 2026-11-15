#![cfg(feature = "anthropic-gateway")]
#[allow(dead_code)]
#[path = "common/anthropic.rs"]
mod anthropic;
#[path = "common/skeleton.rs"]
mod skeleton;

use anthropic::{MockAnthropic, Reply};
use oxc_gateway::{
    anthropic_client::{Cancellation, Control},
    ledger::Context,
    runtime::{Options, Runtime},
    Gateway,
};
use serde_json::{json, Value};

fn native() -> Vec<Value> {
    vec![
        json!({"type":"thinking","thinking":"synthetic plan","signature":"synthetic-signature"}),
        json!({"type":"redacted_thinking","data":"synthetic-opaque-data"}),
        json!({"type":"text","text":"I will read."}),
        json!({"type":"tool_use","id":"tool-1","name":"read","input":{"path":"synthetic.txt"}}),
    ]
}
fn thinking_tool_reply() -> Reply {
    let mut events = vec![json!({"type":"message_start","message":{
        "id":"synthetic-message","role":"assistant","content":[],
        "usage":{"input_tokens":11,"output_tokens":0,"cache_read_input_tokens":7,
            "cache_creation_input_tokens":3}}})];
    for (index, block) in native().into_iter().enumerate() {
        let (start, deltas) = match block["type"].as_str().unwrap() {
            "thinking" => (
                json!({"type":"thinking","thinking":"","signature":""}),
                vec![
                    json!({"type":"thinking_delta","thinking":block["thinking"]}),
                    json!({"type":"signature_delta","signature":block["signature"]}),
                ],
            ),
            "text" => (
                json!({"type":"text","text":""}),
                vec![json!({"type":"text_delta","text":block["text"]})],
            ),
            "tool_use" => (
                json!({"type":"tool_use","id":block["id"],"name":block["name"],"input":{}}),
                vec![json!({"type":"input_json_delta","partial_json":block["input"].to_string()})],
            ),
            _ => (block, vec![]),
        };
        events.push(json!({"type":"content_block_start","index":index,"content_block":start}));
        for delta in deltas {
            events.push(json!({"type":"content_block_delta","index":index,"delta":delta}));
        }
        events.push(json!({"type":"content_block_stop","index":index}));
    }
    events.push(json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},
        "usage":{"output_tokens":5}}));
    events.push(json!({"type":"message_stop"}));
    Reply::events(events)
}

#[tokio::test]
async fn gateway_replays_native_thinking_for_a_tool_turn_but_never_after_key_rotation() {
    let mock = MockAnthropic::start(vec![
        thinking_tool_reply(),
        Reply::text("Synthetic completed turn", 11, 7, 3, 5),
        Reply::text("Synthetic new credential turn", 11, 7, 3, 5),
    ]);
    let fixture = skeleton::Fixture::new(&mock);
    let gateway = Gateway::new(
        Runtime::new(
            fixture.config.clone(),
            fixture.key_path.clone(),
            fixture.ledger_dir.clone(),
            Options::default(),
        )
        .unwrap(),
    )
    .unwrap();
    let mut payload = json!({"model":skeleton::MODEL,"messages":[{
        "role":"user","content":"Read the synthetic file"}],"max_tokens":16,
        "reasoning_effort":"high","stream_options":{"include_usage":true},
        "tools":[{"type":"function","function":{"name":"read","parameters":{
            "type":"object","properties":{"path":{"type":"string"}}}}}]});
    let first = gateway
        .chat(
            &payload,
            Context::default(),
            &Cancellation::default(),
            &mut |_| Control::Continue,
        )
        .await
        .unwrap();
    assert_eq!(first.native_blocks, native());
    payload["messages"].as_array_mut().unwrap().extend([
        first.assistant,
        json!({"role":"tool","tool_call_id":"tool-1","content":"synthetic contents"}),
    ]);
    gateway
        .chat(
            &payload,
            Context::default(),
            &Cancellation::default(),
            &mut |_| Control::Continue,
        )
        .await
        .unwrap();
    let requests = mock.wait_for_requests(2);
    assert_eq!(
        requests[1].body["messages"][1]["content"],
        json!(native()),
        "the real continuation must replay opaque signed blocks unchanged"
    );

    std::fs::write(
        &fixture.key_path,
        json!({"anthropic":"rotated-synthetic-key"}).to_string(),
    )
    .unwrap();
    gateway
        .chat(
            &payload,
            Context::default(),
            &Cancellation::default(),
            &mut |_| Control::Continue,
        )
        .await
        .unwrap();
    let requests = mock.wait_for_requests(3);
    let sent = requests[2].body["messages"][1]["content"].as_array().unwrap();
    assert!(sent
        .iter()
        .all(|block| !matches!(block["type"].as_str(), Some("thinking" | "redacted_thinking"))));
    assert_eq!(requests[2].header("x-api-key"), Some("rotated-synthetic-key"));
    assert_eq!(requests.len(), 3);
    mock.assert_healthy();
}
