//! Private Runtime/Call rows; synthetic inputs, no provider I/O.
use oxc_gateway::{
    accounting::Outcome,
    ledger::Context,
    runtime::{Options, Runtime},
};
use serde_json::{json, Map, Value};
use std::{
    fs,
    io::{self, Read},
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    time::Duration,
};

fn main() {
    let mut input = String::new();
    io::stdin().take(65536).read_to_string(&mut input).unwrap();
    let spec: Value = serde_json::from_str(&input).unwrap();
    let home = PathBuf::from(std::env::var("HOME").unwrap());
    let key = home.join("provider.json");
    fs::write(&key, b"{\"anthropic\":\"synthetic-paid-row-key\"}").unwrap();
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
    let directory = home.join("ledger");
    fs::create_dir(&directory).unwrap();
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
    let salt = directory.join(".salt");
    fs::write(&salt, [1; 32]).unwrap();
    fs::set_permissions(&salt, fs::Permissions::from_mode(0o600)).unwrap();
    let config = json!({"cloud_only":true,"cloud":{"enabled":true,"daily_budget_usd":1,
        "models":{"synthetic-cloud":{"kind":"remote/anthropic","base_url":"http://127.0.0.1:1/v1",
        "upstream_model":"synthetic-model","context_window":1024,"max_output":32,
        "pricing":{"input":2,"output":3,"cache_read_per_million":0.2,"cache_write_per_million":2.5}}}}});
    let runtime = Runtime::new(config, key, directory, Options::default()).unwrap();
    let mut call = runtime
        .begin(
            spec["payload"].clone(),
            Context {
                origin: Some(" LAB "),
                session: spec["session"].as_str(),
                idempotency_key: None,
            },
        )
        .unwrap();
    assert_eq!(call.payload()["max_tokens"], 32);
    call.mark_forwarded();
    call.observe(&spec["frame"]);
    let (outcome, status, error) = match spec["case"].as_str().unwrap() {
        "failed-usage" => (Outcome::Failed, 503, Some("cloud_upstream_error")),
        "cancel-final" => (
            Outcome::Cancelled {
                request_bytes: 0,
                streamed_bytes: 0,
                usage_final: true,
                elapsed: Duration::ZERO,
            },
            499,
            Some("client_closed"),
        ),
        _ => (Outcome::Complete, 200, None),
    };
    let row = call.finish(outcome, status, error).unwrap();
    let selected: Map<String, Value> = spec["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|field| {
            let field = field.as_str().unwrap();
            (field.into(), row[field].clone())
        })
        .collect();
    println!("{}", Value::Object(selected));
}
