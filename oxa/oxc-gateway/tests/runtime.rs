#![cfg(feature = "anthropic-gateway")]
#[allow(dead_code)]
#[path = "common/anthropic.rs"]
mod anthropic;
#[allow(dead_code)]
#[path = "common/skeleton.rs"]
mod skeleton;
use oxc_gateway::{
    catalog::{Access, Model, Provider},
    ledger::Context,
    policy::Refusal,
    pricing::Price,
    runtime::{Options, Runtime},
};
use serde_json::{json, Value};

fn runtime(fixture: &skeleton::Fixture, access: Access, priced: bool, ledger: bool) -> Runtime {
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
    Runtime::new(
        [(
            model,
            priced
                .then(|| Price::from_model(&fixture.config["cloud"]["models"][skeleton::MODEL]).unwrap())
                .map(oxc_gateway::runtime::Billing::Metered),
        )],
        fixture.key_path.clone(),
        fixture.ledger_dir.clone(),
        Options {
            access,
            daily_budget_usd: Some(1.0),
            ledger_enabled: ledger,
            subscription: None,
            subscription_limits: None,
        },
    )
    .unwrap()
}
fn rows(fixture: &skeleton::Fixture) -> Vec<Value> {
    std::fs::read_dir(&fixture.ledger_dir)
        .unwrap()
        .filter_map(|file| {
            let path = file.unwrap().path();
            (path.extension().is_some_and(|ext| ext == "jsonl"))
                .then(|| std::fs::read_to_string(path).unwrap())
        })
        .flat_map(|text| {
            text.lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect::<Vec<Value>>()
        })
        .collect()
}
fn refusal(error: Refusal, status: u16, code: &str) {
    assert_eq!((error.status, error.code), (status, code));
    assert_eq!(error.body()["error"]["code"], code);
    assert!(!error.message.is_empty());
}
#[test]
fn configured_route_requires_its_named_key_and_binds_replay_to_its_endpoint() {
    let fixture = skeleton::Fixture::for_endpoint("https://first.example.invalid");
    let make_runtime = |endpoint| {
        let model = Model::new(
            skeleton::MODEL,
            Provider::Anthropic,
            skeleton::UPSTREAM,
            endpoint,
            1024,
            16,
        )
        .unwrap()
        .with_configured_route("synthetic-provider", "synthetic-pool", "synthetic-key")
        .unwrap();
        let price = Price::from_model(&fixture.config["cloud"]["models"][skeleton::MODEL]).unwrap();
        Runtime::new(
            [(model, Some(oxc_gateway::runtime::Billing::Metered(price)))],
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
        .unwrap()
    };
    let payload = json!({"model":skeleton::MODEL});
    let first = make_runtime("https://first.example.invalid");
    let second = make_runtime("https://second.example.invalid");
    let missing = first.begin(&payload, Context::default()).err().unwrap();
    refusal(missing, 503, "cloud_credentials_unavailable");

    let credential = "synthetic-configured-credential";
    std::fs::write(
        &fixture.key_path,
        json!({"synthetic-key": credential}).to_string(),
    )
    .unwrap();
    let context = || Context {
        session: Some("synthetic-conversation"),
        ..Context::default()
    };
    let a = first.begin(&payload, context()).unwrap();
    let b = second.begin(&payload, context()).unwrap();
    assert_eq!(a.key_handle().unwrap().expose(), credential);
    assert_eq!(b.key_handle().unwrap().expose(), credential);
    assert_ne!(a.provider_account_scope(), b.provider_account_scope());
    assert_ne!(
        a.provider_session_hint().unwrap(),
        b.provider_session_hint().unwrap()
    );
    let route = a.model().configured_route().unwrap();
    assert_eq!(route.provider(), "synthetic-provider");
    assert_eq!(route.pool(), "synthetic-pool");
    assert!(!format!("{:?}", a.model()).contains("synthetic-key"));
}

#[test]
fn discovery_is_borrowed_and_never_reads_a_key_or_initializes_a_ledger() {
    let mock = anthropic::MockAnthropic::start(vec![]);
    let fixture = skeleton::Fixture::new(&mock);
    std::fs::remove_file(&fixture.key_path).unwrap();
    let runtime = runtime(&fixture, Access::Enabled, true, true);
    let first = runtime.discovery().next().unwrap();
    assert_eq!(
        (first.id, first.context_window, first.max_output),
        (skeleton::MODEL, 1024, 16)
    );
    assert_eq!(first.id.as_ptr(), runtime.discovery().next().unwrap().id.as_ptr());
    assert!(rows(&fixture).is_empty());
    assert_eq!(std::fs::read_dir(&fixture.ledger_dir).unwrap().count(), 0);
    assert!(mock.requests().is_empty());
}
#[test]
fn refusal_status_codes_and_zero_rows_do_not_depend_on_python_wording() {
    for (access, priced, ledger, expected) in [
        (Access::Disabled, true, true, (403, "cloud_disabled")),
        (Access::Offline, true, true, (403, "offline")),
        (Access::Enabled, false, true, (400, "cloud_pricing_unavailable")),
        (
            Access::Enabled,
            true,
            true,
            (503, "cloud_credentials_unavailable"),
        ),
        (Access::Enabled, true, false, (503, "cloud_ledger_unavailable")),
    ] {
        let mock = anthropic::MockAnthropic::start(vec![]);
        let fixture = skeleton::Fixture::new(&mock);
        std::fs::remove_file(&fixture.key_path).unwrap();
        let rt = runtime(&fixture, access, priced, ledger);
        let error = rt
            .begin(&json!({"model":skeleton::MODEL}), Context::default())
            .err()
            .unwrap();
        refusal(error, expected.0, expected.1);
        let rows = rows(&fixture);
        if access == Access::Disabled || !ledger {
            assert!(rows.is_empty());
        } else {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0]["status"], expected.0);
            assert_eq!(rows[0]["cost_usd"], 0.0);
            assert_eq!(rows[0]["settled"], "not_forwarded");
        }
        assert!(mock.requests().is_empty());
    }
}
#[test]
fn dropped_guards_record_zero_before_forwarding_and_reserve_unknown_paid_work() {
    let mock = anthropic::MockAnthropic::start(vec![]);
    let fixture = skeleton::Fixture::new(&mock);
    let rt = runtime(&fixture, Access::Enabled, true, true);
    let payload = json!({"model":skeleton::MODEL,"max_tokens":1000});
    let mut call = rt.begin(&payload, Context::default()).unwrap();
    assert_eq!(call.max_output(), 16);
    assert!(std::sync::Arc::ptr_eq(
        &call.key_handle().unwrap(),
        &call.key_handle().unwrap()
    ));
    call.observe(&json!({"usage":{"prompt_tokens":1,"completion_tokens":1}}));
    drop(call);
    assert_eq!(rows(&fixture)[0]["cost_usd"], 0.0);
    let mut paid = rt.begin(&payload, Context::default()).unwrap();
    paid.mark_forwarded().unwrap();
    drop(paid);
    let rows = rows(&fixture);
    assert_eq!(rows.len(), 2);
    assert!(rows[1]["cost_usd"].as_f64().unwrap() > 0.0);
    assert_eq!(rows[1]["settled"], "reserved");
    assert_eq!(rows[1]["error"], "cloud_interrupted");
}
#[test]
fn policy_and_output_limits_refuse_before_reading_a_missing_key() {
    let mock = anthropic::MockAnthropic::start(vec![]);
    let fixture = skeleton::Fixture::new(&mock);
    std::fs::remove_file(&fixture.key_path).unwrap();
    let rt = runtime(&fixture, Access::Enabled, true, true);
    for payload in [
        json!({"model":skeleton::MODEL,"n":2}),
        json!({"model":skeleton::MODEL,"max_tokens":0}),
    ] {
        refusal(
            rt.begin(&payload, Context::default()).err().unwrap(),
            400,
            "invalid_value",
        );
    }
    refusal(
        rt.begin(&json!({"model":"missing"}), Context::default())
            .err()
            .unwrap(),
        400,
        "unknown_model",
    );
    assert_eq!(rows(&fixture).len(), 2);
}

#[test]
fn an_unpaid_append_failure_preserves_its_refusal_and_blocks_a_new_runtime() {
    use std::{
        fs,
        os::unix::fs::{symlink, PermissionsExt},
    };
    let mock = anthropic::MockAnthropic::start(vec![]);
    let fixture = skeleton::Fixture::new(&mock);
    let rt = runtime(&fixture, Access::Enabled, true, true);
    let payload = json!({"model":skeleton::MODEL});
    drop(rt.begin(&payload, Context::default()).unwrap());
    let path = fs::read_dir(&fixture.ledger_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .unwrap();
    let original = fs::read(&path).unwrap();
    fs::remove_file(&path).unwrap();
    symlink(fixture.ledger_dir.join("unrelated-synthetic-file"), &path).unwrap();
    fs::remove_file(&fixture.key_path).unwrap();
    refusal(
        rt.begin(&payload, Context::default()).err().unwrap(),
        503,
        "cloud_credentials_unavailable",
    );
    fs::remove_file(&path).unwrap();
    fs::write(&path, original).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(&fixture.key_path, br#"{"anthropic":"synthetic-restored"}"#).unwrap();
    fs::set_permissions(&fixture.key_path, fs::Permissions::from_mode(0o600)).unwrap();
    let other = runtime(&fixture, Access::Enabled, true, true);
    refusal(
        other.begin(&payload, Context::default()).err().unwrap(),
        503,
        "cloud_ledger_unavailable",
    );
    assert!(mock.requests().is_empty());
}

#[test]
fn native_unknown_cost_survives_reopen_and_closes_capped_admission() {
    const UNBOUNDED: &str = "synthetic-unbounded";
    const BOUNDED: &str = "synthetic-bounded";
    let mock = anthropic::MockAnthropic::start(vec![]);
    let fixture = skeleton::Fixture::new(&mock);
    let price = || Price::from_model(&fixture.config["cloud"]["models"][skeleton::MODEL]).unwrap();
    let open = |cap: Option<f64>| {
        let endpoint = fixture.config["cloud"]["models"][skeleton::MODEL]["base_url"]
            .as_str()
            .unwrap();
        // The first alias has a context beyond the price-bound ceiling; the second is ordinary.
        let aliases = [(UNBOUNDED, 200_000_000), (BOUNDED, 4096)].map(|(id, context)| {
            let model =
                Model::new(id, Provider::Anthropic, skeleton::UPSTREAM, endpoint, context, 16).unwrap();
            (model, Some(oxc_gateway::runtime::Billing::Metered(price())))
        });
        Runtime::new(
            aliases,
            fixture.key_path.clone(),
            fixture.ledger_dir.clone(),
            Options {
                access: Access::Enabled,
                daily_budget_usd: cap,
                ledger_enabled: true,
                subscription: None,
                subscription_limits: None,
            },
        )
        .unwrap()
    };
    let uncapped = open(None);
    let mut call = uncapped
        .begin(&json!({"model":UNBOUNDED}), Context::default())
        .unwrap();
    call.mark_forwarded().unwrap();
    // Finished without any final usage: the real settlement path writes the durable row.
    call.finish(oxc_gateway::accounting::Outcome::Complete, 200, None)
        .unwrap();
    let rows = rows(&fixture);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["remote_model"], skeleton::UPSTREAM);
    assert_eq!(rows[0]["cost_unknown"], true);
    assert_eq!(rows[0]["usage_missing"], true);
    assert_eq!(rows[0]["cost_usd"], 0.0);
    // Uncapped admission keeps working on the ordinary alias.
    uncapped
        .begin(&json!({"model":BOUNDED}), Context::default())
        .unwrap()
        .finish(oxc_gateway::accounting::Outcome::Complete, 200, None)
        .unwrap();
    // Reopened with a cap: the persisted unknown cost closes admission for the bounded
    // alias, whose own price bound would otherwise fit well under the cap.
    let capped = open(Some(100.0));
    refusal(
        capped
            .begin(&json!({"model":BOUNDED}), Context::default())
            .err()
            .unwrap(),
        429,
        "budget_exceeded",
    );
}

#[test]
fn unpriced_model_admitted_without_cap() {
    for (cap, admitted) in [(None, true), (Some(1.0), false)] {
        let mock = anthropic::MockAnthropic::start(vec![]);
        let fixture = skeleton::Fixture::new(&mock);
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
        let rt = Runtime::new(
            [(model, None)],
            fixture.key_path.clone(),
            fixture.ledger_dir.clone(),
            Options {
                access: Access::Enabled,
                daily_budget_usd: cap,
                ledger_enabled: true,
                subscription: None,
                subscription_limits: None,
            },
        )
        .unwrap();
        let result = rt.begin(&json!({"model":skeleton::MODEL}), Context::default());
        if admitted {
            assert!(result.is_ok());
        } else {
            refusal(result.err().unwrap(), 400, "cloud_pricing_unavailable");
        }
    }
}
