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
        std::fs::metadata(&fixture.key_path).unwrap().permissions().mode() & 0o777,
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
    assert!(std::fs::read_dir(&fixture.ledger_dir).unwrap().next().is_none());
    mock.assert_healthy();
}

#[cfg(feature = "anthropic-client")]
#[tokio::test]
async fn real_anthropic_client_uses_the_shared_key_and_native_wire_fixture() {
    use oxc_gateway::anthropic_client::{Cancellation, Chat, Client, Control, Event};
    let text = format!("Synthetic λ {CREDENTIAL} reply");
    let mock = MockAnthropic::start(vec![Reply::text(&text, 11, 7, 3, 5)]);
    let fixture = skeleton::Fixture::new(&mock);
    let key = oxc_gateway::secrets::read(&fixture.key_path, "anthropic").unwrap();
    let payload = json!({"model":skeleton::MODEL,"max_tokens":1000,"messages":[{"role":"user","content":"Synthetic request"}],"stream_options":{"include_usage":true}});
    let base = mock.base_url();
    let mut sending = 0;
    let mut chunks = vec![];
    let completed = Client::new()
        .unwrap()
        .stream(
            Chat {
                base_url: &base,
                upstream_model: skeleton::UPSTREAM,
                alias: skeleton::MODEL,
                max_output: Some(16),
                payload: &payload,
                created: 0,
            },
            &key,
            &Cancellation::default(),
            &mut |event| {
                match event {
                    Event::Sending => sending += 1,
                    Event::Chunk(value) => chunks.push(value),
                }
                Control::Continue
            },
        )
        .await
        .unwrap();
    assert_eq!(sending, 1);
    let requests = mock.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].target, "/v1/messages");
    assert_eq!(requests[0].header("x-api-key"), Some(CREDENTIAL));
    assert_eq!(requests[0].body["model"], skeleton::UPSTREAM);
    assert_eq!(requests[0].body["max_tokens"], 16);
    let usage = completed.usage.unwrap().openai();
    assert_eq!(usage["prompt_tokens"], 21);
    assert_eq!(usage["completion_tokens"], 5);
    assert_eq!(usage["prompt_tokens_details"]["cached_tokens"], 7);
    assert_eq!(usage["prompt_tokens_details"]["cache_write_tokens"], 3);
    let normalized: String = chunks
        .iter()
        .filter_map(|value| value["choices"][0]["delta"]["content"].as_str())
        .collect();
    assert!(normalized.starts_with("Synthetic λ ") && normalized.ends_with(" reply"));
    assert!(!normalized.contains(CREDENTIAL));
    assert!(!completed.assistant.to_string().contains(CREDENTIAL));
    mock.assert_healthy();
}
