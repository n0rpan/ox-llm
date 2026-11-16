#![cfg(feature = "accounting")]
use oxc_gateway::{
    accounting::{Budget, Model, Outcome, Usage},
    pricing::Price,
    spend::append_private,
};
use serde_json::{json, Value};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::fs::{symlink, PermissionsExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

struct Home(PathBuf);
impl Home {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "oxc-process-budget-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }
    fn path(&self) -> PathBuf {
        self.0.join("requests.jsonl")
    }
}
impl Drop for Home {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn model() -> Model {
    Model {
        provider: "openrouter".into(),
        upstream: "synthetic".into(),
        context: 1000,
        max_output: 100,
        price: Price::from_model(&json!({"pricing":{"input":1,"output":2}})).unwrap(),
    }
}
struct Worker {
    child: Child,
    events: Receiver<String>,
    reader: Option<thread::JoinHandle<()>>,
}
impl Worker {
    fn new(path: &Path, cap: f64) -> Self {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "processes_share_holds_and_settled_spend",
                "--nocapture",
            ])
            .env_clear()
            .env("HOME", path.parent().unwrap())
            .env("OXC_BUDGET_TEST_PATH", path)
            .env("OXC_BUDGET_TEST_CAP", cap.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (send, events) = mpsc::channel();
        let reader = thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some((_, event)) = line.split_once("budget-test:") {
                    if send.send(event.into()).is_err() {
                        break;
                    }
                }
            }
        });
        let worker = Self {
            child,
            events,
            reader: Some(reader),
        };
        assert_eq!(worker.read(), "ready");
        worker
    }
    fn send(&mut self, command: &str) {
        writeln!(self.child.stdin.as_mut().unwrap(), "{command}").unwrap();
    }
    fn read(&self) -> String {
        self.events.recv_timeout(Duration::from_secs(10)).unwrap()
    }
    fn reserve(&mut self) -> String {
        self.send("go");
        self.read()
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.reader.take().unwrap().join().unwrap();
    }
}
fn signal(message: &str) {
    println!("budget-test:{message}");
    std::io::stdout().flush().unwrap();
}
fn worker(path: PathBuf) {
    let cap: f64 = std::env::var("OXC_BUDGET_TEST_CAP").unwrap().parse().unwrap();
    let mut input = std::io::stdin().lock().lines();
    signal("ready");
    assert_eq!(input.next().unwrap().unwrap(), "go");
    let budget = Budget::new(path.clone());
    let mut hold = match budget.reserve(&json!(cap), true, &model(), 100) {
        Ok(hold) => {
            signal("reserved");
            hold
        }
        Err(error) => {
            signal(error.code);
            return;
        }
    };
    match input.next().unwrap().unwrap().as_str() {
        "release" => drop(hold),
        "settle" => {
            hold.mark_forwarded();
            let usage = Usage {
                prompt: Some(100),
                output: Some(0),
                ..Usage::default()
            };
            hold.finish(&usage, Outcome::Complete, |row| append_private(&path, row))
                .unwrap();
        }
        "fail" => {
            hold.mark_forwarded();
            assert!(hold
                .finish(&Usage::default(), Outcome::Failed, |_| Err(
                    std::io::ErrorKind::Other.into()
                ))
                .is_err());
        }
        "drop" => {
            hold.mark_forwarded();
            drop(hold);
        }
        "commit-then-crash" => {
            hold.mark_forwarded();
            hold.finish(&Usage::default(), Outcome::Complete, |row| {
                append_private(&path, row)?;
                std::process::exit(23);
            })
            .unwrap();
        }
        "crash" => {
            hold.mark_forwarded();
            std::process::exit(23);
        }
        _ => panic!("unknown worker command"),
    }
    signal("done");
}

#[test]
fn processes_share_holds_and_settled_spend() {
    if let Some(path) = std::env::var_os("OXC_BUDGET_TEST_PATH") {
        worker(path.into());
        return;
    }
    let home = Home::new();
    let mut workers: Vec<_> = (0..8).map(|_| Worker::new(&home.path(), 0.003)).collect();
    for worker in &mut workers {
        worker.send("go");
    }
    let admitted: Vec<_> = workers
        .iter()
        .enumerate()
        .filter_map(|(index, worker)| match worker.read().as_str() {
            "reserved" => Some(index),
            "budget_exceeded" => None,
            other => panic!("unexpected admission: {other}"),
        })
        .collect();
    assert_eq!(admitted.len(), 2, "all processes must share the same cap");
    for index in admitted {
        workers[index].send("release");
        assert_eq!(workers[index].read(), "done");
    }
    let mut first = Worker::new(&home.path(), 0.0017);
    assert_eq!(first.reserve(), "reserved");
    let mut second = Worker::new(&home.path(), 0.0017);
    assert_eq!(second.reserve(), "budget_exceeded");
    first.send("settle");
    assert_eq!(first.read(), "done");
    let rows: Vec<Value> = fs::read_to_string(home.path())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["cost_usd"], 0.0001);
    let mut third = Worker::new(&home.path(), 0.0017);
    assert_eq!(
        third.reserve(),
        "reserved",
        "only actual paid usage remains reserved"
    );
    third.send("release");
    assert_eq!(third.read(), "done");
    let mut fourth = Worker::new(&home.path(), 0.00155);
    assert_eq!(
        fourth.reserve(),
        "budget_exceeded",
        "settled spend survives a new process"
    );
}

#[test]
fn a_crashed_process_does_not_release_an_unknown_paid_hold() {
    for command in ["crash", "commit-then-crash"] {
        let home = Home::new();
        let mut first = Worker::new(&home.path(), 0.0017);
        assert_eq!(first.reserve(), "reserved");
        first.send(command);
        assert_eq!(first.child.wait().unwrap().code(), Some(23));
        let mut next = Worker::new(&home.path(), 0.0017);
        assert_eq!(next.reserve(), "budget_exceeded");
    }
}

#[test]
fn many_processes_settle_complete_rows_without_losing_holds_or_charges() {
    let home = Home::new();
    let mut workers: Vec<_> = (0..8).map(|_| Worker::new(&home.path(), 1.0)).collect();
    for worker in &mut workers {
        assert_eq!(worker.reserve(), "reserved");
    }
    for worker in &mut workers {
        worker.send("settle");
    }
    for worker in &workers {
        assert_eq!(worker.read(), "done");
    }
    let text = fs::read_to_string(home.path()).unwrap();
    assert_eq!(text.lines().count(), 8);
    assert!(text
        .lines()
        .all(|line| serde_json::from_str::<Value>(line).unwrap()["cost_usd"] == 0.0001));
    assert_eq!(Budget::new(home.path()).spend().unwrap(), 0.0008);
}

#[test]
fn persisted_state_is_private_bounded_metadata_and_new_days_are_independent() {
    let home = Home::new();
    let budget = Budget::new(home.path());
    assert_eq!(budget.spend().unwrap(), 0.0);
    let hold = budget.reserve(&json!(0.0017), true, &model(), 100).unwrap();
    for suffix in [".holds", ".budget-lock"] {
        let path = home.0.join(format!("requests.jsonl{suffix}"));
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }
    let snapshot = home.0.join("requests.jsonl.holds");
    let value: Value = serde_json::from_slice(&fs::read(&snapshot).unwrap()).unwrap();
    assert_eq!(value.as_object().unwrap().len(), 2);
    assert_eq!(value["failed"], false);
    let holds = value["holds"].as_object().unwrap();
    assert_eq!(holds.len(), 1);
    assert_eq!(holds.keys().next().unwrap().len(), 32);
    assert_eq!(holds.values().next().unwrap(), hold.reserved());
    assert!(Budget::new(home.path())
        .reserve(&json!(0.0017), true, &model(), 100)
        .is_err());
    assert!(Budget::new(home.0.join("next-day.jsonl"))
        .reserve(&json!(0.0017), true, &model(), 100)
        .is_ok());
    drop(hold);
    assert!(budget.reserve(&json!(0.0017), true, &model(), 100).is_ok());
    fs::OpenOptions::new()
        .write(true)
        .open(snapshot)
        .unwrap()
        .set_len(2 * 1024 * 1024 + 1)
        .unwrap();
    assert!(Budget::new(home.path())
        .reserve(&json!(1), true, &model(), 100)
        .is_err());
}

#[test]
fn failed_or_abandoned_paid_writes_block_other_processes() {
    for command in ["fail", "drop"] {
        let home = Home::new();
        let mut first = Worker::new(&home.path(), 1.0);
        assert_eq!(first.reserve(), "reserved");
        first.send(command);
        assert_eq!(first.read(), "done");
        let mut next = Worker::new(&home.path(), 1.0);
        assert_eq!(next.reserve(), "cloud_ledger_unavailable");
    }
}

#[test]
fn malformed_or_unsafe_persisted_state_fails_closed() {
    for bytes in [
        b"".as_slice(),
        b"{",
        b"{}",
        b"{\"failed\":false,\"holds\":{\"bad\":-1}}",
    ] {
        let home = Home::new();
        let path = home.0.join("requests.jsonl.holds");
        fs::write(&path, bytes).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            Budget::new(home.path())
                .reserve(&json!(1), true, &model(), 100)
                .err()
                .unwrap()
                .code,
            "cloud_ledger_unavailable"
        );
    }
    for suffix in [".holds", ".budget-lock"] {
        let home = Home::new();
        let path = home.0.join(format!("requests.jsonl{suffix}"));
        let other = home.0.join("other");
        fs::write(&other, "synthetic").unwrap();
        symlink(&other, &path).unwrap();
        assert!(Budget::new(home.path())
            .reserve(&json!(1), true, &model(), 100)
            .is_err());
        assert_eq!(fs::read_to_string(&other).unwrap(), "synthetic");
        fs::remove_file(&path).unwrap();
        fs::write(&path, b"{}").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(Budget::new(home.path())
            .reserve(&json!(1), true, &model(), 100)
            .is_err());
    }
}
