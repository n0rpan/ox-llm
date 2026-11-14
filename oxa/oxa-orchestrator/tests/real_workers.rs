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
        workspace: None,
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
            workspace: None,
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
        persist_results: false,
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

fn a2a_scheduler_resumes_only_never_started_real_workers() {
    use oxa_orchestrator::recovery::{ExclusiveRun, InspectedRun, Phase};
    let env = TestEnv::new();
    let engine = FakeServer::start(
        vec![ScriptTurn::text("resumable native result"); 2],
        FakeOpts::default(),
    )
    .unwrap();
    let agent = native_agent(&env, &engine, "resume-native-agent", 1);
    let server = Server::start(agent.clone(), ServerLimits::default()).unwrap();
    let mut config = a2a_settings(&env, "resume-native-parent");
    config.parallelism = 1;
    config.persist_results = true;
    let path = config.directory.join("transcript.jsonl");
    let (_tx, rx) = mpsc::sync_channel(2);
    let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_a2a(
            config,
            vec![a2a_worker(&server, "reader"), a2a_worker(&server, "reviewer")],
            rx,
            Arc::new(AtomicBool::new(false)),
            |_| {
                if InspectedRun::inspect(&path)
                    .unwrap()
                    .snapshot()
                    .workers
                    .iter()
                    .any(|w| w.worker == "reader" && w.phase == Phase::Finished)
                {
                    panic!("synthetic controller failure after native worker checkpoint");
                }
            },
        )
    }));
    assert!(failed.is_err());
    let before = std::fs::read(&path).unwrap();
    let owner = ExclusiveRun::claim(&path).unwrap();
    assert_eq!(
        owner
            .inspected()
            .snapshot()
            .workers
            .iter()
            .find(|w| w.worker == "reviewer")
            .unwrap()
            .phase,
        Phase::Queued
    );
    assert_eq!(engine.requests().iter().filter(|r| r.max_tokens > 0).count(), 1);
    let (_tx, rx) = mpsc::sync_channel(2);
    let report = oxa_orchestrator::resume::resume_queued(
        &owner,
        &env.root().join("resume-native-child"),
        vec![a2a_worker(&server, "reviewer")],
        rx,
        Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    assert!(report.succeeded(), "{report:?}");
    assert_eq!(report.workers.len(), 2);
    assert_eq!(engine.requests().iter().filter(|r| r.max_tokens > 0).count(), 2);
    let mut ids = std::collections::BTreeSet::new();
    for worker in &report.workers {
        let task = worker.task.as_ref().unwrap();
        assert!(ids.insert(task.id.clone()));
        let native = agent.report(&task.id).unwrap();
        assert!(!Path::new(&format!("/proc/{}", native.process_id.unwrap())).exists());
    }
    let child_path = env.root().join("resume-native-child/transcript.jsonl");
    let child = ExclusiveRun::claim(&child_path).unwrap();
    let collected = oxa_orchestrator::lineage::collect(&[&owner, &child]).unwrap();
    assert_eq!(
        serde_json::to_value(&collected).unwrap(),
        serde_json::to_value(&report).unwrap()
    );
    let before_collect = engine.requests().len();
    // A separate inspector cannot claim journals still owned by this fixture.
    let inspect = || {
        let mut command = std::process::Command::new(cli_binary());
        command
            .env_clear()
            .current_dir(env.work())
            .args(["orchestrate-inspect", "--lineage"])
            .arg(&path)
            .arg("--lineage")
            .arg(&child_path);
        command
    };
    assert_eq!(inspect().output().unwrap().status.code(), Some(1));
    drop(child);
    drop(owner);
    let output = inspect().output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
        serde_json::to_value(&report).unwrap()
    );
    assert_eq!(engine.requests().len(), before_collect);
    assert_eq!(std::fs::read(path).unwrap(), before);
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

fn cli_binary() -> std::path::PathBuf {
    // Local feature-enabled binary smoke runs can point at the checkout build;
    // hosted default tests use this fixture's identical command implementation.
    std::env::var_os("OXA_ORCHESTRATE_TEST_BINARY")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::current_exe().unwrap())
}
fn cli_command(env: &TestEnv, engine: &FakeServer, name: &str) -> std::process::Command {
    let mut command = std::process::Command::new(cli_binary());
    command
        .env_clear()
        .envs(env.envs(&engine.url()))
        .env("OXA_PARENT_PRIVATE_MARKER", "synthetic-cli-parent")
        .current_dir(env.work())
        .args([
            "orchestrate",
            "--engine",
            engine.url().trim_start_matches("http://"),
            "--model",
            "fake-model",
            "--local-only",
            "--directory",
        ])
        .arg(env.root().join(name));
    command
}

fn orchestrate_cli_success_and_validation() {
    let env = TestEnv::new();
    let engine = FakeServer::start(
        vec![ScriptTurn::text("synthetic CLI result \u{001b}\u{009d}\u{007f}"); 2],
        FakeOpts::default(),
    )
    .unwrap();
    let output = cli_command(&env, &engine, "cli-success")
        .arg("--save-results")
        .args([
            "--workers",
            "2",
            "--role",
            "reader",
            "--role",
            "reviewer",
            "--",
            "synthetic CLI goal",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "CLI stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["workers"].as_array().unwrap().len(), 2);
    assert!(report["workers"]
        .as_array()
        .unwrap()
        .iter()
        .all(|w| w["outcome"] == "completed"));
    assert!(!output.stdout.contains(&0x1b), "JSON escapes terminal controls");
    assert!(!String::from_utf8_lossy(&output.stdout)
        .chars()
        .any(|c| c.is_control() && c != '\n'));
    let status = String::from_utf8(output.stderr).unwrap();
    assert!(status.contains("worker-1: completed"));
    assert!(status.contains("worker-2: completed"));
    let root = env.root().join("cli-success");
    assert_eq!(std::fs::read(root.join("report.json")).unwrap(), output.stdout);
    assert_eq!(std::fs::metadata(&root).unwrap().permissions().mode() & 0o077, 0);
    let audit = std::fs::read_to_string(root.join("run/transcript.jsonl")).unwrap();
    assert!(!audit.contains("synthetic CLI goal"));
    assert!(engine.requests().iter().all(|r| r.tools_json == "[]"));
    let before = engine.requests().len();
    let output = cli_command(&env, &engine, "cli-invalid")
        .args(["--yolo", "synthetic goal"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(!env.root().join("cli-invalid").exists());
    let output = cli_command(&env, &engine, "cli-help")
        .arg("--help")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(!env.root().join("cli-help").exists());
    assert_eq!(
        engine.requests().len(),
        before,
        "help and rejected options never probe the engine"
    );
    std::fs::write(root.join("keep"), "keep-existing-data").unwrap();
    let output = cli_command(&env, &engine, "cli-success")
        .arg("goal")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        std::fs::read_to_string(root.join("keep")).unwrap(),
        "keep-existing-data"
    );
}

struct CliChild(std::process::Child);
impl Drop for CliChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

fn orchestrate_cli_signal_cleanup() {
    use std::io::Read;
    let env = TestEnv::new();
    let engine = FakeServer::start(
        vec![ScriptTurn::text("canceled CLI result")],
        FakeOpts {
            hold_headers_ms: 1500,
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let mut child = CliChild(
        cli_command(&env, &engine, "cli-signal")
            .arg("--save-results")
            .args([
                "--workers",
                "1",
                "--worker-timeout",
                "4",
                "--timeout",
                "10",
                "synthetic cancel goal",
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut stderr = std::io::BufReader::new(child.0.stderr.take().unwrap());
    let mut line = String::new();
    let mut working = false;
    while stderr.read_line(&mut line).unwrap() != 0 {
        if line.contains("worker-1: working") {
            working = true;
            break;
        }
        line.clear();
    }
    assert!(working, "CLI reached active worker status");
    // SAFETY: this PID is our live, unreaped test child, not a system process.
    assert_eq!(unsafe { libc::kill(child.0.id() as i32, libc::SIGINT) }, 0);
    let mut stdout = Vec::new();
    child.0.stdout.take().unwrap().read_to_end(&mut stdout).unwrap();
    assert_eq!(child.0.wait().unwrap().code(), Some(130));
    assert_eq!(
        std::fs::read(env.root().join("cli-signal/report.json")).unwrap(),
        stdout
    );
    let report: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
    assert_eq!(report["workers"][0]["outcome"], "canceled");
    assert_eq!(report["workers"][0]["stop"], "confirmed");
    let mut spawned = 0;
    for entry in std::fs::read_dir(env.root().join("cli-signal/agents")).unwrap() {
        let path = entry.unwrap().path().join("transcript.jsonl");
        assert_native_cancellation_inspection(&path, &env.root().join("signal-cancellation-prefix.jsonl"));
        let log = std::fs::read_to_string(path).unwrap();
        for line in log.lines() {
            let event: serde_json::Value = serde_json::from_str(line).unwrap();
            if let Some(pid) = event["details"]["processId"].as_u64() {
                spawned += 1;
                assert!(!Path::new(&format!("/proc/{pid}")).exists());
            }
        }
    }
    assert!(spawned > 0);
}

fn approved_workspace_edit_and_read_only_policy() {
    use oxa_orchestrator::workspace::{Grant, RelativePath, ReviewBundle, Snapshot};
    let env = TestEnv::new();
    std::fs::write(env.root().join("input.txt"), "original\n").unwrap();
    let snapshot = Arc::new(
        Snapshot::approve(env.root(), vec![RelativePath::new("input.txt".into()).unwrap()]).unwrap(),
    );
    let server = FakeServer::start(
        vec![
            ScriptTurn::tool("read-1", "read", r#"{"path":"input.txt"}"#),
            ScriptTurn::tool(
                "write-1",
                "write",
                r#"{"path":"input.txt","content":"proposed\n"}"#,
            ),
            ScriptTurn::text("proposal ready"),
        ],
        FakeOpts::default(),
    )
    .unwrap();
    let mut spec = worker(&server, "editor");
    spec.workspace = Some(Arc::new(Grant::editing(snapshot.clone(), vec![]).unwrap()));
    let mut config = settings(&env, "workspace-edit");
    config.mode = PermissionMode::Workspace;
    let (_tx, rx) = mpsc::sync_channel(4);
    let report = run(config, vec![spec], rx, |_| {}).unwrap();
    assert!(report.succeeded(), "{report:?}");
    assert_reaped(&report);
    let review = report.workers[0]
        .task
        .artifacts
        .iter()
        .flat_map(|a| &a.parts)
        .find_map(|p| match p {
            Part::Data { data } => data.get("review").cloned(),
            _ => None,
        })
        .expect("review artifact");
    let review: ReviewBundle = serde_json::from_value(review).unwrap();
    review.verify(&snapshot, &[]).unwrap();
    review.verify_current_base(env.root()).unwrap();
    assert_eq!(review.changes[0].after.as_deref(), Some("proposed\n"));
    assert_eq!(
        std::fs::read_to_string(env.root().join("input.txt")).unwrap(),
        "original\n"
    );
    let log = std::fs::read_to_string(env.root().join("workspace-edit/transcript.jsonl")).unwrap();
    assert!(!log.contains("proposed"));
    assert!(log.contains("workspace_review"));
    let server = FakeServer::start(
        vec![ScriptTurn::tool(
            "write-2",
            "write",
            r#"{"path":"input.txt","content":"denied"}"#,
        )],
        FakeOpts::default(),
    )
    .unwrap();
    let mut spec = worker(&server, "reader");
    spec.workspace = Some(Arc::new(Grant::read_only(snapshot.clone())));
    let (_tx, rx) = mpsc::sync_channel(4);
    let report = run(settings(&env, "workspace-read"), vec![spec], rx, |_| {}).unwrap();
    assert_eq!(report.workers[0].outcome, Outcome::PolicyViolation);
    assert_reaped(&report);
    let mut spec = worker(&server, "invalid");
    spec.workspace = Some(Arc::new(Grant::editing(snapshot, vec![]).unwrap()));
    let (_tx, rx) = mpsc::sync_channel(4);
    assert!(run(settings(&env, "workspace-preflight"), vec![spec], rx, |_| {}).is_err());
    assert!(!env.root().join("workspace-preflight").exists());
}

fn a2a_workspace_review_round_trip() {
    use oxa_orchestrator::workspace::{Grant, RelativePath, ReviewBundle, Snapshot};
    let env = TestEnv::new();
    std::fs::write(env.root().join("input.txt"), "baseline\n").unwrap();
    let snapshot = Arc::new(
        Snapshot::approve(env.root(), vec![RelativePath::new("input.txt".into()).unwrap()]).unwrap(),
    );
    let engine = FakeServer::start(
        vec![
            ScriptTurn::tool(
                "edit-1",
                "edit",
                r#"{"path":"input.txt","oldText":"baseline","newText":"reviewed"}"#,
            ),
            ScriptTurn::text("review this proposal"),
        ],
        FakeOpts::default(),
    )
    .unwrap();
    let config = settings(&env, "workspace-agent");
    let template = worker(&engine, "template");
    let agent = Arc::new(
        RpcAgent::new(RpcAgentConfig {
            workspace: Some(Arc::new(Grant::editing(snapshot.clone(), vec![]).unwrap())),
            worker_binary: config.worker_binary,
            bwrap_binary: config.bwrap_binary,
            directory: config.directory,
            role: "editing worker".into(),
            inference: template.inference,
            mode: PermissionMode::Workspace,
            budget: template.budget,
            max_live: 1,
            max_tasks: 2,
            journal_bytes: config.journal_bytes,
        })
        .unwrap(),
    );
    let server = Server::start(agent.clone(), ServerLimits::default()).unwrap();
    let (_tx, rx) = mpsc::sync_channel(4);
    let report = run_a2a(
        a2a_settings(&env, "workspace-controller"),
        vec![a2a_worker(&server, "editor")],
        rx,
        Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    assert!(report.succeeded(), "{report:?}");
    let task = report.workers[0].task.as_ref().unwrap();
    let review = task
        .artifacts
        .iter()
        .flat_map(|a| &a.parts)
        .find_map(|p| match p {
            Part::Data { data } => data.get("review").cloned(),
            _ => None,
        })
        .expect("review survived A2A serialization and collection");
    let review: ReviewBundle = serde_json::from_value(review).unwrap();
    review.verify(&snapshot, &[]).unwrap();
    review.verify_current_base(env.root()).unwrap();
    assert_eq!(review.changes[0].after.as_deref(), Some("reviewed\n"));
    assert_eq!(
        std::fs::read_to_string(env.root().join("input.txt")).unwrap(),
        "baseline\n"
    );
    let native = agent.report(&task.id).unwrap();
    assert!(!Path::new(&format!("/proc/{}", native.process_id.unwrap())).exists());
}

fn orchestrate_cli_approved_files() {
    use oxa_orchestrator::workspace::{RelativePath, ReviewBundle, Snapshot};
    let env = TestEnv::new();
    std::fs::write(env.work().join("input.txt"), "source stays unchanged\n").unwrap();
    let snapshot =
        Snapshot::approve(&env.work(), vec![RelativePath::new("input.txt".into()).unwrap()]).unwrap();
    let engine = FakeServer::start(
        vec![
            ScriptTurn::tool("read-1", "read", r#"{"path":"input.txt"}"#),
            ScriptTurn::tool(
                "write-1",
                "write",
                r#"{"path":"input.txt","content":"proposed CLI edit\n"}"#,
            ),
            ScriptTurn::tool(
                "write-2",
                "write",
                r#"{"path":"new.txt","content":"proposed new file\n"}"#,
            ),
            ScriptTurn::text("CLI review ready"),
        ],
        FakeOpts::default(),
    )
    .unwrap();
    let output = cli_command(&env, &engine, "cli-files")
        .arg("--save-results")
        .args(["--workers", "1", "--source"])
        .arg(env.work())
        .args([
            "--file",
            "input.txt",
            "--edit",
            "--permission-mode",
            "workspace",
            "--allow-new",
            "new.txt",
            "propose changes",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let artifact = report["workers"][0]["task"]["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["parts"][0]["data"]["kind"] == "oxa.workspace.review.v1")
        .expect("CLI review artifact");
    let review: ReviewBundle =
        serde_json::from_value(artifact["parts"][0]["data"]["review"].clone()).unwrap();
    review
        .verify(&snapshot, &[RelativePath::new("new.txt".into()).unwrap()])
        .unwrap();
    review.verify_current_base(&env.work()).unwrap();
    assert_eq!(review.changes.len(), 2);
    assert_eq!(
        std::fs::read_to_string(env.work().join("input.txt")).unwrap(),
        "source stays unchanged\n"
    );
    assert!(!env.work().join("new.txt").exists());
    // Review the real A2A report through the same default-off binary hook.
    let saved = env.root().join("cli-files/report.json");
    assert_eq!(std::fs::read(&saved).unwrap(), output.stdout);
    let review_command = || {
        let mut command = std::process::Command::new(cli_binary());
        command
            .env_clear()
            .current_dir(env.work())
            .args(["orchestrate-review", "--report"])
            .arg(&saved)
            .arg("--source")
            .arg(env.work())
            .args([
                "--file",
                "input.txt",
                "--allow-new",
                "new.txt",
                "--select",
                "worker-1",
            ]);
        command
    };
    let before_review = engine.requests().len();
    let preview = review_command().output().unwrap();
    assert!(
        preview.status.success(),
        "{}",
        String::from_utf8_lossy(&preview.stderr)
    );
    let preview: serde_json::Value = serde_json::from_slice(&preview.stdout).unwrap();
    let approval = preview["approval_sha256"].as_str().unwrap();
    assert!(preview["conflicts"].as_array().unwrap().is_empty());
    let candidate = env.root().join("reviewed-candidate");
    let rejected = review_command()
        .arg("--directory")
        .arg(&candidate)
        .args(["--approve", &"0".repeat(64)])
        .output()
        .unwrap();
    assert_eq!(rejected.status.code(), Some(1));
    assert!(!candidate.exists());
    let shared = env.root().join("replaceable-parent");
    std::fs::create_dir(&shared).unwrap();
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o770)).unwrap();
    let unsafe_parent = review_command()
        .arg("--directory")
        .arg(shared.join("candidate"))
        .args(["--approve", approval])
        .output()
        .unwrap();
    assert_eq!(unsafe_parent.status.code(), Some(1));
    assert!(!shared.join("candidate").exists());
    assert!(std::fs::read_dir(&shared).unwrap().next().is_none());
    let staged = review_command()
        .arg("--directory")
        .arg(&candidate)
        .args(["--approve", approval])
        .output()
        .unwrap();
    assert!(
        staged.status.success(),
        "{}",
        String::from_utf8_lossy(&staged.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(candidate.join("input.txt")).unwrap(),
        "proposed CLI edit\n"
    );
    assert_eq!(
        std::fs::read_to_string(candidate.join("new.txt")).unwrap(),
        "proposed new file\n"
    );
    let receipt: serde_json::Value =
        serde_json::from_slice(&std::fs::read(candidate.join(".oxa-review.json")).unwrap()).unwrap();
    assert_eq!(receipt["approval_sha256"], approval);
    let repeated = review_command()
        .arg("--directory")
        .arg(&candidate)
        .args(["--approve", approval])
        .output()
        .unwrap();
    assert_eq!(repeated.status.code(), Some(1));
    std::fs::write(env.work().join("input.txt"), "concurrent edit\n").unwrap();
    let stale = review_command()
        .arg("--directory")
        .arg(env.root().join("stale-candidate"))
        .args(["--approve", approval])
        .output()
        .unwrap();
    assert_eq!(stale.status.code(), Some(1));
    assert!(!env.root().join("stale-candidate").exists());
    assert_eq!(
        std::fs::read_to_string(env.work().join("input.txt")).unwrap(),
        "concurrent edit\n"
    );
    std::fs::write(env.work().join("input.txt"), "source stays unchanged\n").unwrap();
    // Overlaps require a new explicit selection, including identical edits.
    let mut conflicted = report.clone();
    let mut other = conflicted["workers"][0].clone();
    other["worker"] = serde_json::json!("worker-2");
    other["task"]["id"] = serde_json::json!("other-task");
    conflicted["workers"].as_array_mut().unwrap().push(other);
    std::fs::write(&saved, serde_json::to_vec(&conflicted).unwrap()).unwrap();
    let conflict = review_command().args(["--select", "worker-2"]).output().unwrap();
    assert_eq!(conflict.status.code(), Some(1));
    let conflict: serde_json::Value = serde_json::from_slice(&conflict.stdout).unwrap();
    assert_eq!(conflict["conflicts"].as_array().unwrap().len(), 2);
    assert!(conflict["candidate_manifest_sha256"].is_null());
    assert_eq!(
        before_review,
        engine.requests().len(),
        "review never contacts inference"
    );
    let count = engine.requests().len();
    let output = cli_command(&env, &engine, "cli-files-invalid")
        .args(["--source"])
        .arg(env.work())
        .args(["--file", ".env", "goal"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(!env.root().join("cli-files-invalid").exists());
    assert_eq!(count, engine.requests().len());
    std::os::unix::fs::symlink("input.txt", env.work().join("link.txt")).unwrap();
    let output = cli_command(&env, &engine, "cli-files-link")
        .args(["--source"])
        .arg(env.work())
        .args(["--file", "link.txt", "goal"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(!env.root().join("cli-files-link").exists());
    assert_eq!(count, engine.requests().len());
    let engine = FakeServer::start(
        vec![
            ScriptTurn::tool("read-2", "read", r#"{"path":"input.txt"}"#),
            ScriptTurn::text("read-only complete"),
        ],
        FakeOpts::default(),
    )
    .unwrap();
    let output = cli_command(&env, &engine, "cli-files-read")
        .args(["--workers", "1", "--source"])
        .arg(env.work())
        .args(["--file", "input.txt", "summarize"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for request in engine.requests().iter().filter(|r| r.max_tokens > 0) {
        let tools: serde_json::Value = serde_json::from_str(&request.tools_json).unwrap();
        let names: Vec<_> = tools
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["read"]);
    }
}

fn orchestrate_cli_crash_inspection() {
    use oxa_orchestrator::recovery::{InspectedRun, Phase, Transport};
    use std::os::fd::{AsRawFd, FromRawFd};
    let env = TestEnv::new();
    let engine = FakeServer::start(
        vec![ScriptTurn::text("interrupted result")],
        FakeOpts {
            hold_headers_ms: 1500,
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let mut child = CliChild(
        cli_command(&env, &engine, "cli-crash")
            .args(["--workers", "1", "synthetic crash goal"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut stderr = std::io::BufReader::new(child.0.stderr.take().unwrap());
    let mut line = String::new();
    let mut working = false;
    while stderr.read_line(&mut line).unwrap() != 0 {
        if line.contains("worker-1: working") {
            working = true;
            break;
        }
        line.clear();
    }
    assert!(working);
    let native_log = std::fs::read_dir(env.root().join("cli-crash/agents"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path()
        .join("transcript.jsonl");
    let native = InspectedRun::inspect(&native_log).unwrap();
    assert_eq!(native.snapshot().transport, Transport::LocalRpc);
    let pid = native.snapshot().workers[0].recorded_process_id.unwrap();
    // Pin the test-owned live child's identity BEFORE terminating its owner.
    // Recovery itself never opens a PID or sends a signal from journal data.
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let parent: u32 = stat
        .rsplit_once(')')
        .unwrap()
        .1
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(parent, child.0.id());
    // SAFETY: Linux pidfd_open for the verified test-owned direct child.
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    assert!(raw >= 0);
    struct Pinned(std::fs::File);
    impl Drop for Pinned {
        fn drop(&mut self) {
            // SAFETY: pinned descriptor identifies only our owned fixture process;
            // on failure this also prevents a test leak without PID reuse hazards.
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    self.0.as_raw_fd(),
                    libc::SIGKILL,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                );
            }
        }
    }
    // SAFETY: syscall returned a new owned descriptor.
    let pinned = Pinned(unsafe { std::fs::File::from_raw_fd(raw as i32) });
    child.0.kill().unwrap();
    let status = child.0.wait().unwrap();
    assert!(!status.success());
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut exited = false;
    while Instant::now() < deadline {
        let mut poll = libc::pollfd {
            fd: pinned.0.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one initialized pollfd with a live owned descriptor.
        if unsafe { libc::poll(&mut poll, 1, 0) } > 0 && poll.revents & libc::POLLIN != 0 {
            exited = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(exited, "bwrap worker exited when its controller died");
    let original = env.root().join("cli-crash/run/transcript.jsonl");
    let before = std::fs::read(&original).unwrap();
    let inspected = InspectedRun::inspect(&original).unwrap();
    assert!(!inspected.snapshot().recorded_run_finished);
    assert_eq!(inspected.snapshot().workers[0].phase, Phase::Bound);
    assert!(inspected.snapshot().workers[0].recorded_outcome.is_none());
    let count = engine.requests().len();
    let output = std::process::Command::new(cli_binary())
        .env_clear()
        .args(["orchestrate-inspect", "--journal"])
        .arg(&original)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let snapshot: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(snapshot["recorded_run_finished"], false);
    assert_eq!(snapshot["workers"][0]["phase"], "bound");
    assert!(snapshot["workers"][0]["recorded_outcome"].is_null());
    assert_eq!(std::fs::read(&original).unwrap(), before);
    assert_eq!(
        engine.requests().len(),
        count,
        "inspection never replays a task or contacts inference"
    );
}

fn assert_native_cancellation_inspection(path: &Path, prefix_path: &Path) {
    use oxa_orchestrator::recovery::{InspectedRun, Phase, Transport};
    let original = std::fs::read(path).unwrap();
    let inspected = InspectedRun::inspect(path).unwrap();
    assert_eq!(inspected.snapshot().transport, Transport::LocalRpc);
    assert!(inspected.snapshot().recorded_run_finished);
    assert_eq!(
        inspected.snapshot().workers[0].recorded_outcome,
        Some(Outcome::Canceled)
    );
    let mut end = 0;
    let mut cancellation = false;
    for line in original.split_inclusive(|b| *b == b'\n') {
        end += line.len();
        let record: serde_json::Value = serde_json::from_slice(line).unwrap();
        if record["event"] == "cancel_all" {
            cancellation = true;
            break;
        }
    }
    assert!(cancellation, "the actual native producer emitted cancel_all");
    std::fs::write(prefix_path, &original[..end]).unwrap();
    let interrupted = InspectedRun::inspect(prefix_path).unwrap();
    assert!(!interrupted.snapshot().recorded_run_finished);
    assert!(matches!(
        interrupted.snapshot().workers[0].phase,
        Phase::Queued | Phase::Started
    ));
    assert!(interrupted.snapshot().workers[0].recorded_outcome.is_none());
    assert_eq!(std::fs::read(path).unwrap(), original);
}
fn native_control_cancel_journal_inspection() {
    let env = TestEnv::new();
    let engine = FakeServer::start(vec![], FakeOpts::default()).unwrap();
    let (tx, rx) = mpsc::sync_channel(1);
    tx.send(Control::CancelAll).unwrap();
    let report = run(
        settings(&env, "native-control-cancel"),
        vec![worker(&engine, "one")],
        rx,
        |_| {},
    )
    .unwrap();
    assert_eq!(report.workers[0].outcome, Outcome::Canceled);
    assert_native_cancellation_inspection(
        &env.root().join("native-control-cancel/transcript.jsonl"),
        &env.root().join("control-cancellation-prefix.jsonl"),
    );
    assert!(engine.requests().is_empty());
}

fn owned_git_admin(root: &Path) -> std::path::PathBuf {
    let entries = std::fs::read_dir(root.join("git-workspaces"))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        entries.len(),
        1,
        "one owned repository for this approved snapshot"
    );
    entries[0].path().join("repository.git")
}
fn assert_git_worktrees(admin: &Path, count: usize) {
    let output = std::process::Command::new("/usr/bin/git")
        .env_clear()
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .arg("--git-dir")
        .arg(admin)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert_eq!(text.matches("detached").count(), count);
}
fn native_git_worktree_isolation() {
    use oxa_orchestrator::workspace::{Grant, RelativePath, Snapshot};
    let env = TestEnv::new();
    std::fs::write(env.root().join("input.txt"), "approved source\n").unwrap();
    let snapshot = Arc::new(
        Snapshot::approve(env.root(), vec![RelativePath::new("input.txt".into()).unwrap()]).unwrap(),
    );
    let grant = Arc::new(
        Grant::editing(snapshot, vec![])
            .unwrap()
            .with_git_worktrees()
            .unwrap(),
    );
    let server = FakeServer::start(
        vec![ScriptTurn::text("separate worktree result"); 2],
        FakeOpts {
            hold_headers_ms: 100,
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let mut specs = vec![worker(&server, "one"), worker(&server, "two")];
    for spec in &mut specs {
        spec.workspace = Some(grant.clone());
        spec.role = "git-metadata-probe".into();
    }
    let mut config = settings(&env, "native-worktrees");
    config.mode = PermissionMode::Workspace;
    let (_tx, rx) = mpsc::sync_channel(4);
    let report = run(config, specs, rx, |_| {}).unwrap();
    assert!(report.succeeded(), "{report:?}");
    assert_reaped(&report);
    assert_git_worktrees(&owned_git_admin(&env.root().join("native-worktrees")), 2);
    assert_eq!(
        std::fs::read_to_string(env.root().join("input.txt")).unwrap(),
        "approved source\n"
    );
}
fn orchestrate_cli_git_worktrees() {
    let env = TestEnv::new();
    std::fs::write(env.work().join("input.txt"), "approved CLI source\n").unwrap();
    let engine = FakeServer::start(
        vec![ScriptTurn::text("parallel worktree result"); 2],
        FakeOpts::default(),
    )
    .unwrap();
    let output = cli_command(&env, &engine, "cli-git-pair")
        .args(["--workers", "2", "--source"])
        .arg(env.work())
        .args([
            "--file",
            "input.txt",
            "--edit",
            "--permission-mode",
            "workspace",
            "--git-worktrees",
            "review in separate worktrees",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_git_worktrees(&owned_git_admin(&env.root().join("cli-git-pair/agents")), 2);
    let engine = FakeServer::start(
        vec![
            ScriptTurn::tool("git-read", "read", r#"{"path":".git"}"#),
            ScriptTurn::tool(
                "git-write",
                "write",
                r#"{"path":".git","content":"tamper pointer"}"#,
            ),
            ScriptTurn::tool(
                "source-write",
                "write",
                r#"{"path":"input.txt","content":"worktree proposal\n"}"#,
            ),
            ScriptTurn::text("worktree review ready"),
        ],
        FakeOpts::default(),
    )
    .unwrap();
    let output = cli_command(&env, &engine, "cli-git-edit")
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "core.fsmonitor")
        .env("GIT_CONFIG_VALUE_0", "/nonexistent-poisoned-git-command")
        .env("GIT_DIR", env.root().join("nonexistent-user-repository"))
        .env("GIT_OBJECT_DIRECTORY", env.root().join("must-not-be-used"))
        .args(["--workers", "1", "--source"])
        .arg(env.work())
        .args([
            "--file",
            "input.txt",
            "--edit",
            "--permission-mode",
            "workspace",
            "--git-worktrees",
            "propose edit",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let artifacts = report["workers"][0]["task"]["artifacts"].as_array().unwrap();
    let proposal = artifacts
        .iter()
        .find(|a| a["parts"][0]["data"]["kind"] == "oxa.workspace.review.v1")
        .unwrap();
    assert_eq!(
        proposal["parts"][0]["data"]["review"]["changes"][0]["after"],
        "worktree proposal\n"
    );
    assert_eq!(
        std::fs::read_to_string(env.work().join("input.txt")).unwrap(),
        "approved CLI source\n"
    );
    let request_text = engine
        .requests()
        .iter()
        .flat_map(|r| r.messages.iter())
        .filter_map(|m| m.content.as_deref())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!request_text.contains("gitdir: "));
    assert!(!request_text.contains("repository.git"));
    assert_git_worktrees(&owned_git_admin(&env.root().join("cli-git-edit/agents")), 1);
    assert!(!env.root().join("must-not-be-used").exists());
}

fn owned_listener(pid: u32) -> Option<std::net::SocketAddr> {
    let inodes = std::fs::read_dir(format!("/proc/{pid}/fd"))
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|f| std::fs::read_link(f.path()).ok())
        .filter_map(|path| {
            path.to_str()?
                .strip_prefix("socket:[")?
                .strip_suffix(']')
                .map(str::to_owned)
        })
        .collect::<Vec<_>>();
    let tcp = std::fs::read_to_string(format!("/proc/{pid}/net/tcp")).ok()?;
    for line in tcp.lines().skip(1) {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.len() > 9 && fields[3] == "0A" && inodes.iter().any(|i| i == fields[9]) {
            let (address, port) = fields[1].split_once(':')?;
            if address == "0100007F" {
                return Some(([127, 0, 0, 1], u16::from_str_radix(port, 16).ok()?).into());
            }
        }
    }
    None
}
fn orchestrate_cli_stalled_stderr() {
    use std::io::Read;
    use std::os::fd::{AsRawFd, FromRawFd};
    let env = TestEnv::new();
    let engine = FakeServer::start(
        vec![ScriptTurn::text("stalled status result")],
        FakeOpts {
            hold_headers_ms: 2000,
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let mut descriptors = [-1; 2];
    // SAFETY: initialized output array; both returned descriptors are owned.
    assert_eq!(
        unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) },
        0
    );
    // SAFETY: successful pipe2 returned these new, distinct descriptors.
    let _reader = unsafe { std::fs::File::from_raw_fd(descriptors[0]) };
    let mut writer = unsafe { std::fs::File::from_raw_fd(descriptors[1]) };
    // Fill the empty pipe completely before passing its BLOCKING write end to
    // the child. The reader stays open and undrained for the whole command.
    let capacity = unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_GETPIPE_SZ) };
    assert!(capacity > 0);
    writer.write_all(&vec![b'x'; capacity as usize]).unwrap();
    let original_flags = unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_GETFL) };
    assert_eq!(original_flags & libc::O_NONBLOCK, 0);
    let start = Instant::now();
    let mut child = CliChild(
        cli_command(&env, &engine, "cli-stalled-stderr")
            .args([
                "--workers",
                "1",
                "--timeout",
                "1",
                "--worker-timeout",
                "4",
                "synthetic deadline goal",
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::from(writer.try_clone().unwrap()))
            .spawn()
            .unwrap(),
    );
    let mut listener = None;
    let mut exited = None;
    while start.elapsed() < Duration::from_secs(3) {
        if listener.is_none() {
            listener = owned_listener(child.0.id());
        }
        if let Some(status) = child.0.try_wait().unwrap() {
            exited = Some(status);
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        exited
            .expect("CLI timeout must progress with a full undrained stderr pipe")
            .code(),
        Some(1)
    );
    let listener = listener.expect("observed only the owned child's loopback listener");
    assert!(
        std::net::TcpStream::connect_timeout(&listener, Duration::from_millis(50)).is_err(),
        "owned listener closed before final output"
    );
    assert_eq!(
        unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_GETFL) },
        original_flags,
        "caller stderr flags stay unchanged"
    );
    let mut bytes = Vec::new();
    child.0.stdout.take().unwrap().read_to_end(&mut bytes).unwrap();
    let report: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(report["workers"][0]["outcome"], "deadline");
    assert_eq!(report["workers"][0]["stop"], "confirmed");
    let mut processes = 0;
    for entry in std::fs::read_dir(env.root().join("cli-stalled-stderr/agents")).unwrap() {
        let log = std::fs::read_to_string(entry.unwrap().path().join("transcript.jsonl")).unwrap();
        for line in log.lines() {
            let record: serde_json::Value = serde_json::from_str(line).unwrap();
            if let Some(pid) = record["details"]["processId"].as_u64() {
                processes += 1;
                assert!(!Path::new(&format!("/proc/{pid}")).exists());
            }
        }
    }
    assert!(processes > 0);
}

fn orchestrate_cli_structural_input_limits() {
    let env = TestEnv::new();
    let engine = FakeServer::start(vec![], FakeOpts::default()).unwrap();
    let deep = (0..17).map(|i| format!("d{i}")).collect::<Vec<_>>().join("/");
    std::fs::create_dir_all(env.work().join(&deep)).unwrap();
    let deep = format!("{deep}/file.txt");
    std::fs::write(env.work().join(&deep), "approved").unwrap();
    let mut paths = Vec::new();
    for i in 0..32 {
        let directory = if i == 0 {
            "e0/p/q/extra".to_owned()
        } else {
            format!("e{i}/p/q")
        };
        std::fs::create_dir_all(env.work().join(&directory)).unwrap();
        let path = format!("{directory}/file.txt");
        std::fs::write(env.work().join(&path), "approved").unwrap();
        paths.push(path);
    }
    for (name, paths) in [("cli-deep-input", vec![deep]), ("cli-many-entries", paths)] {
        let mut command = cli_command(&env, &engine, name);
        command.arg("--source").arg(env.work());
        for path in paths {
            command.args(["--file", &path]);
        }
        let output = command.arg("synthetic structural input goal").output().unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(
            !env.root().join(name).exists(),
            "validation precedes listener/run state creation"
        );
    }
    assert!(
        engine.requests().is_empty(),
        "uncollectable input never starts a worker or probes inference"
    );
}

fn orchestrate_cli_protected_run_namespace() {
    let env = TestEnv::new();
    let engine = FakeServer::start(vec![], FakeOpts::default()).unwrap();
    let shared = env.root().join("shared-output");
    std::fs::create_dir(&shared).unwrap();
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o770)).unwrap();
    let output = cli_command(&env, &engine, "shared-output/run")
        .arg("synthetic namespace goal")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(std::fs::read_dir(&shared).unwrap().next().is_none());
    assert!(engine.requests().is_empty());
}

fn orchestrate_cli_independent_agent_registration_and_shutdown() {
    use oxa_orchestrator::{recovery::InspectedRun, registration::Registration};
    let env = TestEnv::new();
    let engine = FakeServer::start(
        vec![ScriptTurn::text("independent agent result"); 2],
        FakeOpts {
            hold_headers_ms: 400,
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let command = |name: &str, lifetime: &str| {
        let mut command = std::process::Command::new(cli_binary());
        command
            .env_clear()
            .envs(env.envs(&engine.url()))
            .current_dir(env.work())
            .args([
                "orchestrate-agent",
                "--engine",
                engine.url().trim_start_matches("http://"),
                "--model",
                "fake-model",
                "--local-only",
                "--directory",
            ])
            .arg(env.root().join(name))
            .args([
                "--parallel",
                "1",
                "--capacity",
                "4",
                "--worker-timeout",
                "5",
                "--lifetime",
                lifetime,
            ]);
        command
    };
    assert!(command("agent-help", "10")
        .arg("--help")
        .output()
        .unwrap()
        .status
        .success());
    assert!(!env.root().join("agent-help").exists());
    let missing_assertion = std::process::Command::new(cli_binary())
        .env_clear()
        .current_dir(env.work())
        .args([
            "orchestrate-agent",
            "--engine",
            "127.0.0.1:12345",
            "--model",
            "fake-model",
            "--directory",
        ])
        .arg(env.root().join("agent-unverified"))
        .output()
        .unwrap();
    assert_eq!(missing_assertion.status.code(), Some(2));
    assert!(!env.root().join("agent-unverified").exists());
    assert_eq!(
        command("agent-invalid", "10")
            .args(["--tools", "bash"])
            .output()
            .unwrap()
            .status
            .code(),
        Some(2)
    );
    assert!(!env.root().join("agent-invalid").exists());
    assert!(engine.requests().is_empty());
    let root = env.root().join("independent-agent");
    let mut host = CliChild(
        command("independent-agent", "10")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let path = root.join("registration.json");
    let deadline = Instant::now() + Duration::from_secs(4);
    let registration = loop {
        if let Ok(registration) = Registration::load(&path) {
            break registration;
        }
        assert!(
            host.0.try_wait().unwrap().is_none(),
            "host exited before private registration"
        );
        assert!(Instant::now() < deadline, "host failed to publish registration");
        std::thread::sleep(Duration::from_millis(5));
    };
    let bytes = std::fs::read(&path).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let secret = value["capability"].as_str().unwrap();
    assert!(!format!("{registration:?}").contains(secret));
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(std::fs::metadata(&root).unwrap().permissions().mode() & 0o077, 0);
    assert!(engine.requests().is_empty(), "hosting does not probe inference");
    let limits = Limits {
        timeout: Duration::from_millis(200),
        ..Limits::default()
    };
    let wrong = oxa_a2a::loopback::Client::new(
        registration.address(),
        oxa_a2a::loopback::Capability::new("x".repeat(64)).unwrap(),
        limits,
    )
    .unwrap();
    assert!(wrong.card().is_err());
    let original = registration.client(limits).unwrap();
    assert!(original.card().is_ok());
    drop(original);
    let submitted = std::process::Command::new(std::env::current_exe().unwrap())
        .env_clear()
        .current_dir(env.work())
        .arg("--fixture-registration-send")
        .arg(&path)
        .output()
        .unwrap();
    assert!(
        submitted.status.success(),
        "owned controller fixture submitted task"
    );
    assert!(!String::from_utf8_lossy(&submitted.stdout).contains(secret));
    let Event::Task(task) = serde_json::from_slice(&submitted.stdout).unwrap() else {
        panic!("expected task")
    };
    // The submitting controller process exited. The separate host owns work.
    let mut reconnected = Registration::load(&path).unwrap().client(limits).unwrap();
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        let current = reconnected.get(&task.id).unwrap();
        assert_eq!(current.context_id, task.context_id);
        if current.status.state.is_terminal() {
            assert_eq!(current.status.state, TaskState::Completed);
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(engine.requests().iter().filter(|r| r.max_tokens > 0).count(), 1);
    let Event::Task(active) = reconnected
        .send(a2a_message(
            "independent-second",
            "synthetic second delegated goal",
            None,
        ))
        .unwrap()
    else {
        panic!("expected task")
    };
    let deadline = Instant::now() + Duration::from_secs(4);
    while engine.requests().iter().filter(|r| r.max_tokens > 0).count() < 2 {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    let journal = root.join(&active.id).join("transcript.jsonl");
    let pid = InspectedRun::inspect(&journal).unwrap().snapshot().workers[0]
        .recorded_process_id
        .unwrap();
    // SAFETY: this is the still-owned, unreaped host test child.
    assert_eq!(unsafe { libc::kill(host.0.id() as i32, libc::SIGTERM) }, 0);
    let deadline = Instant::now() + Duration::from_secs(3);
    let status = loop {
        if let Some(status) = host.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "host shutdown must join its active worker"
        );
        std::thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(status.code(), Some(143));
    assert!(!Path::new(&format!("/proc/{pid}")).exists());
    assert!(reconnected.card().is_err());
    use std::io::Read;
    let mut stdout = String::new();
    host.0.stdout.take().unwrap().read_to_string(&mut stdout).unwrap();
    let mut stderr = String::new();
    host.0.stderr.take().unwrap().read_to_string(&mut stderr).unwrap();
    assert!(stdout.is_empty());
    assert!(!stderr.contains(secret));
    assert!(!std::fs::read_to_string(journal).unwrap().contains(secret));
    assert_eq!(std::fs::read(path).unwrap(), bytes);
    let mut expiring = CliChild(
        command("expiring-agent", "1")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        if let Some(status) = expiring.0.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "host lifetime must expire without a signal"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let fresh = Registration::load(&env.root().join("expiring-agent/registration.json")).unwrap();
    assert_ne!(fresh.instance_sha256(), registration.instance_sha256());
    assert!(fresh.client(limits).unwrap().card().is_err());
    assert_eq!(engine.requests().iter().filter(|r| r.max_tokens > 0).count(), 2);
}

fn orchestrate_cli_registered_controller_permissions_and_task_ownership() {
    use oxa_orchestrator::registration::{AgentHost, Registration};
    let env = TestEnv::new();
    let engine = FakeServer::start(
        vec![ScriptTurn::text("registered controller result"); 6],
        FakeOpts {
            hold_headers_ms: 600,
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let host_root = env.root().join("registered-host");
    let mut command = std::process::Command::new(cli_binary());
    command
        .env_clear()
        .envs(env.envs(&engine.url()))
        .current_dir(env.work())
        .args([
            "orchestrate-agent",
            "--engine",
            engine.url().trim_start_matches("http://"),
            "--model",
            "fake-model",
            "--local-only",
            "--directory",
        ])
        .arg(&host_root)
        .args([
            "--parallel",
            "2",
            "--capacity",
            "16",
            "--worker-timeout",
            "5",
            "--lifetime",
            "20",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut host = CliChild(command.spawn().unwrap());
    let path = host_root.join("registration.json");
    let deadline = Instant::now() + Duration::from_secs(4);
    let registration = loop {
        if let Ok(value) = Registration::load(&path) {
            break value;
        }
        assert!(host.0.try_wait().unwrap().is_none());
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    };
    registration.require_tool_free_plan().unwrap();
    let credentials = std::fs::read(&path).unwrap();
    let wire: serde_json::Value = serde_json::from_slice(&credentials).unwrap();
    let secret = wire["capability"].as_str().unwrap();
    let controller = |name: &str, file: &Path, workers: &str| {
        let mut command = std::process::Command::new(cli_binary());
        command
            .env_clear()
            .current_dir(env.work())
            .args(["orchestrate", "--registration"])
            .arg(file)
            .args(["--local-only", "--directory"])
            .arg(env.root().join(name))
            .args([
                "--workers",
                workers,
                "--parallel",
                workers,
                "--worker-timeout",
                "5",
                "--timeout",
                "10",
                "--save-results",
                "identical synthetic registered goal",
            ]);
        command
    };
    let mut seen_tasks = std::collections::BTreeSet::new();
    let mut seen_runs = std::collections::BTreeSet::new();
    for name in ["registered-run-one", "registered-run-two"] {
        let output = controller(name, &path, "2").output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(seen_runs.insert(report["run"].as_str().unwrap().to_owned()));
        for worker in report["workers"].as_array().unwrap() {
            assert_eq!(worker["outcome"], "completed");
            assert!(
                seen_tasks.insert(worker["task"]["id"].as_str().unwrap().to_owned()),
                "fresh run must not reuse an old task"
            );
        }
        let root = env.root().join(name);
        assert_eq!(std::fs::read(root.join("report.json")).unwrap(), output.stdout);
        assert!(
            !root.join("agents").exists(),
            "registered route must not launch an internal native host"
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains(secret));
        assert!(!String::from_utf8_lossy(&output.stderr).contains(secret));
        assert!(!std::fs::read_to_string(root.join("run/transcript.jsonl"))
            .unwrap()
            .contains(secret));
        assert!(host.0.try_wait().unwrap().is_none());
    }
    assert_eq!(engine.requests().iter().filter(|r| r.max_tokens > 0).count(), 4);
    let invalid = controller("registered-mixed-route", &path, "1")
        .args(["--model", "fake-model"])
        .output()
        .unwrap();
    assert_eq!(invalid.status.code(), Some(2));
    assert!(!env.root().join("registered-mixed-route").exists());
    let legacy_file = env.root().join("legacy-registration.json");
    let mut legacy = wire.clone();
    legacy.as_object_mut().unwrap().remove("policy");
    std::fs::write(&legacy_file, serde_json::to_vec(&legacy).unwrap()).unwrap();
    std::fs::set_permissions(&legacy_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        controller("registered-legacy", &legacy_file, "1")
            .output()
            .unwrap()
            .status
            .code(),
        Some(1)
    );
    assert!(!env.root().join("registered-legacy").exists());
    let config = settings(&env, "more-permissive-host");
    let template = worker(&engine, "template");
    let privileged = AgentHost::start(RpcAgentConfig {
        worker_binary: config.worker_binary,
        bwrap_binary: config.bwrap_binary,
        directory: config.directory,
        role: "synthetic workspace policy".into(),
        inference: template.inference,
        mode: PermissionMode::Workspace,
        workspace: None,
        budget: template.budget,
        max_live: 1,
        max_tasks: 1,
        journal_bytes: config.journal_bytes,
    })
    .unwrap();
    assert_eq!(
        controller("registered-widening", &privileged.registration_file(), "1")
            .output()
            .unwrap()
            .status
            .code(),
        Some(1)
    );
    assert!(!env.root().join("registered-widening").exists());
    drop(privileged);
    assert_eq!(engine.requests().iter().filter(|r| r.max_tokens > 0).count(), 4);
    let limits = Limits {
        timeout: Duration::from_millis(200),
        ..Limits::default()
    };
    let mut client = registration.client(limits).unwrap();
    let Event::Task(unrelated) = client
        .send(a2a_message(
            "unrelated-controller-job",
            "synthetic unrelated controller",
            None,
        ))
        .unwrap()
    else {
        panic!("expected task")
    };
    let mut owned = CliChild(
        controller("registered-cancel", &path, "1")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(4);
    while engine.requests().iter().filter(|r| r.max_tokens > 0).count() < 6 {
        assert!(owned.0.try_wait().unwrap().is_none());
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    // SAFETY: known owned unreaped controller child, never a historical PID.
    assert_eq!(unsafe { libc::kill(owned.0.id() as i32, libc::SIGINT) }, 0);
    let mut stdout = Vec::new();
    use std::io::Read;
    owned.0.stdout.take().unwrap().read_to_end(&mut stdout).unwrap();
    assert_eq!(owned.0.wait().unwrap().code(), Some(130));
    let canceled: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
    assert_eq!(canceled["workers"][0]["outcome"], "canceled");
    assert_eq!(canceled["workers"][0]["stop"], "confirmed");
    assert!(host.0.try_wait().unwrap().is_none());
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        let task = client.get(&unrelated.id).unwrap();
        if task.status.state.is_terminal() {
            assert_eq!(task.status.state, TaskState::Completed);
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(client.card().is_ok());
    assert_eq!(std::fs::read(&path).unwrap(), credentials);
    // SAFETY: known owned unreaped host child.
    assert_eq!(unsafe { libc::kill(host.0.id() as i32, libc::SIGTERM) }, 0);
    assert_eq!(host.0.wait().unwrap().code(), Some(143));
}

fn orchestrate_cli_report_survives_closed_stdout() {
    let env = TestEnv::new();
    let engine = FakeServer::start(
        vec![ScriptTurn::text("durable synthetic result")],
        FakeOpts::default(),
    )
    .unwrap();
    let mut child = CliChild(
        cli_command(&env, &engine, "cli-closed-stdout")
            .arg("--save-results")
            .args(["--workers", "1", "synthetic checkpoint goal"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    drop(child.0.stdout.take());
    let deadline = Instant::now() + Duration::from_secs(8);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "closed stdout must not stall cleanup");
        std::thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(status.code(), Some(1));
    let root = env.root().join("cli-closed-stdout");
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("report.json")).unwrap()).unwrap();
    assert_eq!(report["workers"][0]["outcome"], "completed");
    assert_eq!(report["workers"][0]["task"]["status"]["state"], "completed");
    assert_ne!(report["workers"][0]["stop"], "unconfirmed");
    assert!(report.to_string().contains("durable synthetic result"));
    assert!(!root.join(".report.json.pending").exists());
    for entry in std::fs::read_dir(root.join("agents")).unwrap() {
        let bytes = std::fs::read_to_string(entry.unwrap().path().join("transcript.jsonl")).unwrap();
        for line in bytes.lines() {
            let record: serde_json::Value = serde_json::from_str(line).unwrap();
            if record["event"] == "spawned" {
                let pid = record["details"]["processId"].as_u64().unwrap();
                assert!(!Path::new(&format!("/proc/{pid}")).exists());
            }
        }
    }
}

fn orchestrate_cli_result_persistence_requires_opt_in() {
    use oxa_orchestrator::recovery::InspectedRun;
    let env = TestEnv::new();
    let engine = FakeServer::start(
        vec![ScriptTurn::text("stdout-only synthetic result")],
        FakeOpts::default(),
    )
    .unwrap();
    let output = cli_command(&env, &engine, "cli-no-result-retention")
        .args(["--workers", "1", "synthetic retention goal"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["workers"][0]["outcome"], "completed");
    assert!(report.to_string().contains("stdout-only synthetic result"));
    let root = env.root().join("cli-no-result-retention");
    assert!(!root.join("report.json").exists());
    assert!(!root.join(".report.json.pending").exists());
    let entries = std::fs::read_dir(root.join("run"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    assert_eq!(entries, vec![std::ffi::OsString::from("transcript.jsonl")]);
    let journal = root.join("run/transcript.jsonl");
    let inspected = InspectedRun::inspect(&journal).unwrap();
    assert!(inspected.snapshot().recorded_run_finished);
    assert!(inspected.snapshot().workers[0].checkpoint_sha256.is_none());
    assert!(!String::from_utf8(std::fs::read(&journal).unwrap())
        .unwrap()
        .contains("stdout-only"));
}

fn orchestrate_cli_completed_checkpoint_survives_controller_crash() {
    use oxa_orchestrator::recovery::{ExclusiveRun, InspectedRun};
    let env = TestEnv::new();
    let engine = FakeServer::start(
        vec![ScriptTurn::text("checkpointed worker result"); 2],
        FakeOpts {
            hold_headers_ms: 300,
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let mut child = CliChild(
        cli_command(&env, &engine, "cli-worker-checkpoint")
            .arg("--save-results")
            .args([
                "--workers",
                "2",
                "--parallel",
                "1",
                "synthetic checkpoint crash goal",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut stderr = std::io::BufReader::new(child.0.stderr.take().unwrap());
    let mut line = String::new();
    loop {
        assert_ne!(
            stderr.read_line(&mut line).unwrap(),
            0,
            "controller finished before crash fixture"
        );
        if line.contains("worker-1: completed") {
            break;
        }
        line.clear();
    }
    // A streamed completed status can precede local checkpoint publication.
    // Kill only after observing the actual complete durable finished record.
    let root = env.root().join("cli-worker-checkpoint");
    let journal = root.join("run/transcript.jsonl");
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if InspectedRun::inspect(&journal).is_ok_and(|run| {
            run.snapshot()
                .workers
                .iter()
                .any(|worker| worker.worker == "worker-1" && worker.checkpoint_sha256.is_some())
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "first worker checkpoint was not committed"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        ExclusiveRun::claim(&journal).err().unwrap().kind(),
        std::io::ErrorKind::WouldBlock
    );
    child.0.kill().unwrap();
    assert!(!child.0.wait().unwrap().success());
    assert!(!root.join("report.json").exists());
    let before = std::fs::read(&journal).unwrap();
    let exclusive = ExclusiveRun::claim(&journal).unwrap();
    let inspected = exclusive.inspected();
    assert!(!inspected.snapshot().recorded_run_finished);
    let recovered = inspected
        .load_checkpoint("worker-1", &root.join("run/worker-worker-1.json"))
        .unwrap();
    assert_eq!(recovered.outcome, Outcome::Completed);
    assert!(serde_json::to_string(&recovered)
        .unwrap()
        .contains("checkpointed worker result"));
    assert!(inspected
        .load_checkpoint("worker-2", &root.join("run/worker-worker-1.json"))
        .is_err());
    let output = std::process::Command::new(cli_binary())
        .env_clear()
        .current_dir(env.work())
        .arg("orchestrate-inspect")
        .arg("--journal")
        .arg(&journal)
        .args(["--worker", "worker-1", "--checkpoint"])
        .arg(root.join("run/worker-worker-1.json"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["workers"].as_array().unwrap().len(), 1);
    assert_eq!(report["workers"][0], serde_json::to_value(&recovered).unwrap());
    assert_eq!(std::fs::read(&journal).unwrap(), before);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--fixture-registration-send") {
        let registration = oxa_orchestrator::registration::Registration::load(Path::new(&args[1])).unwrap();
        let mut client = registration
            .client(Limits {
                timeout: Duration::from_millis(200),
                ..Limits::default()
            })
            .unwrap();
        let task = client
            .send(a2a_message(
                "independent-first",
                "synthetic first delegated goal",
                None,
            ))
            .unwrap();
        std::io::stdout()
            .write_all(&serde_json::to_vec(&task).unwrap())
            .unwrap();
        return;
    }
    if let Some(code) = oxa_orchestrator::cli::maybe_run(&args) {
        std::process::exit(code);
    }
    if args.first().map(String::as_str) == Some("--mode") {
        assert_eq!(std::env::var("HOME").unwrap(), "/home/worker");
        assert!(std::env::var("OXA_PARENT_PRIVATE_MARKER").is_err());
        if args
            .last()
            .is_some_and(|role| role.contains("git-metadata-probe"))
        {
            assert_eq!(std::fs::read("/work/.git").unwrap(), b"");
            assert!(std::fs::write("/work/.git", b"tamper").is_err());
            assert!(!Path::new("/work/.git/objects").exists());
        }
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
        (
            "approved workspace edits and read-only policy",
            approved_workspace_edit_and_read_only_policy as fn(),
        ),
        (
            "A2A workspace review round trip",
            a2a_workspace_review_round_trip as fn(),
        ),
        (
            "native Git worktree isolation",
            native_git_worktree_isolation as fn(),
        ),
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
            "orchestrate CLI continuation lineage collection",
            a2a_scheduler_resumes_only_never_started_real_workers,
        ),
        (
            "A2A scheduler steering, cancellation and wire caps",
            a2a_scheduler_steering_cancellation_and_wire_caps,
        ),
        (
            "orchestrate CLI success and inert validation",
            orchestrate_cli_success_and_validation,
        ),
        ("orchestrate CLI signal cleanup", orchestrate_cli_signal_cleanup),
        ("orchestrate CLI approved files", orchestrate_cli_approved_files),
        (
            "orchestrate CLI crash inspection",
            orchestrate_cli_crash_inspection,
        ),
        (
            "native cancellation journal inspection",
            native_control_cancel_journal_inspection,
        ),
        ("orchestrate CLI Git worktrees", orchestrate_cli_git_worktrees),
        ("orchestrate CLI stalled stderr", orchestrate_cli_stalled_stderr),
        (
            "orchestrate CLI durable report with closed stdout",
            orchestrate_cli_report_survives_closed_stdout,
        ),
        (
            "orchestrate CLI result persistence requires opt-in",
            orchestrate_cli_result_persistence_requires_opt_in,
        ),
        (
            "orchestrate CLI completed worker checkpoint after crash",
            orchestrate_cli_completed_checkpoint_survives_controller_crash,
        ),
        (
            "orchestrate CLI structural input limits",
            orchestrate_cli_structural_input_limits,
        ),
        (
            "orchestrate CLI protected run namespace",
            orchestrate_cli_protected_run_namespace,
        ),
        (
            "orchestrate CLI independent agent registration",
            orchestrate_cli_independent_agent_registration_and_shutdown,
        ),
        (
            "orchestrate CLI registered controller isolation",
            orchestrate_cli_registered_controller_permissions_and_task_ownership,
        ),
    ] {
        if args.iter().any(|a| a == "--cli-smoke") && !name.starts_with("orchestrate CLI") {
            continue;
        }
        println!("test {name} ...");
        test();
        println!("test {name} ... ok");
    }
}
