#![cfg(feature = "ledger")]
use oxc_gateway::{
    accounting::{Model, Outcome, Usage},
    ledger::{Context, Ledger, Origin},
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
#[test]
#[cfg(feature = "anthropic-gateway")]
fn history_authentication_binds_native_items_route_account_and_exact_visible_prefix() {
    use oxa_proto::{FrozenPrefix, ProviderHistory, ToolCall, WireMsg};
    use oxc_gateway::catalog::{Model, Provider};
    let home = Home::new();
    let ledger = Ledger::open(&home.0).unwrap();
    let route = Model::new(
        "synthetic",
        Provider::OpenAi,
        "synthetic-upstream",
        "https://example.invalid",
        1000,
        100,
    )
    .unwrap();
    let prefix = FrozenPrefix {
        sections: vec![("system".into(), "synthetic system".into())],
        tools_json: "[]".into(),
        sha256: [0; 32],
    };
    let input = vec![WireMsg::user("read a.rs")];
    let mut assistant = WireMsg::assistant("");
    assistant.content = None;
    assistant.tool_calls.push(ToolCall {
        id: "synthetic-call".into(),
        name: "read".into(),
        args_raw: "{ \"path\" : \"a.rs\" }".into(),
    });
    let items = vec![
        json!({"type":"reasoning","encrypted_content":"synthetic-opaque"}),
        json!({"type":"function_call","call_id":"synthetic-call","arguments":"{ \"path\" : \"a.rs\" }"}),
    ];
    let signed = ledger
        .seal_provider_history(&route, &[1; 32], &prefix, &input, &assistant, items.clone())
        .unwrap();
    assistant.provider_history = Some(signed.clone());
    let mut wire = input;
    wire.push(assistant);
    wire.push(WireMsg::tool_result("synthetic-call", "synthetic file"));
    Ledger::open(&home.0)
        .unwrap()
        .verify_provider_history(&route, &[1; 32], &prefix, &wire)
        .unwrap();
    let bad_account = ledger
        .verify_provider_history(&route, &[2; 32], &prefix, &wire)
        .unwrap_err();
    assert_eq!(
        (bad_account.status, bad_account.code),
        (400, "provider_history_mismatch")
    );
    for (alias, provider, upstream, url) in [
        (
            "other",
            Provider::OpenAi,
            "synthetic-upstream",
            "https://example.invalid",
        ),
        (
            "synthetic",
            Provider::Anthropic,
            "synthetic-upstream",
            "https://example.invalid",
        ),
        ("synthetic", Provider::OpenAi, "other", "https://example.invalid"),
        (
            "synthetic",
            Provider::OpenAi,
            "synthetic-upstream",
            "https://other.invalid",
        ),
    ] {
        let changed = Model::new(alias, provider, upstream, url, 1000, 100).unwrap();
        let error = ledger
            .verify_provider_history(&changed, &[1; 32], &prefix, &wire)
            .unwrap_err();
        assert_eq!((error.status, error.code), (400, "provider_history_mismatch"));
    }
    for index in [0, 1] {
        let mut changed = wire.clone();
        changed[index].content = Some("changed visible prefix".into());
        assert_eq!(
            ledger
                .verify_provider_history(&route, &[1; 32], &prefix, &changed)
                .unwrap_err()
                .code,
            "provider_history_mismatch"
        );
    }
    let mut changed = wire.clone();
    changed[1].tool_calls[0].args_raw = "{\"path\":\"a.rs\"}".into();
    assert!(ledger
        .verify_provider_history(&route, &[1; 32], &prefix, &changed)
        .is_err());
    let mut changed_prefix = prefix.clone();
    changed_prefix.sections[0].1 = "changed system with unchanged caller hash".into();
    assert!(ledger
        .verify_provider_history(&route, &[1; 32], &changed_prefix, &wire)
        .is_err());
    changed_prefix = prefix.clone();
    changed_prefix.tools_json = "[{\"type\":\"function\"}]".into();
    assert!(ledger
        .verify_provider_history(&route, &[1; 32], &changed_prefix, &wire)
        .is_err());
    for altered in [
        vec![items[1].clone(), items[0].clone()],
        vec![json!({"type":"reasoning","encrypted_content":"changed"})],
    ] {
        let mut changed = wire.clone();
        changed[1].provider_history =
            Some(ProviderHistory::new(*signed.scope(), *signed.prefix(), altered).unwrap());
        let error = ledger
            .verify_provider_history(&route, &[1; 32], &prefix, &changed)
            .unwrap_err();
        assert_eq!((error.status, error.code), (400, "provider_history_mismatch"));
    }
    let other_home = Home::new();
    let other = Ledger::open(&other_home.0).unwrap();
    assert!(other
        .verify_provider_history(&route, &[1; 32], &prefix, &wire)
        .is_err());
    assert_eq!(
        wire[1].provider_history.as_ref().unwrap(),
        &signed,
        "verification never re-signs"
    );
}

#[test]
#[cfg(feature = "anthropic-gateway")]
fn an_authorized_rebase_can_continue_visible_history_only_after_dropping_old_authority() {
    use oxa_proto::{FrozenPrefix, WireMsg};
    use oxc_gateway::catalog::{Model, Provider};
    let home = Home::new();
    let ledger = Ledger::open(&home.0).unwrap();
    let route = Model::new(
        "synthetic",
        Provider::OpenAi,
        "synthetic",
        "https://example.invalid",
        1000,
        100,
    )
    .unwrap();
    let old = FrozenPrefix {
        sections: vec![("system".into(), "before".into())],
        tools_json: "[]".into(),
        sha256: [0; 32],
    };
    let input = vec![WireMsg::user("before")];
    let mut assistant = WireMsg::assistant("complete visible answer");
    assistant.provider_history = Some(
        ledger
            .seal_provider_history(
                &route,
                &[1; 32],
                &old,
                &input,
                &assistant,
                vec![json!({"type":"reasoning","encrypted_content":"synthetic"})],
            )
            .unwrap(),
    );
    let mut rebase = vec![WireMsg::user("summary"), assistant];
    assert!(ledger
        .verify_provider_history(&route, &[1; 32], &old, &rebase)
        .is_err());
    rebase[1].provider_history = None;
    ledger
        .verify_provider_history(&route, &[1; 32], &old, &rebase)
        .unwrap();
}
#[test]
fn subscription_rows_keep_zero_usd_and_checked_request_token_dimensions() {
    let home = Home::new();
    let ledger = Ledger::open(&home.0).unwrap();
    let daily = ledger.day("20260102").unwrap();
    let good = json!({"remote":true,"remote_provider":"chatgpt","remote_model":"synthetic-model",
        "cost_usd":0.0,"billing":"subscription","settled":"usage",
        "subscription_requests":1,"subscription_tokens":26,"prompt_tokens":21,"completion_tokens":5});
    let row = daily
        .entry("synthetic-model", Context::default())
        .unwrap()
        .append(&good, 200, None, None)
        .unwrap();
    assert_eq!(row["billing"], "subscription");
    assert_eq!(row["cost_usd"], 0.0);
    assert_eq!(row["subscription_requests"], 1);
    assert_eq!(row["subscription_tokens"], 26);
    for (field, value) in [
        ("remote_provider", json!("openai")),
        ("cost_usd", json!(0.1)),
        ("billing", json!("metered")),
        ("billing", Value::Null),
        ("subscription_requests", json!(0)),
        ("subscription_requests", json!(2)),
        ("subscription_tokens", json!(-1)),
        ("subscription_tokens", Value::Null),
        ("settled", json!("not_forwarded")),
        ("settled", json!("refused")),
    ] {
        let mut invalid = good.clone();
        invalid[field] = value;
        assert!(daily
            .entry("synthetic-model", Context::default())
            .unwrap()
            .append(&invalid, 200, None, None)
            .is_err());
    }
    assert_eq!(home.rows("20260102"), vec![row]);
}
#[test]
fn refused_settlement_requires_zero_cost_and_a_client_error_status() {
    let home = Home::new();
    let ledger = Ledger::open(&home.0).unwrap();
    let daily = ledger.day("20260102").unwrap();
    let mut refused = fields();
    refused["settled"] = json!("refused");
    for (status, cost) in [(200, 0.0), (503, 0.0), (429, 0.1)] {
        refused["cost_usd"] = json!(cost);
        assert!(daily
            .entry("synthetic-alias", Context::default())
            .unwrap()
            .append(&refused, status, Some("cloud_upstream_error"), None)
            .is_err());
    }
    assert!(!home.0.join("requests-20260102.jsonl").exists());
    refused["cost_usd"] = json!(0.0);
    let row = daily
        .entry("synthetic-alias", Context::default())
        .unwrap()
        .append(&refused, 429, Some("cloud_upstream_error"), None)
        .unwrap();
    assert_eq!(row["cost_usd"], 0.0);
    assert_eq!(row["settled"], "refused");
    assert_eq!(home.rows("20260102"), vec![row]);
}
fn model() -> Model {
    Model {
        provider: "anthropic".into(),
        upstream: "synthetic-model".into(),
        context: 1000,
        max_output: 1000,
        billing: oxc_gateway::billing::Billing::Metered(
            Price::from_model(&json!({"pricing":{"input":1,"output":1}})).unwrap(),
        ),
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
fn keyed_identifiers_are_stable_separated_and_no_request_content_is_stored() {
    let home = Home::new();
    home.private(".salt", &[1; 32]);
    let ledger = Ledger::open(&home.0).unwrap();
    let daily = ledger.day("20260102").unwrap();
    let context = || Context {
        origin: Origin::Oxa,
        session: Some("synthetic-private-identifier"),
        idempotency_key: Some("synthetic-private-identifier"),
    };
    let mut entry = daily.entry("synthetic-alias", context()).unwrap();
    entry.admitted();
    entry.observe();
    let mut remote = fields();
    remote["messages"] = json!([{"content":"synthetic-private-content"}]);
    remote["prompt"] = json!("synthetic-private-content");
    remote["session"] = json!("synthetic-private-identifier");
    let row = entry.append(&remote, 200, None, None).unwrap();
    assert_eq!(row["origin"], "oxa");
    assert_eq!(row["id"].as_str().unwrap().len(), 32);
    assert_eq!(row["session"].as_str().unwrap().len(), 32);
    assert_ne!(row["session"], row["idempotency"]);
    assert!(row["wall_ms"].as_f64().unwrap() >= 0.0);
    assert!(row["ttfb_ms"].as_f64().unwrap() >= 0.0);
    for removed in [
        "agent",
        "gap_ms",
        "messages",
        "prompt",
        "tool_bytes",
        "path",
        "completeness",
    ] {
        assert!(row.get(removed).is_none());
    }
    let saved = fs::read_to_string(home.0.join("requests-20260102.jsonl")).unwrap();
    assert!(!saved.contains("synthetic-private-content"));
    assert!(!saved.contains("synthetic-private-identifier"));
    assert_eq!(home.rows("20260102"), vec![row.clone()]);
    let reopened = Ledger::open(&home.0).unwrap();
    let again = reopened
        .day("20260102")
        .unwrap()
        .entry("synthetic-alias", context())
        .unwrap()
        .append(&fields(), 200, None, None)
        .unwrap();
    assert_eq!(again["session"], row["session"]);
    assert_eq!(again["idempotency"], row["idempotency"]);
    for path in [
        home.0.clone(),
        home.0.join(".salt"),
        home.0.join("requests-20260102.jsonl"),
    ] {
        let info = fs::metadata(path).unwrap();
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
    let entry = first.entry("synthetic-alias", context()).unwrap();
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
    let entry = second.entry("synthetic-alias", context()).unwrap();
    hold.mark_forwarded();
    hold.finish(&usage(), Outcome::Complete, |fields| {
        entry.append(fields, 200, None, None).map(|_| ())
    })
    .unwrap();
    let rows = home.rows("20260102");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["session"], rows[1]["session"]);
    assert_ne!(rows[0]["id"], rows[1]["id"]);
    assert_eq!(first.budget().spend().unwrap(), 0.00022);
}

#[test]
fn append_failure_stops_all_handles_and_is_not_retried() {
    let home = Home::new();
    let ledger = Ledger::open(&home.0).unwrap();
    let daily = ledger.day("20260102").unwrap();
    let mut hold = daily.budget().reserve(&json!(1), true, &model(), 1000).unwrap();
    let entry = daily.entry("synthetic-alias", Context::default()).unwrap();
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
    let entry = before.entry("synthetic-alias", Context::default()).unwrap();
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
    let nested = home.0.join("new/nested");
    assert!(Ledger::open(&nested).is_err());
    assert!(!home.0.join("new").exists());
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
        .entry("synthetic-private-key", Context::default())
        .unwrap();
    let mut fields = fields();
    fields["remote_model"] = json!("prefix synthetic-private-key suffix");
    let row = entry.append(&fields, 200, None, Some(&key)).unwrap();
    assert_eq!(row["model"], "[redacted]");
    assert_eq!(row["remote_model"], "prefix [redacted] suffix");
    let saved = fs::read_to_string(home.0.join("requests-20260102.jsonl")).unwrap();
    assert!(!saved.contains(key.expose()));
}

#[test]
fn invalid_billing_or_identifier_fields_create_no_request_row() {
    let home = Home::new();
    let ledger = Ledger::open(&home.0).unwrap();
    let daily = ledger.day("20260102").unwrap();
    for (name, invalid) in [
        ("remote", json!(false)),
        ("remote_provider", json!("unknown")),
        ("remote_model", json!("")),
        ("remote_model", json!("x".repeat(1025))),
        ("cost_usd", json!(-1)),
        ("cost_usd", json!(true)),
        ("prompt_tokens", json!(-1)),
        ("cached_tokens", json!(true)),
        ("settled", json!("unknown")),
    ] {
        let mut raw = fields();
        raw[name] = invalid;
        let entry = daily.entry("synthetic-alias", Context::default()).unwrap();
        assert_eq!(
            entry.append(&raw, 200, None, None).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
    }
    assert!(daily.entry("", Context::default()).is_err());
    let long = "x".repeat(4097);
    assert!(daily
        .entry(
            "synthetic-alias",
            Context {
                session: Some(&long),
                ..Context::default()
            }
        )
        .is_err());
    assert!(!home.0.join("requests-20260102.jsonl").exists());
    let row = daily
        .entry(
            "synthetic-alias",
            Context {
                origin: Origin::Api,
                ..Context::default()
            },
        )
        .unwrap()
        .append(&fields(), 200, None, None)
        .unwrap();
    assert_eq!(row["origin"], "api");
    assert_eq!(row["session"], Value::Null);
}

#[test]
fn shared_writers_append_complete_rows_without_retaining_request_bodies() {
    let home = Home::new();
    let ledger = Ledger::open(&home.0).unwrap();
    let barrier = Arc::new(Barrier::new(32));
    let handles: Vec<_> = (0..32)
        .map(|_| {
            let ledger = ledger.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let daily = ledger.day("20260102").unwrap();
                let entry = daily.entry("synthetic-alias", Context::default()).unwrap();
                barrier.wait();
                entry.append(&fields(), 200, None, None).unwrap();
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }
    let rows = home.rows("20260102");
    assert_eq!(rows.len(), 32);
    assert!(rows
        .iter()
        .all(|row| row["remote"] == true && row["settled"] == "usage"));
    assert_eq!(ledger.day("20260102").unwrap().budget().spend().unwrap(), 0.032);
}

#[test]
fn response_data_before_admission_does_not_record_a_first_byte() {
    let home = Home::new();
    let ledger = Ledger::open(&home.0).unwrap();
    let mut entry = ledger
        .day("20260102")
        .unwrap()
        .entry("synthetic", Context::default())
        .unwrap();
    entry.observe();
    entry.admitted();
    let row = entry.append(&fields(), 200, None, None).unwrap();
    assert!(row.get("ttfb_ms").is_none());
}
