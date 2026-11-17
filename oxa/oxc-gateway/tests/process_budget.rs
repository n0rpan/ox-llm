#![cfg(feature = "accounting")]
use oxc_gateway::{
    accounting::{Budget, Model, Outcome, Usage},
    billing::{Billing, SubscriptionLimits},
    pricing::Price,
    spend::append_private,
};
use serde_json::{json, Value};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::fs::{symlink, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver},
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

struct Home(PathBuf);
impl Home {
    fn new() -> Self {
        Self::at_timestamp(SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos())
    }
    fn at_timestamp(nanos: u128) -> Self {
        static NEXT_HOME: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "oxc-process-budget-{}-{}-{}",
            std::process::id(),
            nanos,
            NEXT_HOME.fetch_add(1, Ordering::Relaxed)
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
#[test]
fn homes_with_the_same_timestamp_are_independent() {
    // Wall-clock reads may repeat, including across concurrently running tests.
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let first = Home::at_timestamp(nanos);
    let second = Home::at_timestamp(nanos);
    assert_ne!(first.0, second.0);
    for home in [&first, &second] {
        assert_eq!(fs::metadata(&home.0).unwrap().permissions().mode() & 0o777, 0o700);
    }
    fs::write(first.path(), b"first").unwrap();
    assert!(!second.path().exists());
    drop(first);
    assert!(second.0.is_dir());
}

fn model() -> Model {
    Model {
        provider: "openrouter".into(),
        upstream: "synthetic".into(),
        context: 1000,
        max_output: 100,
        billing: oxc_gateway::billing::Billing::Metered(
            Price::from_model(&json!({"pricing":{"input":1,"output":2}})).unwrap(),
        ),
    }
}
struct Worker {
    child: Child,
    events: Receiver<String>,
    reader: Option<thread::JoinHandle<()>>,
}
impl Worker {
    fn new(path: &Path, cap: f64) -> Self {
        Self::spawn(Command::new(std::env::current_exe().unwrap()), path, cap, None)
    }
    fn subscription(path: &Path, limits: SubscriptionLimits) -> Self {
        Self::spawn(
            Command::new(std::env::current_exe().unwrap()),
            path,
            0.0,
            Some(limits),
        )
    }
    #[cfg(target_os = "linux")]
    fn traced(path: &Path, cap: f64, trace: &Path, inject: bool) -> Self {
        let mut command = Command::new("strace");
        command
            .args([
                "-f",
                "-qq",
                "-yy",
                "--kill-on-exit",
                "-e",
                "trace=fsync,fdatasync,ftruncate",
                "-o",
            ])
            .arg(trace);
        if inject {
            command.args(["-e", "inject=fsync:error=EIO:when=1"]);
        }
        command.arg(std::env::current_exe().unwrap());
        Self::spawn(command, path, cap, None)
    }
    fn spawn(mut command: Command, path: &Path, cap: f64, limits: Option<SubscriptionLimits>) -> Self {
        let mut child = command
            .args([
                "--exact",
                "processes_share_holds_and_settled_spend",
                "--nocapture",
            ])
            .env_clear()
            .env("HOME", path.parent().unwrap())
            .env("OXC_BUDGET_TEST_PATH", path)
            .env("OXC_BUDGET_TEST_CAP", cap.to_string())
            .env(
                "OXC_SUBSCRIPTION_TEST_LIMITS",
                limits
                    .map(|l| format!("{},{}", l.requests(), l.tokens()))
                    .unwrap_or_default(),
            )
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
        drop(self.child.stdin.take());
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
    let limits = std::env::var("OXC_SUBSCRIPTION_TEST_LIMITS")
        .ok()
        .filter(|s| !s.is_empty())
        .map(|s| {
            let (requests, tokens) = s.split_once(',').unwrap();
            SubscriptionLimits::new(requests.parse().unwrap(), tokens.parse().unwrap()).unwrap()
        });
    let mut route = model();
    let admitted = if limits.is_some() {
        route.provider = "chatgpt".into();
        route.billing = Billing::Subscription;
        budget.reserve_subscription(limits, true, &route, 100)
    } else {
        budget.reserve(&json!(cap), true, &route, 100)
    };
    let mut hold = match admitted {
        Ok(hold) => {
            signal("reserved");
            hold
        }
        Err(error) => {
            // Keep this subprocess protocol finite; never forward arbitrary
            // refusal fields into captured child output.
            match error.code {
                "budget_exceeded" => signal("budget_exceeded"),
                "subscription_limit_exceeded" => signal("subscription_limit_exceeded"),
                "cloud_ledger_unavailable" => signal("cloud_ledger_unavailable"),
                _ => panic!("unexpected admission refusal"),
            }
            return;
        }
    };
    let mut command = input.next().unwrap().unwrap();
    if command == "forward" {
        hold.mark_forwarded();
        signal("forwarded");
        command = input.next().unwrap().unwrap();
    }
    match command.as_str() {
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
        "partial-fail" => {
            hold.mark_forwarded();
            assert!(hold
                .finish(&Usage::default(), Outcome::Failed, |_| {
                    let mut file = fs::OpenOptions::new()
                        .append(true)
                        .create(true)
                        .mode(0o600)
                        .open(&path)?;
                    file.write_all(b"{\"remote\":true,")?;
                    Err(std::io::ErrorKind::Other.into())
                })
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
fn subscription_processes_share_token_holds_actual_usage_and_request_counts() {
    let home = Home::new();
    let limits = SubscriptionLimits::new(3, 2300).unwrap();
    let mut first = Worker::subscription(&home.path(), limits);
    let mut second = Worker::subscription(&home.path(), limits);
    assert_eq!(first.reserve(), "reserved");
    assert_eq!(second.reserve(), "reserved");
    let mut blocked = Worker::subscription(&home.path(), limits);
    assert_eq!(blocked.reserve(), "subscription_limit_exceeded");
    first.send("settle");
    assert_eq!(first.read(), "done");
    let mut third = Worker::subscription(&home.path(), limits);
    assert_eq!(
        third.reserve(),
        "reserved",
        "settled actual tokens replace the full-window hold"
    );
    second.send("settle");
    assert_eq!(second.read(), "done");
    let mut request_blocked = Worker::subscription(&home.path(), limits);
    assert_eq!(request_blocked.reserve(), "subscription_limit_exceeded");
    third.send("release");
    assert_eq!(third.read(), "done");
    let rows: Vec<Value> = fs::read_to_string(home.path())
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert_eq!(rows.len(), 2);
    for row in rows {
        assert_eq!(row["billing"], "subscription");
        assert_eq!(row["cost_usd"], 0.0);
        assert_eq!(row["subscription_requests"], 1);
        assert_eq!(row["subscription_tokens"], 100);
    }
}

#[test]
fn crashed_subscription_processes_keep_unknown_token_holds() {
    for command in ["crash", "commit-then-crash"] {
        let home = Home::new();
        let limits = SubscriptionLimits::new(10, 1500).unwrap();
        let mut first = Worker::subscription(&home.path(), limits);
        assert_eq!(first.reserve(), "reserved");
        first.send(command);
        assert_eq!(first.child.wait().unwrap().code(), Some(23));
        let mut next = Worker::subscription(&home.path(), limits);
        assert_eq!(next.reserve(), "subscription_limit_exceeded");
    }
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
    let mut oversized = fs::read(&snapshot).unwrap();
    oversized.resize(2 * 1024 * 1024 + 1, b' ');
    assert!(serde_json::from_slice::<Value>(&oversized).is_ok());
    fs::write(snapshot, oversized).unwrap();
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
        b"{\"failed\":false,\"holds\":{\"00000000000000000000000000000000\":-1}}",
        b"{\"failed\":false,\"holds\":{\"00000000000000000000000000000000\":1e400}}",
        b"{\"failed\":false,\"holds\":{\"bad\":1}}",
        b"{\"failed\":null,\"holds\":{}}",
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
        let valid = b"{\"failed\":false,\"holds\":{}}";
        fs::write(&other, valid).unwrap();
        fs::set_permissions(&other, fs::Permissions::from_mode(0o600)).unwrap();
        symlink(&other, &path).unwrap();
        assert!(Budget::new(home.path())
            .reserve(&json!(1), true, &model(), 100)
            .is_err());
        fs::remove_file(&path).unwrap();
        nix::unistd::mkfifo(
            &path,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .unwrap();
        assert!(Budget::new(home.path())
            .reserve(&json!(1), true, &model(), 100)
            .is_err());
        assert_eq!(fs::read(&other).unwrap(), valid);
        fs::remove_file(&path).unwrap();
        fs::write(&path, valid).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(Budget::new(home.path())
            .reserve(&json!(1), true, &model(), 100)
            .is_err());
    }
}

#[test]
fn paid_inflight_requests_settle_after_another_process_disables_admission() {
    for failure in ["fail", "drop", "partial-fail"] {
        let home = Home::new();
        let mut failing = Worker::new(&home.path(), 1.0);
        let mut healthy = Worker::new(&home.path(), 1.0);
        assert_eq!(failing.reserve(), "reserved");
        assert_eq!(healthy.reserve(), "reserved");
        healthy.send("forward");
        assert_eq!(healthy.read(), "forwarded");
        failing.send(failure);
        assert_eq!(failing.read(), "done");
        healthy.send("settle");
        assert_eq!(healthy.read(), "done");
        let text = fs::read_to_string(home.path()).unwrap();
        let mut rows = text.lines();
        if failure == "partial-fail" {
            assert_eq!(rows.next(), Some("{\"remote\":true,"));
        }
        let row: Value = serde_json::from_str(rows.next().unwrap()).unwrap();
        assert_eq!(row["cost_usd"], 0.0001);
        assert!(rows.next().is_none());
        let snapshot: Value =
            serde_json::from_slice(&fs::read(home.0.join("requests.jsonl.holds")).unwrap()).unwrap();
        assert_eq!(snapshot["failed"], true);
        assert_eq!(snapshot["holds"].as_object().unwrap().len(), 1);
        let mut next = Worker::new(&home.path(), 1.0);
        assert_eq!(next.reserve(), "cloud_ledger_unavailable");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn durable_directory_names_precede_release_and_failed_initialization_stays_closed() {
    let home = Home::new();
    let trace = home.0.join("syscalls");
    let mut worker = Worker::traced(&home.path(), 1.0, &trace, false);
    assert_eq!(worker.reserve(), "reserved");
    worker.send("settle");
    assert_eq!(worker.read(), "done");
    assert!(worker.child.wait().unwrap().success());
    let text = fs::read_to_string(trace).unwrap();
    let lines: Vec<_> = text.lines().collect();
    let ledger = format!("<{}>", home.path().display());
    let holds = format!("<{}.holds>", home.path().display());
    let directory = format!("<{}>", home.0.display());
    let data = lines
        .iter()
        .position(|line| line.contains("fdatasync(") && line.contains(&ledger) && line.ends_with("= 0"))
        .expect("ledger data must be synced");
    let release = lines
        .iter()
        .enumerate()
        .skip(data + 1)
        .find(|(_, line)| line.contains("ftruncate(") && line.contains(&holds))
        .unwrap()
        .0;
    assert!(
        lines[data + 1..release]
            .iter()
            .any(|line| line.contains("fsync(") && line.contains(&directory) && line.ends_with("= 0")),
        "ledger directory must be durable before its hold is removed"
    );

    let home = Home::new();
    let trace = home.0.join("sync-error");
    let mut first = Worker::traced(&home.path(), 1.0, &trace, true);
    assert_eq!(first.reserve(), "cloud_ledger_unavailable");
    assert!(first.child.wait().unwrap().success());
    assert!(fs::read_to_string(trace).unwrap().contains("INJECTED"));
    let mut next = Worker::new(&home.path(), 1.0);
    assert_eq!(
        next.reserve(),
        "cloud_ledger_unavailable",
        "another process cannot trust a failed initialization"
    );
}
