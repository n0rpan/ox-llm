use super::*;
use crate::a2a_recovery::InspectedRun;
use crate::a2a_run::{run_a2a, LocalAgent};
use crate::supervisor::Outcome;
use oxa_a2a::{
    loopback::Limits,
    server::{error, Backend, Server, ServerLimits},
    wire::*,
};
use oxa_fake::TestEnv;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::Instant;
fn journal(root: &Path, name: &str, rows: &[Value], tail: &[u8]) -> PathBuf {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let parent = root.join(name);
    fs::DirBuilder::new().mode(0o700).create(&parent).unwrap();
    let path = parent.join("transcript.jsonl");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .unwrap();
    for row in rows {
        serde_json::to_writer(&mut file, row).unwrap();
        file.write_all(b"\n").unwrap();
    }
    file.write_all(tail).unwrap();
    path
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
    let (_controller, inbox) = crate::a2a_run::controls();
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
            inbox,
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
    use super::resume_queued;
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
        let (_controller, inbox) = crate::a2a_run::controls();
        assert!(resume_queued(&owner, &env.root().join("rejected"), vec![spec], inbox, |_| {}).is_err());
        assert!(!env.root().join("rejected").exists());
        assert!(!env.root().join("resume-parent/resume.json").exists());
    }
    let (_controller, inbox) = crate::a2a_run::controls();
    let report = resume_queued(
        &owner,
        &env.root().join("resumed"),
        vec![queued_spec("two", &b)],
        inbox,
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
    let (_controller, inbox) = crate::a2a_run::controls();
    assert!(resume_queued(
        &owner,
        &env.root().join("duplicate"),
        vec![queued_spec("two", &b)],
        inbox,
        |_| {}
    )
    .is_err());
    assert!(!env.root().join("duplicate").exists());
    assert_eq!(second.sends.load(Ordering::Relaxed), 1);
}

#[test]
fn queued_resume_refuses_ambiguous_starts_expiry_and_replay_after_child_crash() {
    use super::resume_queued;
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
    let (_controller, inbox) = crate::a2a_run::controls();
    assert!(resume_queued(
        &owner,
        &env.root().join("no-replay"),
        vec![queued_spec("two", &b)],
        inbox,
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
        Duration::from_millis(100),
    );
    std::thread::sleep(Duration::from_millis(120));
    let owner = ExclusiveRun::claim(&path).unwrap();
    let (_controller, inbox) = crate::a2a_run::controls();
    assert!(resume_queued(
        &owner,
        &env.root().join("too-late"),
        vec![queued_spec("one", &a), queued_spec("two", &b)],
        inbox,
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
    let (_controller, inbox) = crate::a2a_run::controls();
    let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        resume_queued(
            &owner,
            &env.root().join("child-crash"),
            vec![queued_spec("one", &a), queued_spec("two", &b)],
            inbox,
            |_| panic!("synthetic child controller crash before send"),
        )
    }));
    assert!(crashed.is_err());
    assert!(env.root().join("consumed/resume.json").exists());
    let (_controller, inbox) = crate::a2a_run::controls();
    assert!(resume_queued(
        &owner,
        &env.root().join("parent-replay"),
        vec![queued_spec("one", &a), queued_spec("two", &b)],
        inbox,
        |_| {}
    )
    .is_err());
    assert_eq!(second.sends.load(Ordering::Relaxed), sends);
    assert!(!env.root().join("parent-replay").exists());
}

#[test]
fn queued_resume_counts_controller_downtime_against_the_original_run_deadline() {
    use super::resume_queued;
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
    let (_controller, inbox) = crate::a2a_run::controls();
    let report = resume_queued(
        &owner,
        &env.root().join("remaining-only"),
        vec![queued_spec("one", &a), queued_spec("two", &b)],
        inbox,
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
fn missing_results_and_receipt_obstructions_fail_before_any_child_send() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let env = TestEnv::new();
    let first = Agent::new(TaskState::Completed);
    let second = Agent::new(TaskState::Completed);
    let a = Server::start(first.clone(), ServerLimits::default()).unwrap();
    let b = Server::start(second.clone(), ServerLimits::default()).unwrap();
    let path = interrupted_manifest(
        &env,
        "parent",
        (&a, &b),
        StopAt::FirstFinished,
        Duration::from_secs(10),
    );
    let owner = ExclusiveRun::claim(&path).unwrap();
    let checkpoint = env.root().join("parent/worker-one.json");
    let original = fs::read(&checkpoint).unwrap();
    fs::write(&checkpoint, b"altered").unwrap();
    let (_controller, inbox) = crate::a2a_run::controls();
    assert!(resume_queued(
        &owner,
        &env.root().join("bad-result"),
        vec![queued_spec("two", &b)],
        inbox,
        |_| {}
    )
    .is_err());
    assert!(!env.root().join("parent/resume.json").exists());
    fs::write(&checkpoint, original).unwrap();
    let target = env.write("retained", "operator-owned");
    let pending = env.root().join("parent/.resume.json.pending");
    symlink(&target, &pending).unwrap();
    let (_controller, inbox) = crate::a2a_run::controls();
    assert!(resume_queued(
        &owner,
        &env.root().join("bad-pending"),
        vec![queued_spec("two", &b)],
        inbox,
        |_| {}
    )
    .is_err());
    assert!(fs::symlink_metadata(&pending).unwrap().file_type().is_symlink());
    assert_eq!(fs::read_to_string(target).unwrap(), "operator-owned");
    fs::remove_file(pending).unwrap();
    let receipt = env.root().join("parent/resume.json");
    fs::write(&receipt, "retained receipt").unwrap();
    fs::set_permissions(&receipt, fs::Permissions::from_mode(0o600)).unwrap();
    let (_controller, inbox) = crate::a2a_run::controls();
    assert!(resume_queued(
        &owner,
        &env.root().join("bad-receipt"),
        vec![queued_spec("two", &b)],
        inbox,
        |_| {}
    )
    .is_err());
    assert_eq!(fs::read_to_string(receipt).unwrap(), "retained receipt");
    assert_eq!(first.sends.load(Ordering::Relaxed), 1);
    assert_eq!(second.sends.load(Ordering::Relaxed), 0);
    for name in ["bad-result", "bad-pending", "bad-receipt"] {
        assert!(!env.root().join(name).exists());
    }
}
#[test]
fn captured_parent_inode_survives_rename_and_exact_queue_cardinality_is_required() {
    use std::os::unix::fs::MetadataExt;
    let env = TestEnv::new();
    let first = Agent::new(TaskState::Completed);
    let second = Agent::new(TaskState::Completed);
    let a = Server::start(first.clone(), ServerLimits::default()).unwrap();
    let b = Server::start(second.clone(), ServerLimits::default()).unwrap();
    let path = interrupted_manifest(
        &env,
        "parent",
        (&a, &b),
        StopAt::FirstFinished,
        Duration::from_secs(10),
    );
    let owner = ExclusiveRun::claim(&path).unwrap();
    for plan in [
        vec![],
        vec![queued_spec("one", &a)],
        vec![queued_spec("two", &b), queued_spec("two", &b)],
    ] {
        let (_controller, inbox) = crate::a2a_run::controls();
        assert!(resume_queued(&owner, &env.root().join("rejected"), plan, inbox, |_| {}).is_err());
        assert!(!env.root().join("parent/resume.json").exists());
    }
    let moved = env.root().join("moved");
    fs::rename(env.root().join("parent"), &moved).unwrap();
    let (_controller, inbox) = crate::a2a_run::controls();
    let report = resume_queued(
        &owner,
        &env.root().join("resumed"),
        vec![queued_spec("two", &b)],
        inbox,
        |_| {},
    )
    .unwrap();
    assert!(report.succeeded());
    assert_eq!(
        fs::metadata(moved.join("resume.json")).unwrap().mode() & 0o777,
        0o600
    );
    let receipt: ResumeReceipt =
        serde_json::from_slice(&fs::read(moved.join("resume.json")).unwrap()).unwrap();
    assert_eq!(
        receipt.journal_sha256,
        owner.inspected().snapshot().journal_sha256
    );
    assert_eq!(
        receipt.limits.deadline_ns,
        owner
            .inspected()
            .snapshot()
            .resume_binding
            .as_ref()
            .unwrap()
            .deadline_ns
    );
    assert_eq!(first.sends.load(Ordering::Relaxed), 1);
    assert_eq!(second.sends.load(Ordering::Relaxed), 1);
    // The same captured parent remains consumed after rename.
    let (_controller, inbox) = crate::a2a_run::controls();
    assert!(resume_queued(
        &owner,
        &env.root().join("duplicate"),
        vec![queued_spec("two", &b)],
        inbox,
        |_| {}
    )
    .is_err());
}

#[test]
fn resuming_the_remaining_queue_does_not_upgrade_a_retained_failure() {
    let env = TestEnv::new();
    let first = Agent::new(TaskState::Failed);
    let second = Agent::new(TaskState::Completed);
    let a = Server::start(first.clone(), ServerLimits::default()).unwrap();
    let b = Server::start(second.clone(), ServerLimits::default()).unwrap();
    let path = interrupted_manifest(
        &env,
        "partial",
        (&a, &b),
        StopAt::FirstFinished,
        Duration::from_secs(10),
    );
    let owner = ExclusiveRun::claim(&path).unwrap();
    let (_controller, inbox) = crate::a2a_run::controls();
    let report = resume_queued(
        &owner,
        &env.root().join("remaining"),
        vec![queued_spec("two", &b)],
        inbox,
        |_| {},
    )
    .unwrap();
    assert!(!report.succeeded());
    assert_eq!(report.workers[0].outcome, Outcome::Failed);
    assert_eq!(report.workers[1].outcome, Outcome::Completed);
    assert_eq!(first.sends.load(Ordering::Relaxed), 1);
    assert_eq!(second.sends.load(Ordering::Relaxed), 1);
}
#[test]
fn lineage_preserves_ancestor_results_and_failed_leaf_outcomes_without_replay() {
    use crate::{lineage, resume::resume_queued};
    use std::os::unix::fs::DirBuilderExt;
    let env = TestEnv::new();
    let first = Agent::new(TaskState::Completed);
    let second = Agent::new(TaskState::Completed);
    let third = Agent::new(TaskState::Failed);
    let a = Server::start(first.clone(), ServerLimits::default()).unwrap();
    let b = Server::start(second.clone(), ServerLimits::default()).unwrap();
    let c = Server::start(third.clone(), ServerLimits::default()).unwrap();
    let root_path = env.root().join("lineage-root/transcript.jsonl");
    let (_controller, inbox) = crate::a2a_run::controls();
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
            inbox,
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
    let (_controller, inbox) = crate::a2a_run::controls();
    let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        resume_queued(
            &root,
            &env.root().join("lineage-child"),
            vec![queued_spec("two", &b), queued_spec("three", &c)],
            inbox,
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
    let (_controller, inbox) = crate::a2a_run::controls();
    let report = resume_queued(
        &child,
        &env.root().join("lineage-leaf"),
        vec![queued_spec("three", &c)],
        inbox,
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
    fs::DirBuilder::new().mode(0o700).create(&copy).unwrap();
    fs::copy(&leaf_path, copy.join("transcript.jsonl")).unwrap();
    fs::copy(
        env.root().join("lineage-leaf/worker-three.json"),
        copy.join("worker-three.json"),
    )
    .unwrap();
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
fn lineage_accepts_a_destination_spelled_with_a_current_directory_component() {
    let env = TestEnv::new();
    let first = Agent::new(TaskState::Completed);
    let second = Agent::new(TaskState::Completed);
    let a = Server::start(first.clone(), ServerLimits::default()).unwrap();
    let b = Server::start(second.clone(), ServerLimits::default()).unwrap();
    let path = interrupted_manifest(
        &env,
        "root",
        (&a, &b),
        StopAt::FirstFinished,
        Duration::from_secs(10),
    );
    let root = ExclusiveRun::claim(&path).unwrap();
    let (_controller, inbox) = crate::a2a_run::controls();
    resume_queued(
        &root,
        &env.root().join(".").join("child"),
        vec![queued_spec("two", &b)],
        inbox,
        |_| {},
    )
    .unwrap();
    let child = ExclusiveRun::claim(&env.root().join("child/transcript.jsonl")).unwrap();
    let collected = crate::lineage::collect(&[&root, &child]).unwrap();
    assert!(collected.succeeded());
    assert_eq!(collected.workers.len(), 2);
    assert_eq!(first.sends.load(Ordering::Relaxed), 1);
    assert_eq!(second.sends.load(Ordering::Relaxed), 1);
}
