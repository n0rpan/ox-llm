use super::*;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SyntheticRefusal {
    Rejected,
}

/// Exercise the seam with opaque preparation and non-Go usage/refusal types.
/// This deliberately supplies no OpenRouter policy or billing implementation.
#[test]
fn prepared_chat_reuses_transport_without_go_policy() {
    use crate::anthropic_client::Control;
    use crate::opencode::client::{stream_prepared, Error, Event, PreparedChat};
    use openai::{MockOpenAi, Reply};
    use oxa_proto::CancelToken;
    use reqwest::{
        header::{HeaderMap, HeaderValue},
        Url,
    };
    for case in ["final", "early", "refused", "invalid-endpoint"] {
        let reply = if case == "refused" {
            Reply::raw(
                418,
                "application/json",
                br#"{"error":{"synthetic":true}}"#.to_vec(),
                vec![],
            )
        } else {
            let content =
                json!({"choices":[{"index":0,"delta":{"content":"Synthetic answer"},"finish_reason":null}]});
            let finish = json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]});
            let usage = json!({"choices":[],"usage":{"units":7}});
            let frames = if case == "early" {
                vec![usage, content, finish]
            } else {
                vec![content, finish, usage]
            };
            let mut bytes = Vec::new();
            for frame in frames {
                bytes.extend_from_slice(format!("data: {frame}\r\n\r\n").as_bytes());
            }
            bytes.extend_from_slice(b"data: [DONE]\r\n\r\n");
            Reply::raw(200, "text/event-stream", bytes, vec![])
        };
        let mock = MockOpenAi::start(if case == "invalid-endpoint" {
            vec![]
        } else {
            vec![reply]
        });
        let fixture = skeleton::Fixture::for_endpoint(&mock.base_url());
        let key = crate::secrets::read(&fixture.key_path, "anthropic").unwrap();
        let transport = crate::anthropic_client::Client::new().unwrap().shared_transport();
        let payload = json!({"model":"synthetic-model","messages":[{"role":"user","content":"Synthetic task"}],"stream":true,"provider":{"synthetic":"opaque"}});
        let endpoint = if case == "invalid-endpoint" {
            "http://example.invalid/v1/chat/completions".into()
        } else {
            format!("{}/chat/completions", mock.base_url())
        };
        let mut headers = HeaderMap::new();
        headers.insert("x-synthetic-provider", HeaderValue::from_static("prepared"));
        let mut usage = None;
        let mut sent = 0;
        let mut chunks = Vec::new();
        let result = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(stream_prepared(
                &transport,
                PreparedChat {
                    endpoint: Url::parse(&endpoint).unwrap(),
                    alias: "synthetic-alias",
                    payload: &payload,
                    key: &key,
                    headers,
                    parse_usage: |value| value["units"].as_u64(),
                    refusal: |status, body| {
                        assert_eq!(status, 418);
                        assert_eq!(body["error"]["synthetic"], true);
                        SyntheticRefusal::Rejected
                    },
                },
                &CancelToken::new(),
                &mut |event| {
                    match event {
                        Event::Sending => sent += 1,
                        Event::Chunk(chunk) => chunks.push(chunk),
                    }
                    Control::Continue
                },
                &mut usage,
            ));
        match case {
            "final" => {
                assert_eq!(result.unwrap().assistant["content"], "Synthetic answer");
                assert_eq!(usage, Some(7));
                assert!(chunks.iter().any(|chunk| chunk["model"] == "synthetic-alias"));
            }
            "early" => {
                assert!(matches!(result, Err(Error::Protocol)));
                assert_eq!(usage, None);
            }
            "refused" => {
                assert!(matches!(result, Err(Error::Refused(SyntheticRefusal::Rejected))));
                assert_eq!(usage, None);
            }
            "invalid-endpoint" => {
                assert!(matches!(result, Err(Error::Endpoint)));
                assert_eq!(usage, None);
            }
            _ => unreachable!(),
        }
        assert_eq!(sent, usize::from(case != "invalid-endpoint"));
        let requests = mock.requests();
        if case != "invalid-endpoint" {
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].target, "/v1/chat/completions");
            assert_eq!(requests[0].body, payload);
            assert_eq!(requests[0].header("x-synthetic-provider"), Some("prepared"));
            assert_eq!(requests[0].header("x-opencode-session"), None);
            assert_eq!(
                requests[0].header("authorization"),
                Some(format!("Bearer {}", anthropic::CREDENTIAL).as_str())
            );
        } else {
            assert!(requests.is_empty());
        }
        mock.assert_healthy();
    }
}
#[allow(dead_code)]
#[path = "anthropic.rs"]
mod anthropic;
#[path = "skeleton.rs"]
mod skeleton;
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
            call.mark_forwarded();
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
            assert_eq!(saved[0]["cost_usd"], 0.0);
            assert_eq!(saved[0]["subscription_tokens"], if known { 105 } else { 1124 });
            assert_eq!(saved[0]["settled"], if known { "usage" } else { "reserved" });
            if known {
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

#[allow(dead_code)]
#[path = "openai.rs"]
mod openai;

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
            vec![finish.clone(), final_usage.clone(), early_usage],
            false,
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
            if !known {
                bytes.extend_from_slice(b"data: {malformed\n\n");
            }
            // Known controls end after valid final usage but before [DONE].
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
        let driver = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let error = driver
            .block_on(
                gateway.chat_payload(&payload, context(), &Cancellation::default(), &mut |event| {
                    done |= matches!(event, Event::Done);
                    Control::Continue
                }),
            )
            .err()
            .expect("broken stream must not complete");
        assert_eq!(error.refusal().code, "cloud_upstream_stream_broken", "{name}");
        assert!(!done, "{name}");
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
