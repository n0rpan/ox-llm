//! Shared native-wire fixture checks. Runtime E2E cases bind the real gateway entrypoint.
#![cfg(all(feature = "policy", feature = "transport"))]
#[path = "common/anthropic.rs"]
mod anthropic;
#[cfg(feature = "secrets")]
#[path = "common/skeleton.rs"]
mod skeleton;
use anthropic::{MockAnthropic, Reply, CREDENTIAL};
use serde_json::{json, Value};
use std::time::Duration;

#[tokio::test]
async fn native_upstream_captures_credentials_and_streams_split_utf8_and_usage() {
    let mock = MockAnthropic::start(vec![Reply::text("Synthetic λ reply", 11, 7, 3, 5)]);
    assert!(mock.address().ip().is_loopback());
    assert!(mock.requests().is_empty());
    let body = json!({"model":"synthetic-anthropic","max_tokens":16,"stream":true,"messages":[{"role":"user","content":"Synthetic request"}]});
    let response = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
        .post(format!("{}/messages", mock.base_url()))
        .header("x-api-key", CREDENTIAL)
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    let text = response.text().await.unwrap();
    let events: Vec<Value> = text
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|value| serde_json::from_str(value).unwrap())
        .collect();
    assert_eq!(
        events[0]["message"]["usage"],
        json!({"input_tokens":11,"cache_read_input_tokens":7,"cache_creation_input_tokens":3,"output_tokens":0})
    );
    assert_eq!(events[2]["delta"]["text"], "Synthetic λ reply");
    assert_eq!(events[4]["usage"]["output_tokens"], 5);
    assert_eq!(events.last().unwrap()["type"], "message_stop");
    let requests = mock.wait_for_requests(1);
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].target, "/v1/messages");
    assert_eq!(requests[0].header("X-Api-Key"), Some(CREDENTIAL));
    assert_eq!(requests[0].header("anthropic-version"), Some("2023-06-01"));
    assert_eq!(requests[0].body, body);
    assert!(!format!("{:?}", requests[0]).contains(CREDENTIAL));
    mock.assert_healthy();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gated_request_does_not_block_another_upstream_connection() {
    let (reply, release) = Reply::text("Synthetic reply", 11, 7, 3, 5).gated();
    let mock = MockAnthropic::start(vec![reply, Reply::error(503, "Synthetic overload")]);
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let first_url = format!("{}/messages", mock.base_url());
    let first = tokio::spawn({
        let client = client.clone();
        async move {
            client
                .post(first_url)
                .json(&json!({"stream":true}))
                .send()
                .await
                .unwrap()
        }
    });
    tokio::task::block_in_place(|| mock.wait_for_requests(1));
    let second = client
        .post(format!("{}/messages", mock.base_url()))
        .json(&json!({"stream":true}))
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), 503);
    assert_eq!(
        second.json::<Value>().await.unwrap()["error"]["message"],
        "Synthetic overload"
    );
    assert_eq!(mock.requests().len(), 2);
    release.send(()).unwrap();
    assert!(first
        .await
        .unwrap()
        .text()
        .await
        .unwrap()
        .contains("message_stop"));
    mock.assert_healthy();
}

#[cfg(feature = "secrets")]
#[test]
fn runtime_fixture_uses_a_real_owner_only_key_file_outside_the_repository() {
    use std::os::unix::fs::PermissionsExt;
    let mock = MockAnthropic::start(vec![]);
    let fixture = skeleton::Fixture::new(&mock);
    let key = oxc_gateway::secrets::read(&fixture.key_path, "anthropic").unwrap();
    assert_eq!(key.expose(), CREDENTIAL);
    assert!(!format!("{key:?}").contains(CREDENTIAL));
    assert_eq!(
        std::fs::metadata(&fixture.key_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(&fixture.ledger_dir)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    let model = &fixture.config["cloud"]["models"][skeleton::MODEL];
    assert_eq!(model["upstream_model"], skeleton::UPSTREAM);
    assert_eq!(model["base_url"], mock.base_url());
    assert!(mock.requests().is_empty());
    mock.assert_healthy();
}

#[cfg(feature = "runtime")]
#[test]
fn real_runtime_discovers_the_fixture_without_provider_or_ledger_activity() {
    let mock = anthropic::MockAnthropic::start(vec![]);
    let fixture = skeleton::Fixture::new(&mock);
    let runtime = oxc_gateway::runtime::Runtime::new(
        fixture.config.clone(),
        fixture.key_path.clone(),
        fixture.ledger_dir.clone(),
        oxc_gateway::runtime::Options::default(),
    )
    .unwrap();
    std::fs::remove_file(&fixture.key_path).unwrap();
    let discovery = runtime.discovery();
    assert_eq!(discovery.models["data"].as_array().unwrap().len(), 1);
    assert_eq!(discovery.models["data"][0]["id"], skeleton::MODEL);
    assert_eq!(discovery.status["loaded"], false);
    assert_eq!(discovery.status["sleeping"], true);
    assert!(!discovery.models.to_string().contains(anthropic::CREDENTIAL));
    assert!(!discovery
        .models
        .to_string()
        .contains(fixture.key_path.to_str().unwrap()));
    assert!(mock.requests().is_empty());
    assert!(std::fs::read_dir(&fixture.ledger_dir)
        .unwrap()
        .next()
        .is_none());
    mock.assert_healthy();
}

#[cfg(feature = "anthropic-gateway")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_native_request_reaches_settlement_before_done() {
    use oxc_gateway::{
        anthropic_client::{Cancellation, Control},
        gateway::{Event, Gateway},
        ledger::Context,
        runtime::{Options, Runtime},
    };
    let prompt = "Synthetic user prompt";
    let upstream_text = format!("Synthetic λ {CREDENTIAL} reply");
    let mock = MockAnthropic::start(vec![Reply::text(&upstream_text, 11, 7, 3, 5)]);
    let fixture = skeleton::Fixture::new(&mock);
    let runtime = Runtime::new(
        fixture.config.clone(),
        fixture.key_path.clone(),
        fixture.ledger_dir.clone(),
        Options::default(),
    )
    .unwrap();
    let gateway = Gateway::new(runtime).unwrap();
    let discovery = gateway.discovery();
    assert_eq!(discovery.models["data"][0]["id"], skeleton::MODEL);
    assert!(mock.requests().is_empty());
    let payload = json!({"model":skeleton::MODEL,"max_tokens":1000,"stream":true,"stream_options":{"include_usage":true},"messages":[{"role":"user","content":prompt}]});
    let mut chunks = vec![];
    let mut done = 0;
    tokio::time::timeout(
        Duration::from_secs(10),
        gateway.chat(
            &payload,
            Context {
                origin: Some("oxa"),
                session: Some("synthetic-session"),
                idempotency_key: Some("synthetic-turn"),
            },
            &Cancellation::default(),
            &mut |event| {
                match event {
                    Event::Chunk(value) => chunks.push(value),
                    Event::Done => {
                        assert_eq!(
                            ledger_rows(&fixture.ledger_dir).len(),
                            1,
                            "Done preceded durable settlement"
                        );
                        done += 1;
                    }
                }
                Control::Continue
            },
        ),
    )
    .await
    .expect("gateway request finished within the test deadline")
    .unwrap();
    assert_eq!(done, 1);
    let requests = mock.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].target, "/v1/messages");
    assert_eq!(requests[0].header("x-api-key"), Some(CREDENTIAL));
    assert_eq!(requests[0].header("anthropic-version"), Some("2023-06-01"));
    assert_eq!(requests[0].body["model"], skeleton::UPSTREAM);
    assert_eq!(requests[0].body["max_tokens"], 16);
    assert_eq!(requests[0].body["stream"], true);
    assert_eq!(requests[0].body["messages"][0]["role"], "user");
    assert_eq!(
        requests[0].body["messages"][0]["content"][0]["text"],
        prompt
    );
    let text: String = chunks
        .iter()
        .filter_map(|value| value["choices"][0]["delta"]["content"].as_str())
        .collect();
    assert!(text.starts_with("Synthetic λ ") && text.ends_with(" reply"));
    assert!(!text.contains(CREDENTIAL));
    let usage = chunks
        .iter()
        .find_map(|value| value.get("usage").filter(|value| value.is_object()))
        .unwrap();
    assert_eq!(usage["prompt_tokens"], 21);
    assert_eq!(usage["completion_tokens"], 5);
    assert_eq!(usage["prompt_tokens_details"]["cached_tokens"], 7);
    assert_eq!(usage["prompt_tokens_details"]["cache_write_tokens"], 3);
    let rows = ledger_rows(&fixture.ledger_dir);
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row["origin"], "organic");
    assert_eq!(row["model"], skeleton::MODEL);
    assert_eq!(row["max_tokens"], 1000);
    assert_eq!(row["remote"], true);
    assert_eq!(row["remote_provider"], "anthropic");
    assert_eq!(row["remote_model"], skeleton::UPSTREAM);
    assert_eq!(row["prompt_tokens"], 21);
    assert_eq!(row["completion_tokens"], 5);
    assert_eq!(row["cached_tokens"], 7);
    assert_eq!(row["cache_write_tokens"], 3);
    assert_eq!(row["settled"], "usage");
    assert!((row["cost_usd"].as_f64().unwrap() - 0.0000459).abs() < 1e-12);
    let stored = row.to_string();
    for private in [
        CREDENTIAL,
        prompt,
        upstream_text.as_str(),
        "synthetic-session",
        "synthetic-turn",
        fixture.key_path.to_str().unwrap(),
    ] {
        assert!(
            !stored.contains(private),
            "ledger retained private request material"
        );
    }
    mock.assert_healthy();
}

#[cfg(feature = "anthropic-gateway")]
fn ledger_rows(directory: &std::path::Path) -> Vec<Value> {
    use std::os::unix::fs::PermissionsExt;
    let mut rows = vec![];
    for entry in std::fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path
            .extension()
            .is_some_and(|extension| extension == "jsonl")
        {
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            for line in std::fs::read_to_string(path).unwrap().lines() {
                rows.push(serde_json::from_str(line).unwrap());
            }
        }
    }
    rows
}

#[cfg(feature = "anthropic-gateway")]
#[tokio::test]
async fn live_request_keeps_its_reservation_until_the_paid_row_is_written() {
    use oxc_gateway::{
        anthropic_client::{Cancellation, Control},
        gateway::Gateway,
        ledger::Context,
        runtime::{Options, Runtime},
    };
    let (first_reply, release) = Reply::text("Synthetic reply", 11, 7, 3, 5).gated();
    let mock = MockAnthropic::start(vec![
        first_reply,
        Reply::text("Synthetic reply", 11, 7, 3, 5),
    ]);
    let mut fixture = skeleton::Fixture::new(&mock);
    // One full-window hold fits; two simultaneous holds do not. Two actual
    // settled responses fit, so the last request also proves release.
    fixture.config["cloud"]["daily_budget_usd"] = json!(0.003);
    let runtime = Runtime::new(
        fixture.config.clone(),
        fixture.key_path.clone(),
        fixture.ledger_dir.clone(),
        Options::default(),
    )
    .unwrap();
    let gateway = Gateway::new(runtime).unwrap();
    let payload = json!({"model":skeleton::MODEL,"max_tokens":1000,"stream":true,"stream_options":{"include_usage":false},"messages":[{"role":"user","content":"Synthetic request"}]});
    let first_cancel = Cancellation::default();
    let mut first_on = |_| Control::Continue;
    let first = gateway.chat(&payload, Context::default(), &first_cancel, &mut first_on);
    let probe = async {
        tokio::time::timeout(Duration::from_secs(4), async {
            while mock.requests().is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("first request reached the owned upstream");
        let cancel = Cancellation::default();
        let refusal = gateway
            .chat(&payload, Context::default(), &cancel, &mut |_| {
                Control::Continue
            })
            .await
            .err()
            .expect("a live hold must refuse another request");
        assert_eq!(refusal.refusal().code, "budget_exceeded");
        assert_eq!(mock.requests().len(), 1);
        release.send(()).unwrap();
    };
    let (first, ()) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(first, probe)
    })
    .await
    .expect("admission probe and first request finished");
    first.unwrap();
    let first_rows = ledger_rows(&fixture.ledger_dir);
    assert_eq!(first_rows.iter().filter(|r| r["status"] == 200).count(), 1);
    let refused = first_rows.iter().find(|r| r["status"] == 429).unwrap();
    assert_eq!(refused["cost_usd"].as_f64(), Some(0.0));
    let cancel = Cancellation::default();
    tokio::time::timeout(
        Duration::from_secs(10),
        gateway.chat(&payload, Context::default(), &cancel, &mut |_| {
            Control::Continue
        }),
    )
    .await
    .expect("a settled hold was released")
    .unwrap();
    assert_eq!(mock.requests().len(), 2);
    let rows = ledger_rows(&fixture.ledger_dir);
    assert_eq!(rows.len(), 3);
    let paid: Vec<_> = rows.iter().filter(|row| row["status"] == 200).collect();
    assert_eq!(paid.len(), 2);
    for row in paid {
        assert_eq!(row["settled"], "usage");
        assert_eq!(row["prompt_tokens"], 21);
        assert!((row["cost_usd"].as_f64().unwrap() - 0.0000459).abs() < 1e-12);
    }
    mock.assert_healthy();
}
