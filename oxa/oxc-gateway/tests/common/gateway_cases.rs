use super::*;
#[allow(dead_code)]
#[path = "anthropic.rs"]
pub(super) mod anthropic;
#[path = "skeleton.rs"]
pub(super) mod skeleton;
use crate::{
    catalog::{Access, Model, Provider},
    pricing::Price,
    runtime::Options,
};
use anthropic::{MockAnthropic, Reply};

fn gateway(fixture: &skeleton::Fixture) -> Gateway {
    gateway_with_cap(fixture, 1.0)
}
fn gateway_with_cap(fixture: &skeleton::Fixture, cap: f64) -> Gateway {
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
    let runtime = Runtime::new(
        [(
            model,
            Price::from_model(&fixture.config["cloud"]["models"][skeleton::MODEL])
                .map(crate::runtime::Billing::Metered),
        )],
        fixture.key_path.clone(),
        fixture.ledger_dir.clone(),
        Options {
            access: Access::Enabled,
            daily_budget_usd: Some(cap),
            ledger_enabled: true,
            subscription: None,
            subscription_limits: None,
        },
    )
    .unwrap();
    Gateway::new(runtime).unwrap()
}
fn payload(include_usage: bool) -> Value {
    json!({"model":skeleton::MODEL,"max_tokens":1000,
        "messages":[{"role":"user","content":"Synthetic question"}],
        "stream_options":{"include_usage":include_usage}})
}
fn rows(fixture: &skeleton::Fixture) -> Vec<Value> {
    std::fs::read_dir(&fixture.ledger_dir)
        .unwrap()
        .filter_map(|file| {
            let path = file.unwrap().path();
            path.extension()
                .is_some_and(|ext| ext == "jsonl")
                .then(|| std::fs::read_to_string(path).unwrap())
        })
        .flat_map(|text| {
            text.lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect::<Vec<Value>>()
        })
        .collect()
}
fn metered(row: &Value) {
    assert_eq!(
        (row["prompt_tokens"].as_u64(), row["completion_tokens"].as_u64()),
        (Some(21), Some(5))
    );
    assert_eq!(row["cached_tokens"], 7);
    assert_eq!(row["cache_write_tokens"], 3);
    assert_eq!(row["settled"], "usage");
    assert!((row["cost_usd"].as_f64().unwrap() - 0.0000459).abs() < 1e-12);
}
#[tokio::test]
async fn http_clean_refusals_do_not_spend_or_retain_the_reservation() {
    for status in [400, 401, 403, 404, 413, 429] {
        let mock = MockAnthropic::start(vec![
            Reply::error(status, "Synthetic refusal"),
            Reply::error(status, "Synthetic refusal"),
        ]);
        let fixture = skeleton::Fixture::new(&mock);
        // One reservation fits, but two charges at the reservation would not.
        let gateway = gateway_with_cap(&fixture, 0.003);
        for attempt in 1..=2 {
            let error = gateway
                .chat_payload(
                    &payload(true),
                    Context::default(),
                    &Cancellation::default(),
                    &mut |_| panic!("an HTTP refusal cannot emit reply data or Done"),
                )
                .await
                .err()
                .unwrap()
                .refusal();
            assert_eq!((error.status, error.code), (status, "cloud_upstream_error"));
            let rows = rows(&fixture);
            assert_eq!(rows.len(), attempt);
            assert_eq!(rows[attempt - 1]["status"], status);
            assert_eq!(rows[attempt - 1]["error"], "cloud_upstream_error");
            assert_eq!(rows[attempt - 1]["cost_usd"], 0.0);
            assert_eq!(rows[attempt - 1]["settled"], "refused");
            assert_eq!(mock.requests().len(), attempt);
        }
    }
}
#[tokio::test]
async fn http_ambiguous_failures_keep_the_conservative_charge() {
    for status in [408, 500, 503, 529] {
        let mock = MockAnthropic::start(vec![Reply::error(status, "Synthetic failure")]);
        let fixture = skeleton::Fixture::new(&mock);
        let gateway = gateway_with_cap(&fixture, 0.003);
        let error = gateway
            .chat_payload(
                &payload(true),
                Context::default(),
                &Cancellation::default(),
                &mut |_| panic!("an HTTP failure cannot emit reply data or Done"),
            )
            .await
            .err()
            .unwrap()
            .refusal();
        assert_eq!((error.status, error.code), (status, "cloud_upstream_error"));
        let rows = rows(&fixture);
        assert_eq!(rows.len(), 1);
        assert!(rows[0]["cost_usd"].as_f64().unwrap() > 0.002);
        assert_eq!(rows[0]["settled"], "reserved");
        let error = gateway
            .chat_payload(
                &payload(true),
                Context::default(),
                &Cancellation::default(),
                &mut |_| panic!("the spent budget cannot admit another call"),
            )
            .await
            .err()
            .unwrap()
            .refusal();
        assert_eq!((error.status, error.code), (429, "budget_exceeded"));
        assert_eq!(mock.requests().len(), 1);
    }
}
#[tokio::test]
async fn an_invalid_typed_prefix_records_a_refusal_without_reading_a_key() {
    let mock = MockAnthropic::start(vec![]);
    let fixture = skeleton::Fixture::new(&mock);
    std::fs::remove_file(&fixture.key_path).unwrap();
    let prefix = oxa_proto::FrozenPrefix {
        sections: vec![],
        tools_json: "invalid tools".into(),
        sha256: [0; 32],
    };
    let request = oxa_proto::ChatRequest::new(&prefix, &[], skeleton::MODEL);
    let error = gateway(&fixture)
        .chat(
            &request,
            Context::default(),
            &Cancellation::default(),
            &mut |_| panic!("invalid prefix cannot be forwarded"),
        )
        .await
        .err()
        .unwrap()
        .refusal();
    assert_eq!(
        (error.status, error.code, error.param),
        (400, "invalid_value", "tools")
    );
    let rows = rows(&fixture);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["status"], 400);
    assert_eq!(rows[0]["cost_usd"], 0.0);
    assert!(mock.requests().is_empty());
}
#[tokio::test]
async fn final_callback_cancellation_and_usage_opt_out_keep_actual_cache_metering() {
    for include_usage in [false, true] {
        let mock = MockAnthropic::start(vec![Reply::text("Synthetic", 11, 7, 3, 5)]);
        let fixture = skeleton::Fixture::new(&mock);
        let mut done = false;
        let result = gateway(&fixture)
            .chat_payload(
                &payload(include_usage),
                Context::default(),
                &Cancellation::default(),
                &mut |event| match event {
                    Event::Done => {
                        done = true;
                        Control::Continue
                    }
                    Event::Chunk(chunk) if !chunk["choices"][0]["finish_reason"].is_null() => Control::Stop,
                    _ => Control::Continue,
                },
            )
            .await;
        assert!(matches!(
            result,
            Err(Error::Upstream(anthropic_client::Error::Cancelled))
        ));
        assert!(!done);
        let rows = rows(&fixture);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["status"], 499);
        metered(&rows[0]);
    }
}
#[tokio::test]
async fn done_observes_one_committed_row_and_late_cancel_keeps_success() {
    let mock = MockAnthropic::start(vec![Reply::text("Synthetic", 11, 7, 3, 5)]);
    let fixture = skeleton::Fixture::new(&mock);
    let cancel = Cancellation::default();
    let mut done = 0;
    let result = gateway(&fixture)
        .chat_payload(&payload(false), Context::default(), &cancel, &mut |event| {
            if matches!(event, Event::Done) {
                let rows = rows(&fixture);
                assert_eq!(rows.len(), 1);
                metered(&rows[0]);
                assert_eq!(rows[0]["status"], 200);
                done += 1;
                cancel.cancel();
                return Control::Stop;
            }
            Control::Continue
        })
        .await
        .unwrap();
    assert_eq!(done, 1);
    assert_eq!(result.assistant["content"], "Synthetic");
    assert_eq!(mock.requests()[0].body["max_tokens"], 16);
}
#[tokio::test]
async fn a_failed_paid_append_emits_no_done_and_blocks_later_requests() {
    let mock = MockAnthropic::start(vec![Reply::text("Synthetic", 11, 7, 3, 5)]);
    let fixture = skeleton::Fixture::new(&mock);
    let gateway = gateway(&fixture);
    let path = fixture.ledger_dir.join(format!(
        "requests-{}.jsonl",
        chrono::Local::now().format("%Y%m%d")
    ));
    let mut blocked = false;
    let mut done = false;
    let error = gateway
        .chat_payload(
            &payload(true),
            Context::default(),
            &Cancellation::default(),
            &mut |event| {
                if matches!(event, Event::Done) {
                    done = true;
                }
                if !blocked {
                    std::fs::create_dir(&path).unwrap();
                    blocked = true;
                }
                Control::Continue
            },
        )
        .await
        .err()
        .unwrap()
        .refusal();
    assert_eq!((error.status, error.code), (503, "cloud_ledger_unavailable"));
    assert!(!done);
    std::fs::remove_dir(path).unwrap();
    let error = gateway
        .chat_payload(
            &payload(true),
            Context::default(),
            &Cancellation::default(),
            &mut |_| panic!("failed ledger cannot admit another request"),
        )
        .await
        .err()
        .unwrap()
        .refusal();
    assert_eq!((error.status, error.code), (503, "cloud_ledger_unavailable"));
    assert_eq!(mock.requests().len(), 1);
}

#[test]
fn go_captured_terminal_usage_settles_even_without_public_delivery() {
    use crate::{billing::SubscriptionLimits, opencode, runtime::Billing};
    for cancelled in [true, false] {
        for known in [true, false] {
            let fixture = skeleton::Fixture::for_endpoint("https://example.invalid");
            std::fs::write(
                &fixture.key_path,
                json!({"opencode":"synthetic-go-credential"}).to_string(),
            )
            .unwrap();
            let route = Model::new(
                "synthetic-go",
                Provider::OpenCode,
                "synthetic-go",
                "https://example.invalid",
                1024,
                100,
            )
            .unwrap();
            let runtime = Runtime::new(
                [(route, Some(Billing::Subscription))],
                fixture.key_path.clone(),
                fixture.ledger_dir.clone(),
                Options {
                    access: Access::Enabled,
                    daily_budget_usd: None,
                    ledger_enabled: true,
                    subscription: None,
                    subscription_limits: Some(SubscriptionLimits::new(10, 1500).unwrap()),
                },
            )
            .unwrap();
            let gateway = Gateway::new(runtime).unwrap();
            let payload = json!({"model":"synthetic-go","max_tokens":100,"messages":[]});
            let mut call = gateway.runtime.begin(&payload, Context::default()).unwrap();
            call.mark_forwarded().unwrap();
            // Transport already parsed this terminal record, but cancellation
            // (or protocol rejection) prevented its public Chunk callback.
            let usage = known.then(|| opencode::Usage::parse(&json!({
                    "prompt_tokens":100,"completion_tokens":5,"cost":0,
                    "prompt_tokens_details":{"cached_tokens":80,"cache_write_tokens":0},
                "opencode":{"billing_source":"go","allowance":0.0008,"list_cost":0.001,
                "tokens":{"input":20,"output":5,"reasoning":1,"cache_read":80,"cache_write_5m":0,"cache_write_1h":0}}
            })).unwrap());
            let error = if cancelled {
                opencode::client::Error::Cancelled
            } else {
                opencode::client::Error::Protocol
            };
            assert!(gateway
                .finish_go(call, Err(error), usage, &mut |_| panic!(
                    "failed reply must emit no Chunk or Done"
                ))
                .is_err());
            let saved = rows(&fixture);
            assert_eq!(saved.len(), 1);
            assert_eq!(saved[0]["subscription_requests"], 1);
            assert_eq!(saved[0]["subscription_tokens"], if known { 105 } else { 1124 });
            assert_eq!(saved[0]["settled"], if known { "usage" } else { "reserved" });
            if known {
                assert_eq!(saved[0]["cost_usd"], 0.0);
                assert_eq!(saved[0]["opencode"]["allowance"], 0.0008);
                assert_eq!(saved[0]["opencode"]["tokens"]["cache_read"], 80);
                assert_eq!(saved[0]["cached_tokens"], 80);
                assert_eq!(saved[0]["cache_write_tokens"], 0);
                let next = gateway
                    .runtime
                    .begin(&payload, Context::default())
                    .expect("actual usage leaves room for next hold");
                next.finish(Outcome::Complete, 200, None).unwrap();
            } else {
                assert!(saved[0].get("cost_usd").is_none());
                assert_eq!(
                    gateway
                        .runtime
                        .begin(&payload, Context::default())
                        .err()
                        .unwrap()
                        .code,
                    "subscription_limit_exceeded"
                );
            }
        }
    }
}

use crate::mock_openai as openai;

#[test]
fn go_stream_usage_must_follow_a_validated_terminal_transition() {
    use crate::billing::SubscriptionLimits;
    let usage = |prompt, completion| {
        json!({"prompt_tokens":prompt,"completion_tokens":completion,"cost":0,
        "opencode":{"billing_source":"go","allowance":0.0008,"list_cost":0.001,
        "tokens":{"input":prompt,"output":completion,"reasoning":0,"cache_read":0,"cache_write_5m":0,"cache_write_1h":0}}})
    };
    let finish = json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]});
    let final_usage = json!({"choices":[],"usage":usage(100,5)});
    let early_usage = json!({"choices":[],"usage":usage(0,0)});
    let content = json!({"choices":[{"index":0,"delta":{"content":"synthetic paid content ".repeat(16)},"finish_reason":null}]});
    for (name, frames, known) in [
        (
            "completed length with incomplete arguments",
            vec![
                json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"synthetic-call","function":{"name":"bash","arguments":r#"{"command":"synthetic"#}}]},"finish_reason":null}]}),
                json!({"choices":[{"index":0,"delta":{},"finish_reason":"length"}]}),
                final_usage.clone(),
            ],
            true,
        ),
        (
            "length incomplete arguments then broken stream",
            vec![
                json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"synthetic-call","function":{"name":"bash","arguments":r#"{"command":"synthetic"#}}]},"finish_reason":null}]}),
                json!({"choices":[{"index":0,"delta":{},"finish_reason":"length"}]}),
                final_usage.clone(),
            ],
            true,
        ),
        (
            "early zero usage then generation",
            vec![early_usage.clone(), content],
            false,
        ),
        (
            "usage with nonterminal choice",
            vec![
                json!({"choices":[{"index":0,"delta":{"content":"synthetic"},"finish_reason":null}],"usage":usage(0,0)}),
            ],
            false,
        ),
        (
            "invalid final choice",
            vec![json!({"choices":[{"index":7,"delta":{},"finish_reason":"stop"}],"usage":usage(0,0)})],
            false,
        ),
        (
            "conflicting final usage",
            vec![finish.clone(), final_usage.clone(), early_usage.clone()],
            false,
        ),
        (
            "early usage then finish without DONE",
            vec![early_usage.clone(), finish.clone()],
            false,
        ),
        (
            "cancel a combined terminal frame",
            vec![json!({
                "choices":[{"index":0,"delta":{"content":"synthetic visible reply"},"finish_reason":"stop"}],
                "usage":usage(100,5)
            })],
            true,
        ),
        (
            "ordered terminal usage then broken stream",
            vec![finish, final_usage],
            true,
        ),
        (
            "combined terminal usage then broken stream",
            vec![json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":usage(100,5)})],
            true,
        ),
    ] {
        let mock = openai::MockOpenAi::start_with(|base| {
            let catalog = json!({"data":[{"id":"synthetic-go","canonical_id":"synthetic-go","limit":{"context":1024,"output":100},"api":{"chat-completions":{"base_url":format!("{base}/inference/go/openai/v1")}}}]});
            let mut bytes = Vec::new();
            for frame in frames {
                bytes.extend_from_slice(format!("data: {frame}\n\n").as_bytes());
            }
            if !known && !name.ends_with("without DONE") {
                bytes.extend_from_slice(b"data: {malformed\n\n");
            }
            if name.starts_with("completed length") {
                bytes.extend_from_slice(b"data: [DONE]\n\n");
            }
            // Other known controls end after valid final usage but before [DONE].
            vec![
                openai::Reply::json(catalog),
                openai::Reply::raw(200, "text/event-stream", bytes, vec![]),
            ]
        });
        let base = format!("http://{}/inference/go", mock.address());
        let fixture = skeleton::Fixture::for_endpoint(&base);
        std::fs::write(
            &fixture.key_path,
            json!({"opencode":"synthetic-go-credential"}).to_string(),
        )
        .unwrap();
        let runtime = Runtime::new(
            [],
            fixture.key_path.clone(),
            fixture.ledger_dir.clone(),
            Options {
                access: Access::Enabled,
                daily_budget_usd: None,
                ledger_enabled: true,
                subscription: None,
                subscription_limits: Some(SubscriptionLimits::new(10, 1500).unwrap()),
            },
        )
        .unwrap();
        let mut gateway = Gateway::new(runtime).unwrap();
        gateway
            .discover_opencode(&base, std::time::Duration::from_secs(2))
            .unwrap();
        let payload = json!({"model":"opencode/synthetic-go","max_tokens":100,"messages":[]});
        let context = || Context {
            session: Some("synthetic-go-session"),
            ..Context::default()
        };
        let mut done = false;
        let mut length = false;
        let mut arguments_delivered = false;
        let mut usage_delivered = 0;
        let cancel_on_chunk = name.starts_with("cancel ");
        let driver = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = driver.block_on(gateway.chat_payload(
            &payload,
            context(),
            &Cancellation::default(),
            &mut |event| {
                done |= matches!(event, Event::Done);
                if let Event::Chunk(chunk) = event {
                    usage_delivered += usize::from(chunk.get("usage").is_some());
                    if cancel_on_chunk {
                        return Control::Stop;
                    }
                    length |= chunk.pointer("/choices/0/finish_reason") == Some(&json!("length"));
                    if let Some(calls) = chunk
                        .pointer("/choices/0/delta/tool_calls")
                        .and_then(Value::as_array)
                    {
                        arguments_delivered |= calls
                            .iter()
                            .any(|call| call.pointer("/function/arguments").is_some());
                    }
                }
                Control::Continue
            },
        ));
        if name.starts_with("completed length") {
            let complete = result.expect("length finish with final usage must complete");
            assert_eq!(complete.finish_reason, "length");
            assert_eq!(complete.assistant["tool_calls"], json!([]));
            assert!(done);
        } else if cancel_on_chunk {
            assert!(matches!(
                result,
                Err(Error::Upstream(anthropic_client::Error::Cancelled))
            ));
            assert!(!done, "{name}");
        } else {
            let error = result.err().expect("broken stream must not complete");
            assert_eq!(error.refusal().code, "cloud_upstream_stream_broken", "{name}");
            assert!(!done, "{name}");
        }
        assert_eq!(usage_delivered, usize::from(done), "{name}");
        if name.contains("length") {
            assert!(length, "length transition must be delivered: {name}");
            assert!(!arguments_delivered, "incomplete arguments stay private: {name}");
        }
        let saved = rows(&fixture);
        assert_eq!(saved.len(), 1, "{name}");
        assert_eq!(
            saved[0]["subscription_tokens"],
            if known { 105 } else { 1124 },
            "{name}"
        );
        assert_eq!(
            saved[0]["settled"],
            if known { "usage" } else { "reserved" },
            "{name}"
        );
        if known {
            assert_eq!(saved[0]["opencode"]["allowance"], 0.0008, "{name}");
            let next = gateway
                .runtime
                .begin(&payload, context())
                .expect("actual terminal usage leaves room");
            next.finish(Outcome::Complete, 200, None).unwrap();
        } else {
            assert_eq!(saved[0]["usage_missing"], true, "{name}");
            assert!(
                saved[0]["opencode"].is_null(),
                "invalidated allowance must not be reported as final: {name}"
            );
            assert_eq!(
                gateway.runtime.begin(&payload, context()).err().unwrap().code,
                "subscription_limit_exceeded",
                "{name}"
            );
        }
        assert_eq!(mock.requests().len(), 2, "discovery plus one generation only");
        mock.assert_healthy();
    }
}

#[cfg(feature = "upstream-openai")]
#[tokio::test]
async fn forwarding_refusal_stops_paid_transport_after_admission() {
    for malformed in [false, true] {
        let mock = openai::MockOpenAi::start(vec![openai::Reply::json(json!({}))]);
        let fixture = skeleton::Fixture::for_endpoint(&mock.base_url());
        std::fs::write(
            &fixture.key_path,
            json!({"openai":{"api_key":anthropic::CREDENTIAL}}).to_string(),
        )
        .unwrap();
        let model = Model::new(
            skeleton::MODEL,
            Provider::OpenAi,
            "synthetic-upstream",
            &mock.base_url(),
            1024,
            16,
        )
        .unwrap();
        let price = || Price::from_model(&fixture.config["cloud"]["models"][skeleton::MODEL]).unwrap();
        let runtime = Runtime::new(
            [(model, Some(crate::runtime::Billing::Metered(price())))],
            fixture.key_path.clone(),
            fixture.ledger_dir.clone(),
            Options {
                access: Access::Enabled,
                daily_budget_usd: Some(1.0),
                ledger_enabled: true,
                subscription: None,
                subscription_limits: None,
            },
        )
        .unwrap();
        let gateway = Gateway::new(runtime).unwrap();
        let payload = payload(true);
        let call = gateway.runtime.begin(&payload, Context::default()).unwrap();
        let snapshot = std::fs::read_dir(&fixture.ledger_dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.extension().is_some_and(|ext| ext == "holds"))
            .unwrap();
        if malformed {
            std::fs::write(&snapshot, "invalid").unwrap();
        } else {
            // Another accounting instance fails settlement after this call's admission.
            let budget = crate::accounting::Budget::new(snapshot.with_extension(""));
            let model = crate::accounting::Model {
                provider: "openai".into(),
                upstream: "synthetic-upstream".into(),
                context: 1024,
                max_output: 16,
                billing: crate::billing::Billing::Metered(price()),
            };
            let mut failed = budget.reserve(&json!(1), true, &model, 16).unwrap();
            failed.mark_forwarded().unwrap();
            assert!(failed
                .finish(&Usage::default(), Outcome::Failed, |_| Err(
                    std::io::ErrorKind::StorageFull.into()
                ))
                .is_err());
        }
        let error = gateway
            .openai_chat(call, &payload, None, &Cancellation::default(), &mut |_| {
                panic!("refused forwarding must emit no Chunk or Done")
            })
            .await
            .err()
            .unwrap();
        assert_eq!(error.refusal().code, "cloud_ledger_unavailable");
        assert!(mock.requests().is_empty(), "no paid HTTP request may be sent");
        assert!(rows(&fixture).iter().all(|row| row["cost_usd"] == 0.0));
        assert_eq!(
            gateway
                .runtime
                .begin(&payload, Context::default())
                .err()
                .unwrap()
                .code,
            "cloud_ledger_unavailable",
            "the failure must remain closed"
        );
        mock.assert_healthy();
    }
}
