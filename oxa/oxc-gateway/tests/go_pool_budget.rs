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

fn runtime(fixture: &skeleton::Fixture) -> Runtime {
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
            daily_budget_usd: Some(400.0),
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
    let fixture = skeleton::Fixture::for_endpoint("https://example.invalid");
    fs::write(
        &fixture.key_path,
        json!({"opencode":"synthetic-plan-key","anthropic":"synthetic-api-key"}).to_string(),
    )
    .unwrap();
    let first = runtime(&fixture);
    let second = runtime(&fixture);
    let metered = runtime(&fixture);
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
