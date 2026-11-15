//! Shared native-wire fixture checks. Runtime E2E cases bind the real gateway entrypoint.
#![cfg(all(feature = "policy", feature = "transport"))]
#[path = "common/anthropic.rs"]
mod anthropic;
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
