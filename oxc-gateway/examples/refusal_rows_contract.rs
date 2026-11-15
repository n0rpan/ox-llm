//! Real runtime refusal/file boundary; synthetic isolated inputs only.
use oxc_gateway::{
    ledger::Context,
    runtime::{Options, Runtime},
};
use serde_json::{json, Value};
use std::{
    fs,
    io::{self, Read},
    os::unix::fs::PermissionsExt,
    path::PathBuf,
};
fn main() {
    let mut input = String::new();
    io::stdin().take(4096).read_to_string(&mut input).unwrap();
    let input: Value = serde_json::from_str(&input).unwrap();
    let case = input["case"].as_str().unwrap();
    let home = PathBuf::from(std::env::var("HOME").unwrap());
    let key = home.join("provider.json");
    if case != "missing-key" {
        fs::write(&key, b"{\"anthropic\":\"synthetic-refusal-key\"}").unwrap();
        fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let mut config = json!({"cloud_only":true,"cloud":{"enabled":true,"daily_budget_usd":1,
        "models":{"synthetic-cloud":{"kind":"remote/anthropic","base_url":"http://127.0.0.1:1/v1",
        "upstream_model":"synthetic-model","context_window":64000,"max_output":32,"pricing":{"input":1,"output":2}}}}});
    if case == "unknown-price" {
        config["cloud"]["models"]["synthetic-cloud"]
            .as_object_mut()
            .unwrap()
            .remove("pricing");
    }
    if case == "missing-budget" {
        config["cloud"]["daily_budget_usd"] = Value::Null;
    }
    if case == "exhausted" {
        config["cloud"]["daily_budget_usd"] = json!(0.000000001);
    }
    let runtime = Runtime::new(
        config,
        key,
        home.join("ledger"),
        Options {
            offline: case == "offline",
            ledger_enabled: case != "disabled-ledger",
        },
    )
    .unwrap();
    let mut payload = json!({"model":"synthetic-cloud","messages":[{"role":"user","content":"Synthetic request."}]});
    if case == "invalid-limit" {
        payload["max_tokens"] = json!(0);
    }
    let error = runtime
        .begin(
            payload,
            Context {
                origin: Some("lab"),
                session: Some("synthetic-session"),
                idempotency_key: None,
            },
        )
        .err()
        .expect("real refusal");
    let mut rows = Vec::new();
    if let Ok(paths) = fs::read_dir(home.join("ledger")) {
        for path in paths {
            let path = path.unwrap().path();
            if path.extension().is_none_or(|ext| ext != "jsonl") {
                continue;
            }
            for line in fs::read_to_string(path).unwrap().lines() {
                let row: Value = serde_json::from_str(line).unwrap();
                let raw = row.to_string();
                assert!(
                    !raw.contains("synthetic-session")
                        && !raw.contains("Synthetic request.")
                        && !raw.contains("synthetic-refusal-key")
                );
                assert_eq!(row["session"].as_str().unwrap().len(), 16);
                let mut selected = serde_json::Map::new();
                for field in [
                    "remote",
                    "remote_provider",
                    "model",
                    "origin",
                    "status",
                    "error",
                    "cost_usd",
                    "settled",
                    "n_messages",
                ] {
                    selected.insert(field.into(), row[field].clone());
                }
                rows.push(Value::Object(selected));
            }
        }
    }
    println!(
        "{}",
        json!({"status":error.status,"code":error.code,"rows":rows})
    );
}
