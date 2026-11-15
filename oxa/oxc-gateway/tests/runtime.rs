#![cfg(feature = "runtime")]
use oxc_gateway::{
    accounting::Outcome,
    ledger::Context,
    runtime::{Options, Runtime},
};
use serde_json::{json, Value};
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf, time::Duration};

struct Home(PathBuf);
impl Home {
    fn new() -> Self {
        let mut bytes = [0; 12];
        getrandom::fill(&mut bytes).unwrap();
        let suffix: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        let path = std::env::temp_dir().join(format!("oxc-runtime-{suffix}"));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }
    fn key(&self, value: &str) {
        let path = self.0.join("provider.json");
        fs::write(&path, json!({"anthropic":value}).to_string()).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    fn runtime(&self, config: Value, options: Options) -> Runtime {
        Runtime::new(
            config,
            self.0.join("provider.json"),
            self.0.join("ledger"),
            options,
        )
        .unwrap()
    }
    fn rows(&self) -> Vec<Value> {
        let path = self.0.join("ledger").join(format!(
            "requests-{}.jsonl",
            chrono::Local::now().format("%Y%m%d")
        ));
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}
impl Drop for Home {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn config() -> Value {
    json!({"cloud_only":true,"cloud":{"enabled":true,"daily_budget_usd":0.1,"models":{
        "synthetic-cloud":{"kind":"remote/anthropic","base_url":"http://127.0.0.1:1/v1",
            "upstream_model":"synthetic-model","context_window":64000,"max_output":32,
            "pricing":{"input":1,"output":2,"cache_read_per_million":0.1,"cache_write_per_million":1.25}}
    }}})
}
fn payload() -> Value {
    json!({"model":"synthetic-cloud","messages":[{"role":"user","content":"Synthetic question."}],"max_tokens":32,"stream":true})
}
fn canonical() -> Value {
    json!({"usage":{"prompt_tokens":21,"completion_tokens":5,
        "prompt_tokens_details":{"cached_tokens":7,"cache_write_tokens":3}},
        "choices":[{"finish_reason":"stop"}]})
}

#[test]
fn construction_and_discovery_need_no_credential_bytes_or_ledger() {
    let home = Home::new();
    home.key("synthetic-private-key");
    fs::set_permissions(home.0.join("provider.json"), fs::Permissions::from_mode(0o644)).unwrap();
    let runtime = home.runtime(config(), Options::default());
    let discovery = runtime.discovery();
    assert_eq!(discovery.models["data"][0]["id"], "synthetic-cloud");
    assert_eq!(discovery.status["engine_mode"], "none");
    assert_eq!(discovery.status["loaded"], false);
    assert_eq!(
        discovery.status["model_limits"]["synthetic-cloud"]["context_window"],
        64000
    );
    let public = format!("{}{}", discovery.models, discovery.status);
    assert!(!public.contains("127.0.0.1"));
    assert!(!public.contains("provider.json"));
    assert!(!public.contains("synthetic-private-key"));
    assert!(!home.0.join("ledger").exists());
    let error = runtime.begin(payload(), Context::default()).err().unwrap();
    assert_eq!(error.code, "cloud_credentials_unavailable");
    assert!(error.message.contains("0600"));
    assert!(!home.0.join("ledger").exists());
}

#[test]
fn translated_usage_settles_cache_categories_and_private_metadata_once() {
    let home = Home::new();
    home.key("synthetic-private-key");
    let runtime = home.runtime(config(), Options::default());
    let mut call = runtime
        .begin(
            payload(),
            Context {
                origin: Some("lab"),
                session: Some("synthetic-session"),
                idempotency_key: Some("synthetic-idempotency"),
            },
        )
        .unwrap();
    assert_eq!(call.model()["upstream_model"], "synthetic-model");
    assert_eq!(call.payload()["max_tokens"], 32);
    assert_eq!(call.key().expose(), "synthetic-private-key");
    call.mark_forwarded();
    call.observe(&canonical());
    let row = call.finish(Outcome::Complete, 200, None).unwrap();
    assert_eq!(row["cost_usd"], json!(0.00002545));
    assert_eq!(row["settled"], "usage");
    assert_eq!(row["cached_tokens"], 7);
    assert_eq!(row["cache_write_tokens"], 3);
    assert_eq!(row["prompt_tokens"], 21);
    assert_eq!(row["status"], 200);
    assert_eq!(row["origin"], "lab");
    assert_eq!(home.rows(), vec![row.clone()]);
    let text = row.to_string();
    for raw in [
        "Synthetic question.",
        "synthetic-private-key",
        "synthetic-session",
        "synthetic-idempotency",
    ] {
        assert!(!text.contains(raw));
    }
}

#[test]
fn shared_runtime_admits_atomically_and_releases_only_after_the_row() {
    let home = Home::new();
    home.key("synthetic-private-key");
    let runtime = home.runtime(config(), Options::default());
    let mut call = runtime.begin(payload(), Context::default()).unwrap();
    assert_eq!(
        runtime.begin(payload(), Context::default()).err().unwrap().code,
        "budget_exceeded"
    );
    call.mark_forwarded();
    call.observe(&canonical());
    call.finish(Outcome::Complete, 200, None).unwrap();
    assert_eq!(home.rows().len(), 1);
    assert!(runtime.begin(payload(), Context::default()).is_ok());
}

#[test]
fn abort_records_a_conservative_charge_and_an_unforwarded_drop_costs_zero() {
    let home = Home::new();
    home.key("synthetic-private-key");
    let runtime = home.runtime(config(), Options::default());
    drop(runtime.begin(payload(), Context::default()).unwrap());
    let rows = home.rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["cost_usd"], 0.0);
    assert_eq!(rows[0]["settled"], "not_forwarded");
    let mut call = runtime.begin(payload(), Context::default()).unwrap();
    call.mark_forwarded();
    drop(call); // also happens when an async task/future is aborted
    let rows = home.rows();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1]["settled"], "reserved");
    assert_eq!(rows[1]["error"], "cloud_interrupted");
    assert_eq!(rows[1]["status"], 503);
    assert_eq!(rows[1]["cost_usd"], json!(0.080064));
    assert_eq!(
        runtime.begin(payload(), Context::default()).err().unwrap().code,
        "budget_exceeded"
    );
}

#[test]
fn cancellation_without_final_usage_keeps_an_estimated_remote_row() {
    let home = Home::new();
    home.key("synthetic-private-key");
    let runtime = home.runtime(config(), Options::default());
    let mut call = runtime.begin(payload(), Context::default()).unwrap();
    call.mark_forwarded();
    let row = call
        .finish(
            Outcome::Cancelled {
                request_bytes: 128,
                streamed_bytes: 12,
                usage_final: false,
                elapsed: Duration::ZERO,
            },
            499,
            Some("cancelled"),
        )
        .unwrap();
    assert_eq!(row["settled"], "estimated");
    assert!(row["cost_usd"].as_f64().unwrap() > 0.0);
    assert!(row["cost_usd"].as_f64().unwrap() < 0.080064);
    assert_eq!(row["error"], "cancelled");
    assert_eq!(home.rows().len(), 1);
}

#[test]
fn a_failed_append_closes_admission_for_every_following_call() {
    let home = Home::new();
    home.key("synthetic-private-key");
    let runtime = home.runtime(config(), Options::default());
    let mut call = runtime.begin(payload(), Context::default()).unwrap();
    call.mark_forwarded();
    call.observe(&canonical());
    let path = home.0.join("ledger").join(format!(
        "requests-{}.jsonl",
        chrono::Local::now().format("%Y%m%d")
    ));
    fs::create_dir(&path).unwrap();
    assert_eq!(
        call.finish(Outcome::Complete, 200, None).err().unwrap().code,
        "cloud_ledger_unavailable"
    );
    fs::remove_dir(path).unwrap();
    assert_eq!(
        runtime.begin(payload(), Context::default()).err().unwrap().code,
        "cloud_ledger_unavailable"
    );
}

#[test]
fn credentials_rotate_per_request_and_specific_repository_refusals_stay_safe() {
    let home = Home::new();
    home.key("synthetic-first");
    let runtime = home.runtime(config(), Options::default());
    let call = runtime.begin(payload(), Context::default()).unwrap();
    assert_eq!(call.key().expose(), "synthetic-first");
    drop(call);
    home.key("synthetic-second");
    let call = runtime.begin(payload(), Context::default()).unwrap();
    assert_eq!(call.key().expose(), "synthetic-second");
    drop(call);
    fs::create_dir(home.0.join(".git")).unwrap();
    let runtime = home.runtime(config(), Options::default());
    let error = runtime.begin(payload(), Context::default()).err().unwrap();
    assert!(error.message.contains("inside a source repository"));
    assert!(!error.message.contains(&home.0.display().to_string()));
    assert!(!error.message.contains("synthetic-second"));
}

#[test]
fn default_off_offline_and_policy_refusals_precede_keys_budget_and_io() {
    let home = Home::new();
    let mut disabled = config();
    disabled["cloud"]["enabled"] = json!(false);
    let runtime = home.runtime(disabled, Options::default());
    assert!(runtime.discovery().models["data"].as_array().unwrap().is_empty());
    assert_eq!(
        runtime.begin(payload(), Context::default()).err().unwrap().code,
        "cloud_disabled"
    );
    let offline = home.runtime(
        config(),
        Options {
            offline: true,
            ..Options::default()
        },
    );
    assert!(offline.discovery().models["data"].as_array().unwrap().is_empty());
    assert_eq!(
        offline.begin(payload(), Context::default()).err().unwrap().code,
        "offline"
    );
    let mut no_price = config();
    no_price["cloud"]["models"]["synthetic-cloud"]
        .as_object_mut()
        .unwrap()
        .remove("pricing");
    assert_eq!(
        home.runtime(no_price, Options::default())
            .begin(payload(), Context::default())
            .err()
            .unwrap()
            .code,
        "cloud_pricing_unavailable"
    );
    let mut no_budget = config();
    no_budget["cloud"]["daily_budget_usd"] = Value::Null;
    assert_eq!(
        home.runtime(no_budget, Options::default())
            .begin(payload(), Context::default())
            .err()
            .unwrap()
            .code,
        "cloud_credentials_unavailable"
    );
    assert!(!home.0.join("ledger").exists());
    home.key("synthetic-private-key");
    let runtime = home.runtime(
        config(),
        Options {
            ledger_enabled: false,
            ..Options::default()
        },
    );
    assert_eq!(
        runtime.begin(payload(), Context::default()).err().unwrap().code,
        "cloud_ledger_unavailable"
    );
    assert!(!home.0.join("ledger").exists());
}

#[test]
fn catalog_bounded_output_is_shared_with_the_provider_client() {
    let home = Home::new();
    home.key("synthetic-private-key");
    let runtime = home.runtime(config(), Options::default());
    for bad in [
        json!(true),
        json!(0),
        json!(-1),
        json!(2.5),
        json!("3"),
        Value::Null,
    ] {
        let mut body = payload();
        body["max_tokens"] = bad;
        assert_eq!(
            runtime.begin(body, Context::default()).err().unwrap().param,
            "max_tokens"
        );
    }
    let mut body = payload();
    body["max_tokens"] = serde_json::from_str("9999999999999999999999999999999999999999999").unwrap();
    let call = runtime.begin(body, Context::default()).unwrap();
    assert_eq!(call.payload()["max_tokens"], 32);
    assert_eq!(call.model()["max_output"], 32);
}
