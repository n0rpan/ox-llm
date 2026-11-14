//! A harness-free test executable doubles as the REAL oxa RPC worker: when
//! invoked with --mode rpc it delegates to oxa_cli::run. Normal invocation runs
//! the integration suite. No installed binary, model or service is touched.
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
use oxa_orchestrator::*;
use std::io::{BufRead, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn settings(env: &TestEnv, name: &str) -> RunSettings {
    RunSettings {
        id: name.into(),
        worker_binary: std::env::current_exe().unwrap(),
        bwrap_binary: "/usr/bin/bwrap".into(),
        directory: env.root().join(name),
        parallelism: 3,
        mode: PermissionMode::Plan,
        wall_time: Duration::from_secs(20),
        output_reservation: 8 * 1024 * 1024,
        token_reservation: 2_000_000,
        journal_bytes: 1024 * 1024,
    }
}

fn worker(server: &FakeServer, id: &str) -> WorkerSpec {
    WorkerSpec {
        id: id.into(),
        role: format!("role-{id}"),
        task: format!("synthetic task for {id}"),
        inference: LocalInference::register_verified_local(
            server.url().trim_start_matches("http://").parse().unwrap(),
            "fake-model".into(),
        )
        .unwrap(),
        budget: WorkerBudget {
            wall_time: Duration::from_secs(10),
            output_bytes: 1024 * 1024,
            reported_tokens: 500_000,
        },
    }
}

fn assert_reaped(report: &RunReport) {
    for worker in &report.workers {
        if let Some(pid) = worker.process_id {
            assert!(
                !Path::new(&format!("/proc/{pid}")).exists(),
                "worker process was reaped"
            );
        }
    }
}

fn parallel_roles_and_fan_in() {
    let env = TestEnv::new();
    let server = FakeServer::start(
        vec![ScriptTurn::text("synthetic result"); 3],
        FakeOpts {
            hold_headers_ms: 100,
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let specs = vec![
        worker(&server, "reader"),
        worker(&server, "reviewer"),
        worker(&server, "planner"),
    ];
    let (_tx, rx) = mpsc::sync_channel(32);
    let report = run(settings(&env, "parallel"), specs, rx, |_| {}).unwrap();
    assert!(report.succeeded(), "all real RPC workers complete: {report:?}");
    assert_eq!(report.workers.len(), 3);
    assert!(
        server.max_in_flight() >= 2,
        "workers actually overlap at the fake engine"
    );
    for id in ["reader", "reviewer", "planner"] {
        assert!(
            server
                .requests()
                .iter()
                .any(|r| r.system.contains(&format!("role-{id}"))),
            "assigned role reached its worker"
        );
        let report = report.workers.iter().find(|w| w.worker == id).unwrap();
        assert!(report.reported_tokens.is_some());
        assert_eq!(report.task.context_id, "parallel");
        assert!(!report.task.artifacts.is_empty());
    }
    assert!(server.requests().iter().all(|r| r.tools_json == "[]"));
    assert_reaped(&report);
    let encoded = serde_json::to_value(&report).unwrap();
    assert_eq!(encoded["workers"][0]["task"]["kind"], "task");
    let decoded: RunReport = serde_json::from_value(encoded).unwrap();
    assert!(decoded.succeeded());
    let log = std::fs::read_to_string(env.root().join("parallel/transcript.jsonl")).unwrap();
    assert_eq!(
        std::fs::metadata(env.root().join("parallel"))
            .unwrap()
            .permissions()
            .mode()
            & 0o077,
        0
    );
    assert_eq!(
        std::fs::metadata(env.root().join("parallel/transcript.jsonl"))
            .unwrap()
            .permissions()
            .mode()
            & 0o077,
        0
    );
    assert!(!log.contains("synthetic task for"));
    assert!(!log.contains("synthetic result"));
    let records: Vec<serde_json::Value> = log.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    for (sequence, record) in records.iter().enumerate() {
        assert_eq!(record["sequence"], sequence);
    }
}

fn queued_cancellation_and_preflight() {
    let env = TestEnv::new();
    let server = FakeServer::start(vec![], FakeOpts::default()).unwrap();
    let spec = worker(&server, "canceled");
    let (tx, rx) = mpsc::sync_channel(1);
    tx.send(Control::CancelAll).unwrap();
    let report = run(settings(&env, "pre-cancel"), vec![spec.clone()], rx, |_| {}).unwrap();
    assert_eq!(report.workers[0].outcome, Outcome::Canceled);
    assert!(report.workers[0].process_id.is_none());
    assert!(server.requests().is_empty());
    let mut config = settings(&env, "invalid");
    config.output_reservation = 1;
    let (_tx, rx) = mpsc::sync_channel(1);
    assert!(run(config, vec![spec], rx, |_| {}).is_err());
    assert!(!env.root().join("invalid").exists());
    assert!(
        LocalInference::register_verified_local("192.0.2.1:1234".parse().unwrap(), "model".into()).is_err()
    );
}

fn deadline_and_output_and_usage_caps() {
    let env = TestEnv::new();
    let slow = FakeServer::start(
        vec![ScriptTurn::text("slow")],
        FakeOpts {
            hold_headers_ms: 1000,
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let mut spec = worker(&slow, "deadline");
    spec.budget.wall_time = Duration::from_millis(100);
    let (_tx, rx) = mpsc::sync_channel(1);
    let start = Instant::now();
    let report = run(settings(&env, "deadline"), vec![spec], rx, |_| {}).unwrap();
    assert_eq!(report.workers[0].outcome, Outcome::Deadline);
    assert!(start.elapsed() < Duration::from_secs(3));
    assert_reaped(&report);
    let server = FakeServer::start(
        vec![ScriptTurn::text("large output"), ScriptTurn::text("usage")],
        FakeOpts::default(),
    )
    .unwrap();
    for (name, expected) in [
        ("output", Outcome::OutputBudget),
        ("tokens", Outcome::TokenBudget),
    ] {
        let mut spec = worker(&server, name);
        if name == "output" {
            spec.budget.output_bytes = 64;
        } else {
            spec.budget.reported_tokens = 1;
        }
        let (_tx, rx) = mpsc::sync_channel(1);
        let report = run(settings(&env, name), vec![spec], rx, |_| {}).unwrap();
        assert_eq!(report.workers[0].outcome, expected);
        assert_reaped(&report);
    }
}

fn live_cancellation_and_steering() {
    let env = TestEnv::new();
    let server = FakeServer::start(
        vec![ScriptTurn::text("initial"), ScriptTurn::text("steered result")],
        FakeOpts {
            hold_headers_ms: 200,
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let (tx, rx) = mpsc::sync_channel(8);
    let mut sent = false;
    let report = run(
        settings(&env, "steer"),
        vec![worker(&server, "steered")],
        rx,
        |status| {
            if !sent && status.state == "working" && server.requests().iter().any(|r| r.max_tokens > 0) {
                tx.send(Control::Steer {
                    worker: "steered".into(),
                    text: "add the synthetic steering detail".into(),
                })
                .unwrap();
                sent = true;
            }
        },
    )
    .unwrap();
    assert!(sent, "steered while a real inference request was running");
    assert!(report.succeeded(), "steered worker completes: {report:?}");
    assert!(server.requests().iter().any(|r| r.messages.iter().any(|m| m
        .content
        .as_deref()
        .is_some_and(|s| s.contains("synthetic steering detail")))));
    assert_reaped(&report);
    let slow = FakeServer::start(
        vec![ScriptTurn::text("canceled")],
        FakeOpts {
            hold_headers_ms: 1000,
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let (tx, rx) = mpsc::sync_channel(8);
    let mut canceled = false;
    let report = run(
        settings(&env, "cancel"),
        vec![worker(&slow, "live")],
        rx,
        |status| {
            if !canceled && status.state == "working" {
                tx.send(Control::Cancel {
                    worker: "live".into(),
                })
                .unwrap();
                canceled = true;
            }
        },
    )
    .unwrap();
    assert!(canceled);
    assert_eq!(report.workers[0].outcome, Outcome::Canceled);
    assert_reaped(&report);
}

fn partial_failure_and_private_sandbox() {
    let env = TestEnv::new();
    let outside = env.write("outside-private-marker", "synthetic private data");
    let good = FakeServer::start(vec![ScriptTurn::text("good")], FakeOpts::default()).unwrap();
    let mut spec = worker(&good, "isolated");
    spec.role = format!("isolation-probe:{}", outside.display());
    let unavailable = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = unavailable.local_addr().unwrap();
    drop(unavailable);
    let mut bad = worker(&good, "unavailable");
    bad.inference = LocalInference::register_verified_local(address, "fake-model".into()).unwrap();
    let (_tx, rx) = mpsc::sync_channel(1);
    let report = run(settings(&env, "partial"), vec![spec, bad], rx, |_| {}).unwrap();
    assert!(!report.succeeded());
    assert_eq!(
        report
            .workers
            .iter()
            .find(|w| w.worker == "isolated")
            .unwrap()
            .outcome,
        Outcome::Completed
    );
    assert_ne!(
        report
            .workers
            .iter()
            .find(|w| w.worker == "unavailable")
            .unwrap()
            .outcome,
        Outcome::Completed
    );
    assert_reaped(&report);
    assert_eq!(
        std::fs::read_to_string(outside).unwrap(),
        "synthetic private data"
    );
}

fn audit_failure_stops_workers() {
    let env = TestEnv::new();
    let server = FakeServer::start(
        vec![ScriptTurn::text("audit")],
        FakeOpts {
            hold_headers_ms: 1000,
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let mut config = settings(&env, "audit");
    config.journal_bytes = 1024;
    let (_tx, rx) = mpsc::sync_channel(1);
    assert!(run(
        config,
        vec![worker(&server, "one"), worker(&server, "two")],
        rx,
        |_| {}
    )
    .is_err());
    let log = std::fs::read_to_string(env.root().join("audit/transcript.jsonl")).unwrap();
    for line in log.lines() {
        let value: serde_json::Value = serde_json::from_str(line).unwrap();
        if let Some(pid) = value["details"]["processId"].as_u64() {
            assert!(!Path::new(&format!("/proc/{pid}")).exists());
        }
    }
}

fn hostile_workers_and_run_deadline() {
    let env = TestEnv::new();
    let server = FakeServer::start(vec![], FakeOpts::default()).unwrap();
    for (name, outcome) in [
        ("fixture-flood", Outcome::OutputBudget),
        ("fixture-bad-state", Outcome::PolicyViolation),
        ("fixture-ack-only", Outcome::Failed),
        ("fixture-descendant", Outcome::Lost),
        ("fixture-mode-change", Outcome::PolicyViolation),
        ("fixture-ui", Outcome::Completed),
    ] {
        let mut spec = worker(&server, name);
        spec.role = name.into();
        spec.budget.output_bytes = 64 * 1024;
        let (_tx, rx) = mpsc::sync_channel(1);
        let start = Instant::now();
        let report = run(settings(&env, name), vec![spec], rx, |_| {}).unwrap();
        assert_eq!(report.workers[0].outcome, outcome, "hostile fixture {name}");
        assert!(start.elapsed() < Duration::from_secs(3));
        assert_reaped(&report);
    }
    let slow = FakeServer::start(
        vec![ScriptTurn::text("run deadline")],
        FakeOpts {
            hold_headers_ms: 1000,
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let mut config = settings(&env, "run-deadline");
    config.wall_time = Duration::from_millis(100);
    config.parallelism = 1;
    let (_tx, rx) = mpsc::sync_channel(1);
    let report = run(
        config,
        vec![worker(&slow, "active"), worker(&slow, "queued")],
        rx,
        |_| {},
    )
    .unwrap();
    assert!(report.workers.iter().all(|w| w.outcome == Outcome::Deadline));
    assert!(report
        .workers
        .iter()
        .find(|w| w.worker == "queued")
        .unwrap()
        .process_id
        .is_none());
    assert_reaped(&report);
}

/// Test-only hostile RPC processes exercise limits independently of the trusted
/// oxa implementation. These branches are not part of any production binary.
fn hostile_fixture(args: &[String]) {
    let role = args.last().unwrap();
    if role.ends_with("fixture-flood") {
        loop {
            println!("{}", serde_json::json!({"type":"notice","text":"x".repeat(4096)}));
        }
    }
    if role.ends_with("fixture-descendant") {
        let _child = std::process::Command::new("/usr/bin/sh")
            .args(["-c", "sleep 60"])
            .stdin(std::process::Stdio::null())
            .spawn()
            .unwrap();
        std::process::exit(0); // descendant retains stdout/stderr; PID namespace must clean it up
    }
    for line in std::io::stdin().lock().lines() {
        let request: serde_json::Value = serde_json::from_str(&line.unwrap()).unwrap();
        let kind = request["type"].as_str().unwrap();
        if kind == "extension_ui_response" {
            assert_eq!(request["value"], false);
            assert_eq!(request["cancelled"], true);
            println!(
                "{}",
                serde_json::json!({"type":"message_end","message":{"role":"assistant","stopReason":"stop","content":[{"type":"text","text":"permission denied safely"}],"usage":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0}}})
            );
            println!("{}", serde_json::json!({"type":"agent_settled","aborted":false}));
            std::io::stdout().flush().unwrap();
            continue;
        }
        let data = if kind == "get_state" {
            serde_json::json!({"mode":if role.ends_with("fixture-bad-state") {"workspace"} else {"plan"},"model":{"id":"fake-model"},"isStreaming":false})
        } else {
            serde_json::Value::Null
        };
        println!(
            "{}",
            serde_json::json!({"type":"response","command":kind,"id":request["id"],"success":true,"data":data})
        );
        if kind == "prompt" {
            if role.ends_with("fixture-mode-change") {
                println!(
                    "{}",
                    serde_json::json!({"type":"mode_changed","mode":"workspace"})
                );
            } else if role.ends_with("fixture-ui") {
                println!(
                    "{}",
                    serde_json::json!({"type":"extension_ui_request","id":"permission1","method":"confirm","title":"synthetic permission request"})
                );
            } else {
                println!("{}", serde_json::json!({"type":"agent_settled","aborted":false}));
            }
        }
        std::io::stdout().flush().unwrap();
    }
    std::process::exit(0);
}

fn inherited_modes_and_task_command_wrapping() {
    let env = TestEnv::new();
    let server = FakeServer::start(vec![ScriptTurn::text("safe task data"); 3], FakeOpts::default()).unwrap();
    for (name, mode) in [
        ("plan", PermissionMode::Plan),
        ("ask", PermissionMode::Ask),
        ("workspace", PermissionMode::Workspace),
    ] {
        let mut config = settings(&env, name);
        config.mode = mode;
        let mut spec = worker(&server, name);
        spec.task = "!touch /tmp/task-must-remain-data".into();
        let (_tx, rx) = mpsc::sync_channel(1);
        let report = run(config, vec![spec], rx, |_| {}).unwrap();
        assert!(report.succeeded(), "mode {name} respects the parent setting");
        assert_reaped(&report);
    }
    let generating: Vec<_> = server
        .requests()
        .into_iter()
        .filter(|r| r.max_tokens > 0)
        .collect();
    assert_eq!(generating.len(), 3);
    assert!(generating.iter().all(|r| r
        .messages
        .last()
        .unwrap()
        .content
        .as_deref()
        .unwrap()
        .starts_with("Delegated task data:\n")));
    assert!(generating.iter().all(|r| r.tools_json == "[]"));
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--mode") {
        assert_eq!(std::env::var("HOME").unwrap(), "/home/worker");
        assert!(std::env::var("OXA_PARENT_PRIVATE_MARKER").is_err());
        if args.last().is_some_and(|s| s.contains("\nfixture-")) {
            hostile_fixture(&args);
        }
        if let Some(role) = args
            .last()
            .and_then(|s| s.split_once("isolation-probe:").map(|(_, s)| s))
        {
            assert!(!Path::new(role).exists(), "parent filesystem is invisible");
        }
        let code = oxa_cli::run(&args, &oxa_config::Env::from_process(), oxa_cli::Stdio::real());
        std::process::exit(code);
    }
    std::env::set_var("OXA_PARENT_PRIVATE_MARKER", "synthetic-parent-value");
    for (name, test) in [
        ("parallel roles and fan-in", parallel_roles_and_fan_in as fn()),
        (
            "queued cancellation and preflight",
            queued_cancellation_and_preflight,
        ),
        ("deadlines and caps", deadline_and_output_and_usage_caps),
        ("live cancellation and steering", live_cancellation_and_steering),
        (
            "partial failure and sandbox isolation",
            partial_failure_and_private_sandbox,
        ),
        ("audit failure cleanup", audit_failure_stops_workers),
        (
            "hostile RPC workers and run deadline",
            hostile_workers_and_run_deadline,
        ),
        (
            "inherited modes and task command wrapping",
            inherited_modes_and_task_command_wrapping,
        ),
    ] {
        println!("test {name} ...");
        test();
        println!("test {name} ... ok");
    }
}
