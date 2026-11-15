//! Refusal-stage test driver; never reads credentials or contacts a provider.
use oxc_gateway::policy::{self, Refusal};
use serde_json::{json, Value};
use std::io::{self, Read};
fn main() {
    let mut text = String::new();
    io::stdin()
        .take(65537)
        .read_to_string(&mut text)
        .expect("contract input");
    assert!(text.len() <= 65536);
    let input: Value = serde_json::from_str(&text).expect("contract JSON");
    let result = policy::before_credentials(
        &input["payload"],
        input["shared"] == true,
        input["offline"] == true,
        input["pricing"] == true,
    )
    .and_then(|()| {
        if input["credentials"] == true {
            Ok(())
        } else {
            Err(Refusal::credentials())
        }
    })
    .and_then(|()| {
        policy::daily_budget(
            &input["cap"],
            input["ledger_enabled"] == true,
            input["spent"].as_f64(),
            input["reserved"].as_f64().unwrap_or(0.0),
            input["required"].as_f64().unwrap_or(f64::INFINITY),
        )
    });
    let output = match result {
        Err(error) => json!({"status":error.status,"body":error.body()}),
        Ok(()) => Value::Null,
    };
    println!("{}", serde_json::to_string(&output).expect("contract output"));
}
