#![cfg(feature = "accounting")]
use oxc_gateway::{
    accounting::{Budget, Model, Outcome, Usage},
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
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&path).unwrap();
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
    price:Price::from_model(&json!({"pricing":{"input_per_million":1,"output_per_million":2,"cache_read_per_million":0.1,"cache_write_per_million":1.25}})).unwrap()}
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
    file.write_all(b"{\"remote\":true,\"cost_usd\":0.3}")
        .unwrap();
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
    for cost in [
        json!(true),
        json!(-1),
        Value::Null,
        json!("synthetic-invalid"),
    ] {
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
        budget
            .reserve(&json!(1), true, &model, 0)
            .err()
            .unwrap()
            .code,
        "invalid_value"
    );
    model.price =
        Price::from_model(&json!({"pricing":{"input_per_million":1,"output_per_million":2}}))
            .unwrap();
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
    hold.mark_forwarded();
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
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(&home.0).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(row["remote"], true);
    assert_eq!(row["settled"], "usage");
    assert_eq!(row["cost_usd"], 0.000163);
    assert_eq!(row["cache_write_tokens"], 40);
    assert_eq!(budget.spend().unwrap(), 0.000163);
    let mut hold = budget.reserve(&json!(1), true, &model(), 32).unwrap();
    hold.mark_forwarded();
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
    hold.mark_forwarded();
    let row = hold
        .finish(&Usage::default(), Outcome::Complete, |row| {
            append(&path, row)
        })
        .unwrap();
    assert_eq!(row["cost_usd"], reserved);
    assert_eq!(row["settled"], "reserved");
    assert_eq!(row["usage_missing"], true);
    let mut hold = budget.reserve(&json!(1), true, &model(), 32).unwrap();
    hold.mark_forwarded();
    assert!(hold
        .finish(&Usage::default(), Outcome::Failed, |_| Err(
            io::ErrorKind::PermissionDenied.into()
        ))
        .is_err());
    assert_eq!(
        budget
            .reserve(&json!(1), true, &model(), 32)
            .err()
            .unwrap()
            .code,
        "cloud_ledger_unavailable"
    );
}
#[test]
fn dropping_a_potentially_paid_hold_without_recording_fails_closed() {
    let home = Home::new();
    let budget = Budget::new(home.ledger());
    let mut hold = budget.reserve(&json!(1), true, &model(), 32).unwrap();
    hold.mark_forwarded();
    drop(hold);
    assert_eq!(
        budget
            .reserve(&json!(1), true, &model(), 32)
            .err()
            .unwrap()
            .code,
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
    model.price = Price::from_model(&json!({"pricing":{"input":1,"output":2}})).unwrap();
    let usage = Usage {
        prompt: Some(100_000),
        output: Some(0),
        cache_write: Some(100_000),
        ..Usage::default()
    };
    let mut hold = budget.reserve(&json!(1), true, &model, 32).unwrap();
    hold.mark_forwarded();
    let cancel = Outcome::Cancelled {
        request_bytes: 100_000,
        streamed_bytes: 0,
        usage_final: false,
        elapsed: Duration::from_millis(100),
    };
    let row = hold
        .finish(&usage, cancel, |row| append(&path, row))
        .unwrap();
    assert_eq!(row["cost_usd"], 0.20006);
    assert_eq!(row["settled"], "estimated");
    let mut hold = budget.reserve(&json!(1), true, &model, 32).unwrap();
    hold.mark_forwarded();
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
    hold.mark_forwarded();
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
    hold.mark_forwarded();
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
