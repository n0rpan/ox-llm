#![cfg(feature = "anthropic-gateway")]

#[allow(dead_code)]
#[path = "common/anthropic.rs"]
mod anthropic;
#[allow(dead_code)]
#[path = "common/skeleton.rs"]
mod skeleton;

use oxc_gateway::{
    accounting::{Outcome, Usage},
    billing::{Billing, SubscriptionLimits},
    catalog::{Access, Model, Provider},
    ledger::{Context, Ledger},
    pricing::Price,
    runtime::{Call, Options, Runtime},
};
use serde_json::{json, Value};
use std::fs;

const GO: &str = "opencode/synthetic-go";
const API: &str = "synthetic-metered";

fn fixture() -> skeleton::Fixture {
    let fixture = skeleton::Fixture::for_endpoint("https://example.invalid");
    fs::write(
        &fixture.key_path,
        json!({"opencode":"synthetic-plan-key","anthropic":"synthetic-api-key"}).to_string(),
    )
    .unwrap();
    fixture
}

fn runtime(fixture: &skeleton::Fixture, cap: f64) -> Runtime {
    let go = Model::new(
        GO,
        Provider::OpenCode,
        "synthetic-go",
        "https://example.invalid/inference/go/openai/v1",
        4096,
        64,
    )
    .unwrap();
    let api = Model::new(
        API,
        Provider::Anthropic,
        "synthetic-api",
        "https://example.invalid",
        1024,
        64,
    )
    .unwrap();
    let price = Price::from_model(&json!({
        "pricing":{"input_per_million":2000,"output_per_million":2000}
    }))
    .unwrap();
    Runtime::new(
        [
            (go, Some(Billing::Subscription)),
            (api, Some(Billing::Metered(price))),
        ],
        fixture.key_path.clone(),
        fixture.ledger_dir.clone(),
        Options {
            access: Access::Enabled,
            daily_budget_usd: Some(cap),
            ledger_enabled: true,
            subscription_limits: Some(SubscriptionLimits::new(20, 2_000_000).unwrap()),
            ..Options::default()
        },
    )
    .unwrap()
}

fn settle(mut call: Call<'_>, prompt: u64, output: u64, cost: Option<f64>) -> Value {
    call.metered_usage(Usage {
        prompt: Some(prompt),
        output: Some(output),
        reported_cost: cost,
        ..Usage::default()
    });
    call.finish(Outcome::Complete, 200, None).unwrap()
}

#[test]
fn two_go_calls_and_a_metered_call_hold_and_settle_together() {
    let fixture = fixture();
    let first = runtime(&fixture, 400.0);
    let second = runtime(&fixture, 400.0);
    let metered = runtime(&fixture, 400.0);
    let begin = |alias| json!({"model":alias,"max_tokens":32});

    let mut previous = metered.begin(&begin(API), Context::default()).unwrap();
    previous.mark_forwarded().unwrap();
    assert_eq!(settle(previous, 20, 10, None)["cost_usd"].as_f64(), Some(0.06));

    // All three calls use distinct runtimes on the same durable ledger. The
    // first Go hold is still in flight when the other two try admission.
    let mut go_one = first.begin(&begin(GO), Context::default()).unwrap();
    go_one.mark_forwarded().unwrap();
    let go_two = second.begin(&begin(GO), Context::default());
    let api = metered.begin(&begin(API), Context::default());
    assert!(
        go_two.is_ok() && api.is_ok(),
        "second Go refusal: {:?}; metered refusal: {:?}",
        go_two.as_ref().err(),
        api.as_ref().err()
    );
    let mut go_two = go_two.unwrap();
    let mut api = api.unwrap();
    go_two.mark_forwarded().unwrap();
    api.mark_forwarded().unwrap();

    for (row, expected) in [
        (settle(go_two, 22, 10, Some(0.0)), 0.0),
        (settle(api, 30, 10, None), 0.08),
        (settle(go_one, 16, 10, Some(0.02)), 0.02),
    ] {
        assert_eq!(row["settled"], "usage");
        assert_eq!(row["cost_usd"].as_f64(), Some(expected));
    }

    let day = Ledger::open(&fixture.ledger_dir).unwrap().today().unwrap();
    assert_eq!(day.budget().spend().unwrap(), 0.16);
}

#[test]
fn configured_go_pool_and_metered_pool_do_not_consume_each_others_headroom() {
    let fixture = fixture();
    let first = runtime(&fixture, 2.2)
        .with_opencode_budget(&json!({"daily_budget_usd":0.25,"max_request_usd":0.25}))
        .unwrap();
    let second = runtime(&fixture, 2.2)
        .with_opencode_budget(&json!({"daily_budget_usd":0.25,"max_request_usd":0.25}))
        .unwrap();
    let begin = |alias| json!({"model":alias,"max_tokens":32});

    let mut go = first.begin(&begin(GO), Context::default()).unwrap();
    let mut api = second.begin(&begin(API), Context::default()).unwrap();
    let blocked_go = first.begin(&begin(GO), Context::default()).err().unwrap();
    assert_eq!(blocked_go.code, "cloud_budget_reserved");
    assert!(!blocked_go.message.contains("exhausted"));
    assert_eq!(
        second.begin(&begin(API), Context::default()).err().unwrap().code,
        "budget_exceeded"
    );

    api.mark_forwarded().unwrap();
    assert_eq!(settle(api, 20, 10, None)["cost_usd"].as_f64(), Some(0.06));
    go.mark_forwarded().unwrap();
    assert_eq!(settle(go, 16, 10, Some(0.25))["cost_usd"].as_f64(), Some(0.25));

    assert_eq!(
        first.begin(&begin(GO), Context::default()).err().unwrap().code,
        "budget_exceeded"
    );
    // A settled Go charge plus the metered debit would leave too little
    // aggregate headroom for a full metered reservation.
    assert!(second.begin(&begin(API), Context::default()).is_ok());
}

#[test]
fn a_charge_above_the_declared_bound_is_recorded_and_closes_go_admission() {
    let fixture = fixture();
    let policy = json!({"daily_budget_usd":1.0,"max_request_usd":0.25});
    let first = runtime(&fixture, 400.0).with_opencode_budget(&policy).unwrap();
    let mut call = first.begin(&json!({"model":GO}), Context::default()).unwrap();
    call.mark_forwarded().unwrap();
    let row = settle(call, 16, 10, Some(0.3));
    assert_eq!(row["cost_usd"].as_f64(), Some(0.3));
    assert_eq!(row["go_reserved_usd"].as_f64(), Some(0.25));

    let restarted = runtime(&fixture, 400.0).with_opencode_budget(&policy).unwrap();
    assert_eq!(
        restarted
            .begin(&json!({"model":GO}), Context::default())
            .err()
            .unwrap()
            .code,
        "go_budget_bound_exceeded"
    );
    assert!(restarted.begin(&json!({"model":API}), Context::default()).is_ok());
}

#[test]
fn a_legacy_unscoped_go_hold_contributes_zero_to_metered_reservations() {
    const LEGACY: &str = "00000000000000000000000000000000";
    // A missing stored hold, a token-only hold, and a hold whose recorded USD
    // dwarfs the entire daily cap. All three must behave identically for money.
    for legacy in [
        None,
        Some(json!({"subscription_tokens":120})),
        Some(json!({"subscription_tokens":120,"reserved_usd":4000.0})),
    ] {
        let fixture = fixture();
        // Settle one metered call so the day's durable files exist, then plant
        // a hold exactly as a pre-pool binary would have persisted it.
        let warm = runtime(&fixture, 400.0);
        let mut opening = warm
            .begin(&json!({"model":API,"max_tokens":32}), Context::default())
            .unwrap();
        opening.mark_forwarded().unwrap();
        assert_eq!(settle(opening, 20, 10, None)["cost_usd"].as_f64(), Some(0.06));
        let holds_path = fs::read_dir(&fixture.ledger_dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().ends_with(".jsonl.holds"))
            })
            .expect("the settled warm-up call created the durable hold file");
        if let Some(legacy) = &legacy {
            let mut seeded = json!({"failed":false,"holds":{}});
            seeded["holds"][LEGACY] = legacy.clone();
            fs::write(&holds_path, seeded.to_string()).unwrap();
        }

        // Both admission paths proceed while the legacy record is in flight:
        // its recorded USD holds no metered and no Go headroom.
        let metered = runtime(&fixture, 400.0);
        let go = runtime(&fixture, 400.0);
        let begin = |alias| json!({"model":alias,"max_tokens":32});
        let mut api = metered.begin(&begin(API), Context::default()).unwrap();
        let mut call = go.begin(&begin(GO), Context::default()).unwrap();
        api.mark_forwarded().unwrap();
        call.mark_forwarded().unwrap();
        assert_eq!(settle(api, 20, 10, None)["cost_usd"].as_f64(), Some(0.06));
        let row = settle(call, 16, 10, Some(0.02));
        assert_eq!(row["cost_usd"].as_f64(), Some(0.02));
        assert_eq!(row["go_reserved_usd"].as_f64(), Some(0.25));

        // The daily total contains only the actual settled usage.
        let day = Ledger::open(&fixture.ledger_dir).unwrap().today().unwrap();
        assert_eq!(day.budget().spend().unwrap(), 0.14);

        // The stored record itself stays readable and untouched.
        let stored: Value = serde_json::from_slice(&fs::read(&holds_path).unwrap()).unwrap();
        assert_eq!(stored["failed"], false);
        let holds = stored["holds"].as_object().unwrap();
        assert_eq!(holds.len(), usize::from(legacy.is_some()));
        if let Some(legacy) = &legacy {
            assert_eq!(holds.get(LEGACY), Some(legacy));
        }

        // A restarted process keeps working with the legacy record in place.
        let restarted = runtime(&fixture, 400.0);
        assert!(restarted.begin(&begin(API), Context::default()).is_ok());
        assert!(restarted.begin(&begin(GO), Context::default()).is_ok());
    }
}
