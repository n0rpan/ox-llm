#![cfg(feature = "accounting")]
use oxc_gateway::{
    accounting::{Budget, Model, Outcome, Usage},
    billing::{Billing, SubscriptionLimits},
    pricing::Price,
    spend::SpendReader,
};
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Barrier},
    thread,
    time::{SystemTime, UNIX_EPOCH},
};
struct Home(PathBuf);
impl Home {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "oxc-accounting-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir(&path).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }
    fn ledger(&self) -> PathBuf {
        self.0.join("requests-synthetic.jsonl")
    }
}
impl Drop for Home {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn append(path: &Path, row: &Value) -> io::Result<()> {
    oxc_gateway::spend::append_private(path, row)
}

fn model() -> Model {
    Model {provider:"openrouter".into(),upstream:"synthetic/upstream".into(),context:1000,max_output:100,
    billing:oxc_gateway::billing::Billing::Metered(Price::from_model(&json!({"pricing":{"input_per_million":1,"output_per_million":2,"cache_read_per_million":0.1,"cache_write_per_million":1.25}})).unwrap())}
}

fn subscription() -> Model {
    Model {
        provider: "chatgpt".into(),
        upstream: "synthetic-subscription".into(),
        context: 100,
        max_output: 20,
        billing: Billing::Subscription,
    }
}

#[test]
fn subscription_reserves_full_window_and_settles_zero_usd_with_actual_token_counts() {
    let home = Home::new();
    let path = home.ledger();
    let limits = Some(SubscriptionLimits::new(10, 146).unwrap());
    let first = Budget::new(path.clone());
    let other = Budget::new(path.clone());
    let mut hold = first
        .reserve_subscription(limits, true, &subscription(), 20)
        .unwrap();
    assert_eq!(hold.reserved(), 0.0);
    let error = other
        .reserve_subscription(limits, true, &subscription(), 20)
        .err()
        .unwrap();
    assert_eq!((error.status, error.code), (429, "subscription_limit_exceeded"));
    hold.mark_forwarded().unwrap();
    let row = hold
        .finish(
            &Usage {
                prompt: Some(21),
                output: Some(5),
                cache_read: Some(7),
                ..Usage::default()
            },
            Outcome::Complete,
            |row| append(&path, row),
        )
        .unwrap();
    assert_eq!(row["cost_usd"], 0.0);
    assert_eq!(row["billing"], "subscription");
    assert_eq!(row["subscription_requests"], 1);
    assert_eq!(row["subscription_tokens"], 26);
    assert_eq!(row["settled"], "usage");
    assert_eq!(other.spend().unwrap(), 0.0);
    let mut next = other
        .reserve_subscription(limits, true, &subscription(), 20)
        .unwrap();
    next.mark_forwarded().unwrap();
    next.finish(
        &Usage {
            prompt: Some(21),
            output: Some(5),
            ..Usage::default()
        },
        Outcome::Complete,
        |row| append(&path, row),
    )
    .unwrap();
    let error = Budget::new(path)
        .reserve_subscription(limits, true, &subscription(), 20)
        .err()
        .unwrap();
    assert_eq!((error.status, error.code), (429, "subscription_limit_exceeded"));
}

#[test]
fn clean_subscription_refusals_release_tokens_but_still_consume_request_allowance() {
    let home = Home::new();
    let path = home.ledger();
    let limits = Some(SubscriptionLimits::new(3, 120).unwrap());
    for _ in 0..3 {
        let mut hold = Budget::new(path.clone())
            .reserve_subscription(limits, true, &subscription(), 20)
            .unwrap();
        hold.mark_forwarded().unwrap();
        let row = hold
            .finish(&Usage::default(), Outcome::Refused, |row| append(&path, row))
            .unwrap();
        assert_eq!(row["cost_usd"], 0.0);
        assert_eq!(row["subscription_requests"], 1);
        assert_eq!(row["subscription_tokens"], 0);
        assert_eq!(row["settled"], "refused");
    }
    let error = Budget::new(path)
        .reserve_subscription(limits, true, &subscription(), 20)
        .err()
        .unwrap();
    assert_eq!((error.status, error.code), (429, "subscription_limit_exceeded"));
}

#[test]
fn unknown_subscription_outcomes_never_refund_the_token_reservation() {
    for (usage, outcome) in [
        (Usage::default(), Outcome::Complete),
        (
            Usage::default(),
            Outcome::Cancelled {
                request_bytes: 0,
                streamed_bytes: 0,
                usage_final: false,
                elapsed: std::time::Duration::ZERO,
            },
        ),
        (
            Usage {
                prompt: Some(10),
                output: Some(5),
                ..Usage::default()
            },
            Outcome::Failed,
        ),
        (
            Usage {
                prompt: Some(10),
                output: Some(5),
                cache_read: Some(11),
                ..Usage::default()
            },
            Outcome::Complete,
        ),
        (
            Usage {
                prompt: Some(u64::MAX),
                output: Some(1),
                ..Usage::default()
            },
            Outcome::Complete,
        ),
    ] {
        let home = Home::new();
        let path = home.ledger();
        let limits = Some(SubscriptionLimits::new(10, 239).unwrap());
        let mut hold = Budget::new(path.clone())
            .reserve_subscription(limits, true, &subscription(), 20)
            .unwrap();
        hold.mark_forwarded().unwrap();
        let row = hold.finish(&usage, outcome, |row| append(&path, row)).unwrap();
        assert_eq!(row["subscription_tokens"], 120);
        assert_eq!(row["usage_missing"], true);
        assert_eq!(row["settled"], "reserved");
        let error = Budget::new(path)
            .reserve_subscription(limits, true, &subscription(), 20)
            .err()
            .unwrap();
        assert_eq!((error.status, error.code), (429, "subscription_limit_exceeded"));
    }
}

#[test]
fn subscription_requires_caps_and_cannot_make_an_api_provider_free() {
    let home = Home::new();
    let path = home.ledger();
    let budget = Budget::new(path.clone());
    let error = budget
        .reserve_subscription(None, true, &subscription(), 20)
        .err()
        .unwrap();
    assert_eq!(
        (error.status, error.code),
        (403, "subscription_limits_not_configured")
    );
    assert_eq!(fs::read_dir(&home.0).unwrap().count(), 0);
    let limits = Some(SubscriptionLimits::new(3, 1000).unwrap());
    let mut wrong = subscription();
    wrong.provider = "openai".into();
    for error in [
        budget
            .reserve_subscription(limits, true, &wrong, 20)
            .err()
            .unwrap(),
        budget
            .reserve_subscription(limits, true, &model(), 20)
            .err()
            .unwrap(),
        budget
            .reserve(&json!(1), true, &subscription(), 20)
            .err()
            .unwrap(),
    ] {
        assert_eq!((error.status, error.code), (400, "invalid_billing"));
    }
    assert!(!path.exists());
}

#[test]
fn subscription_audit_failure_closes_both_classes_across_independent_instances() {
    let home = Home::new();
    let path = home.ledger();
    let limits = Some(SubscriptionLimits::new(3, 1000).unwrap());
    let mut hold = Budget::new(path.clone())
        .reserve_subscription(limits, true, &subscription(), 20)
        .unwrap();
    hold.mark_forwarded().unwrap();
    assert!(hold
        .finish(&Usage::default(), Outcome::Failed, |_| Err(
            io::ErrorKind::Other.into()
        ))
        .is_err());
    let next = Budget::new(path);
    for error in [
        next.reserve_subscription(limits, true, &subscription(), 20)
            .err()
            .unwrap(),
        next.reserve(&json!(1), true, &model(), 20).err().unwrap(),
    ] {
        assert_eq!((error.status, error.code), (503, "cloud_ledger_unavailable"));
    }
}

#[test]
fn legacy_unscoped_subscription_usd_reserves_nothing_but_its_tokens_still_count() {
    const LEGACY: &str = "00000000000000000000000000000000";
    // A missing hold, a token-only hold, and holds whose recorded USD is far
    // over the entire daily cap in both legacy shapes: the pre-pool
    // `reserved_usd` form and the bounded-pool `go_usd` form.
    for legacy in [
        None,
        Some(json!({"subscription_tokens":120})),
        Some(json!({"subscription_tokens":120,"reserved_usd":400.0})),
        Some(json!({"subscription_tokens":120,"go_usd":400.0})),
    ] {
        let home = Home::new();
        let path = home.ledger();
        let snapshot = home.0.join("requests-synthetic.jsonl.holds");
        if let Some(legacy) = &legacy {
            let mut seeded = json!({"failed":false,"holds":{}});
            seeded["holds"][LEGACY] = legacy.clone();
            fs::write(&snapshot, seeded.to_string()).unwrap();
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&snapshot, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let budget = Budget::new(path.clone());
        let cap = json!(0.01);
        // The stored reservation USD contributes zero to the held total:
        // metered admission proceeds even though 400 exceeds the whole cap.
        let metered = model();
        let Billing::Metered(price) = &metered.billing else {
            unreachable!()
        };
        let expected = price
            .reservation("openrouter", "synthetic/upstream", metered.context, 100)
            .unwrap();
        let mut hold = budget.reserve(&cap, true, &metered, 100).unwrap();
        assert_eq!(hold.reserved(), expected);
        // Subscription admission also proceeds; no USD is held for it either.
        let subscription_hold = budget
            .reserve_subscription(
                Some(SubscriptionLimits::new(10, 1000).unwrap()),
                true,
                &subscription(),
                20,
            )
            .unwrap();
        let current: Value = serde_json::from_slice(&fs::read(&snapshot).unwrap()).unwrap();
        let holds = current["holds"].as_object().unwrap();
        assert_eq!(holds.len(), 2 + usize::from(legacy.is_some()));
        if let Some(legacy) = &legacy {
            assert_eq!(holds.get(LEGACY), Some(legacy));
        }
        // The new subscription hold is token-only: it records no USD at all.
        assert!(holds
            .values()
            .any(|value| *value == json!({"subscription_tokens":120})));
        subscription_hold
            .finish(&Usage::default(), Outcome::Complete, |row| append(&path, row))
            .unwrap();
        // The legacy hold's tokens still count toward the subscription caps.
        let tight = SubscriptionLimits::new(10, 130).unwrap();
        let admission = budget.reserve_subscription(Some(tight), true, &subscription(), 20);
        match &legacy {
            None => drop(admission.unwrap()),
            Some(_) => {
                let error = admission.err().unwrap();
                assert_eq!((error.status, error.code), (429, "subscription_limit_exceeded"));
            }
        }
        // Actual paid usage settles normally and the legacy USD never lands in
        // the daily total.
        hold.mark_forwarded().unwrap();
        let row = hold
            .finish(
                &Usage {
                    prompt: Some(20),
                    output: Some(10),
                    ..Usage::default()
                },
                Outcome::Complete,
                |row| append(&path, row),
            )
            .unwrap();
        assert_eq!(row["settled"], "usage");
        assert_eq!(row["cost_usd"].as_f64(), Some(0.00004));
        assert_eq!(budget.spend().unwrap(), 0.00004);
        // The stored record itself stays readable and untouched, and a fresh
        // instance keeps admitting while it is in flight.
        let stored: Value = serde_json::from_slice(&fs::read(&snapshot).unwrap()).unwrap();
        assert_eq!(stored["failed"], false);
        let holds = stored["holds"].as_object().unwrap();
        assert_eq!(holds.len(), usize::from(legacy.is_some()));
        if let Some(legacy) = &legacy {
            assert_eq!(holds.get(LEGACY), Some(legacy));
        }
        assert!(Budget::new(path).reserve(&cap, true, &metered, 100).is_ok());
    }
}

#[test]
fn unfinished_tail_is_not_a_corrupt_row_and_unchanged_reads_are_constant_work() {
    let home = Home::new();
    let path = home.ledger();
    fs::write(
        &path,
        b"{\"remote\":true,\"cost_usd\":0.1}\n{\"remote\":true,\"cost_usd\":",
    )
    .unwrap();
    let mut reader = SpendReader::default();
    assert_eq!(reader.read(&path).unwrap(), 0.1);
    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(b"0.2}\n").unwrap();
    assert_eq!(reader.read(&path).unwrap(), 0.3);
    let read = reader.bytes_read();
    for _ in 0..100 {
        assert_eq!(reader.read(&path).unwrap(), 0.3);
    }
    assert!(reader.bytes_read() - read <= 64 * 100);
    file.write_all(b"{\"remote\":true,\"cost_usd\":0.3}").unwrap();
    assert_eq!(reader.read(&path).unwrap(), 0.3);
    file.write_all(b"\n").unwrap();
    assert_eq!(reader.read(&path).unwrap(), 0.6);
    file.write_all(b"[\"complete-invalid-row\"]\n").unwrap();
    assert!(reader.read(&path).is_err());
}
#[test]
fn same_inode_rewrite_of_equal_or_larger_size_resets_the_running_total() {
    let home = Home::new();
    let path = home.ledger();
    append(&path, &json!({"remote":true,"cost_usd":0.1})).unwrap();
    let mut reader = SpendReader::default();
    assert_eq!(reader.read(&path).unwrap(), 0.1);
    fs::write(&path, b"{\"remote\":true,\"cost_usd\":0.9}\n").unwrap();
    assert_eq!(reader.read(&path).unwrap(), 0.9);
    fs::write(
        &path,
        b"{\"remote\":true,\"cost_usd\":0.8}\n{\"remote\":true,\"cost_usd\":0.1}\n",
    )
    .unwrap();
    assert_eq!(reader.read(&path).unwrap(), 0.9);
}
#[test]
fn replace_truncate_and_malformed_costs_fail_or_reset_correctly() {
    let home = Home::new();
    let path = home.ledger();
    let mut reader = SpendReader::default();
    append(&path, &json!({"remote":true,"cost_usd":0.25})).unwrap();
    assert_eq!(reader.read(&path).unwrap(), 0.25);
    fs::write(&path, b"\n").unwrap();
    assert_eq!(reader.read(&path).unwrap(), 0.0);
    let other = home.0.join("replacement");
    append(&other, &json!({"remote":true,"cost_usd":0.5})).unwrap();
    fs::rename(&other, &path).unwrap();
    assert_eq!(reader.read(&path).unwrap(), 0.5);
    for cost in [json!(true), json!(-1), Value::Null, json!("synthetic-invalid")] {
        let other = home.0.join("replacement");
        append(&other, &json!({"remote":true,"cost_usd":cost})).unwrap();
        fs::rename(&other, &path).unwrap();
        assert!(reader.read(&path).is_err());
    }
}
#[test]
fn readers_never_refuse_an_append_in_progress_and_count_each_row_once() {
    let home = Home::new();
    let path = home.ledger();
    fs::write(&path, b"").unwrap();
    let path = Arc::new(path);
    let barrier = Arc::new(Barrier::new(5));
    let mut jobs = Vec::new();
    for _ in 0..4 {
        let path = path.clone();
        let barrier = barrier.clone();
        jobs.push(thread::spawn(move || {
            let mut reader = SpendReader::default();
            barrier.wait();
            for _ in 0..2000 {
                let total = reader.read(&path).unwrap();
                assert!((0.0..=0.5).contains(&total));
                thread::yield_now();
            }
        }));
    }
    barrier.wait();
    let mut writer = OpenOptions::new().append(true).open(&*path).unwrap();
    for _ in 0..500 {
        writer.write_all(b"{\"remote\":true,").unwrap();
        thread::yield_now();
        writer.write_all(b"\"cost_usd\":0.001}\n").unwrap();
    }
    for job in jobs {
        job.join().unwrap();
    }
    assert_eq!(SpendReader::default().read(&path).unwrap(), 0.5);
}
#[test]
fn reservations_are_atomic_and_abandoned_unforwarded_holds_release() {
    let home = Home::new();
    let budget = Budget::new(home.ledger());
    let mut model = model();
    assert_eq!(
        budget.reserve(&json!(1), true, &model, 0).err().unwrap().code,
        "invalid_value"
    );
    model.billing = Billing::Metered(
        Price::from_model(&json!({"pricing":{"input_per_million":1,"output_per_million":2}})).unwrap(),
    );
    let barrier = Arc::new(Barrier::new(20));
    let mut jobs = Vec::new();
    for _ in 0..20 {
        let budget = budget.clone();
        let model = model.clone();
        let barrier = barrier.clone();
        jobs.push(thread::spawn(move || {
            barrier.wait();
            budget.reserve(&json!(0.003), true, &model, 100)
        }));
    }
    let holds = jobs
        .into_iter()
        .filter_map(|job| job.join().unwrap().ok())
        .collect::<Vec<_>>();
    assert_eq!(holds.len(), 2);
    assert_eq!(
        budget
            .reserve(&json!(0.003), true, &model, 100)
            .err()
            .unwrap()
            .code,
        "budget_exceeded"
    );
    drop(holds);
    assert!(budget.reserve(&json!(0.003), true, &model, 100).is_ok());
}
#[test]
fn settled_cache_tokens_and_reported_costs_become_remote_rows() {
    let home = Home::new();
    let path = home.ledger();
    let budget = Budget::new(path.clone());
    let mut hold = budget.reserve(&json!(1), true, &model(), 32).unwrap();
    hold.mark_forwarded().unwrap();
    let usage = Usage {
        prompt: Some(1000),
        output: Some(4),
        cache_read: Some(950),
        cache_write: Some(40),
        finish: Some("stop".into()),
        ..Usage::default()
    };
    let row = hold
        .finish(&usage, Outcome::Complete, |row| append(&path, row))
        .unwrap();
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(fs::metadata(&home.0).unwrap().permissions().mode() & 0o777, 0o700);
    assert_eq!(row["remote"], true);
    assert_eq!(row["settled"], "usage");
    assert_eq!(row["cost_usd"], 0.000163);
    assert_eq!(row["cache_write_tokens"], 40);
    assert_eq!(budget.spend().unwrap(), 0.000163);
    let mut hold = budget.reserve(&json!(1), true, &model(), 32).unwrap();
    hold.mark_forwarded().unwrap();
    let usage = Usage {
        reported_cost: Some(0.00001),
        ..usage
    };
    let row = hold
        .finish(&usage, Outcome::Complete, |row| append(&path, row))
        .unwrap();
    assert_eq!(row["catalog_cost_usd"], 0.000163);
    assert_eq!(row["cost_usd"], 0.00001);
    assert_eq!(budget.spend().unwrap(), 0.000173);
}
#[test]
fn missing_usage_charges_the_reservation_and_write_failures_block_further_spending() {
    let home = Home::new();
    let path = home.ledger();
    let budget = Budget::new(path.clone());
    let mut hold = budget.reserve(&json!(1), true, &model(), 32).unwrap();
    let reserved = hold.reserved();
    hold.mark_forwarded().unwrap();
    let row = hold
        .finish(&Usage::default(), Outcome::Complete, |row| append(&path, row))
        .unwrap();
    assert_eq!(row["cost_usd"], reserved);
    assert_eq!(row["settled"], "reserved");
    assert_eq!(row["usage_missing"], true);
    let mut hold = budget.reserve(&json!(1), true, &model(), 32).unwrap();
    hold.mark_forwarded().unwrap();
    assert!(hold
        .finish(&Usage::default(), Outcome::Failed, |_| Err(
            io::ErrorKind::PermissionDenied.into()
        ))
        .is_err());
    assert_eq!(
        budget.reserve(&json!(1), true, &model(), 32).err().unwrap().code,
        "cloud_ledger_unavailable"
    );
}
#[test]
fn dropping_a_potentially_paid_hold_without_recording_fails_closed() {
    let home = Home::new();
    let budget = Budget::new(home.ledger());
    let mut hold = budget.reserve(&json!(1), true, &model(), 32).unwrap();
    hold.mark_forwarded().unwrap();
    drop(hold);
    assert_eq!(
        budget.reserve(&json!(1), true, &model(), 32).err().unwrap().code,
        "cloud_ledger_unavailable"
    );
}
#[test]
fn cancelled_cache_writes_include_hidden_output_and_final_usage_stays_exact() {
    use std::time::Duration;
    let home = Home::new();
    let path = home.ledger();
    let budget = Budget::new(path.clone());
    let mut model = model();
    model.upstream = "anthropic/synthetic".into();
    model.context = 100_000;
    model.billing = Billing::Metered(Price::from_model(&json!({"pricing":{"input":1,"output":2}})).unwrap());
    let usage = Usage {
        prompt: Some(100_000),
        output: Some(0),
        cache_write: Some(100_000),
        ..Usage::default()
    };
    let mut hold = budget.reserve(&json!(1), true, &model, 32).unwrap();
    hold.mark_forwarded().unwrap();
    let cancel = Outcome::Cancelled {
        request_bytes: 100_000,
        streamed_bytes: 0,
        usage_final: false,
        elapsed: Duration::from_millis(100),
    };
    let row = hold.finish(&usage, cancel, |row| append(&path, row)).unwrap();
    assert_eq!(row["cost_usd"], 0.20006);
    assert_eq!(row["settled"], "estimated");
    let mut hold = budget.reserve(&json!(1), true, &model, 32).unwrap();
    hold.mark_forwarded().unwrap();
    let row = hold
        .finish(
            &usage,
            Outcome::Cancelled {
                request_bytes: 100_000,
                streamed_bytes: 0,
                usage_final: true,
                elapsed: Duration::from_millis(100),
            },
            |row| append(&path, row),
        )
        .unwrap();
    assert_eq!(row["cost_usd"], 0.2);
    assert_eq!(row["settled"], "usage");
    let mut hold = budget.reserve(&json!(1), true, &model, 32).unwrap();
    let reserved = hold.reserved();
    hold.mark_forwarded().unwrap();
    let row = hold
        .finish(
            &Usage::default(),
            Outcome::Cancelled {
                request_bytes: usize::MAX,
                streamed_bytes: 0,
                usage_final: false,
                elapsed: Duration::from_secs(1000),
            },
            |row| append(&path, row),
        )
        .unwrap();
    assert_eq!(row["cost_usd"], reserved);
    assert_eq!(row["settled"], "estimated");
}
#[test]
fn cancellation_never_caps_a_known_charge_at_the_reservation() {
    let home = Home::new();
    let path = home.ledger();
    let budget = Budget::new(path.clone());
    let mut hold = budget.reserve(&json!(1), true, &model(), 32).unwrap();
    assert!(hold.reserved() < 0.004);
    hold.mark_forwarded().unwrap();
    let usage = Usage {
        reported_cost: Some(0.004),
        ..Usage::default()
    };
    let row = hold
        .finish(
            &usage,
            Outcome::Cancelled {
                request_bytes: 0,
                streamed_bytes: 0,
                usage_final: false,
                elapsed: std::time::Duration::ZERO,
            },
            |row| append(&path, row),
        )
        .unwrap();
    assert_eq!(row["cost_usd"], 0.004);
    assert_eq!(budget.spend().unwrap(), 0.004);
}

fn settle_frames(frames: &[Value], outcome: Outcome) -> Value {
    let home = Home::new();
    let path = home.ledger();
    let budget = Budget::new(path.clone());
    let mut hold = budget.reserve(&json!(1), true, &model(), 32).unwrap();
    hold.mark_forwarded().unwrap();
    let mut usage = Usage::default();
    for frame in frames {
        usage.observe(frame);
    }
    let mut writes = 0;
    let row = hold
        .finish(&usage, outcome, |row| {
            writes += 1;
            append(&path, row)
        })
        .unwrap();
    assert_eq!(writes, 1);
    assert_eq!(fs::read_to_string(&path).unwrap().lines().count(), 1);
    assert_eq!(budget.spend().unwrap(), row["cost_usd"].as_f64().unwrap());
    row
}

#[test]
fn usage_frames_settle_once_and_missing_or_invalid_usage_reserves() {
    let full = json!({"usage": {"prompt_tokens": 11, "completion_tokens": 4},
        "choices": [{"finish_reason": "stop"}]});
    for (frames, outcome, expected, settled) in [
        (vec![full.clone()], Outcome::Complete, 0.000019, "usage"),
        (vec![], Outcome::Complete, 0.0016425, "reserved"),
        (vec![full.clone()], Outcome::Failed, 0.0016425, "reserved"),
        (
            vec![json!({"usage": {"prompt_tokens": 0, "completion_tokens": 0}})],
            Outcome::Complete,
            0.0,
            "usage",
        ),
        (
            vec![json!({"usage": {"prompt_tokens": true, "completion_tokens": 4}})],
            Outcome::Complete,
            0.0016425,
            "reserved",
        ),
        (
            vec![json!({"usage": {"prompt_tokens": 11}})],
            Outcome::Complete,
            0.0016425,
            "reserved",
        ),
    ] {
        let row = settle_frames(&frames, outcome);
        assert_eq!(row["cost_usd"].as_f64(), Some(expected));
        assert_eq!(row["settled"], settled);
        assert_eq!(row["usage_missing"] == true, settled == "reserved");
    }
    let row = settle_frames(
        &[json!({"usage": {"prompt_tokens": 1000, "completion_tokens": 4,
        "prompt_tokens_details": {"cached_tokens": 950, "cache_write_tokens": 40}}})],
        Outcome::Complete,
    );
    assert_eq!(row["cost_usd"].as_f64(), Some(0.000163));
    assert_eq!(row["cached_tokens"], 950);
    assert_eq!(row["cache_write_tokens"], 40);
    assert_eq!(row["settled"], "usage");
    for details in [
        json!({"cached_tokens": 999, "cache_write_tokens": 40}),
        json!({"cached_tokens": true, "cache_write_tokens": 40}),
        json!({"cached_tokens": null, "cache_write_tokens": null}),
        json!({"cached_tokens": 950, "cache_write_tokens": -1}),
        json!({"cached_tokens": 1.0, "cache_write_tokens": 40}),
    ] {
        let row = settle_frames(
            &[json!({"usage": {"prompt_tokens": 1000, "completion_tokens": 4,
            "prompt_tokens_details": details}})],
            Outcome::Complete,
        );
        assert_eq!(row["settled"], "reserved");
        assert_eq!(row["usage_missing"], true);
        assert_eq!(row["cost_usd"].as_f64(), Some(0.0016425));
    }
}

#[test]
fn reported_router_cost_is_authoritative_and_repeated_frames_keep_the_maximum() {
    let first = json!({"usage": {"prompt_tokens": 11, "completion_tokens": 4, "cost": 0.002},
        "choices": [{"finish_reason": "stop"}]});
    for frames in [
        vec![first.clone()],
        vec![first, json!({"usage": {"cost": 0.001}})],
    ] {
        let row = settle_frames(&frames, Outcome::Complete);
        assert_eq!(row["catalog_cost_usd"].as_f64(), Some(0.000019));
        assert_eq!(row["reported_cost_usd"].as_f64(), Some(0.002));
        assert_eq!(row["cost_usd"].as_f64(), Some(0.002));
        assert_eq!(row["settled"], "usage");
    }
}

#[test]
fn cancellation_uses_final_usage_or_a_bounded_estimate_including_hidden_output() {
    use std::time::Duration;
    let full = json!({"usage": {"prompt_tokens": 11, "completion_tokens": 4}});
    let partial = json!({"usage": {"prompt_tokens": 11, "completion_tokens": 0}});
    for (frames, request_bytes, streamed_bytes, usage_final, elapsed, expected, settled) in [
        (
            vec![full],
            100,
            4,
            true,
            Duration::from_secs(5),
            0.000019,
            "usage",
        ),
        (
            vec![partial],
            100,
            4,
            false,
            Duration::from_millis(10),
            0.00002175,
            "estimated",
        ),
        (
            vec![],
            100,
            4,
            false,
            Duration::from_millis(50),
            0.000155,
            "estimated",
        ),
        (
            vec![],
            100_000_001,
            4,
            false,
            Duration::from_secs(1000),
            0.0016425,
            "estimated",
        ),
    ] {
        let row = settle_frames(
            &frames,
            Outcome::Cancelled {
                request_bytes,
                streamed_bytes,
                usage_final,
                elapsed,
            },
        );
        assert_eq!(row["cost_usd"].as_f64(), Some(expected));
        assert_eq!(row["settled"], settled);
        assert_eq!(row["usage_missing"] == true, settled == "estimated");
    }
}

#[test]
fn forwarding_checks_other_instances_failure_latch() {
    let home = Home::new();
    let path = home.ledger();
    let first = Budget::new(path.clone());
    let other = Budget::new(path);
    let mut hold = first.reserve(&json!(1), true, &model(), 32).unwrap();
    let mut failed = other.reserve(&json!(1), true, &model(), 32).unwrap();
    failed.mark_forwarded().unwrap();
    assert!(failed
        .finish(&Usage::default(), Outcome::Failed, |_| {
            Err(io::ErrorKind::StorageFull.into())
        })
        .is_err());
    // A hold admitted before another instance failed must not start paid work.
    assert_eq!(
        hold.mark_forwarded().unwrap_err().code,
        "cloud_ledger_unavailable"
    );
}

#[test]
fn forwarding_persistence_failure_is_returned_before_paid_bytes_can_be_sent() {
    let home = Home::new();
    let path = home.ledger();
    let budget = Budget::new(path.clone());
    let mut hold = budget.reserve(&json!(1), true, &model(), 32).unwrap();
    let holds = path.with_file_name("requests-synthetic.jsonl.holds");
    let snapshot = fs::read(&holds).unwrap();
    fs::write(&holds, "invalid").unwrap();
    assert!(hold.mark_forwarded().is_err());
    fs::write(&holds, snapshot).unwrap();
    // The failed budget stays latched; a retry must not silently permit sending.
    assert!(hold.mark_forwarded().is_err());
    drop(hold);
    assert!(Budget::new(path).reserve(&json!(1), true, &model(), 32).is_ok());
}

#[test]
fn metered_request_without_budget_is_admitted_and_recorded() {
    let home = Home::new();
    let path = home.ledger();
    let budget = Budget::new(path.clone());
    let mut hold = budget.reserve(&json!(null), true, &model(), 32).unwrap();
    hold.mark_forwarded().unwrap();
    let usage = Usage {
        prompt: Some(1000),
        output: Some(4),
        cache_read: Some(950),
        cache_write: Some(40),
        finish: Some("stop".into()),
        ..Usage::default()
    };
    let row = hold
        .finish(&usage, Outcome::Complete, |row| append(&path, row))
        .unwrap();
    assert_eq!(row["settled"], "usage");
    assert_eq!(row["cost_usd"], 0.000163);
    assert!(row.get("cost_unknown").is_none());
    assert_eq!(budget.spend().unwrap(), 0.000163);
    // Spend never turns an unset cap into a limit.
    assert!(budget.reserve(&json!(null), true, &model(), 32).is_ok());
}

#[test]
fn configured_budget_still_refuses_overage() {
    let home = Home::new();
    let path = home.ledger();
    let budget = Budget::new(path.clone());
    let mut hold = budget.reserve(&json!(0.002), true, &model(), 32).unwrap();
    hold.mark_forwarded().unwrap();
    hold.finish(
        &Usage {
            prompt: Some(1000),
            output: Some(100),
            ..Usage::default()
        },
        Outcome::Complete,
        |row| append(&path, row),
    )
    .unwrap();
    assert_eq!(
        budget
            .reserve(&json!(0.002), true, &model(), 32)
            .err()
            .map(|error| (error.status, error.code)),
        Some((429, "budget_exceeded"))
    );
    assert_eq!(
        budget
            .reserve(&json!(0), true, &model(), 32)
            .err()
            .map(|error| (error.status, error.code)),
        Some((403, "budget_not_configured"))
    );
}

#[test]
fn unknown_price_without_budget_is_admitted() {
    let home = Home::new();
    let path = home.ledger();
    let budget = Budget::new(path.clone());
    // A context beyond the reservation bound has no computable price ceiling.
    let mut unknown = model();
    unknown.context = 200_000_000;
    assert_eq!(
        budget
            .reserve(&json!(1), true, &unknown, 32)
            .err()
            .map(|error| error.code),
        Some("budget_exceeded")
    );
    let mut hold = budget.reserve(&json!(null), true, &unknown, 32).unwrap();
    assert_eq!(hold.reserved(), 0.0);
    hold.mark_forwarded().unwrap();
    let row = hold
        .finish(&Usage::default(), Outcome::Complete, |row| append(&path, row))
        .unwrap();
    assert_eq!(row["cost_unknown"], true);
    assert_eq!(row["usage_missing"], true);
    assert_eq!(row["cost_usd"], 0.0);
}

#[test]
fn pending_unknown_cost_hold_closes_later_capped_admission() {
    let home = Home::new();
    let path = home.ledger();
    let mut unknown = model();
    unknown.context = 200_000_000;
    let budget = Budget::new(path.clone());
    // Admitted uncapped with no computable bound, and still in flight.
    let hold = budget.reserve(&json!(null), true, &unknown, 32).unwrap();
    // The same daily ledger reopened (as after a restart) with a cap configured: the paid
    // request's cost is committed but not established, so capped admission fails closed.
    let reopened = Budget::new(path.clone());
    assert_eq!(
        reopened
            .reserve(&json!(1), true, &model(), 32)
            .err()
            .map(|error| (error.status, error.code)),
        Some((429, "budget_exceeded"))
    );
    // Uncapped requests keep working, before and after the pending hold is released.
    assert!(reopened.reserve(&json!(null), true, &model(), 32).is_ok());
    drop(hold);
    assert!(Budget::new(path)
        .reserve(&json!(null), true, &model(), 32)
        .is_ok());
}

#[test]
fn settled_usage_with_a_computed_cost_does_not_close_a_later_cap() {
    let home = Home::new();
    let path = home.ledger();
    let mut unknown = model();
    unknown.context = 200_000_000;
    let budget = Budget::new(path.clone());
    let mut hold = budget.reserve(&json!(null), true, &unknown, 32).unwrap();
    hold.mark_forwarded().unwrap();
    let row = hold
        .finish(
            &Usage {
                prompt: Some(1_000_000),
                output: Some(4),
                ..Usage::default()
            },
            Outcome::Complete,
            |row| append(&path, row),
        )
        .unwrap();
    // Settlement priced the usage, so the row's cost is known even though the admission
    // bound was not; a marker here would close a later cap forever.
    assert!(row.get("cost_unknown").is_none());
    assert_eq!(row["settled"], "usage");
    let cost = row["cost_usd"].as_f64().unwrap();
    assert!((cost - 1.000_008).abs() < 1e-9, "{cost}");
    let reopened = Budget::new(path.clone());
    assert!(reopened.reserve(&json!(10), true, &model(), 32).is_ok());
    assert_eq!(
        reopened
            .reserve(&json!(1), true, &model(), 32)
            .err()
            .map(|error| (error.status, error.code)),
        Some((429, "budget_exceeded"))
    );
}

#[test]
fn refused_uncapped_request_does_not_close_a_later_cap() {
    let home = Home::new();
    let path = home.ledger();
    let mut unknown = model();
    unknown.context = 200_000_000;
    let budget = Budget::new(path.clone());
    let mut hold = budget.reserve(&json!(null), true, &unknown, 32).unwrap();
    hold.mark_forwarded().unwrap();
    let row = hold
        .finish(&Usage::default(), Outcome::Refused, |row| append(&path, row))
        .unwrap();
    assert_eq!(row["settled"], "refused");
    assert_eq!(row["cost_usd"], 0.0);
    assert!(row.get("cost_unknown").is_none());
    let reopened = Budget::new(path.clone());
    assert!(reopened.reserve(&json!(1), true, &model(), 32).is_ok());
}

#[test]
fn cancelled_unpriced_request_keeps_cost_unknown() {
    let home = Home::new();
    let path = home.ledger();
    let mut unknown = model();
    unknown.context = 200_000_000;
    let budget = Budget::new(path.clone());
    let mut hold = budget.reserve(&json!(null), true, &unknown, 32).unwrap();
    hold.mark_forwarded().unwrap();
    let row = hold
        .finish(
            &Usage::default(),
            Outcome::Cancelled {
                request_bytes: 0,
                streamed_bytes: 0,
                usage_final: false,
                elapsed: std::time::Duration::ZERO,
            },
            |row| append(&path, row),
        )
        .unwrap();
    assert_eq!(row["cost_unknown"], true);
    let reopened = Budget::new(path.clone());
    assert_eq!(
        reopened
            .reserve(&json!(1), true, &model(), 32)
            .err()
            .map(|error| (error.status, error.code)),
        Some((429, "budget_exceeded"))
    );
}

#[test]
fn unknown_cost_row_closes_later_capped_admission() {
    let home = Home::new();
    let path = home.ledger();
    let mut unknown = model();
    unknown.context = 200_000_000;
    let budget = Budget::new(path.clone());
    let mut hold = budget.reserve(&json!(null), true, &unknown, 32).unwrap();
    hold.mark_forwarded().unwrap();
    hold.finish(&Usage::default(), Outcome::Complete, |row| append(&path, row))
        .unwrap();
    // The same daily ledger, reopened (as after a restart) with a cap configured.
    let reopened = Budget::new(path.clone());
    assert_eq!(
        reopened
            .reserve(&json!(1), true, &model(), 32)
            .err()
            .map(|error| (error.status, error.code)),
        Some((429, "budget_exceeded"))
    );
    // Uncapped requests keep working.
    assert!(reopened.reserve(&json!(null), true, &model(), 32).is_ok());
}

#[test]
fn budget_reserve_with_null_cap_still_refuses_when_ledger_disabled() {
    let home = Home::new();
    let budget = Budget::new(home.ledger());
    assert_eq!(
        budget
            .reserve(&json!(null), false, &model(), 32)
            .err()
            .map(|error| (error.status, error.code)),
        Some((503, "cloud_ledger_unavailable"))
    );
}
