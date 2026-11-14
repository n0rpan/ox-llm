//! A harness-free test executable doubles as the REAL oxa RPC worker: when
//! invoked with --mode rpc it delegates to oxa_cli::run. Normal invocation runs
//! the integration suite. No installed binary, model or service is touched.
use oxa_a2a::{
    loopback::Limits,
    server::{Backend, Server, ServerLimits},
    wire::{Event, Message, MessageObject, Part, Role, SendParams, Task, TaskParams, TaskState},
};
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
use oxa_orchestrator::a2a_run::*;
use oxa_orchestrator::rpc_agent::{RpcAgent, RpcAgentConfig};
use oxa_orchestrator::*;
use std::io::{BufRead, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::mpsc;
use std::sync::{atomic::AtomicBool, Arc};
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

fn native_agent(env: &TestEnv, engine: &FakeServer, name: &str, max_live: usize) -> Arc<RpcAgent> {
    let config = settings(env, name);
    let spec = worker(engine, "template");
    Arc::new(
        RpcAgent::new(RpcAgentConfig {
            worker_binary: config.worker_binary,
            bwrap_binary: config.bwrap_binary,
            directory: config.directory,
            role: "A2A delegated worker".into(),
            inference: spec.inference,
            mode: PermissionMode::Plan,
            budget: spec.budget,
            max_live,
            max_tasks: 8,
            journal_bytes: config.journal_bytes,
        })
        .unwrap(),
    )
}

fn a2a_message(id: &str, text: &str, task: Option<&Task>) -> SendParams {
    SendParams {
        message: MessageObject::Message(Message {
            message_id: id.into(),
            role: Role::User,
            parts: vec![Part::Text { text: text.into() }],
            task_id: task.map(|t| t.id.clone()),
            context_id: task.map(|t| t.context_id.clone()),
        }),
    }
}

fn wait_working(agent: &RpcAgent, engine: &FakeServer, id: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if agent
            .get(TaskParams {
                id: id.into(),
                history_length: None,
            })
            .unwrap()
            .status
            .state
            == TaskState::Working
            && engine.requests().iter().any(|r| r.max_tokens > 0)
        {
            return;
        }
        assert!(Instant::now() < deadline, "real A2A worker reached inference");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn a2a_rpc_bridge_stream_steer_and_deduplication() {
    let env = TestEnv::new();
    let engine = FakeServer::start(
        vec![
            ScriptTurn::text("first A2A turn"),
            ScriptTurn::text("steered A2A result"),
        ],
        FakeOpts {
            hold_headers_ms: 200,
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let agent = native_agent(&env, &engine, "a2a-steer", 2);
    let server = Server::start(agent.clone(), ServerLimits::default()).unwrap();
    let mut client = server.client(Limits::default()).unwrap();
    let card = client.card().unwrap();
    assert!(card.capabilities.streaming);
    let first = a2a_message("initial", "synthetic A2A task", None);
    let mut stream = client.stream(first.clone()).unwrap();
    let Some(Event::Task(task)) = stream.next_event().unwrap() else {
        panic!()
    };
    assert_eq!(task.status.state, TaskState::Submitted);
    let mut controller = server.client(Limits::default()).unwrap();
    let Event::Task(duplicate) = controller.send(first).unwrap() else {
        panic!()
    };
    assert_eq!(
        duplicate.id, task.id,
        "repeating an initial message does not launch another worker"
    );
    assert!(controller
        .send(a2a_message("initial", "different task", None))
        .unwrap_err()
        .to_string()
        .contains("-32602"));
    wait_working(&agent, &engine, &task.id);
    let mut wrong_context = task.clone();
    wrong_context.context_id = "different-context".into();
    assert!(controller
        .send(a2a_message("bad-steer", "must not apply", Some(&wrong_context)))
        .unwrap_err()
        .to_string()
        .contains("-32602"));
    let follow_up = a2a_message("followup", "include the A2A steering detail", Some(&task));
    controller.send(follow_up.clone()).unwrap();
    controller.send(follow_up).unwrap();
    let mut artifacts = vec![];
    let mut completed = false;
    while let Some(event) = stream.next_event().unwrap() {
        assert!(event.belongs_to(&task.id, &task.context_id));
        match event {
            Event::Artifact(update) => artifacts.push(update.artifact),
            Event::Status(update) if update.final_event => {
                assert_eq!(update.status.state, TaskState::Completed);
                completed = true;
            }
            _ => {}
        }
    }
    assert!(completed);
    assert!(artifacts
        .iter()
        .flat_map(|a| &a.parts)
        .any(|p| matches!(p,Part::Text {text} if text == "steered A2A result")));
    assert!(artifacts
        .iter()
        .flat_map(|a| &a.parts)
        .any(|p| matches!(p,Part::Data {data} if data["reportedTokens"].is_number())));
    let final_task = controller.get(&task.id).unwrap();
    assert_eq!(final_task.status.state, TaskState::Completed);
    let history = controller
        .call("tasks/get", serde_json::json!({"id":task.id,"historyLength":10}))
        .unwrap();
    let Event::Task(history) = serde_json::from_value(history).unwrap() else {
        panic!()
    };
    assert_eq!(history.history.len(), 2, "duplicate steering is not re-queued");
    assert_eq!(engine.requests().iter().filter(|r| r.max_tokens > 0).count(), 2);
    assert!(engine.requests().iter().any(|r| r.messages.iter().any(|m| m
        .content
        .as_deref()
        .is_some_and(|s| s.contains("A2A steering detail")))));
    let report = agent.report(&task.id).unwrap();
    assert!(!Path::new(&format!("/proc/{}", report.process_id.unwrap())).exists());
}

fn a2a_cancellation_disconnect_recovery_and_shutdown() {
    let env = TestEnv::new();
    let engine = FakeServer::start(
        vec![ScriptTurn::text("canceled over A2A")],
        FakeOpts {
            hold_headers_ms: 1000,
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let agent = native_agent(&env, &engine, "a2a-cancel", 1);
    let server = Server::start(
        agent.clone(),
        ServerLimits {
            connections: 2,
            ..ServerLimits::default()
        },
    )
    .unwrap();
    let mut client = server.client(Limits::default()).unwrap();
    let mut stream = client
        .stream(a2a_message("cancel", "cancel over A2A", None))
        .unwrap();
    let Some(Event::Task(task)) = stream.next_event().unwrap() else {
        panic!()
    };
    wait_working(&agent, &engine, &task.id);
    let mut controller = server.client(Limits::default()).unwrap();
    assert_eq!(
        controller.cancel(&task.id).unwrap().status.state,
        TaskState::Canceled
    );
    let mut canceled = false;
    while let Some(event) = stream.next_event().unwrap() {
        if let Event::Status(update) = event {
            if update.final_event {
                assert_eq!(update.status.state, TaskState::Canceled);
                canceled = true;
            }
        }
    }
    assert!(canceled, "cancel remains available with a live SSE connection");
    drop(server);
    let engine = FakeServer::start(
        vec![ScriptTurn::text("resubscribed A2A result")],
        FakeOpts {
            hold_headers_ms: 200,
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let agent = native_agent(&env, &engine, "a2a-resubscribe", 1);
    let server = Server::start(agent.clone(), ServerLimits::default()).unwrap();
    let mut client = server.client(Limits::default()).unwrap();
    let mut stream = client
        .stream(a2a_message("resubscribe", "resume observation", None))
        .unwrap();
    let Some(Event::Task(task)) = stream.next_event().unwrap() else {
        panic!()
    };
    drop(stream);
    let mut resumed = client.resubscribe(&task.id).unwrap();
    let mut done = false;
    while let Some(event) = resumed.next_event().unwrap() {
        if let Event::Task(task) = &event {
            done |= task.status.state == TaskState::Completed;
        }
        if let Event::Status(status) = event {
            if status.final_event {
                assert_eq!(status.status.state, TaskState::Completed);
                done = true;
            }
        }
    }
    assert!(done);
    assert_eq!(
        engine.requests().iter().filter(|r| r.max_tokens > 0).count(),
        1,
        "resubscription never relaunches a task"
    );
    drop(server);
    let engine = FakeServer::start(
        vec![ScriptTurn::text("shutdown")],
        FakeOpts {
            hold_headers_ms: 1000,
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let agent = native_agent(&env, &engine, "a2a-shutdown", 1);
    let server = Server::start(agent.clone(), ServerLimits::default()).unwrap();
    let mut client = server.client(Limits::default()).unwrap();
    let Event::Task(task) = client
        .send(a2a_message("shutdown", "bounded shutdown", None))
        .unwrap()
    else {
        panic!()
    };
    wait_working(&agent, &engine, &task.id);
    assert!(client
        .send(a2a_message("over-capacity", "must not start", None))
        .unwrap_err()
        .to_string()
        .contains("-32004"));
    let stalled = std::net::TcpStream::connect(server.address()).unwrap(); // no headers
    let start = Instant::now();
    drop(server);
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "shutdown interrupts stalled HTTP readers"
    );
    drop(stalled);
    let task = agent
        .get(TaskParams {
            id: task.id,
            history_length: None,
        })
        .unwrap();
    assert_eq!(task.status.state, TaskState::Canceled);
    let report = agent.report(&task.id).unwrap();
    assert!(!Path::new(&format!("/proc/{}", report.process_id.unwrap())).exists());
}

fn cancellation_flag_bypasses_full_controls() {
    let env = TestEnv::new();
    let engine = FakeServer::start(vec![], FakeOpts::default()).unwrap();
    let (tx, rx) = mpsc::sync_channel(1);
    tx.send(Control::Steer {
        worker: "canceled".into(),
        text: "queued control".into(),
    })
    .unwrap();
    let report = run_cancellable(
        settings(&env, "flag-cancel"),
        vec![worker(&engine, "canceled")],
        rx,
        Arc::new(AtomicBool::new(true)),
        |_| {},
    )
    .unwrap();
    assert_eq!(report.workers[0].outcome, Outcome::Canceled);
    assert!(report.workers[0].process_id.is_none());
    assert!(engine.requests().is_empty());
}

fn a2a_settings(env: &TestEnv, name: &str) -> A2ARunSettings {
    A2ARunSettings {
        id: name.into(),
        directory: env.root().join(name),
        parallelism: 2,
        wall_time: Duration::from_secs(15),
        response_reservation: 4 * 1024 * 1024,
        journal_bytes: 1024 * 1024,
        request_timeout: Duration::from_millis(100),
        poll_interval: Duration::from_millis(10),
        cancel_grace: Duration::from_millis(500),
    }
}
fn a2a_worker(server: &Server<RpcAgent>, id: &str) -> A2AWorkerSpec {
    A2AWorkerSpec {
        id: id.into(),
        role: format!("A2A role {id}"),
        task: format!("A2A synthetic goal {id}"),
        agent: LocalAgent::register_verified_local(server.client(Limits::default()).unwrap()),
        wall_time: Duration::from_secs(10),
        response_bytes: 512 * 1024,
        reported_tokens: Some(500_000),
    }
}

fn a2a_scheduler_coordinates_real_rpc_workers() {
    let env = TestEnv::new();
    let engine = FakeServer::start(
        vec![ScriptTurn::text("A2A fan-in result"); 3],
        FakeOpts {
            hold_headers_ms: 200,
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let agent = native_agent(&env, &engine, "a2a-pool-agent", 2);
    let server = Server::start(agent.clone(), ServerLimits::default()).unwrap();
    let plan = vec![
        a2a_worker(&server, "reader"),
        a2a_worker(&server, "reviewer"),
        a2a_worker(&server, "planner"),
    ];
    let (_tx, rx) = mpsc::sync_channel(32);
    let report = run_a2a(
        a2a_settings(&env, "a2a-pool"),
        plan,
        rx,
        Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    assert!(
        report.succeeded(),
        "A2A fan-in through real RPC workers: {report:?}"
    );
    assert_eq!(report.workers.len(), 3);
    assert!(engine.max_in_flight() >= 2);
    for result in &report.workers {
        let task = result.task.as_ref().unwrap();
        assert_eq!(task.status.state, TaskState::Completed);
        assert!(result.reported_tokens.is_some());
        assert!(result.response_bytes <= 512 * 1024);
        let native = agent.report(&task.id).unwrap();
        assert!(!Path::new(&format!("/proc/{}", native.process_id.unwrap())).exists());
        assert!(engine.requests().iter().any(|r| r.messages.iter().any(|m| m
            .content
            .as_deref()
            .is_some_and(|s| s.contains(&format!("A2A role {}", result.worker))))));
    }
    let log = std::fs::read_to_string(env.root().join("a2a-pool/transcript.jsonl")).unwrap();
    assert!(!log.contains("A2A synthetic goal"));
    assert!(log.contains("task_bound"));
}

fn a2a_scheduler_steering_cancellation_and_wire_caps() {
    let env = TestEnv::new();
    let engine = FakeServer::start(
        vec![
            ScriptTurn::text("before outer steering"),
            ScriptTurn::text("after outer steering"),
        ],
        FakeOpts {
            hold_headers_ms: 250,
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let agent = native_agent(&env, &engine, "a2a-control-agent", 1);
    let server = Server::start(agent.clone(), ServerLimits::default()).unwrap();
    let (tx, rx) = mpsc::sync_channel(32);
    let report = std::thread::scope(|scope| {
        scope.spawn(|| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !engine.requests().iter().any(|r| r.max_tokens > 0) {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(2));
            }
            tx.send(Control::Steer {
                worker: "steered".into(),
                text: "outer A2A steering detail".into(),
            })
            .unwrap();
        });
        run_a2a(
            a2a_settings(&env, "a2a-outer-steer"),
            vec![a2a_worker(&server, "steered")],
            rx,
            Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap()
    });
    assert!(report.succeeded(), "outer steering: {report:?}");
    assert!(engine.requests().iter().any(|r| r.messages.iter().any(|m| m
        .content
        .as_deref()
        .is_some_and(|s| s.contains("outer A2A steering detail")))));
    let log = std::fs::read_to_string(env.root().join("a2a-outer-steer/transcript.jsonl")).unwrap();
    assert!(log.contains("steer_admitted"));
    assert!(!log.contains("outer A2A steering detail"));
    let engine = FakeServer::start(
        vec![ScriptTurn::text("outer cancellation")],
        FakeOpts {
            hold_headers_ms: 1000,
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let agent = native_agent(&env, &engine, "a2a-cancel-agent", 1);
    let server = Server::start(agent.clone(), ServerLimits::default()).unwrap();
    let (tx, rx) = mpsc::sync_channel(32);
    let mut canceled = false;
    let report = run_a2a(
        a2a_settings(&env, "a2a-outer-cancel"),
        vec![a2a_worker(&server, "canceled")],
        rx,
        Arc::new(AtomicBool::new(false)),
        |status| {
            if !canceled && status.task_id.is_some() {
                tx.send(Control::Cancel {
                    worker: "canceled".into(),
                })
                .unwrap();
                canceled = true;
            }
        },
    )
    .unwrap();
    assert_eq!(report.workers[0].outcome, Outcome::Canceled);
    assert_eq!(report.workers[0].stop, StopConfirmation::Confirmed);
    assert_eq!(
        report.workers[0].task.as_ref().unwrap().status.state,
        TaskState::Canceled
    );
    let engine = FakeServer::start(
        vec![ScriptTurn::text(&"large result ".repeat(2000))],
        FakeOpts::default(),
    )
    .unwrap();
    let agent = native_agent(&env, &engine, "a2a-byte-agent", 1);
    let server = Server::start(agent.clone(), ServerLimits::default()).unwrap();
    let mut worker = a2a_worker(&server, "limited");
    worker.response_bytes = 8192;
    let (_tx, rx) = mpsc::sync_channel(1);
    let report = run_a2a(
        a2a_settings(&env, "a2a-wire-cap"),
        vec![worker],
        rx,
        Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    assert_eq!(report.workers[0].outcome, Outcome::OutputBudget);
    assert!(report.workers[0].response_bytes <= 8192);
    assert!(report.workers[0].task.is_some());
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
        (
            "A2A RPC bridge streaming, steering and deduplication",
            a2a_rpc_bridge_stream_steer_and_deduplication,
        ),
        (
            "A2A cancellation, resubscription and shutdown",
            a2a_cancellation_disconnect_recovery_and_shutdown,
        ),
        (
            "cancellation bypasses full control queues",
            cancellation_flag_bypasses_full_controls,
        ),
        (
            "A2A scheduler coordinates real RPC workers",
            a2a_scheduler_coordinates_real_rpc_workers,
        ),
        (
            "A2A scheduler steering, cancellation and wire caps",
            a2a_scheduler_steering_cancellation_and_wire_caps,
        ),
    ] {
        println!("test {name} ...");
        test();
        println!("test {name} ... ok");
    }
}
