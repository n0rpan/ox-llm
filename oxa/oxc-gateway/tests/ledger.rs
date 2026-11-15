#![cfg(feature = "ledger")]
use oxc_gateway::{
    accounting::{Model, Outcome, Usage},
    ledger::{Context, Ledger},
    pricing::Price,
    secrets,
};
use serde_json::{json, Value};
use std::{
    fs,
    os::unix::fs::{symlink, MetadataExt, PermissionsExt},
    path::PathBuf,
    sync::{Arc, Barrier},
};

struct Home(PathBuf);
impl Home {
    fn new() -> Self {
        let mut id = [0; 12];
        getrandom::fill(&mut id).unwrap();
        let path = std::env::temp_dir().join(format!(
            "oxc-ledger-{:x}",
            u128::from_le_bytes([
                id[0], id[1], id[2], id[3], id[4], id[5], id[6], id[7], id[8], id[9], id[10], id[11], 0, 0,
                0, 0
            ])
        ));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }
    fn private(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.0.join(name);
        fs::write(&path, bytes).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        path
    }
    fn rows(&self, day: &str) -> Vec<Value> {
        fs::read_to_string(self.0.join(format!("requests-{day}.jsonl")))
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
fn fields() -> Value {
    json!({"remote":true,"remote_provider":"anthropic","remote_model":"synthetic-model",
        "cost_usd":0.001,"settled":"usage","prompt_tokens":100,"completion_tokens":10,
        "cached_tokens":80,"cache_write_tokens":10,"finish_reason":"stop"})
}
fn model() -> Model {
    Model {
        provider: "anthropic".into(),
        upstream: "synthetic-model".into(),
        context: 1000,
        max_output: 1000,
        price: Price::from_model(&json!({"pricing":{"input":1,"output":1}})).unwrap(),
    }
}
fn usage() -> Usage {
    Usage {
        prompt: Some(100),
        output: Some(10),
        ..Usage::default()
    }
}

#[test]
fn private_metadata_uses_reference_hmac_domains_and_never_stores_content() {
    let home = Home::new();
    home.private(".salt", &[1; 32]);
    let ledger = Ledger::open(&home.0).unwrap();
    let daily = ledger.day("20260102").unwrap();
    let payload = json!({"messages":[{"role":"system","content":"synthetic instructions"},
        {"role":"user","content":"synthetic question"},{"role":"tool","content":"λ"}],
        "tools":[{"function":{"name":"synthetic_tool"}}],"stream":true,"max_tokens":100,"temperature":0.5});
    let mut entry = daily
        .entry(
            "synthetic-alias",
            &payload,
            Context {
                origin: Some(" LAB "),
                session: Some("synthetic-session"),
                idempotency_key: Some("synthetic-idempotency"),
            },
        )
        .unwrap();
    entry.admitted();
    entry.observe();
    let mut remote = fields();
    remote["messages"] = payload["messages"].clone();
    remote["prompt"] = json!("synthetic question");
    let row = entry.append(&remote, 200, None, None).unwrap();
    // Exact vectors computed with Python hmac using request_ledger's domains
    // and synthetic salt; never an unkeyed content fingerprint.
    assert_eq!(row["agent"], "0d3f018cdc3d");
    assert_eq!(row["session"], "5b46e37d166b2534");
    assert_eq!(row["origin"], "lab");
    assert_eq!(row["n_messages"], 3);
    assert_eq!(row["n_tool_msgs"], 1);
    assert_eq!(row["tool_bytes"], json!([2]));
    assert_eq!(row["n_tools"], 1);
    assert_eq!(row["last_role"], "tool");
    assert_eq!(row["max_tokens"], 100);
    assert_eq!(row["temperature"], 0.5);
    assert_eq!(row["gap_ms"], Value::Null);
    assert_eq!(row["completeness"], "incomplete");
    assert_eq!(row["id"].as_str().unwrap().len(), 12);
    assert!(row["wall_ms"].as_f64().unwrap() >= 0.0);
    assert!(row["ttfb_ms"].as_f64().unwrap() >= 0.0);
    let saved = fs::read_to_string(home.0.join("requests-20260102.jsonl")).unwrap();
    for raw in [
        "synthetic instructions",
        "synthetic question",
        "synthetic-session",
        "synthetic-idempotency",
        "synthetic_tool",
        "λ",
    ] {
        assert!(!saved.contains(raw));
    }
    assert_eq!(home.rows("20260102"), vec![row]);
    for path in [
        home.0.clone(),
        home.0.join(".salt"),
        home.0.join("requests-20260102.jsonl"),
    ] {
        let info = fs::metadata(&path).unwrap();
        assert_eq!(info.mode() & 0o077, 0);
        assert_eq!(info.uid(), nix::unistd::Uid::current().as_raw());
    }
}

#[test]
fn daily_handles_share_admission_and_record_one_row_before_releasing_each_hold() {
    let home = Home::new();
    let ledger = Ledger::open(&home.0).unwrap();
    let first = ledger.day("20260102").unwrap();
    let second = ledger.clone().day("20260102").unwrap();
    let mut hold = first
        .budget()
        .reserve(&json!(0.005), true, &model(), 1000)
        .unwrap();
    // Direct Anthropic cache writes reserve 2x input: .003 per call.
    assert!(second
        .budget()
        .reserve(&json!(0.005), true, &model(), 1000)
        .is_err());
    let context = || Context {
        session: Some("synthetic-session"),
        ..Context::default()
    };
    let entry = first.entry("synthetic-alias", &json!({}), context()).unwrap();
    hold.mark_forwarded();
    hold.finish(&usage(), Outcome::Complete, |fields| {
        entry.append(fields, 200, None, None).map(|_| ())
    })
    .unwrap();
    assert_eq!(second.budget().spend().unwrap(), 0.00011);
    let mut hold = second
        .budget()
        .reserve(&json!(0.005), true, &model(), 1000)
        .unwrap();
    let entry = second.entry("synthetic-alias", &json!({}), context()).unwrap();
    hold.mark_forwarded();
    hold.finish(&usage(), Outcome::Complete, |fields| {
        entry.append(fields, 200, None, None).map(|_| ())
    })
    .unwrap();
    let rows = home.rows("20260102");
    assert_eq!(rows.len(), 2);
    assert!(rows[1]["gap_ms"].as_f64().unwrap() >= 0.0);
    assert_ne!(rows[0]["id"], rows[1]["id"]);
    assert_eq!(first.budget().spend().unwrap(), 0.00022);
}

#[test]
fn append_failure_stops_all_handles_and_is_not_retried() {
    let home = Home::new();
    let ledger = Ledger::open(&home.0).unwrap();
    let daily = ledger.day("20260102").unwrap();
    let mut hold = daily.budget().reserve(&json!(1), true, &model(), 1000).unwrap();
    let entry = daily
        .entry("synthetic-alias", &json!({}), Context::default())
        .unwrap();
    hold.mark_forwarded();
    let path = home.0.join("requests-20260102.jsonl");
    fs::create_dir(&path).unwrap();
    assert!(hold
        .finish(&usage(), Outcome::Complete, |fields| entry
            .append(fields, 200, None, None)
            .map(|_| ()))
        .is_err());
    fs::remove_dir(&path).unwrap();
    assert!(ledger
        .clone()
        .day("20260102")
        .unwrap()
        .budget()
        .reserve(&json!(1), true, &model(), 1000)
        .is_err());
    assert!(!path.exists());
}

#[test]
fn midnight_does_not_move_an_admitted_charge_or_discard_an_outstanding_hold() {
    let home = Home::new();
    let ledger = Ledger::open(&home.0).unwrap();
    let before = ledger.day("20260102").unwrap();
    let after = ledger.day("20260103").unwrap();
    let mut held = before
        .budget()
        .reserve(&json!(0.005), true, &model(), 1000)
        .unwrap();
    let next = after
        .budget()
        .reserve(&json!(0.005), true, &model(), 1000)
        .unwrap();
    assert!(ledger
        .day("20260102")
        .unwrap()
        .budget()
        .reserve(&json!(0.005), true, &model(), 1000)
        .is_err());
    let entry = before
        .entry("synthetic-alias", &json!({}), Context::default())
        .unwrap();
    held.mark_forwarded();
    held.finish(&usage(), Outcome::Complete, |fields| {
        entry.append(fields, 200, None, None).map(|_| ())
    })
    .unwrap();
    drop(next);
    assert_eq!(before.budget().spend().unwrap(), 0.00011);
    assert_eq!(after.budget().spend().unwrap(), 0.0);
    assert_eq!(home.rows("20260102").len(), 1);
    assert!(!home.0.join("requests-20260103.jsonl").exists());
    for day in ["../outside", "20260229", "202611150", "2026-01-02"] {
        assert!(ledger.day(day).is_err());
    }
    assert!(ledger.day("20240229").is_ok());
}

#[test]
fn salt_is_private_stable_and_published_atomically_during_concurrent_startup() {
    let home = Home::new();
    let barrier = Arc::new(Barrier::new(8));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let path = home.0.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                let ledger = Ledger::open(&path).unwrap();
                ledger
                    .day("20260102")
                    .unwrap()
                    .entry(
                        "synthetic-alias",
                        &json!({}),
                        Context {
                            session: Some("synthetic-session"),
                            ..Context::default()
                        },
                    )
                    .unwrap()
                    .append(&fields(), 200, None, None)
                    .unwrap()["session"]
                    .clone()
            })
        })
        .collect();
    let sessions: Vec<_> = handles.into_iter().map(|handle| handle.join().unwrap()).collect();
    assert!(sessions.iter().all(|value| value == &sessions[0]));
    assert_eq!(fs::read(home.0.join(".salt")).unwrap().len(), 32);
    assert_eq!(fs::read_dir(&home.0).unwrap().count(), 2);
    assert_eq!(home.rows("20260102").len(), 8);
}

#[test]
fn unsafe_salts_and_repository_locations_are_refused_without_rotation() {
    let home = Home::new();
    let path = home.private(".salt", &[1; 32]);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(Ledger::open(&home.0).is_err());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(&path, [1; 31]).unwrap();
    assert!(Ledger::open(&home.0).is_err());
    assert_eq!(fs::read(&path).unwrap().len(), 31);
    fs::remove_file(&path).unwrap();
    let other = home.private("other", &[1; 32]);
    symlink(other, &path).unwrap();
    assert!(Ledger::open(&home.0).is_err());
    fs::remove_file(path).unwrap();
    fs::create_dir(home.0.join(".git")).unwrap();
    assert!(Ledger::open(&home.0).is_err());
    assert!(!home.0.join(".salt").exists());
}

#[test]
fn provider_key_is_redacted_before_the_ledger_is_opened() {
    let home = Home::new();
    let path = home.private("provider.json", br#"{"anthropic":"synthetic-private-key"}"#);
    let key = secrets::read(&path, "anthropic").unwrap();
    let ledger = Ledger::open(&home.0).unwrap();
    let entry = ledger
        .day("20260102")
        .unwrap()
        .entry("synthetic-private-key", &json!({}), Context::default())
        .unwrap();
    let mut fields = fields();
    fields["remote_model"] = json!("prefix synthetic-private-key suffix");
    let row = entry.append(&fields, 200, None, Some(&key)).unwrap();
    assert_eq!(row["model"], "[redacted]");
    assert_eq!(row["remote_model"], "prefix [redacted] suffix");
    let saved = fs::read_to_string(home.0.join("requests-20260102.jsonl")).unwrap();
    assert!(!saved.contains(key.expose()));
}
