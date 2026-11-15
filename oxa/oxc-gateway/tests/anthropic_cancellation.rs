#![cfg(feature = "anthropic-gateway")]
#[allow(dead_code)]
#[path = "common/anthropic.rs"]
mod anthropic;
#[path = "common/skeleton.rs"]
mod skeleton;

use anthropic::{MockAnthropic, Reply};
use oxc_gateway::{
    anthropic_client::{Cancellation, Control},
    gateway::Event,
    ledger::Context,
    runtime::{Options, Runtime},
    Gateway,
};
use serde_json::{json, Value};
use std::fs;

async fn cancel_on_finish(include_usage: bool) {
    let mock = MockAnthropic::start(vec![Reply::text("Synthetic answer", 11, 7, 3, 5)]);
    let fixture = skeleton::Fixture::new(&mock);
    let runtime = Runtime::new(
        fixture.config.clone(),
        fixture.key_path.clone(),
        fixture.ledger_dir.clone(),
        Options::default(),
    )
    .unwrap();
    let gateway = Gateway::new(runtime).unwrap();
    let payload = json!({"model":skeleton::MODEL,"messages":[{
        "role":"user","content":"Synthetic request"}],"max_tokens":16,
        "stream":true,"stream_options":{"include_usage":include_usage}});
    let cancel = Cancellation::default();
    let mut done = false;
    let error = gateway
        .chat(&payload, Context::default(), &cancel, &mut |event| {
            match event {
                Event::Chunk(chunk) if chunk["choices"][0]["finish_reason"].is_string() => {
                    cancel.cancel();
                    return Control::Stop;
                }
                Event::Done => done = true,
                _ => {}
            }
            Control::Continue
        })
        .await
        .err()
        .expect("cancellation cannot succeed");
    assert_eq!(error.refusal().status, 499);
    assert!(!done);
    let rows: Vec<Value> = fs::read_dir(&fixture.ledger_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "jsonl"))
        .flat_map(|path| {
            fs::read_to_string(path)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str::<Value>(line).unwrap())
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row["status"], 499);
    assert_eq!(
        row["settled"], "usage",
        "terminal usage was received before the callback stopped"
    );
    assert_eq!(row["prompt_tokens"], 21);
    assert_eq!(row["completion_tokens"], 5);
    assert_eq!(row["cached_tokens"], 7);
    assert_eq!(row["cache_write_tokens"], 3);
    assert_eq!(row["cost_usd"], json!(0.0000459));
    assert_eq!(mock.requests().len(), 1);
}

#[tokio::test]
async fn cancelling_the_finish_chunk_preserves_private_usage_when_public_usage_is_off() {
    cancel_on_finish(false).await;
}

#[tokio::test]
async fn cancelling_before_the_public_usage_chunk_still_settles_trusted_counts() {
    cancel_on_finish(true).await;
}
