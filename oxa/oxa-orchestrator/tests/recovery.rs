use oxa_a2a::{
    loopback::Limits,
    server::{error, Backend, Server, ServerLimits},
    wire::*,
};
use oxa_fake::TestEnv;
use oxa_orchestrator::{a2a_run::*, recovery::*, Outcome};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc, Arc, Mutex,
};
use std::time::{Duration, Instant};
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn record(sequence: usize, worker: Option<&str>, event: &str, details: Value) -> Value {
    json!({"schema":1,"sequence":sequence,"elapsedMs":sequence,"worker":worker,"event":event,"details":details})
}
fn records(url: &str) -> Vec<Value> {
    vec![
        record(
            0,
            None,
            "run_started",
            json!({"run":"original","transport":"A2A-0.3.0","workers":1}),
        ),
        record(
            1,
            Some("one"),
            "queued",
            json!({"agentUrlSha256":hash(url.as_bytes())}),
        ),
        record(2, Some("one"), "starting", json!({})),
        record(
            3,
            Some("one"),
            "task_bound",
            json!({"taskId":"task-one","contextId":"context-one"}),
        ),
    ]
}
fn journal(root: &Path, name: &str, records: &[Value], tail: &[u8]) -> PathBuf {
    let path = root.join(name);
    let mut bytes = Vec::new();
    for r in records {
        bytes.extend(serde_json::to_vec(r).unwrap());
        bytes.push(b'\n');
    }
    bytes.extend(tail);
    fs::write(&path, bytes).unwrap();
    path
}
fn opts(action: Action) -> ReconcileOptions {
    ReconcileOptions {
        action,
        timeout: Duration::from_millis(200),
        response_bytes: 128 * 1024,
    }
}
struct Agent {
    task: Mutex<Task>,
    cards: AtomicUsize,
    sends: AtomicUsize,
    gets: AtomicUsize,
    cancels: AtomicUsize,
    missing: AtomicBool,
    wrong_context: AtomicBool,
    refuse: AtomicBool,
    lose_cancel: AtomicBool,
    slow: AtomicBool,
    large: AtomicBool,
}
impl Agent {
    fn new(state: TaskState) -> Arc<Self> {
        Arc::new(Self {
            task: Mutex::new(Task {
                id: "task-one".into(),
                context_id: "context-one".into(),
                status: TaskStatus {
                    state,
                    message: None,
                    timestamp: None,
                },
                artifacts: vec![],
                history: vec![],
            }),
            cards: AtomicUsize::new(0),
            sends: AtomicUsize::new(0),
            gets: AtomicUsize::new(0),
            cancels: AtomicUsize::new(0),
            missing: AtomicBool::new(false),
            wrong_context: AtomicBool::new(false),
            refuse: AtomicBool::new(false),
            lose_cancel: AtomicBool::new(false),
            slow: AtomicBool::new(false),
            large: AtomicBool::new(false),
        })
    }
}
impl Backend for Agent {
    fn shutdown(&self) {}
    fn card(&self, url: &str) -> AgentCard {
        self.cards.fetch_add(1, Ordering::Relaxed);
        AgentCard {
            protocol_version: PROTOCOL_VERSION.into(),
            name: "recovery fixture".into(),
            description: "synthetic".into(),
            url: url.into(),
            version: "1".into(),
            preferred_transport: "JSONRPC".into(),
            capabilities: Capabilities::default(),
            default_input_modes: vec!["text/plain".into()],
            default_output_modes: vec!["text/plain".into()],
            skills: vec![],
            security_schemes: BTreeMap::new(),
            security: vec![],
        }
    }
    fn send(&self, _: SendParams) -> Result<Event, RpcError> {
        self.sends.fetch_add(1, Ordering::Relaxed);
        Ok(Event::Task(self.task.lock().unwrap().clone()))
    }
    fn get(&self, _: TaskParams) -> Result<Task, RpcError> {
        self.gets.fetch_add(1, Ordering::Relaxed);
        if self.slow.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(300));
        }
        if self.missing.load(Ordering::Relaxed) {
            return Err(error(-32001, "Task not found"));
        }
        let mut task = self.task.lock().unwrap().clone();
        if self.wrong_context.load(Ordering::Relaxed) {
            task.context_id = "different-context".into();
        }
        if self.large.load(Ordering::Relaxed) {
            task.artifacts.push(Artifact {
                artifact_id: "large".into(),
                parts: vec![Part::Text {
                    text: "x".repeat(64 * 1024),
                }],
                name: None,
                description: None,
            });
        }
        Ok(task)
    }
    fn cancel(&self, _: &str) -> Result<Task, RpcError> {
        self.cancels.fetch_add(1, Ordering::Relaxed);
        if self.refuse.load(Ordering::Relaxed) {
            return Err(error(-32002, "Task not cancelable"));
        }
        let mut task = self.task.lock().unwrap();
        task.status.state = TaskState::Canceled;
        if self.lose_cancel.load(Ordering::Relaxed) {
            return Err(error(-32603, "Synthetic response loss"));
        }
        Ok(task.clone())
    }
}
fn agent(server: &Server<Agent>) -> LocalAgent {
    LocalAgent::register_verified_local(server.client(Limits::default()).unwrap())
}

fn queued_spec(id: &str, server: &Server<Agent>) -> A2AWorkerSpec {
    A2AWorkerSpec {
        id: id.into(),
        role: format!("synthetic {id} role"),
        task: format!("synthetic {id} task"),
        agent: agent(server),
        wall_time: Duration::from_secs(2),
        response_bytes: 128 * 1024,
        reported_tokens: None,
    }
}
#[derive(Clone, Copy)]
enum StopAt {
    Manifest,
    FirstFinished,
    SecondBound,
}
fn interrupted_manifest(
    env: &TestEnv,
    name: &str,
    servers: (&Server<Agent>, &Server<Agent>),
    stop: StopAt,
    wall_time: Duration,
) -> PathBuf {
    let directory = env.root().join(name);
    let journal = directory.join("transcript.jsonl");
    let (_tx, rx) = mpsc::sync_channel(2);
    let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_a2a(
            A2ARunSettings {
                id: "resume-original".into(),
                directory,
                persist_results: true,
                parallelism: 1,
                wall_time,
                response_reservation: 256 * 1024,
                journal_bytes: 128 * 1024,
                request_timeout: Duration::from_millis(100),
                poll_interval: Duration::from_millis(5),
                cancel_grace: Duration::from_millis(100),
            },
            vec![queued_spec("one", servers.0), queued_spec("two", servers.1)],
            rx,
            Arc::new(AtomicBool::new(false)),
            |status| {
                let captured = InspectedRun::inspect(&journal).unwrap();
                let stop_now = match stop {
                    StopAt::Manifest => status.worker == "two" && status.state == "queued",
                    StopAt::FirstFinished => captured
                        .snapshot()
                        .workers
                        .iter()
                        .any(|w| w.worker == "one" && w.phase == Phase::Finished),
                    StopAt::SecondBound => captured
                        .snapshot()
                        .workers
                        .iter()
                        .any(|w| w.worker == "two" && w.phase == Phase::Bound),
                };
                if stop_now {
                    panic!("synthetic controller failure at a known durable boundary");
                }
            },
        )
    }));
    assert!(failed.is_err());
    journal
}

#[test]
fn queued_resume_preserves_original_inputs_results_deadline_and_one_time_consumption() {
    use oxa_orchestrator::resume::resume_queued;
    let env = TestEnv::new();
    let first = Agent::new(TaskState::Completed);
    let second = Agent::new(TaskState::Completed);
    let a = Server::start(first.clone(), ServerLimits::default()).unwrap();
    let b = Server::start(second.clone(), ServerLimits::default()).unwrap();
    let path = interrupted_manifest(
        &env,
        "resume-parent",
        (&a, &b),
        StopAt::FirstFinished,
        Duration::from_secs(10),
    );
    let bytes = fs::read(&path).unwrap();
    let original_records = bytes
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    for case in 0..4 {
        let mut bad = original_records.clone();
        match case {
            0 => bad[0]["details"]["resumeBinding"]["version"] = json!(2),
            1 => bad[0]["details"]["resumeBinding"]["response_reservation"] = json!(1),
            2 => bad[1]["details"]["resumeBinding"]["endpoint_sha256"] = json!("f".repeat(64)),
            _ => {
                bad[1]["details"].as_object_mut().unwrap().remove("resumeBinding");
            }
        }
        assert!(InspectedRun::inspect(&journal(
            env.root(),
            &format!("bad-resume-binding-{case}"),
            &bad,
            b""
        ))
        .is_err());
    }
    let owner = ExclusiveRun::claim(&path).unwrap();
    let parent = owner.inspected().snapshot().resume_binding.clone().unwrap();
    assert_eq!(first.sends.load(Ordering::Relaxed), 1);
    assert_eq!(second.sends.load(Ordering::Relaxed), 0);
    for change in 0..7 {
        let mut spec = queued_spec("two", &b);
        match change {
            0 => spec.role.push_str(" changed"),
            1 => spec.task.push_str(" changed"),
            2 => spec.wall_time += Duration::from_secs(1),
            3 => spec.response_bytes += 1,
            4 => spec.reported_tokens = Some(1),
            5 => spec.agent = agent(&a),
            _ => spec.id = "one".into(),
        }
        let (_tx, rx) = mpsc::sync_channel(2);
        assert!(resume_queued(
            &owner,
            &env.root().join("rejected"),
            vec![spec],
            rx,
            Arc::new(AtomicBool::new(false)),
            |_| {}
        )
        .is_err());
        assert!(!env.root().join("rejected").exists());
        assert!(!env.root().join("resume-parent/resume.json").exists());
    }
    let (_tx, rx) = mpsc::sync_channel(2);
    let report = resume_queued(
        &owner,
        &env.root().join("resumed"),
        vec![queued_spec("two", &b)],
        rx,
        Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    assert!(report.succeeded());
    assert_eq!(
        report
            .workers
            .iter()
            .map(|w| w.worker.as_str())
            .collect::<Vec<_>>(),
        vec!["one", "two"]
    );
    assert_eq!(
        first.sends.load(Ordering::Relaxed),
        1,
        "finished worker must not be sent again"
    );
    assert_eq!(second.sends.load(Ordering::Relaxed), 1);
    assert_eq!(fs::read(&path).unwrap(), bytes);
    let child = InspectedRun::inspect(&env.root().join("resumed/transcript.jsonl")).unwrap();
    let inherited = child.snapshot().resume_binding.as_ref().unwrap();
    assert_eq!(inherited.started_ns, parent.started_ns);
    assert_eq!(inherited.deadline_ns, parent.deadline_ns);
    assert_eq!(inherited.response_reservation, 128 * 1024);
    assert_ne!(child.snapshot().run, report.run);
    assert!(env.root().join("resume-parent/resume.json").exists());
    drop(owner);
    let owner = ExclusiveRun::claim(&path).unwrap();
    let (_tx, rx) = mpsc::sync_channel(2);
    assert!(resume_queued(
        &owner,
        &env.root().join("duplicate"),
        vec![queued_spec("two", &b)],
        rx,
        Arc::new(AtomicBool::new(false)),
        |_| {}
    )
    .is_err());
    assert!(!env.root().join("duplicate").exists());
    assert_eq!(second.sends.load(Ordering::Relaxed), 1);
}

#[test]
fn queued_resume_refuses_ambiguous_starts_expiry_and_replay_after_child_crash() {
    use oxa_orchestrator::resume::resume_queued;
    let env = TestEnv::new();
    let first = Agent::new(TaskState::Completed);
    let second = Agent::new(TaskState::Working);
    let a = Server::start(first.clone(), ServerLimits::default()).unwrap();
    let b = Server::start(second.clone(), ServerLimits::default()).unwrap();
    let path = interrupted_manifest(
        &env,
        "ambiguous",
        (&a, &b),
        StopAt::SecondBound,
        Duration::from_secs(10),
    );
    let owner = ExclusiveRun::claim(&path).unwrap();
    let sends = second.sends.load(Ordering::Relaxed);
    let (_tx, rx) = mpsc::sync_channel(2);
    assert!(resume_queued(
        &owner,
        &env.root().join("no-replay"),
        vec![queued_spec("two", &b)],
        rx,
        Arc::new(AtomicBool::new(false)),
        |_| {}
    )
    .is_err());
    assert_eq!(second.sends.load(Ordering::Relaxed), sends);
    assert!(!env.root().join("ambiguous/resume.json").exists());
    let path = interrupted_manifest(
        &env,
        "expired",
        (&a, &b),
        StopAt::Manifest,
        Duration::from_millis(20),
    );
    std::thread::sleep(Duration::from_millis(30));
    let owner = ExclusiveRun::claim(&path).unwrap();
    let (_tx, rx) = mpsc::sync_channel(2);
    assert!(resume_queued(
        &owner,
        &env.root().join("too-late"),
        vec![queued_spec("one", &a), queued_spec("two", &b)],
        rx,
        Arc::new(AtomicBool::new(false)),
        |_| {}
    )
    .is_err());
    assert!(!env.root().join("expired/resume.json").exists());
    let path = interrupted_manifest(
        &env,
        "consumed",
        (&a, &b),
        StopAt::Manifest,
        Duration::from_secs(10),
    );
    let owner = ExclusiveRun::claim(&path).unwrap();
    let (_tx, rx) = mpsc::sync_channel(2);
    let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        resume_queued(
            &owner,
            &env.root().join("child-crash"),
            vec![queued_spec("one", &a), queued_spec("two", &b)],
            rx,
            Arc::new(AtomicBool::new(false)),
            |_| panic!("synthetic child controller crash before send"),
        )
    }));
    assert!(crashed.is_err());
    assert!(env.root().join("consumed/resume.json").exists());
    let (_tx, rx) = mpsc::sync_channel(2);
    assert!(resume_queued(
        &owner,
        &env.root().join("parent-replay"),
        vec![queued_spec("one", &a), queued_spec("two", &b)],
        rx,
        Arc::new(AtomicBool::new(false)),
        |_| {}
    )
    .is_err());
    assert_eq!(second.sends.load(Ordering::Relaxed), sends);
    assert!(!env.root().join("parent-replay").exists());
}

#[test]
fn queued_resume_counts_controller_downtime_against_the_original_run_deadline() {
    use oxa_orchestrator::resume::resume_queued;
    let env = TestEnv::new();
    let first = Agent::new(TaskState::Working);
    let second = Agent::new(TaskState::Working);
    let a = Server::start(first.clone(), ServerLimits::default()).unwrap();
    let b = Server::start(second.clone(), ServerLimits::default()).unwrap();
    let path = interrupted_manifest(
        &env,
        "downtime",
        (&a, &b),
        StopAt::Manifest,
        Duration::from_millis(600),
    );
    std::thread::sleep(Duration::from_millis(400));
    let owner = ExclusiveRun::claim(&path).unwrap();
    let remaining = owner
        .inspected()
        .snapshot()
        .resume_binding
        .as_ref()
        .unwrap()
        .remaining()
        .unwrap();
    assert!(remaining < Duration::from_millis(300));
    let started = Instant::now();
    let (_tx, rx) = mpsc::sync_channel(2);
    let report = resume_queued(
        &owner,
        &env.root().join("remaining-only"),
        vec![queued_spec("one", &a), queued_spec("two", &b)],
        rx,
        Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    assert!(started.elapsed() < remaining + Duration::from_millis(250));
    assert!(report.workers.iter().all(|w| w.outcome == Outcome::Deadline));
    assert_eq!(first.sends.load(Ordering::Relaxed), 1);
    assert_eq!(second.sends.load(Ordering::Relaxed), 0);
    assert_eq!(first.task.lock().unwrap().status.state, TaskState::Canceled);
}

#[test]
fn lineage_preserves_ancestor_results_and_failed_leaf_outcomes_without_replay() {
    use oxa_orchestrator::{lineage, resume::resume_queued};
    let env = TestEnv::new();
    let first = Agent::new(TaskState::Completed);
    let second = Agent::new(TaskState::Completed);
    let third = Agent::new(TaskState::Failed);
    let a = Server::start(first.clone(), ServerLimits::default()).unwrap();
    let b = Server::start(second.clone(), ServerLimits::default()).unwrap();
    let c = Server::start(third.clone(), ServerLimits::default()).unwrap();
    let root_path = env.root().join("lineage-root/transcript.jsonl");
    let (_tx, rx) = mpsc::sync_channel(2);
    let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_a2a(
            A2ARunSettings {
                id: "lineage-root".into(),
                directory: env.root().join("lineage-root"),
                persist_results: true,
                parallelism: 1,
                wall_time: Duration::from_secs(10),
                response_reservation: 3 * 128 * 1024,
                journal_bytes: 128 * 1024,
                request_timeout: Duration::from_millis(100),
                poll_interval: Duration::from_millis(5),
                cancel_grace: Duration::from_millis(100),
            },
            vec![
                queued_spec("one", &a),
                queued_spec("two", &b),
                queued_spec("three", &c),
            ],
            rx,
            Arc::new(AtomicBool::new(false)),
            |_| {
                if InspectedRun::inspect(&root_path)
                    .unwrap()
                    .snapshot()
                    .workers
                    .iter()
                    .any(|w| w.worker == "one" && w.phase == Phase::Finished)
                {
                    panic!("synthetic root failure after one");
                }
            },
        )
    }));
    assert!(failed.is_err());
    let root = ExclusiveRun::claim(&root_path).unwrap();
    let child_path = env.root().join("lineage-child/transcript.jsonl");
    let (_tx, rx) = mpsc::sync_channel(2);
    let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        resume_queued(
            &root,
            &env.root().join("lineage-child"),
            vec![queued_spec("two", &b), queued_spec("three", &c)],
            rx,
            Arc::new(AtomicBool::new(false)),
            |_| {
                if InspectedRun::inspect(&child_path)
                    .unwrap()
                    .snapshot()
                    .workers
                    .iter()
                    .any(|w| w.worker == "two" && w.phase == Phase::Finished)
                {
                    panic!("synthetic child failure after two");
                }
            },
        )
    }));
    assert!(failed.is_err());
    let child = ExclusiveRun::claim(&child_path).unwrap();
    assert!(
        lineage::collect(&[&root, &child]).is_err(),
        "an unresolved worker cannot disappear from fan-in"
    );
    let (_tx, rx) = mpsc::sync_channel(2);
    let report = resume_queued(
        &child,
        &env.root().join("lineage-leaf"),
        vec![queued_spec("three", &c)],
        rx,
        Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    assert_eq!(
        report.workers.len(),
        2,
        "child-local report intentionally excludes its ancestor"
    );
    let leaf_path = env.root().join("lineage-leaf/transcript.jsonl");
    let leaf = ExclusiveRun::claim(&leaf_path).unwrap();
    let root_bytes = fs::read(&root_path).unwrap();
    let child_bytes = fs::read(&child_path).unwrap();
    let leaf_bytes = fs::read(&leaf_path).unwrap();
    let collected = lineage::collect(&[&root, &child, &leaf]).unwrap();
    assert_eq!(collected.run, "lineage-root");
    assert_eq!(collected.workers.len(), 3);
    assert!(!collected.succeeded(), "failed leaf outcome must remain failed");
    assert_eq!(
        collected
            .workers
            .iter()
            .filter(|w| w.outcome == Outcome::Completed)
            .count(),
        2
    );
    assert_eq!(
        collected
            .workers
            .iter()
            .find(|w| w.worker == "three")
            .unwrap()
            .outcome,
        Outcome::Failed
    );
    assert!(lineage::collect(&[&root, &leaf]).is_err());
    assert!(lineage::collect(&[&root, &child, &child]).is_err());
    assert!(lineage::collect(&[&leaf, &child, &root]).is_err());
    let receipt_path = env.root().join("lineage-root/resume.json");
    let receipt_bytes = fs::read(&receipt_path).unwrap();
    for case in 0..5 {
        let mut bad: Value = serde_json::from_slice(&receipt_bytes).unwrap();
        match case {
            0 => bad["child_run"] = json!("different-child"),
            1 => bad["destination_sha256"] = json!("0".repeat(64)),
            2 => bad["workers"] = json!(["two"]),
            3 => bad["limits"]["deadline_ns"] = json!(1),
            _ => bad["journal_sha256"] = json!("0".repeat(64)),
        }
        fs::write(&receipt_path, serde_json::to_vec(&bad).unwrap()).unwrap();
        assert!(lineage::collect(&[&root, &child, &leaf]).is_err());
    }
    fs::write(&receipt_path, &receipt_bytes).unwrap();
    // A copied journal with identical run identity cannot replace the directory
    // explicitly bound by the durable parent receipt.
    let copy = env.root().join("copied-leaf");
    fs::create_dir(&copy).unwrap();
    fs::copy(&leaf_path, copy.join("transcript.jsonl")).unwrap();
    let copied = ExclusiveRun::claim(&copy.join("transcript.jsonl")).unwrap();
    assert!(lineage::collect(&[&root, &child, &copied]).is_err());
    drop(leaf);
    // Complete durable worker finishes suffice even if the final whole-run
    // marker is torn. No unresolved worker is manufactured or omitted.
    fs::write(&leaf_path, &leaf_bytes[..leaf_bytes.len() - 1]).unwrap();
    let torn_leaf = ExclusiveRun::claim(&leaf_path).unwrap();
    assert!(torn_leaf.inspected().snapshot().truncated_tail);
    assert!(!torn_leaf.inspected().snapshot().recorded_run_finished);
    assert_eq!(
        lineage::collect(&[&root, &child, &torn_leaf])
            .unwrap()
            .workers
            .len(),
        3
    );
    drop(torn_leaf);
    fs::write(&leaf_path, &leaf_bytes).unwrap();
    for backend in [&first, &second, &third] {
        assert_eq!(backend.sends.load(Ordering::Relaxed), 1);
    }
    assert_eq!(fs::read(root_path).unwrap(), root_bytes);
    assert_eq!(fs::read(child_path).unwrap(), child_bytes);
    assert_eq!(fs::read(leaf_path).unwrap(), leaf_bytes);
}
#[test]
fn complete_scheduler_journal_can_recover_results_without_resending() {
    let env = TestEnv::new();
    let backend = Agent::new(TaskState::Completed);
    let server = Server::start(backend.clone(), ServerLimits::default()).unwrap();
    let settings = A2ARunSettings {
        persist_results: true,
        id: "actual".into(),
        directory: env.root().join("actual"),
        parallelism: 1,
        wall_time: Duration::from_secs(3),
        response_reservation: 128 * 1024,
        journal_bytes: 128 * 1024,
        request_timeout: Duration::from_millis(100),
        poll_interval: Duration::from_millis(5),
        cancel_grace: Duration::from_millis(100),
    };
    let spec = A2AWorkerSpec {
        id: "one".into(),
        role: "reader".into(),
        task: "synthetic task".into(),
        agent: agent(&server),
        wall_time: Duration::from_secs(2),
        response_bytes: 128 * 1024,
        reported_tokens: None,
    };
    let (_tx, rx) = mpsc::sync_channel(2);
    let live_path = settings.directory.join("transcript.jsonl");
    let report = run_a2a(
        settings.clone(),
        vec![spec],
        rx,
        Arc::new(AtomicBool::new(false)),
        |_| {
            assert_eq!(
                ExclusiveRun::claim(&live_path).err().unwrap().kind(),
                std::io::ErrorKind::WouldBlock
            );
            assert!(InspectedRun::inspect(&live_path).is_ok());
        },
    )
    .unwrap();
    assert!(report.succeeded());
    let path = env.root().join("actual/transcript.jsonl");
    let before = fs::read(&path).unwrap();
    let inspected = InspectedRun::inspect(&path).unwrap();
    assert!(inspected.snapshot().recorded_run_finished);
    assert!(inspected.snapshot().exclusive_ownership_supported);
    let exclusive = ExclusiveRun::claim(&path).unwrap();
    assert_eq!(
        exclusive.inspected().snapshot().journal_sha256,
        inspected.snapshot().journal_sha256
    );
    assert_eq!(
        ExclusiveRun::claim(&path).err().unwrap().kind(),
        std::io::ErrorKind::WouldBlock
    );
    drop(exclusive);
    drop(ExclusiveRun::claim(&path).unwrap());
    assert_eq!(inspected.snapshot().workers[0].phase, Phase::Finished);
    let checkpoint = env.root().join("actual/worker-one.json");
    let checkpoint_bytes = fs::read(&checkpoint).unwrap();
    let recovered = inspected.load_checkpoint("one", &checkpoint).unwrap();
    assert_eq!(
        serde_json::to_value(&recovered).unwrap(),
        serde_json::to_value(&report.workers[0]).unwrap()
    );
    assert_eq!(
        inspected.snapshot().workers[0].checkpoint_sha256.as_deref(),
        Some(hash(&checkpoint_bytes).as_str())
    );
    fs::write(&checkpoint, b"modified").unwrap();
    assert!(inspected.load_checkpoint("one", &checkpoint).is_err());
    fs::write(&checkpoint, &checkpoint_bytes).unwrap();
    let linked = env.root().join("linked-checkpoint");
    fs::hard_link(&checkpoint, &linked).unwrap();
    assert!(inspected.load_checkpoint("one", &checkpoint).is_err());
    fs::remove_file(&linked).unwrap();
    std::os::unix::fs::symlink(&checkpoint, &linked).unwrap();
    assert!(inspected.load_checkpoint("one", &linked).is_err());
    // A published file alone is not sufficient. Simulate a crash before its
    // finished journal line, including a valid-looking but unterminated line.
    let lines = before.split_inclusive(|b| *b == b'\n').collect::<Vec<_>>();
    let finish = lines
        .iter()
        .position(|line| serde_json::from_slice::<Value>(line).unwrap()["event"] == "finished")
        .unwrap();
    let mut interrupted = lines[..finish].concat();
    interrupted.extend_from_slice(&lines[finish][..lines[finish].len() - 1]);
    let prefix = env.root().join("before-finished.jsonl");
    fs::write(&prefix, interrupted).unwrap();
    assert!(InspectedRun::inspect(&prefix)
        .unwrap()
        .load_checkpoint("one", &checkpoint)
        .is_err());
    // A durable worker finish survives even without a whole-run finish.
    fs::write(&prefix, lines[..=finish].concat()).unwrap();
    let interrupted = InspectedRun::inspect(&prefix).unwrap();
    assert!(!interrupted.snapshot().recorded_run_finished);
    assert_eq!(
        interrupted.load_checkpoint("one", &checkpoint).unwrap().outcome,
        Outcome::Completed
    );
    let result = inspected
        .reconcile("one", agent(&server), opts(Action::Observe))
        .unwrap();
    assert_eq!(result.disposition, Disposition::Observed);
    assert_eq!(result.remote_stop, StopConfirmation::Confirmed);
    assert_eq!(result.recorded_outcome, Some(Outcome::Completed));
    assert!(result.task.is_some());
    assert!(result.response_bytes > 0);
    assert_eq!(backend.sends.load(Ordering::Relaxed), 1);
    assert_eq!(fs::read(&path).unwrap(), before);
    // A terminal peer cannot silently replace recorded artifacts.
    backend.large.store(true, Ordering::Relaxed);
    let result = inspected
        .reconcile("one", agent(&server), opts(Action::Cancel))
        .unwrap();
    assert_eq!(result.disposition, Disposition::ResultMismatch);
    assert_eq!(backend.cancels.load(Ordering::Relaxed), 0);
    backend.large.store(false, Ordering::Relaxed);
    backend.task.lock().unwrap().status.state = TaskState::Working;
    let regressed = inspected
        .reconcile("one", agent(&server), opts(Action::Cancel))
        .unwrap();
    assert_eq!(regressed.disposition, Disposition::ResultMismatch);
    assert_eq!(backend.cancels.load(Ordering::Relaxed), 0);

    // Inject an existing checkpoint at setup. Publication must fail without a
    // durable finish, overwriting prior data or sending the task again.
    let mut fault = settings;
    fault.directory = env.root().join("checkpoint-failure");
    let destination = fault.directory.join("worker-one.json");
    backend.task.lock().unwrap().status.state = TaskState::Completed;
    let spec = A2AWorkerSpec {
        id: "one".into(),
        role: "reader".into(),
        task: "synthetic task".into(),
        agent: agent(&server),
        wall_time: Duration::from_secs(2),
        response_bytes: 128 * 1024,
        reported_tokens: None,
    };
    let (_tx, rx) = mpsc::sync_channel(2);
    let result = run_a2a(
        fault,
        vec![spec],
        rx,
        Arc::new(AtomicBool::new(false)),
        |status| {
            if status.state == "queued" {
                fs::write(&destination, b"preserved existing data").unwrap();
            }
        },
    );
    assert!(result.is_err());
    assert_eq!(fs::read(&destination).unwrap(), b"preserved existing data");
    assert_eq!(backend.sends.load(Ordering::Relaxed), 2);
    let failed = InspectedRun::inspect(&env.root().join("checkpoint-failure/transcript.jsonl")).unwrap();
    assert!(!failed.snapshot().recorded_run_finished);
    assert_ne!(failed.snapshot().workers[0].phase, Phase::Finished);
    assert!(failed.load_checkpoint("one", &destination).is_err());
}
#[test]
fn interrupted_bound_tasks_can_be_observed_or_explicitly_canceled() {
    let env = TestEnv::new();
    let backend = Agent::new(TaskState::Working);
    let server = Server::start(backend.clone(), ServerLimits::default()).unwrap();
    let records = records(&server.client(Limits::default()).unwrap().url());
    let path = journal(
        env.root(),
        "interrupted.jsonl",
        &records,
        b"{\"schema\":1,\"sequence\":4",
    );
    let inspected = InspectedRun::inspect(&path).unwrap();
    assert!(!inspected.snapshot().exclusive_ownership_supported);
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(
        ExclusiveRun::claim(&path)
            .err()
            .unwrap()
            .to_string()
            .contains("legacy journal"),
        "legacy journals never prove controller exclusion"
    );
    assert!(inspected.snapshot().truncated_tail);
    assert!(!inspected.snapshot().recorded_run_finished);
    assert_eq!(inspected.snapshot().workers[0].phase, Phase::Bound);
    let observed = inspected
        .reconcile("one", agent(&server), opts(Action::Observe))
        .unwrap();
    assert_eq!(observed.disposition, Disposition::Observed);
    assert_eq!(observed.remote_stop, StopConfirmation::Unconfirmed);
    assert_eq!(backend.cancels.load(Ordering::Relaxed), 0);
    backend.lose_cancel.store(true, Ordering::Relaxed);
    let canceled = inspected
        .reconcile("one", agent(&server), opts(Action::Cancel))
        .unwrap();
    assert_eq!(canceled.disposition, Disposition::Observed);
    assert_eq!(canceled.remote_stop, StopConfirmation::Confirmed);
    assert_eq!(canceled.task.unwrap().status.state, TaskState::Canceled);
    assert_eq!(backend.sends.load(Ordering::Relaxed), 0);
    assert_eq!(backend.cancels.load(Ordering::Relaxed), 1);
}
#[test]
fn missing_identity_routes_and_peer_mismatches_never_trigger_replay_or_cancel() {
    let env = TestEnv::new();
    let backend = Agent::new(TaskState::Working);
    let server = Server::start(backend.clone(), ServerLimits::default()).unwrap();
    let records = records(&server.client(Limits::default()).unwrap().url());
    let path = journal(
        env.root(),
        "unbound",
        &records[..3],
        serde_json::to_string(&records[3]).unwrap().as_bytes(),
    );
    let inspected = InspectedRun::inspect(&path).unwrap();
    assert_eq!(inspected.snapshot().workers[0].phase, Phase::Started);
    assert!(inspected
        .reconcile("one", agent(&server), opts(Action::Cancel))
        .is_err());
    assert_eq!(backend.cards.load(Ordering::Relaxed), 0);
    let mut old = records.clone();
    old[1]["details"] = json!({});
    let old = InspectedRun::inspect(&journal(env.root(), "old", &old, b"")).unwrap();
    assert!(old
        .reconcile("one", agent(&server), opts(Action::Observe))
        .is_err());
    assert_eq!(backend.cards.load(Ordering::Relaxed), 0);
    let inspected = InspectedRun::inspect(&journal(env.root(), "bound", &records, b"")).unwrap();
    let other = Agent::new(TaskState::Working);
    let other_server = Server::start(other.clone(), ServerLimits::default()).unwrap();
    assert!(inspected
        .reconcile("one", agent(&other_server), opts(Action::Cancel))
        .is_err());
    assert_eq!(other.cards.load(Ordering::Relaxed), 0);
    backend.wrong_context.store(true, Ordering::Relaxed);
    let result = inspected
        .reconcile("one", agent(&server), opts(Action::Cancel))
        .unwrap();
    assert_eq!(result.disposition, Disposition::IdentityMismatch);
    assert_eq!(backend.cancels.load(Ordering::Relaxed), 0);
    backend.wrong_context.store(false, Ordering::Relaxed);
    backend.missing.store(true, Ordering::Relaxed);
    let result = inspected
        .reconcile("one", agent(&server), opts(Action::Cancel))
        .unwrap();
    assert_eq!(result.disposition, Disposition::NotFound);
    assert_eq!(result.remote_stop, StopConfirmation::Unconfirmed);
    assert_eq!(backend.sends.load(Ordering::Relaxed), 0);
}
#[test]
fn refused_cancellation_deadlines_and_wire_caps_remain_unconfirmed() {
    let env = TestEnv::new();
    let backend = Agent::new(TaskState::Working);
    let server = Server::start(backend.clone(), ServerLimits::default()).unwrap();
    let records = records(&server.client(Limits::default()).unwrap().url());
    let inspected = InspectedRun::inspect(&journal(env.root(), "bound", &records, b"")).unwrap();
    backend.refuse.store(true, Ordering::Relaxed);
    let result = inspected
        .reconcile("one", agent(&server), opts(Action::Cancel))
        .unwrap();
    assert_eq!(result.disposition, Disposition::CancellationUnconfirmed);
    assert_eq!(result.remote_stop, StopConfirmation::Unconfirmed);
    backend.large.store(true, Ordering::Relaxed);
    let result = inspected
        .reconcile(
            "one",
            agent(&server),
            ReconcileOptions {
                response_bytes: 1024,
                ..opts(Action::Observe)
            },
        )
        .unwrap();
    assert_ne!(result.disposition, Disposition::Observed);
    assert!(result.response_bytes <= 1024);
    assert_eq!(result.remote_stop, StopConfirmation::Unconfirmed);
    backend.large.store(false, Ordering::Relaxed);
    backend.slow.store(true, Ordering::Relaxed);
    let started = Instant::now();
    let result = inspected
        .reconcile(
            "one",
            agent(&server),
            ReconcileOptions {
                timeout: Duration::from_millis(60),
                ..opts(Action::Observe)
            },
        )
        .unwrap();
    assert!(started.elapsed() < Duration::from_millis(250));
    assert_ne!(result.disposition, Disposition::Observed);
    assert_eq!(backend.sends.load(Ordering::Relaxed), 0);
}
#[test]
fn journal_corruption_links_and_lifecycle_inconsistency_fail_closed() {
    let env = TestEnv::new();
    let records = records("http://127.0.0.1:12345/");
    for (name, mut bad) in [
        ("sequence", records.clone()),
        ("schema", records.clone()),
        ("ownership", records.clone()),
        ("transition", records.clone()),
        ("unknown", records.clone()),
    ] {
        match name {
            "sequence" => bad[2]["sequence"] = json!(4),
            "schema" => bad[0]["schema"] = json!(2),
            "ownership" => bad[0]["details"]["ownership"] = json!("future-lock-protocol"),
            "transition" => bad[2]["event"] = json!("task_bound"),
            _ => bad[2]["event"] = json!("new_unsupported_event"),
        };
        assert!(InspectedRun::inspect(&journal(env.root(), name, &bad, b"")).is_err());
    }
    let path = journal(env.root(), "valid", &records, b"");
    std::os::unix::fs::symlink(&path, env.root().join("link")).unwrap();
    assert!(InspectedRun::inspect(&env.root().join("link")).is_err());
    fs::hard_link(&path, env.root().join("hard")).unwrap();
    assert!(InspectedRun::inspect(&path).is_err());
    let queued = InspectedRun::inspect(&journal(env.root(), "queued", &records[..2], b"")).unwrap();
    assert_eq!(queued.snapshot().workers[0].phase, Phase::Queued);
    assert!(queued.snapshot().workers[0].binding.is_none());
    let mut unlisted = records[..1].to_vec();
    unlisted[0]["details"]["workers"] = json!(2);
    let inspected = InspectedRun::inspect(&journal(env.root(), "unlisted", &unlisted, b"")).unwrap();
    assert_eq!(inspected.snapshot().unlisted_workers, 2);
}

#[test]
fn later_terminal_results_preserve_the_original_budget_failure() {
    let env = TestEnv::new();
    let backend = Agent::new(TaskState::Completed);
    let server = Server::start(backend.clone(), ServerLimits::default()).unwrap();
    let mut records = records(&server.client(Limits::default()).unwrap().url());
    records.push(record(
        4,
        Some("one"),
        "finished",
        json!({"outcome":"deadline","stop":"unconfirmed","taskState":"working","resultSha256":hash(b"[]")}),
    ));
    records.push(record(
        5,
        None,
        "run_finished",
        json!({"workers":1,"completed":0}),
    ));
    let inspected = InspectedRun::inspect(&journal(env.root(), "late-result", &records, b"")).unwrap();
    backend.large.store(true, Ordering::Relaxed);
    let result = inspected
        .reconcile("one", agent(&server), opts(Action::Observe))
        .unwrap();
    assert_eq!(result.disposition, Disposition::Observed);
    assert_eq!(result.remote_stop, StopConfirmation::Confirmed);
    assert_eq!(result.recorded_outcome, Some(Outcome::Deadline));
    assert!(!result.task.unwrap().artifacts.is_empty());
    assert_eq!(backend.sends.load(Ordering::Relaxed), 0);
}

#[test]
fn cancellation_events_require_the_native_run_level_and_complete_manifest() {
    let env = TestEnv::new();
    let mut a2a = records("http://127.0.0.1:12345/");
    a2a.push(record(4, None, "cancel_all", json!({})));
    assert!(InspectedRun::inspect(&journal(env.root(), "a2a-cancel", &a2a, b"")).is_err());
    let native = vec![
        record(0, None, "run_started", json!({"run":"native","workers":1})),
        record(1, Some("one"), "queued", json!({})),
        record(2, Some("one"), "cancel_all", json!({})),
    ];
    assert!(InspectedRun::inspect(&journal(env.root(), "worker-cancel", &native, b"")).is_err());
    let incomplete = vec![
        record(0, None, "run_started", json!({"run":"native","workers":2})),
        record(1, Some("one"), "queued", json!({})),
        record(2, None, "cancel_all", json!({})),
    ];
    assert!(InspectedRun::inspect(&journal(env.root(), "incomplete-cancel", &incomplete, b"")).is_err());
}
