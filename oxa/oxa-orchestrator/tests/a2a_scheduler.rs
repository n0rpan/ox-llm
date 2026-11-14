use oxa_a2a::{
    loopback::{Capability, Client, Limits},
    mock::{Behavior, MockAgent},
    server::{error, Backend, Server, ServerLimits},
    wire::*,
};
use oxa_fake::TestEnv;
use oxa_orchestrator::{a2a_run::*, Control, Outcome};
use std::collections::BTreeMap;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc, Arc, Mutex,
};
use std::time::{Duration, Instant};

fn settings(env: &TestEnv, name: &str) -> A2ARunSettings {
    A2ARunSettings {
        id: name.into(),
        directory: env.root().join(name),
        parallelism: 2,
        wall_time: Duration::from_secs(5),
        response_reservation: 4 * 1024 * 1024,
        journal_bytes: 1024 * 1024,
        request_timeout: Duration::from_millis(100),
        poll_interval: Duration::from_millis(10),
        cancel_grace: Duration::from_millis(200),
    }
}
fn spec(client: Client, id: &str) -> A2AWorkerSpec {
    A2AWorkerSpec {
        id: id.into(),
        role: format!("role {id}"),
        task: format!("synthetic task {id}"),
        agent: LocalAgent::register_verified_local(client),
        wall_time: Duration::from_secs(2),
        response_bytes: 512 * 1024,
        reported_tokens: None,
    }
}

#[test]
fn fan_in_retains_partial_failures_and_unknown_usage() {
    let env = TestEnv::new();
    let mock = MockAgent::start(Behavior::Complete).unwrap();
    let good = spec(mock.client(Limits::default()).unwrap(), "good");
    let invalid = Client::new(mock.address(), Capability::generate().unwrap(), Limits::default()).unwrap();
    let bad = spec(invalid, "bad");
    let (_tx, rx) = mpsc::sync_channel(1);
    let report = run_a2a(
        settings(&env, "partial"),
        vec![good, bad],
        rx,
        Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    assert!(!report.succeeded());
    let good = report.workers.iter().find(|w| w.worker == "good").unwrap();
    assert_eq!(good.outcome, Outcome::Completed);
    assert!(good.task.is_some());
    assert_eq!(good.reported_tokens, None);
    let bad = report.workers.iter().find(|w| w.worker == "bad").unwrap();
    assert_eq!(bad.outcome, Outcome::Failed);
    assert!(bad.task.is_none());
    for worker in &report.workers {
        assert!(worker.response_bytes <= 512 * 1024);
    }
    let log = std::fs::read_to_string(env.root().join("partial/transcript.jsonl")).unwrap();
    assert!(!log.contains("synthetic task good"));
    assert!(log.contains("task_bound"));
}

#[test]
fn cancellation_and_reservation_validation_happen_before_task_launch() {
    let env = TestEnv::new();
    let mock = MockAgent::start(Behavior::Complete).unwrap();
    let (tx, rx) = mpsc::sync_channel(1);
    tx.send(Control::CancelAll).unwrap();
    let report = run_a2a(
        settings(&env, "canceled"),
        vec![spec(mock.client(Limits::default()).unwrap(), "one")],
        rx,
        Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    assert_eq!(report.workers[0].outcome, Outcome::Canceled);
    assert_eq!(report.workers[0].response_bytes, 0);
    assert!(report.workers[0].task.is_none());
    let mut config = settings(&env, "invalid");
    config.response_reservation = 1;
    let (_tx, rx) = mpsc::sync_channel(1);
    assert!(run_a2a(
        config,
        vec![spec(mock.client(Limits::default()).unwrap(), "one")],
        rx,
        Arc::new(AtomicBool::new(false)),
        |_| {}
    )
    .is_err());
    assert!(!env.root().join("invalid").exists());
}

#[test]
fn paused_tasks_keep_their_deadline_and_are_canceled() {
    let env = TestEnv::new();
    let mock = MockAgent::start(Behavior::AwaitInput).unwrap();
    let mut worker = spec(mock.client(Limits::default()).unwrap(), "paused");
    worker.wall_time = Duration::from_millis(150);
    let (_tx, rx) = mpsc::sync_channel(1);
    let mut paused = false;
    let report = run_a2a(
        settings(&env, "pause"),
        vec![worker],
        rx,
        Arc::new(AtomicBool::new(false)),
        |s| paused |= s.state == "input-required",
    )
    .unwrap();
    assert!(paused);
    assert_eq!(report.workers[0].outcome, Outcome::Deadline);
    assert_eq!(report.workers[0].stop, StopConfirmation::Confirmed);
    assert_eq!(
        report.workers[0].task.as_ref().unwrap().status.state,
        TaskState::Canceled
    );
}

struct Uncooperative {
    sends: AtomicUsize,
    task: Mutex<Task>,
    direct: bool,
    lose_identity: bool,
    slow_steers: bool,
    large_cancel: bool,
}
impl Uncooperative {
    fn new(direct: bool, lose_identity: bool) -> Arc<Self> {
        Arc::new(Self {
            sends: AtomicUsize::new(0),
            task: Mutex::new(Task {
                id: "known-task".into(),
                context_id: "known-context".into(),
                status: TaskStatus {
                    state: TaskState::Working,
                    message: None,
                    timestamp: None,
                },
                artifacts: vec![],
                history: vec![],
            }),
            direct,
            lose_identity,
            slow_steers: false,
            large_cancel: false,
        })
    }
}
impl Backend for Uncooperative {
    fn card(&self, endpoint: &str) -> AgentCard {
        AgentCard {
            protocol_version: PROTOCOL_VERSION.into(),
            name: "synthetic".into(),
            description: "synthetic test fixture".into(),
            url: endpoint.into(),
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
    fn send(&self, params: SendParams) -> Result<Event, RpcError> {
        self.sends.fetch_add(1, Ordering::Relaxed);
        let MessageObject::Message(message) = params.message;
        if self.slow_steers && message.task_id.is_some() {
            std::thread::sleep(Duration::from_millis(100));
        }
        if self.lose_identity {
            return Err(error(-32603, "Task observation failed"));
        }
        if self.direct {
            Ok(Event::Message(Message {
                message_id: "direct-result".into(),
                role: Role::Agent,
                parts: vec![Part::Text {
                    text: "synthetic direct result".into(),
                }],
                task_id: None,
                context_id: None,
            }))
        } else {
            Ok(Event::Task(self.task.lock().unwrap().clone()))
        }
    }
    fn get(&self, _: TaskParams) -> Result<Task, RpcError> {
        Ok(self.task.lock().unwrap().clone())
    }
    fn cancel(&self, _: &str) -> Result<Task, RpcError> {
        if self.large_cancel {
            let mut task = self.task.lock().unwrap();
            task.status.state = TaskState::Canceled;
            task.artifacts = vec![Artifact {
                artifact_id: "retained-partial".into(),
                name: None,
                description: None,
                parts: vec![Part::Text {
                    text: "x".repeat(32 * 1024),
                }],
            }];
            return Ok(task.clone());
        }
        Err(error(-32002, "Cancellation refused"))
    }
    fn shutdown(&self) {}
}

#[test]
fn cleanup_can_reuse_unspent_observation_bytes_to_verify_a_large_stop_response() {
    let env = TestEnv::new();
    let mut backend = Uncooperative::new(false, false);
    Arc::get_mut(&mut backend).unwrap().large_cancel = true;
    let server = Server::start(backend, ServerLimits::default()).unwrap();
    let (tx, rx) = mpsc::sync_channel(1);
    let mut sent = false;
    let report = run_a2a(
        settings(&env, "large-stop"),
        vec![spec(server.client(Limits::default()).unwrap(), "one")],
        rx,
        Arc::new(AtomicBool::new(false)),
        |s| {
            if !sent && s.state == "working" {
                sent = true;
                tx.send(Control::Cancel { worker: "one".into() }).unwrap();
            }
        },
    )
    .unwrap();
    assert!(sent);
    assert_eq!(report.workers[0].stop, StopConfirmation::Confirmed);
    assert_eq!(report.workers[0].outcome, Outcome::Canceled);
    assert_eq!(
        report.workers[0].task.as_ref().unwrap().status.state,
        TaskState::Canceled
    );
    assert!(report.workers[0].response_bytes <= 512 * 1024);
}

#[test]
fn slow_steering_requests_cannot_extend_the_worker_deadline() {
    let env = TestEnv::new();
    let mut backend = Uncooperative::new(false, false);
    Arc::get_mut(&mut backend).unwrap().slow_steers = true;
    let server = Server::start(backend.clone(), ServerLimits::default()).unwrap();
    let mut config = settings(&env, "slow-controls");
    config.request_timeout = Duration::from_millis(30);
    let mut worker = spec(server.client(Limits::default()).unwrap(), "one");
    worker.wall_time = Duration::from_millis(150);
    let (tx, rx) = mpsc::sync_channel(64);
    let mut sent = false;
    let started = Instant::now();
    let report = run_a2a(config, vec![worker], rx, Arc::new(AtomicBool::new(false)), |s| {
        if !sent && s.state == "working" {
            sent = true;
            for _ in 0..32 {
                tx.send(Control::Steer {
                    worker: "one".into(),
                    text: "bounded steering".into(),
                })
                .unwrap();
            }
        }
    })
    .unwrap();
    assert!(sent);
    assert_eq!(report.workers[0].outcome, Outcome::Deadline);
    assert!(started.elapsed() < Duration::from_millis(750));
    assert!(
        backend.sends.load(Ordering::Relaxed) < 16,
        "the control queue is not drained past the deadline"
    );
}

#[test]
fn unconfirmed_stops_and_lost_ids_never_become_success_or_replay() {
    for lost in [false, true] {
        let env = TestEnv::new();
        let backend = Uncooperative::new(false, lost);
        let server = Server::start(backend.clone(), ServerLimits::default()).unwrap();
        let mut worker = spec(server.client(Limits::default()).unwrap(), "one");
        worker.wall_time = Duration::from_millis(150);
        let (_tx, rx) = mpsc::sync_channel(1);
        let started = Instant::now();
        let report = run_a2a(
            settings(&env, "uncertain"),
            vec![worker],
            rx,
            Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap();
        assert!(!report.succeeded());
        assert_eq!(report.workers[0].stop, StopConfirmation::Unconfirmed);
        assert_eq!(
            backend.sends.load(Ordering::Relaxed),
            1,
            "failed observation does not repeat send"
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        if lost {
            assert_eq!(report.workers[0].outcome, Outcome::Lost);
            assert!(report.workers[0].task.is_none());
        } else {
            assert_eq!(report.workers[0].outcome, Outcome::Deadline);
            assert_eq!(
                report.workers[0].task.as_ref().unwrap().status.state,
                TaskState::Working
            );
        }
    }
}

#[test]
fn direct_messages_have_no_invented_task_and_usage_caps_require_support() {
    let env = TestEnv::new();
    let backend = Uncooperative::new(true, false);
    let server = Server::start(backend.clone(), ServerLimits::default()).unwrap();
    let (_tx, rx) = mpsc::sync_channel(1);
    let report = run_a2a(
        settings(&env, "direct"),
        vec![spec(server.client(Limits::default()).unwrap(), "direct")],
        rx,
        Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    assert!(report.succeeded());
    assert!(report.workers[0].task.is_none());
    assert!(report.workers[0].message.is_some());
    let mut capped = spec(server.client(Limits::default()).unwrap(), "capped");
    capped.reported_tokens = Some(100);
    let (_tx, rx) = mpsc::sync_channel(1);
    let report = run_a2a(
        settings(&env, "unsupported"),
        vec![capped],
        rx,
        Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    assert_eq!(report.workers[0].outcome, Outcome::PolicyViolation);
    assert_eq!(backend.sends.load(Ordering::Relaxed), 1);
}
