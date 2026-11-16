#![cfg(feature = "anthropic-client")]
#[path = "common/anthropic.rs"]
mod anthropic;
#[path = "common/skeleton.rs"]
mod skeleton;
use anthropic::{MockAnthropic, Reply, CREDENTIAL};
use oxc_gateway::{
    anthropic_client::{Cancellation, Chat, Client, Control, Error, Event},
    secrets,
};
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};

fn chat<'a>(base: &'a str, payload: &'a Value) -> Chat<'a> {
    Chat {
        base_url: base,
        upstream_model: skeleton::UPSTREAM,
        alias: skeleton::MODEL,
        max_output: Some(16),
        payload,
        created: 1700000000,
    }
}
fn payload() -> Value {
    json!({"model":skeleton::MODEL,"messages":[{"role":"user","content":"Synthetic request"}],
        "max_tokens":1000,"stream":true,"stream_options":{"include_usage":true}})
}

#[tokio::test]
async fn native_client_uses_the_shared_mock_and_redacts_decoded_output() {
    let mock = MockAnthropic::start(vec![Reply::text(
        &format!("Synthetic λ {CREDENTIAL}"),
        11,
        7,
        3,
        5,
    )]);
    assert!(mock.address().ip().is_loopback());
    let fixture = skeleton::Fixture::new(&mock);
    assert!(fixture.ledger_dir.is_dir());
    assert_eq!(
        fixture.config["cloud"]["models"][skeleton::MODEL]["max_output"],
        16
    );
    let key = secrets::read(&fixture.key_path, "anthropic").unwrap();
    let base = mock.base_url();
    let mut chunks = vec![];
    let mut attempts = 0;
    let completed = Client::new()
        .unwrap()
        .stream(
            chat(&base, &payload()),
            &key,
            &Cancellation::default(),
            &mut |event| {
                match event {
                    Event::Sending => attempts += 1,
                    Event::Chunk(chunk) => chunks.push(chunk),
                }
                Control::Continue
            },
        )
        .await
        .unwrap();
    assert_eq!(attempts, 1);
    assert_eq!(completed.assistant["content"], "Synthetic λ [redacted]");
    assert!(!serde_json::to_string(&chunks).unwrap().contains(CREDENTIAL));
    let usage = completed.usage.unwrap();
    assert_eq!(
        (
            usage.input_tokens,
            usage.cache_read_input_tokens,
            usage.cache_creation_input_tokens,
            usage.output_tokens
        ),
        (11, 7, 3, 5)
    );
    assert_eq!(chunks.last().unwrap()["usage"]["prompt_tokens"], 21);
    let requests = mock.wait_for_requests(1);
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].target, "/v1/messages");
    assert_eq!(requests[0].header("x-api-key"), Some(CREDENTIAL));
    assert_eq!(requests[0].header("anthropic-version"), Some("2023-06-01"));
    assert_eq!(requests[0].body["model"], skeleton::UPSTREAM);
    assert_eq!(requests[0].body["max_tokens"], 16);
    assert_eq!(requests[0].body["stream"], true);
    mock.assert_healthy();
}

#[tokio::test]
async fn provider_error_bodies_never_become_diagnostics_or_chunks() {
    let mock = MockAnthropic::start(vec![Reply::error(503, CREDENTIAL)]);
    let fixture = skeleton::Fixture::new(&mock);
    let key = secrets::read(&fixture.key_path, "anthropic").unwrap();
    let mut chunks = 0;
    let error = Client::new()
        .unwrap()
        .stream(
            chat(&mock.base_url(), &payload()),
            &key,
            &Cancellation::default(),
            &mut |event| {
                if matches!(event, Event::Chunk(_)) {
                    chunks += 1;
                }
                Control::Continue
            },
        )
        .await
        .err()
        .unwrap();
    assert_eq!(error, Error::Http(503));
    assert!(!format!("{error:?} {error}").contains(CREDENTIAL));
    assert_eq!(chunks, 0);
    assert_eq!(mock.requests().len(), 1);
}

#[tokio::test]
async fn pre_cancelled_or_invalid_requests_never_reach_the_upstream() {
    let mock = MockAnthropic::start(vec![]);
    let fixture = skeleton::Fixture::new(&mock);
    let key = secrets::read(&fixture.key_path, "anthropic").unwrap();
    let client = Client::new().unwrap();
    let cancel = Cancellation::default();
    cancel.cancel();
    let result = client
        .stream(chat(&mock.base_url(), &payload()), &key, &cancel, &mut |_| {
            panic!("a pre-cancelled request cannot emit an event")
        })
        .await;
    assert_eq!(result.err().unwrap(), Error::Cancelled);
    let mut invalid = payload();
    invalid["max_tokens"] = json!(0);
    let result = client
        .stream(
            chat(&mock.base_url(), &invalid),
            &key,
            &Cancellation::default(),
            &mut |_| panic!("an invalid request cannot emit an event"),
        )
        .await;
    assert_eq!(result.err().unwrap(), Error::Request("max_tokens"));
    assert!(mock.requests().is_empty());
}

#[tokio::test]
async fn cancellation_interrupts_a_live_request_waiting_for_provider_headers() {
    let (reply, _gate) = Reply::text("Synthetic reply", 11, 7, 3, 5).gated();
    let mock = Arc::new(MockAnthropic::start(vec![reply]));
    let fixture = skeleton::Fixture::new(&mock);
    let key = secrets::read(&fixture.key_path, "anthropic").unwrap();
    let cancel = Cancellation::default();
    let watcher = std::thread::spawn({
        let mock = mock.clone();
        let cancel = cancel.clone();
        move || {
            mock.wait_for_requests(1);
            cancel.cancel();
        }
    });
    let client = Client::new().unwrap();
    let base = mock.base_url();
    let payload = payload();
    let mut on = |_| Control::Continue;
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        client.stream(chat(&base, &payload), &key, &cancel, &mut on),
    )
    .await
    .unwrap();
    watcher.join().unwrap();
    assert_eq!(result.err().unwrap(), Error::Cancelled);
    assert_eq!(mock.requests().len(), 1);
}

#[tokio::test]
async fn redirects_are_refused_without_contacting_the_redirect_target() {
    let target = MockAnthropic::start(vec![]);
    let mock = MockAnthropic::start(vec![Reply::redirect(&format!("{}/messages", target.base_url()))]);
    let fixture = skeleton::Fixture::new(&mock);
    let key = secrets::read(&fixture.key_path, "anthropic").unwrap();
    let mut chunks = 0;
    let error = Client::new()
        .unwrap()
        .stream(
            chat(&mock.base_url(), &payload()),
            &key,
            &Cancellation::default(),
            &mut |event| {
                if matches!(event, Event::Chunk(_)) {
                    chunks += 1;
                }
                Control::Continue
            },
        )
        .await
        .err()
        .unwrap();
    assert_eq!(error, Error::Http(307));
    assert_eq!(chunks, 0);
    assert_eq!(mock.requests().len(), 1);
    assert!(target.requests().is_empty());
}

#[tokio::test]
async fn malformed_truncated_and_missing_terminal_usage_never_complete() {
    let start = json!({"type":"message_start","message":{"usage":{"input_tokens":11,"output_tokens":0}}});
    let event = |value: Value| format!("data: {value}\n\n");
    for (mime, wire) in [
        ("application/json", "{}".to_owned()),
        (
            "text/event-stream",
            "data: {invalid synthetic JSON}\n\n".to_owned(),
        ),
        ("text/event-stream", event(start.clone())),
        (
            "text/event-stream",
            format!("{}{}", event(start), event(json!({"type":"message_stop"}))),
        ),
        (
            "text/event-stream",
            event(json!({"type":"error","error":{"message":CREDENTIAL}})),
        ),
    ] {
        let mock = MockAnthropic::start(vec![Reply::wire(mime, wire.as_bytes())]);
        let fixture = skeleton::Fixture::new(&mock);
        let key = secrets::read(&fixture.key_path, "anthropic").unwrap();
        let error = Client::new()
            .unwrap()
            .stream(
                chat(&mock.base_url(), &payload()),
                &key,
                &Cancellation::default(),
                &mut |_| Control::Continue,
            )
            .await
            .err()
            .unwrap();
        assert_eq!(error, Error::Protocol);
        assert!(!format!("{error:?} {error}").contains(CREDENTIAL));
        assert_eq!(mock.requests().len(), 1);
    }
}

#[tokio::test]
async fn usage_opt_out_preserves_trusted_private_metering() {
    let mock = MockAnthropic::start(vec![Reply::text("Synthetic", 11, 7, 3, 5)]);
    let fixture = skeleton::Fixture::new(&mock);
    let key = secrets::read(&fixture.key_path, "anthropic").unwrap();
    let mut payload = payload();
    payload["stream_options"]["include_usage"] = json!(false);
    let mut chunks = Vec::new();
    let done = Client::new()
        .unwrap()
        .stream(
            chat(&mock.base_url(), &payload),
            &key,
            &Cancellation::default(),
            &mut |event| {
                if let Event::Chunk(chunk) = event {
                    chunks.push(chunk);
                }
                Control::Continue
            },
        )
        .await
        .unwrap();
    assert!(chunks.iter().all(|chunk| chunk.get("usage").is_none()));
    let usage = done.usage.unwrap();
    assert_eq!(
        (
            usage.input_tokens,
            usage.cache_read_input_tokens,
            usage.cache_creation_input_tokens,
            usage.output_tokens
        ),
        (11, 7, 3, 5)
    );
}

#[tokio::test]
async fn callback_can_refuse_sending_and_stop_final_delivery() {
    let mock = MockAnthropic::start(vec![]);
    let fixture = skeleton::Fixture::new(&mock);
    let key = secrets::read(&fixture.key_path, "anthropic").unwrap();
    let error = Client::new()
        .unwrap()
        .stream(
            chat(&mock.base_url(), &payload()),
            &key,
            &Cancellation::default(),
            &mut |event| {
                assert!(matches!(event, Event::Sending));
                Control::Stop
            },
        )
        .await
        .err()
        .unwrap();
    assert_eq!(error, Error::Cancelled);
    assert!(mock.requests().is_empty());
    for cancel_in_callback in [false, true] {
        let mock = MockAnthropic::start(vec![Reply::text("Synthetic", 11, 7, 3, 5)]);
        let fixture = skeleton::Fixture::new(&mock);
        let key = secrets::read(&fixture.key_path, "anthropic").unwrap();
        let cancel = Cancellation::default();
        let error = Client::new()
            .unwrap()
            .stream(chat(&mock.base_url(), &payload()), &key, &cancel, &mut |event| {
                if let Event::Chunk(chunk) = event {
                    if chunk.get("usage").is_some() {
                        if cancel_in_callback {
                            cancel.cancel();
                        } else {
                            return Control::Stop;
                        }
                    }
                }
                Control::Continue
            })
            .await
            .err()
            .unwrap();
        assert_eq!(error, Error::Cancelled);
        assert_eq!(mock.requests().len(), 1);
    }
}
