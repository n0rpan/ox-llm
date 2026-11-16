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
    paid.mark_forwarded();
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
