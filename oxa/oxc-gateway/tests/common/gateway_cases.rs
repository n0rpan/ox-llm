use super::*;
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
