use super::tests::{anthropic, skeleton};
use super::*;
use crate::{
    catalog::{Access, Model, Provider},
    pricing::Price,
    runtime::Options,
};
use anthropic::{MockAnthropic, Reply, CREDENTIAL};

fn gateway(mock: &MockAnthropic) -> (Gateway, skeleton::Fixture) {
    let fixture = skeleton::Fixture::new(mock);
    std::fs::write(
        &fixture.key_path,
        json!({"openrouter":{"api_key":CREDENTIAL}}).to_string(),
    )
    .unwrap();
    let model = Model::new(
        skeleton::MODEL,
        Provider::OpenRouter,
        "synthetic-router",
        &mock.base_url(),
        1024,
        16,
    )
    .unwrap();
    let runtime = Runtime::new(
        [(
            model,
            Price::from_model(&fixture.config["cloud"]["models"][skeleton::MODEL]),
        )],
        fixture.key_path.clone(),
        fixture.ledger_dir.clone(),
        Options {
            access: Access::Enabled,
            daily_budget_usd: Some(1.0),
            ledger_enabled: true,
        },
    )
    .unwrap();
    (Gateway::new(runtime).unwrap(), fixture)
}
fn payload(include_usage: bool) -> Value {
    json!({"model":skeleton::MODEL,"messages":[{"role":"user","content":"Synthetic question"}],
        "max_tokens":16,"reasoning_effort":"none","provider":{"zdr":false},
        "stream_options":{"include_usage":include_usage}})
}
fn wire(chunks: Vec<Value>, done: bool) -> Reply {
    let mut wire = chunks
        .iter()
        .map(|v| format!("data: {v}\n\n"))
        .collect::<String>();
    if done {
        wire.push_str("data: [DONE]\n\n");
    }
    Reply::wire("text/event-stream", wire.as_bytes())
}
fn chunks() -> Vec<Value> {
    // Split the credential across JSON fields and network fragments.
    let split = CREDENTIAL.len() / 2;
    vec![
        json!({"choices":[{"index":0,"delta":{"content":format!("Safe {}", &CREDENTIAL[..split])},"finish_reason":null}]}),
        json!({"choices":[{"index":0,"delta":{"content":&CREDENTIAL[split..]},"finish_reason":null}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
        json!({"choices":[],"usage":{"prompt_tokens":21,"completion_tokens":5,"prompt_tokens_details":{"cached_tokens":7}}}),
    ]
}
fn rows(fixture: &skeleton::Fixture) -> Vec<Value> {
    std::fs::read_dir(&fixture.ledger_dir)
        .unwrap()
        .filter_map(|entry| {
            let path = entry.unwrap().path();
            (path.extension().is_some_and(|e| e == "jsonl")).then(|| std::fs::read_to_string(path).unwrap())
        })
        .flat_map(|s| {
            s.lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect::<Vec<_>>()
        })
        .collect()
}
#[tokio::test]
async fn openrouter_privacy_redaction_and_settlement_before_done() {
    for include_usage in [false, true] {
        let mock = MockAnthropic::start(vec![wire(chunks(), true)]);
        let (gateway, fixture) = gateway(&mock);
        let mut output = Vec::new();
        let mut done = 0;
        let completed = gateway
            .chat_payload(
                &payload(include_usage),
                Context::default(),
                &Cancellation::default(),
                &mut |event| {
                    match event {
                        Event::Chunk(value) => output.push(value),
                        Event::Done => {
                            done += 1;
                            assert_eq!(rows(&fixture).len(), 1);
                        }
                    }
                    Control::Continue
                },
            )
            .await
            .unwrap();
        assert_eq!(done, 1);
        assert_eq!(completed.assistant["content"], "Safe [redacted]");
        assert!(!json!(output).to_string().contains(CREDENTIAL));
        assert_eq!(output.iter().any(|v| v.get("usage").is_some()), include_usage);
        let row = rows(&fixture).pop().unwrap();
        assert_eq!(row["settled"], "usage");
        assert_eq!(row["prompt_tokens"], 21);
        assert_eq!(row["cached_tokens"], 7);
        assert_eq!(row["remote_provider"], "openrouter");
        let request = mock.requests().pop().unwrap();
        assert_eq!(request.target, "/v1/chat/completions");
        assert_eq!(
            request.header("authorization"),
            Some(format!("Bearer {CREDENTIAL}").as_str())
        );
        assert!(request.header("x-api-key").is_none());
        assert_eq!(request.body["model"], "synthetic-router");
        assert_eq!(
            request.body["provider"],
            json!({"zdr":true,"data_collection":"deny"})
        );
        assert_eq!(request.body["reasoning"], json!({"enabled":false}));
        assert!(request.body.get("reasoning_effort").is_none());
        assert_eq!(request.body["stream_options"]["include_usage"], true);
        mock.finish();
    }
}
#[tokio::test]
async fn openrouter_incomplete_or_invalid_stream_never_signals_done() {
    let mut invalid_usage = chunks();
    invalid_usage[3]["usage"]["prompt_tokens_details"]["cached_tokens"] = json!(22);
    for reply in [
        wire(chunks(), false),
        wire(invalid_usage, true),
        wire(vec![json!({"error":{"message":CREDENTIAL}})], true),
    ] {
        let mock = MockAnthropic::start(vec![reply]);
        let (gateway, fixture) = gateway(&mock);
        let error = gateway
            .chat_payload(
                &payload(true),
                Context::default(),
                &Cancellation::default(),
                &mut |event| {
                    assert!(!matches!(event, Event::Done));
                    Control::Continue
                },
            )
            .await
            .err()
            .unwrap();
        assert_eq!(error.refusal().code, "cloud_upstream_stream_broken");
        assert_eq!(rows(&fixture)[0]["settled"], "reserved");
    }
}
#[tokio::test]
async fn openrouter_credit_refusal_does_not_charge_the_reservation() {
    let mock = MockAnthropic::start(vec![Reply::error(402, CREDENTIAL)]);
    let (gateway, fixture) = gateway(&mock);
    let error = gateway
        .chat_payload(
            &payload(true),
            Context::default(),
            &Cancellation::default(),
            &mut |_| panic!("no output on refusal"),
        )
        .await
        .err()
        .unwrap();
    assert_eq!(error.refusal().status, 402);
    let row = rows(&fixture).pop().unwrap();
    assert_eq!(row["settled"], "refused");
    assert_eq!(row["cost_usd"], 0.0);
}
#[tokio::test]
async fn openrouter_cancel_on_final_chunk_preserves_private_usage() {
    let mock = MockAnthropic::start(vec![wire(chunks(), true)]);
    let (gateway, fixture) = gateway(&mock);
    let result = gateway
        .chat_payload(
            &payload(false),
            Context::default(),
            &Cancellation::default(),
            &mut |event| match event {
                Event::Done => panic!("cancelled delivery cannot signal done"),
                Event::Chunk(chunk) if !chunk["choices"][0]["finish_reason"].is_null() => Control::Stop,
                _ => Control::Continue,
            },
        )
        .await;
    assert!(matches!(
        result,
        Err(Error::Upstream(anthropic_client::Error::Cancelled))
    ));
    let row = rows(&fixture).pop().unwrap();
    assert_eq!(row["status"], 499);
    assert_eq!(row["settled"], "usage");
    assert_eq!(row["cached_tokens"], 7);
}

#[tokio::test]
async fn openrouter_fragmented_tools_and_usage_on_finish_chunk() {
    let mut events = vec![
        json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":3,"id":"synthetic-","function":{"name":"re","arguments":"{\"path\":"}}]},"finish_reason":null}]}),
        json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":3,"id":"call","function":{"name":"ad","arguments":"\"note.txt\"}"}}]},"finish_reason":null}]}),
    ];
    events.push(
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],
        "usage":{"prompt_tokens":21,"completion_tokens":5,"prompt_tokens_details":{"cached_tokens":7}}}),
    );
    let mock = MockAnthropic::start(vec![wire(events, true)]);
    let (gateway, fixture) = gateway(&mock);
    let mut output = Vec::new();
    let completed = gateway
        .chat_payload(
            &payload(true),
            Context::default(),
            &Cancellation::default(),
            &mut |event| {
                if let Event::Chunk(chunk) = event {
                    output.push(chunk);
                }
                Control::Continue
            },
        )
        .await
        .unwrap();
    assert_eq!(completed.finish_reason, "tool_calls");
    assert_eq!(
        completed.assistant["tool_calls"],
        json!([{"id":"synthetic-call","type":"function","function":{"name":"read","arguments":"{\"path\":\"note.txt\"}"}}])
    );
    let call = output
        .iter()
        .find_map(|v| v.pointer("/choices/0/delta/tool_calls/0"))
        .unwrap();
    assert_eq!(call["index"], 0);
    assert_eq!(call["function"]["name"], "read");
    assert_eq!(rows(&fixture)[0]["settled"], "usage");
    mock.finish();
}
