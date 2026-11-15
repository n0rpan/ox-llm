use super::*;
use oxa_a2a::{
    loopback::Limits,
    server::{error, Backend, Server, ServerLimits},
    wire::*,
};
use oxa_fake::TestEnv;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};
struct Agent {
    task: Mutex<Task>,
    cards: AtomicUsize,
    sends: AtomicUsize,
    gets: AtomicUsize,
    cancels: AtomicUsize,
    missing: AtomicBool,
    wrong_context: AtomicBool,
    wrong_cancel_context: AtomicBool,
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
            wrong_cancel_context: AtomicBool::new(false),
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
        let mut reply = task.clone();
        if self.wrong_cancel_context.load(Ordering::Relaxed) {
            reply.context_id = "different-context".into();
        }
        Ok(reply)
    }
}
fn agent(server: &Server<Agent>) -> LocalAgent {
    LocalAgent::register_verified_local(server.client(Limits::default()).unwrap())
}
fn records(url: &str) -> Vec<Value> {
    let digest = hash(b"synthetic input");
    vec![
        json!({"event":"run_started","worker":null,"details":{"run":"original","transport":"A2A-0.3.0","workers":1,"ownership":crate::ownership::PROTOCOL,"persistResults":false}}),
        json!({"event":"queued","worker":"one","details":{"roleSha256":digest,"taskSha256":digest,"agentUrlSha256":hash(url.as_bytes()),"reservedBytes":128*1024}}),
        json!({"event":"starting","worker":"one","details":{}}),
        json!({"event":"task_bound","worker":"one","details":{"taskId":"task-one","contextId":"context-one"}}),
    ]
}
fn journal(root: &Path, name: &str, rows: &[Value], tail: &[u8]) -> PathBuf {
    use std::io::Write;
    let parent = root.join(name);
    fs::DirBuilder::new().mode(0o700).create(&parent).unwrap();
    let path = parent.join("transcript.jsonl");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .unwrap();
    for (i, row) in rows.iter().enumerate() {
        let mut row = row.clone();
        row["schema"] = json!(1);
        row["sequence"] = json!(i);
        row["elapsedMs"] = json!(i);
        serde_json::to_writer(&mut file, &row).unwrap();
        file.write_all(b"\n").unwrap();
    }
    file.write_all(tail).unwrap();
    path
}
fn opts(action: Action) -> ReconcileOptions {
    ReconcileOptions {
        action,
        timeout: Duration::from_millis(200),
        response_bytes: 128 * 1024,
    }
}
fn finished(rows: &mut Vec<Value>, outcome: &str, stop: &str, state: &str) {
    rows.push(json!({"event":"finished","worker":"one","details":{"outcome":outcome,"stop":stop,"taskState":state,"resultSha256":hash(b"[]")}}));
    rows.push(json!({"event":"run_finished","worker":null,"details":{"workers":1,"completed":usize::from(outcome=="completed")}}));
}
#[test]
fn observe_and_lost_cancel_response_reconcile_without_a_send_or_journal_mutation() {
    let env = TestEnv::new();
    let backend = Agent::new(TaskState::Working);
    let server = Server::start(backend.clone(), ServerLimits::default()).unwrap();
    let rows = records(&server.client(Limits::default()).unwrap().url());
    let path = journal(env.root(), "interrupted", &rows, b"{partial");
    let bytes = fs::read(&path).unwrap();
    let claim = ExclusiveRun::claim(&path).unwrap();
    assert!(claim.inspected().snapshot().truncated_tail);
    let observed = claim
        .reconcile("one", agent(&server), opts(Action::Observe))
        .unwrap();
    assert_eq!(observed.disposition, Disposition::Observed);
    assert_eq!(observed.remote_stop, StopConfirmation::Unconfirmed);
    assert_eq!(backend.cancels.load(Ordering::Relaxed), 0);
    backend.lose_cancel.store(true, Ordering::Relaxed);
    let result = claim
        .reconcile("one", agent(&server), opts(Action::Cancel))
        .unwrap();
    assert_eq!(result.disposition, Disposition::Observed);
    assert_eq!(result.remote_stop, StopConfirmation::Confirmed);
    assert_eq!(result.task.unwrap().status.state, TaskState::Canceled);
    assert_eq!(backend.cancels.load(Ordering::Relaxed), 1);
    assert_eq!(backend.sends.load(Ordering::Relaxed), 0);
    assert_eq!(fs::read(path).unwrap(), bytes);
}
#[test]
fn missing_binding_wrong_route_and_invalid_limits_make_no_agent_request() {
    let env = TestEnv::new();
    let backend = Agent::new(TaskState::Working);
    let server = Server::start(backend.clone(), ServerLimits::default()).unwrap();
    let rows = records(&server.client(Limits::default()).unwrap().url());
    let unbound =
        ExclusiveRun::claim(&journal(env.root(), "unbound", &rows[..3], b"{partial binding")).unwrap();
    assert!(unbound
        .reconcile("one", agent(&server), opts(Action::Cancel))
        .is_err());
    let bound = ExclusiveRun::claim(&journal(env.root(), "bound", &rows, b"")).unwrap();
    assert!(bound
        .reconcile("unknown", agent(&server), opts(Action::Observe))
        .is_err());
    for options in [
        ReconcileOptions {
            timeout: Duration::ZERO,
            ..opts(Action::Observe)
        },
        ReconcileOptions {
            timeout: Duration::from_secs(6),
            ..opts(Action::Observe)
        },
        ReconcileOptions {
            response_bytes: 1023,
            ..opts(Action::Observe)
        },
    ] {
        assert!(bound.reconcile("one", agent(&server), options).is_err());
    }
    assert_eq!(backend.cards.load(Ordering::Relaxed), 0);
    let other = Agent::new(TaskState::Working);
    let other_server = Server::start(other.clone(), ServerLimits::default()).unwrap();
    assert!(bound
        .reconcile("one", agent(&other_server), opts(Action::Cancel))
        .is_err());
    assert_eq!(other.cards.load(Ordering::Relaxed), 0);
    assert_eq!(backend.sends.load(Ordering::Relaxed), 0);
}
#[test]
fn mismatched_context_missing_task_and_wrong_capability_never_cancel_or_replay() {
    let env = TestEnv::new();
    let backend = Agent::new(TaskState::Working);
    let server = Server::start(backend.clone(), ServerLimits::default()).unwrap();
    let client = server.client(Limits::default()).unwrap();
    let address = server.address();
    let rows = records(&client.url());
    let claim = ExclusiveRun::claim(&journal(env.root(), "bound", &rows, b"")).unwrap();
    backend.wrong_context.store(true, Ordering::Relaxed);
    assert_eq!(
        claim
            .reconcile("one", agent(&server), opts(Action::Cancel))
            .unwrap()
            .disposition,
        Disposition::IdentityMismatch
    );
    backend.wrong_context.store(false, Ordering::Relaxed);
    backend.missing.store(true, Ordering::Relaxed);
    assert_eq!(
        claim
            .reconcile("one", agent(&server), opts(Action::Cancel))
            .unwrap()
            .disposition,
        Disposition::NotFound
    );
    let wrong = oxa_a2a::loopback::Client::new(
        address,
        oxa_a2a::loopback::Capability::generate().unwrap(),
        Limits::default(),
    )
    .unwrap();
    let result = claim
        .reconcile(
            "one",
            LocalAgent::register_verified_local(wrong),
            opts(Action::Cancel),
        )
        .unwrap();
    assert_eq!(result.disposition, Disposition::Unavailable);
    assert_eq!(result.remote_stop, StopConfirmation::Unconfirmed);
    assert_eq!(backend.cancels.load(Ordering::Relaxed), 0);
    assert_eq!(backend.sends.load(Ordering::Relaxed), 0);
}
#[test]
fn refused_cleanup_slow_peer_and_wire_caps_keep_uncertainty_and_bounds() {
    let env = TestEnv::new();
    let backend = Agent::new(TaskState::Working);
    let server = Server::start(backend.clone(), ServerLimits::default()).unwrap();
    let rows = records(&server.client(Limits::default()).unwrap().url());
    let claim = ExclusiveRun::claim(&journal(env.root(), "bound", &rows, b"")).unwrap();
    backend.refuse.store(true, Ordering::Relaxed);
    let result = claim
        .reconcile("one", agent(&server), opts(Action::Cancel))
        .unwrap();
    assert_eq!(result.disposition, Disposition::CancellationUnconfirmed);
    assert_eq!(result.remote_stop, StopConfirmation::Unconfirmed);
    backend.large.store(true, Ordering::Relaxed);
    let result = claim
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
    let result = claim
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
fn terminal_result_and_state_mismatches_are_not_accepted_or_canceled() {
    for state in [TaskState::Completed, TaskState::Working] {
        let env = TestEnv::new();
        let backend = Agent::new(state.clone());
        let server = Server::start(backend.clone(), ServerLimits::default()).unwrap();
        let mut rows = records(&server.client(Limits::default()).unwrap().url());
        finished(&mut rows, "completed", "not_needed", "completed");
        let claim = ExclusiveRun::claim(&journal(env.root(), "finished", &rows, b"")).unwrap();
        if state == TaskState::Completed {
            backend.large.store(true, Ordering::Relaxed);
        }
        let result = claim
            .reconcile("one", agent(&server), opts(Action::Cancel))
            .unwrap();
        assert_eq!(result.disposition, Disposition::ResultMismatch);
        assert_eq!(result.remote_stop, StopConfirmation::Unconfirmed);
        assert_eq!(backend.cancels.load(Ordering::Relaxed), 0);
        assert_eq!(backend.sends.load(Ordering::Relaxed), 0);
    }
}
#[test]
fn a_later_terminal_result_preserves_the_original_budget_failure() {
    let env = TestEnv::new();
    let backend = Agent::new(TaskState::Completed);
    let server = Server::start(backend.clone(), ServerLimits::default()).unwrap();
    let mut rows = records(&server.client(Limits::default()).unwrap().url());
    finished(&mut rows, "deadline", "unconfirmed", "working");
    let claim = ExclusiveRun::claim(&journal(env.root(), "late", &rows, b"")).unwrap();
    backend.large.store(true, Ordering::Relaxed);
    let result = claim
        .reconcile("one", agent(&server), opts(Action::Observe))
        .unwrap();
    assert_eq!(result.disposition, Disposition::Observed);
    assert_eq!(result.remote_stop, StopConfirmation::Confirmed);
    assert_eq!(result.recorded_outcome, Some(Outcome::Deadline));
    assert!(!result.task.unwrap().artifacts.is_empty());
    assert_eq!(backend.sends.load(Ordering::Relaxed), 0);
}

#[test]
fn a_changed_cancel_context_never_establishes_remote_stop() {
    let env = TestEnv::new();
    let backend = Agent::new(TaskState::Working);
    let server = Server::start(backend.clone(), ServerLimits::default()).unwrap();
    let rows = records(&server.client(Limits::default()).unwrap().url());
    let claim = ExclusiveRun::claim(&journal(env.root(), "changed-cancel", &rows, b"")).unwrap();
    backend.wrong_cancel_context.store(true, Ordering::Relaxed);
    let result = claim
        .reconcile("one", agent(&server), opts(Action::Cancel))
        .unwrap();
    assert_eq!(result.disposition, Disposition::IdentityMismatch);
    assert_eq!(result.remote_stop, StopConfirmation::Unconfirmed);
    assert_eq!(backend.cancels.load(Ordering::Relaxed), 1);
    assert_eq!(backend.gets.load(Ordering::Relaxed), 1);
    assert_eq!(backend.sends.load(Ordering::Relaxed), 0);
}
#[test]
fn unchanged_terminal_evidence_is_observed_without_another_cancellation() {
    let env = TestEnv::new();
    let backend = Agent::new(TaskState::Completed);
    let server = Server::start(backend.clone(), ServerLimits::default()).unwrap();
    let mut rows = records(&server.client(Limits::default()).unwrap().url());
    finished(&mut rows, "completed", "not_needed", "completed");
    let claim = ExclusiveRun::claim(&journal(env.root(), "unchanged", &rows, b"")).unwrap();
    let result = claim
        .reconcile("one", agent(&server), opts(Action::Cancel))
        .unwrap();
    assert_eq!(result.disposition, Disposition::Observed);
    assert_eq!(result.remote_stop, StopConfirmation::Confirmed);
    assert_eq!(result.recorded_outcome, Some(Outcome::Completed));
    assert_eq!(backend.cancels.load(Ordering::Relaxed), 0);
    assert_eq!(backend.sends.load(Ordering::Relaxed), 0);
}
