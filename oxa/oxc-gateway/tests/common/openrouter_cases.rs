use super::tests::{anthropic, skeleton};
use super::*;
use crate::{
    billing::Billing,
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
            Price::from_model(&fixture.config["cloud"]["models"][skeleton::MODEL]).map(Billing::Metered),
        )],
        fixture.key_path.clone(),
        fixture.ledger_dir.clone(),
        Options {
            access: Access::Enabled,
            daily_budget_usd: Some(1.0),
            ledger_enabled: true,
            ..Options::default()
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
async fn openrouter_preserves_reported_reasoning_tokens() {
    for reported in [None, Some(json!(3)), Some(json!(0)), Some(json!("invalid"))] {
        let mut events = chunks();
        if let Some(value) = &reported {
            events[3]["usage"]["completion_tokens_details"] = json!({"reasoning_tokens":value});
        }
        let mock = MockAnthropic::start(vec![wire(events, true)]);
        let (gateway, fixture) = gateway(&mock);
        let mut output = Vec::new();
        let completed = gateway
            .chat_payload(
                &payload(true),
                Context::default(),
                &Cancellation::default(),
                &mut |event| {
                    if let Event::Chunk(value) = event {
                        output.push(value);
                    }
                    Control::Continue
                },
            )
            .await
            .unwrap();
        let usage = &output.iter().find(|value| value.get("usage").is_some()).unwrap()["usage"];
        assert_eq!(
            usage["completion_tokens_details"]["reasoning_tokens"],
            reported.and_then(|value| value.as_u64()).unwrap_or(0)
        );
        assert_eq!(usage["completion_tokens"], 5);
        assert!(completed.replay_items.is_empty());
        assert!(completed.history.is_none());
        assert_eq!(rows(&fixture)[0]["settled"], "usage");
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
    let mock = MockAnthropic::start(vec![wire(chunks_with_cost(Some(json!(0.25))), true)]);
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
    assert_eq!(row["reported_cost_usd"], 0.25);
    assert_eq!(row["cost_usd"], 0.25);
    mock.finish();
}

#[tokio::test]
async fn openrouter_decoded_tool_arguments_refuse_held_key_and_preserve_clean_bytes() {
    // Escapes exercise ordinary tool JSON decoding, not credential discovery.
    let escaped: String = CREDENTIAL
        .chars()
        .map(|c| format!("\\u{:04x}", c as u32))
        .collect();
    let clean = r#"{ "path" : "note.txt", "meta\u0064ata" : [{"text":"benign \u03bb"}] }"#;
    let cases = [
        format!(r#"{{"path":"{escaped}"}}"#),
        format!(r#"{{"metadata":[{{"value":"{escaped}"}}]}}"#),
        format!(r#"{{"metadata":[{{"{escaped}":"value"}}]}}"#),
        format!(r#"{{"{escaped}":"value"}}"#),
        clean.to_owned(),
    ];
    for arguments in cases {
        let benign = arguments == clean;
        let calls = json!([
            {"index":0,"id":"clean-first","type":"function",
                "function":{"name":"read","arguments":"{\"path\":\"note.txt\"}"}},
            {"index":1,"id":"synthetic-call","type":"function",
                "function":{"name":"read","arguments":arguments}}
        ]);
        let mock = MockAnthropic::start(vec![wire(
            vec![
                json!({"choices":[{"index":0,"delta":{"tool_calls":calls},"finish_reason":null}]}),
                json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],
                "usage":{"prompt_tokens":21,"completion_tokens":5}}),
            ],
            true,
        )]);
        let (gateway, fixture) = gateway(&mock);
        let mut output = Vec::new();
        let mut done = false;
        let result = gateway
            .chat_payload(
                &payload(true),
                Context::default(),
                &Cancellation::default(),
                &mut |event| {
                    match event {
                        Event::Chunk(chunk) => output.push(chunk),
                        Event::Done => done = true,
                    }
                    Control::Continue
                },
            )
            .await;
        if benign {
            let completed = result.unwrap();
            assert!(done);
            let call = &completed.assistant["tool_calls"][1];
            assert_eq!(call["id"], "synthetic-call");
            assert_eq!(call["function"]["name"], "read");
            assert_eq!(call["function"]["arguments"], clean);
            let streamed = output
                .iter()
                .find_map(|chunk| chunk.pointer("/choices/0/delta/tool_calls"))
                .unwrap();
            assert_eq!(streamed[1]["function"]["arguments"], clean);
            assert_eq!(streamed[1]["id"], "synthetic-call");
            assert_eq!(rows(&fixture)[0]["settled"], "usage");
        } else {
            assert!(
                result.is_err(),
                "decoded held key must not reach executable calls"
            );
            assert!(!done);
            assert!(
                output
                    .iter()
                    .all(|chunk| chunk.pointer("/choices/0/delta/tool_calls").is_none()),
                "even clean earlier calls must stay buffered on refusal"
            );
            assert_eq!(rows(&fixture)[0]["settled"], "reserved");
        }
        mock.finish();
    }
}

#[tokio::test]
async fn openrouter_held_key_equal_to_redaction_marker_is_still_refused() {
    let mock = MockAnthropic::start(vec![wire(
        vec![
            json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"synthetic-call",
            "function":{"name":"write","arguments":r#"{"path":"leak.txt","content":"[redacted]"}"#}}]},"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],
            "usage":{"prompt_tokens":21,"completion_tokens":5}}),
        ],
        true,
    )]);
    let (gateway, fixture) = gateway(&mock);
    std::fs::write(
        &fixture.key_path,
        json!({"openrouter":{"api_key":"[redacted]"}}).to_string(),
    )
    .unwrap();
    let result = gateway
        .chat_payload(
            &payload(true),
            Context::default(),
            &Cancellation::default(),
            &mut |event| {
                assert!(!matches!(event, Event::Done));
                if let Event::Chunk(chunk) = event {
                    assert!(chunk.pointer("/choices/0/delta/tool_calls").is_none());
                }
                Control::Continue
            },
        )
        .await;
    assert!(result.is_err());
    mock.finish();
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

fn chunks_with_cost(cost: Option<Value>) -> Vec<Value> {
    let mut events = chunks();
    if let Some(cost) = cost {
        events[3]["usage"]["cost"] = cost;
    }
    events
}

#[tokio::test]
async fn openrouter_reported_cost_reaches_ledger_and_budget_with_or_without_public_usage() {
    for reported in [0.0, 0.00000001, 1.0, 1e101] {
        for include_usage in [false, true] {
            let mock = MockAnthropic::start(vec![wire(chunks_with_cost(Some(json!(reported))), true)]);
            let (gateway, fixture) = gateway(&mock);
            let mut request = payload(include_usage);
            // Client-supplied billing fields are never a source of settled cost.
            request["usage"] = json!({"cost":0.5});
            request["cost"] = json!(0.5);
            request["reported_cost"] = json!(0.5);
            let mut public_usage = None;
            gateway
                .chat_payload(
                    &request,
                    Context::default(),
                    &Cancellation::default(),
                    &mut |event| {
                        match event {
                            Event::Chunk(value) => {
                                if value.get("usage").is_some() {
                                    public_usage = Some(value["usage"].clone());
                                }
                            }
                            Event::Done => {
                                let row = rows(&fixture).pop().unwrap();
                                assert_eq!(row["cost_usd"], json!(reported));
                                assert_eq!(row["reported_cost_usd"], json!(reported));
                            }
                        }
                        Control::Continue
                    },
                )
                .await
                .unwrap();
            let row = rows(&fixture).pop().unwrap();
            assert_eq!(row["settled"], "usage");
            assert_eq!(row["cost_usd"], json!(reported));
            assert!(row["catalog_cost_usd"]
                .as_f64()
                .is_some_and(|cost| cost > 0.0 && cost < 1.0));
            assert_eq!(public_usage.is_some(), include_usage);
            if let Some(usage) = public_usage {
                assert_eq!(usage["cost"], json!(reported));
            }
            if reported >= 1.0 {
                let error = gateway
                    .chat_payload(
                        &payload(false),
                        Context::default(),
                        &Cancellation::default(),
                        &mut |_| panic!("budget refusal must precede forwarding"),
                    )
                    .await
                    .err()
                    .unwrap();
                assert_eq!(error.refusal().code, "budget_exceeded");
            }
            assert_eq!(mock.requests().len(), 1);
            let sent = &mock.requests()[0].body;
            for name in ["usage", "cost", "reported_cost"] {
                assert!(sent.get(name).is_none());
            }
            mock.finish();
        }
    }
}

#[tokio::test]
async fn openrouter_absent_or_invalid_reported_cost_uses_catalog() {
    for cost in [
        None,
        Some(Value::Null),
        Some(json!(-0.1)),
        Some(json!("0.1")),
        Some(json!(true)),
        Some(json!({"cost":0.1})),
        // The existing arbitrary-precision JSON parser can retain numbers
        // outside f64; they are invalid reported costs, not token errors.
        Some(serde_json::from_str("1e999").unwrap()),
    ] {
        let mock = MockAnthropic::start(vec![wire(chunks_with_cost(cost), true)]);
        let (gateway, fixture) = gateway(&mock);
        let mut public_usage = None;
        gateway
            .chat_payload(
                &payload(true),
                Context::default(),
                &Cancellation::default(),
                &mut |event| {
                    if let Event::Chunk(value) = event {
                        if value.get("usage").is_some() {
                            public_usage = Some(value["usage"].clone());
                        }
                    }
                    Control::Continue
                },
            )
            .await
            .unwrap();
        let row = rows(&fixture).pop().unwrap();
        assert_eq!(row["cost_usd"], row["catalog_cost_usd"]);
        assert!(row["cost_usd"].as_f64().is_some_and(|cost| cost > 0.0));
        assert!(row.get("reported_cost_usd").is_none());
        assert!(public_usage.unwrap().get("cost").is_none());
        assert_eq!(row["settled"], "usage");
        mock.finish();
    }
}

#[tokio::test]
async fn openrouter_preserves_catalog_thinking_mapping() {
    for (effort, expected) in [
        ("none", json!({"enabled":false})),
        ("minimal", json!({"enabled":false})),
        ("low", json!({"effort":"low"})),
        ("medium", json!({"effort":"medium"})),
        ("high", json!({"effort":"high"})),
        ("xhigh", json!({"effort":"high"})),
    ] {
        let mock = MockAnthropic::start(vec![wire(chunks(), true)]);
        let (gateway, _fixture) = gateway(&mock);
        let mut request = payload(false);
        request["reasoning_effort"] = json!(effort);
        gateway
            .chat_payload(
                &request,
                Context::default(),
                &Cancellation::default(),
                &mut |_| Control::Continue,
            )
            .await
            .unwrap();
        assert_eq!(mock.requests()[0].body["reasoning"], expected, "{effort}");
        mock.finish();
    }
}

#[tokio::test]
async fn openrouter_length_discards_tools_and_preserves_terminal_metering() {
    for include_usage in [false, true] {
        // Even syntactically complete calls cannot execute after a length stop.
        for arguments in [r#"{"path":"#, r#"{"path":"note.txt"}"#, CREDENTIAL] {
            let events = vec![
                json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"synthetic-call",
                    "function":{"name":"read","arguments":arguments}}]},"finish_reason":null}]}),
                json!({"choices":[{"index":0,"delta":{},"finish_reason":"length"}],
                    "usage":{"prompt_tokens":21,"completion_tokens":5,"cost":0.25}}),
            ];
            let mock = MockAnthropic::start(vec![wire(events, true)]);
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
                                assert_eq!(rows(&fixture)[0]["cost_usd"], 0.25);
                            }
                        }
                        Control::Continue
                    },
                )
                .await
                .unwrap();
            assert_eq!(done, 1);
            assert_eq!(completed.finish_reason, "length");
            assert!(completed.assistant.get("tool_calls").is_none());
            assert!(output
                .iter()
                .all(|value| value.pointer("/choices/0/delta/tool_calls").is_none()));
            assert!(output
                .iter()
                .any(|value| value.pointer("/choices/0/finish_reason") == Some(&json!("length"))));
            assert_eq!(
                output.iter().any(|value| value.get("usage").is_some()),
                include_usage
            );
            assert!(!json!(output).to_string().contains(CREDENTIAL));
            let row = rows(&fixture).pop().unwrap();
            assert_eq!(row["settled"], "usage");
            assert_eq!(row["reported_cost_usd"], 0.25);
            assert_eq!(row["prompt_tokens"], 21);
            assert_eq!(row["completion_tokens"], 5);
            mock.finish();
        }
    }
}

#[tokio::test]
async fn openrouter_reasoning_content_takes_precedence_over_alias() {
    for (delta, expected) in [
        (json!({"reasoning_content":"Think", "reasoning":"Think"}), "Think"),
        (
            json!({"reasoning_content":"Canonical", "reasoning":"Ignored"}),
            "Canonical",
        ),
        (json!({"reasoning":"Fallback"}), "Fallback"),
        (json!({"reasoning_content":"Canonical"}), "Canonical"),
        (json!({"reasoning_content":"", "reasoning":"Ignored"}), ""),
        (json!({"reasoning_content":null, "reasoning":"Ignored"}), ""),
    ] {
        let mut events = chunks();
        events.insert(
            0,
            json!({"choices":[{"index":0,"delta":delta,"finish_reason":null}]}),
        );
        let mock = MockAnthropic::start(vec![wire(events, true)]);
        let (gateway, fixture) = gateway(&mock);
        let mut reasoning = String::new();
        let completed = gateway
            .chat_payload(
                &payload(true),
                Context::default(),
                &Cancellation::default(),
                &mut |event| {
                    if let Event::Chunk(chunk) = event {
                        assert!(chunk.pointer("/choices/0/delta/reasoning").is_none());
                        if let Some(text) = chunk
                            .pointer("/choices/0/delta/reasoning_content")
                            .and_then(Value::as_str)
                        {
                            reasoning.push_str(text);
                        }
                    }
                    Control::Continue
                },
            )
            .await
            .unwrap();
        assert_eq!(reasoning, expected, "{delta}");
        assert_eq!(
            completed.assistant["reasoning_content"].as_str().unwrap_or(""),
            expected,
            "{delta}"
        );
        assert_eq!(rows(&fixture)[0]["settled"], "usage");
        mock.finish();
    }
}

async fn normalized_tool_reply(calls: Value) -> (Completed, Vec<Value>, Vec<Value>) {
    let mock = MockAnthropic::start(vec![wire(
        vec![
            json!({"choices":[{"index":0,"delta":{"tool_calls":calls},"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],
            "usage":{"prompt_tokens":21,"completion_tokens":5,"cost":0.25}}),
        ],
        true,
    )]);
    let (gateway, fixture) = gateway(&mock);
    let mut output = Vec::new();
    let completed = gateway
        .chat_payload(
            &payload(true),
            Context::default(),
            &Cancellation::default(),
            &mut |event| {
                match event {
                    Event::Chunk(value) => output.push(value),
                    Event::Done => assert_eq!(rows(&fixture)[0]["cost_usd"], 0.25),
                }
                Control::Continue
            },
        )
        .await
        .unwrap();
    let ledger = rows(&fixture);
    mock.finish();
    (completed, output, ledger)
}

#[tokio::test]
async fn openrouter_missing_tool_indices_use_array_positions() {
    let raw = r#"{ "path" : "note.txt" }"#;
    let calls = json!([
        {"id":"first-call","function":{"name":"read","arguments":raw}},
        {"id":"second-call","function":{"name":"read","arguments":raw}},
    ]);
    let (completed, output, ledger) = normalized_tool_reply(calls).await;
    let tools = completed.assistant["tool_calls"].as_array().unwrap();
    assert_eq!(tools.len(), 2);
    let streamed = output
        .iter()
        .find_map(|v| v.pointer("/choices/0/delta/tool_calls"))
        .unwrap();
    for (index, id) in ["first-call", "second-call"].into_iter().enumerate() {
        assert_eq!(tools[index]["id"], id);
        assert_eq!(tools[index]["function"]["name"], "read");
        assert_eq!(tools[index]["function"]["arguments"], raw);
        assert_eq!(streamed[index]["index"], index);
        assert_eq!(streamed[index]["id"], id);
        assert_eq!(streamed[index]["function"]["arguments"], raw);
    }
    assert_eq!(ledger[0]["settled"], "usage");
}

#[tokio::test]
async fn openrouter_object_tool_arguments_are_encoded_before_execution() {
    for indexed in [false, true] {
        let arguments = json!({"path":"note.txt","metadata":{"text":"benign λ"}});
        let mut call = json!({"id":"object-call","function":{"name":"read","arguments":arguments}});
        if indexed {
            call["index"] = json!(7);
        }
        let (completed, output, ledger) = normalized_tool_reply(json!([call])).await;
        let tool = &completed.assistant["tool_calls"][0];
        assert_eq!(tool["id"], "object-call");
        assert_eq!(tool["function"]["name"], "read");
        let raw = tool["function"]["arguments"].as_str().unwrap();
        assert_eq!(serde_json::from_str::<Value>(raw).unwrap(), arguments);
        let streamed = output
            .iter()
            .find_map(|v| v.pointer("/choices/0/delta/tool_calls/0"))
            .unwrap();
        assert_eq!(streamed["index"], 0);
        assert_eq!(streamed["id"], "object-call");
        assert_eq!(streamed["function"]["arguments"], raw);
        assert_eq!(ledger[0]["settled"], "usage");
        assert_eq!(ledger[0]["reported_cost_usd"], 0.25);
    }
}

#[tokio::test]
async fn openrouter_normalized_tool_objects_still_refuse_held_key_atomically() {
    for arguments in [
        json!({"metadata":[{"value":CREDENTIAL}]}),
        json!({"metadata":[{CREDENTIAL:"value"}]}),
    ] {
        let mock = MockAnthropic::start(vec![wire(
            vec![
                json!({"choices":[{"index":0,"delta":{"tool_calls":[
                {"id":"clean-first","function":{"name":"read","arguments":{"path":"note.txt"}}},
                {"id":"unsafe-second","function":{"name":"read","arguments":arguments}},
            ]},"finish_reason":null}]}),
                json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],
                "usage":{"prompt_tokens":21,"completion_tokens":5,"cost":0.25}}),
            ],
            true,
        )]);
        let (gateway, fixture) = gateway(&mock);
        let mut output = Vec::new();
        let error = gateway
            .chat_payload(
                &payload(true),
                Context::default(),
                &Cancellation::default(),
                &mut |event| {
                    match event {
                        Event::Chunk(value) => output.push(value),
                        Event::Done => panic!("unsafe normalized tools cannot complete"),
                    }
                    Control::Continue
                },
            )
            .await
            .err()
            .unwrap();
        assert_eq!(error.refusal().code, "cloud_upstream_stream_broken");
        assert!(output
            .iter()
            .all(|v| v.pointer("/choices/0/delta/tool_calls").is_none()));
        assert!(!json!(output).to_string().contains(CREDENTIAL));
        assert_eq!(rows(&fixture)[0]["settled"], "reserved");
        mock.finish();
    }
}
