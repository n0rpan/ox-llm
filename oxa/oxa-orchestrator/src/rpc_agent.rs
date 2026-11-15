//! An A2A backend that owns bounded, isolated oxa RPC workers. Endpoint/model,
//! tools, permissions, budgets and executable paths are fixed by the operator;
//! A2A messages only provide task data and bounded steering text.
use crate::native::{checked_program, VerifiedLocal as LocalInference};
use crate::supervisor::{
    self, Cleanup, Controller, Outcome, Settings as RunSettings, WorkerReport, WorkerSpec,
};
use oxa_a2a::server::{error, Backend};
use oxa_a2a::wire::*;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{DirBuilder, File};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[derive(Clone)]
pub struct WorkerBudget {
    pub wall_time: Duration,
    pub output_bytes: usize,
    pub reported_tokens: u64,
}
#[derive(Clone)]
pub struct RpcAgentConfig {
    pub worker_binary: PathBuf,
    pub bwrap_binary: PathBuf,
    /// A new private directory, not an existing runtime/session directory.
    pub directory: PathBuf,
    pub role: String,
    pub inference: LocalInference,
    pub budget: WorkerBudget,
    pub max_live: usize,
    /// Includes retained terminal tasks; no silent eviction/relaunch.
    pub max_tasks: usize,
    pub journal_bytes: usize,
}

pub struct RpcAgent {
    config: RpcAgentConfig,
    prefix: String,
    state: Arc<Mutex<State>>,
    shutdown_gate: Mutex<()>,
}

struct State {
    stopping: bool,
    next_id: u64,
    tasks: BTreeMap<String, Entry>,
    initial_messages: BTreeMap<String, (String, String)>,
}

struct Entry {
    task: Task,
    history: Vec<MessageObject>,
    history_bytes: usize,
    messages: BTreeMap<String, (String, bool)>,
    canceled: bool,
    controls: Controller,
    cleanup_confirmed: bool,
    thread: Option<JoinHandle<()>>,
    finished: bool,
    steering: bool,
    report: Option<WorkerReport>,
}

impl RpcAgent {
    pub(crate) fn cleanup_confirmed(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .tasks
            .values()
            .all(|entry| entry.finished && entry.cleanup_confirmed && entry.thread.is_none())
    }
    pub fn new(mut config: RpcAgentConfig) -> io::Result<Self> {
        if config.max_live == 0
            || config.max_live > 32
            || config.max_tasks < config.max_live
            || config.max_tasks > 128
            || config.budget.output_bytes > 1024 * 1024
        {
            return Err(io::Error::other("invalid agent capacity"));
        }
        let settings = settings(&config, "preflight");
        supervisor::validate(&settings, &[spec(&config, "synthetic preflight".into())])?;
        config.worker_binary = checked_program(&config.worker_binary)?;
        config.bwrap_binary = checked_program(&config.bwrap_binary)?;
        let absolute = if config.directory.is_absolute() {
            config.directory.clone()
        } else {
            std::env::current_dir()?.join(&config.directory)
        };
        let parent = absolute
            .parent()
            .ok_or_else(|| io::Error::other("missing agent directory parent"))?;
        let namespace = crate::namespace::protected_directory(parent)?;
        let name = absolute
            .file_name()
            .ok_or_else(|| io::Error::other("missing agent directory name"))?;
        let pinned = std::path::Path::new("/proc/self/fd")
            .join(namespace.as_raw_fd().to_string())
            .join(name);
        config.directory = absolute.clone();
        let mut random = [0u8; 16];
        File::open("/dev/urandom")?.read_exact(&mut random)?;
        let prefix = random.iter().map(|b| format!("{b:02x}")).collect();
        DirBuilder::new().mode(0o700).create(&pinned)?;
        Ok(Self {
            shutdown_gate: Mutex::new(()),
            config,
            prefix,
            state: Arc::new(Mutex::new(State {
                stopping: false,
                next_id: 0,
                tasks: BTreeMap::new(),
                initial_messages: BTreeMap::new(),
            })),
        })
    }

    /// Local audit/accounting access. Provider credentials and process IDs are
    /// never added to the wire response; unknown token counts remain None.
    pub fn report(&self, id: &str) -> Option<WorkerReport> {
        self.state.lock().ok()?.tasks.get(id)?.report.clone()
    }

    fn start(&self, message: Message, text: String, fingerprint: String) -> Result<Event, RpcError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| error(-32603, "Agent state unavailable"))?;
        if state.stopping {
            return Err(error(-32004, "Agent is stopping"));
        }
        if let Some((id, hash)) = state.initial_messages.get(&message.message_id) {
            if hash != &fingerprint {
                return Err(error(-32602, "Message ID reused with different data"));
            }
            return Ok(Event::Task(state.tasks[id].task.clone()));
        }
        if state.tasks.len() == self.config.max_tasks
            || state
                .tasks
                .values()
                .filter(|e| !e.finished || !e.cleanup_confirmed)
                .count()
                >= self.config.max_live
        {
            return Err(error(-32004, "Agent task capacity reached"));
        }
        state.next_id = state
            .next_id
            .checked_add(1)
            .ok_or_else(|| error(-32603, "Task ID exhausted"))?;
        let id = format!("{}-{}", self.prefix, state.next_id);
        let task = Task {
            id: id.clone(),
            context_id: format!("{id}-context"),
            status: TaskStatus {
                state: TaskState::Submitted,
                message: None,
                timestamp: None,
            },
            artifacts: vec![],
            history: vec![],
        };
        let (controls, receiver) = supervisor::controls();
        state.tasks.insert(
            id.clone(),
            Entry {
                task: task.clone(),
                history: vec![MessageObject::Message(message.clone())],
                history_bytes: text.len(),
                messages: BTreeMap::from([(message.message_id.clone(), (fingerprint.clone(), true))]),
                canceled: false,
                cleanup_confirmed: false,
                controls,
                thread: None,
                finished: false,
                steering: false,
                report: None,
            },
        );
        let shared = self.state.clone();
        let mut worker_settings = settings(&self.config, &id);
        let accepted_deadline = Instant::now() + self.config.budget.wall_time;
        let plan = vec![spec(&self.config, text)];
        let task_id = id.clone();
        let handle = thread::Builder::new()
            .name("oxa-a2a-rpc-task".into())
            .spawn(move || {
                let observe_state = shared.clone();
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    worker_settings.wall_time = accepted_deadline
                        .saturating_duration_since(Instant::now())
                        .max(Duration::from_nanos(1));
                    supervisor::run(worker_settings, plan, receiver, |status| {
                        // Publish terminal states only after run() returns, so the
                        // final artifact and audit result cannot arrive too late.
                        if status.state == "working" {
                            if let Some(entry) = observe_state
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .tasks
                                .get_mut(&task_id)
                            {
                                entry.task.status.state = TaskState::Working;
                            }
                        }
                    })
                }));
                let mut state = shared.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(entry) = state.tasks.get_mut(&task_id) {
                    match outcome {
                        Ok(Ok(mut report)) if report.workers.len() == 1 => {
                            let worker = report.workers.remove(0);
                            entry.cleanup_confirmed = worker.cleanup != Cleanup::Unconfirmed;
                            entry.task.status.state = if !entry.cleanup_confirmed {
                                TaskState::Unknown
                            } else {
                                match worker.outcome {
                                    Outcome::Completed => TaskState::Completed,
                                    Outcome::Canceled
                                    | Outcome::Deadline
                                    | Outcome::OutputBudget
                                    | Outcome::TokenBudget => TaskState::Canceled,
                                    Outcome::PolicyViolation => TaskState::Rejected,
                                    _ => TaskState::Failed,
                                }
                            };
                            if !worker.result.is_empty() {
                                entry.task.artifacts.push(Artifact {
                                    artifact_id: format!("{task_id}-result"),
                                    name: Some("worker result".into()),
                                    description: None,
                                    parts: vec![Part::Text {
                                        text: worker.result.clone(),
                                    }],
                                });
                            }
                            entry.task.artifacts.push(Artifact {
                                artifact_id: format!("{task_id}-usage"),
                                name: Some("worker usage".into()),
                                description: None,
                                parts: vec![Part::Data {
                                    data: BTreeMap::from([
                                        ("reportedTokens".into(), json!(worker.observed_tokens)),
                                        ("outputBytes".into(), json!(worker.output_bytes)),
                                        ("outcome".into(), json!(worker.outcome)),
                                        ("cleanupConfirmed".into(), json!(entry.cleanup_confirmed)),
                                    ]),
                                }],
                            });
                            entry.report = Some(worker);
                        }
                        _ => {
                            entry.task.status.state = TaskState::Unknown;
                            entry.task.artifacts.push(Artifact {
                                artifact_id: format!("{task_id}-cleanup"),
                                name: Some("worker cleanup".into()),
                                description: None,
                                parts: vec![Part::Data {
                                    data: BTreeMap::from([
                                        ("outcome".into(), json!("supervisor_error")),
                                        ("cleanupConfirmed".into(), json!(false)),
                                    ]),
                                }],
                            });
                        }
                    }
                    entry.finished = true;
                }
            });
        match handle {
            Ok(handle) => {
                state.tasks.get_mut(&id).unwrap().thread = Some(handle);
                state
                    .initial_messages
                    .insert(message.message_id, (id, fingerprint));
                Ok(Event::Task(task))
            }
            Err(_) => {
                state.tasks.remove(&id);
                Err(error(-32603, "Cannot start task supervisor"))
            }
        }
    }

    fn steer(&self, message: Message, text: String, fingerprint: String) -> Result<Event, RpcError> {
        let id = message.task_id.as_deref().unwrap();
        let reply;
        {
            let mut state = self
                .state
                .lock()
                .map_err(|_| error(-32603, "Agent state unavailable"))?;
            if state.stopping {
                return Err(error(-32004, "Agent is stopping"));
            }
            let entry = state
                .tasks
                .get_mut(id)
                .ok_or_else(|| error(-32001, "Task not found"))?;
            if message.context_id.as_ref() != Some(&entry.task.context_id) {
                return Err(error(-32602, "Task context mismatch"));
            }
            if let Some((previous, admitted)) = entry.messages.get(&message.message_id) {
                if previous != &fingerprint {
                    return Err(error(-32602, "Message ID reused with different data"));
                }
                return if *admitted {
                    Ok(Event::Task(entry.task.clone()))
                } else {
                    Err(error(-32004, "Message admission unresolved or rejected"))
                };
            }
            if entry.finished
                || entry.task.status.state != TaskState::Working
                || entry.steering
                || entry.canceled
            {
                return Err(error(-32004, "Task is not accepting steering"));
            }
            if entry.messages.len() >= 128
                || text.len() > (64 * 1024usize).saturating_sub(entry.history_bytes)
            {
                return Err(error(-32004, "Task message limit reached"));
            }
            reply = entry
                .controls
                .steer("worker".into(), text.clone())
                .map_err(|_| error(-32004, "Task control queue unavailable"))?;
            // Reserve the ID and content budget for every queued attempt. A
            // timed-out acknowledgement must never make a retry execute twice.
            entry
                .messages
                .insert(message.message_id.clone(), (fingerprint.clone(), false));
            entry.history_bytes += text.len();
            entry.steering = true;
        }
        // Never wait for the supervisor while holding the snapshot lock.
        let admitted = reply.recv_timeout(Duration::from_millis(500)).unwrap_or(false);
        let mut state = self
            .state
            .lock()
            .map_err(|_| error(-32603, "Agent state unavailable"))?;
        let entry = state
            .tasks
            .get_mut(id)
            .ok_or_else(|| error(-32001, "Task not found"))?;
        entry.steering = false;
        if !admitted {
            return Err(error(-32004, "Task did not admit steering"));
        }
        entry
            .messages
            .insert(message.message_id.clone(), (fingerprint, true));
        entry.history.push(MessageObject::Message(message));
        Ok(Event::Task(entry.task.clone()))
    }
}

impl Backend for RpcAgent {
    fn card(&self, endpoint: &str) -> AgentCard {
        AgentCard {
            protocol_version: PROTOCOL_VERSION.into(),
            name: "oxa local RPC worker".into(),
            description: "Bounded tool-free offline oxa worker".into(),
            url: endpoint.into(),
            version: "1".into(),
            preferred_transport: "JSONRPC".into(),
            capabilities: Capabilities {
                streaming: true,
                extensions: vec![Extension {
                    uri: "urn:oxa:a2a:reported-usage:1".into(),
                    required: false,
                }],
                ..Capabilities::default()
            },
            default_input_modes: vec!["text/plain".into(), "application/json".into()],
            default_output_modes: vec!["text/plain".into(), "application/json".into()],
            skills: vec![Skill {
                id: "delegated-task".into(),
                name: "Delegated task".into(),
                description: "Return reviewed task data without local tools".into(),
                tags: vec!["local".into()],
            }],
            security_schemes: BTreeMap::new(),
            security: vec![],
        }
    }

    fn send(&self, params: SendParams) -> Result<Event, RpcError> {
        let MessageObject::Message(message) = params.message;
        if message.role != Role::User
            || message.message_id.is_empty()
            || message.message_id.len() > 256
            || message.message_id.chars().any(char::is_control)
            || message.parts.is_empty()
        {
            return Err(error(-32602, "Invalid task message"));
        }
        let text =
            serde_json::to_string(&message.parts).map_err(|_| error(-32602, "Invalid task content"))?;
        if text.len() > 32 * 1024 {
            return Err(error(-32602, "Task content limit exceeded"));
        }
        let fingerprint = format!("{:x}", Sha256::digest(text.as_bytes()));
        if message.task_id.is_some() {
            self.steer(message, text, fingerprint)
        } else if message.context_id.is_some() {
            Err(error(-32602, "Unknown task context"))
        } else {
            self.start(message, text, fingerprint)
        }
    }

    fn get(&self, params: TaskParams) -> Result<Task, RpcError> {
        let state = self
            .state
            .lock()
            .map_err(|_| error(-32603, "Agent state unavailable"))?;
        let entry = state
            .tasks
            .get(&params.id)
            .ok_or_else(|| error(-32001, "Task not found"))?;
        let mut task = entry.task.clone();
        let count = (params.history_length.unwrap_or(0) as usize).min(entry.history.len());
        task.history = entry.history[entry.history.len() - count..].to_vec();
        Ok(task)
    }

    fn cancel(&self, id: &str) -> Result<Task, RpcError> {
        {
            let mut state = self
                .state
                .lock()
                .map_err(|_| error(-32603, "Agent state unavailable"))?;
            let entry = state
                .tasks
                .get_mut(id)
                .ok_or_else(|| error(-32001, "Task not found"))?;
            if entry.finished {
                return Err(if entry.cleanup_confirmed {
                    error(-32002, "Task not cancelable")
                } else {
                    error(-32603, "Cancellation cleanup is unconfirmed")
                });
            }
            entry.canceled = true;
            entry.controls.cancel_all();
        }
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            {
                let state = self
                    .state
                    .lock()
                    .map_err(|_| error(-32603, "Agent state unavailable"))?;
                let entry = &state.tasks[id];
                if entry.finished && entry.cleanup_confirmed {
                    return Ok(entry.task.clone());
                }
                if entry.finished {
                    return Err(error(-32603, "Cancellation cleanup is unconfirmed"));
                }
            }
            if Instant::now() >= deadline {
                return Err(error(-32603, "Cancellation is not yet confirmed"));
            }
            thread::sleep(Duration::from_millis(2));
        }
    }

    fn shutdown(&self) {
        let _gate = self.shutdown_gate.lock().unwrap_or_else(|e| e.into_inner());
        let handles: Vec<_> = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.stopping = true;
            for entry in state.tasks.values_mut() {
                entry.canceled = true;
                entry.controls.cancel_all();
            }
            state.tasks.values_mut().filter_map(|e| e.thread.take()).collect()
        };
        for handle in handles {
            let _ = handle.join();
        }
    }
}

impl Drop for RpcAgent {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn settings(config: &RpcAgentConfig, id: &str) -> RunSettings {
    RunSettings {
        id: id.into(),
        worker_binary: config.worker_binary.clone(),
        bwrap_binary: config.bwrap_binary.clone(),
        journal: config.directory.join(format!("{id}.jsonl")),
        parallelism: 1,
        wall_time: config.budget.wall_time,
        output_reservation: config.budget.output_bytes,
        token_reservation: config.budget.reported_tokens,
        journal_bytes: config.journal_bytes,
    }
}

fn spec(config: &RpcAgentConfig, task: String) -> WorkerSpec {
    WorkerSpec {
        id: "worker".into(),
        role: config.role.clone(),
        task,
        inference: config.inference.clone(),
        wall_time: config.budget.wall_time,
        output_bytes: config.budget.output_bytes,
        tokens: config.budget.reported_tokens,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxa_a2a::loopback::{Client, Limits};
    use oxa_a2a::server::{Server, ServerLimits};
    use oxa_fake::TestEnv;
    use std::fs;
    fn fixture(max_live: usize, max_tasks: usize) -> (TestEnv, Arc<RpcAgent>) {
        let (env, settings, _) = crate::supervisor::tests::fixture(&["normal"]);
        let script=fs::read_to_string(&settings.worker_binary).unwrap().replace("task=request['message']","task=request['message']\n  try: task=''.join(p.get('text','') for p in json.loads(task))\n  except (ValueError,TypeError): pass");
        fs::write(&settings.worker_binary, script).unwrap();
        let config = RpcAgentConfig {
            worker_binary: settings.worker_binary,
            bwrap_binary: settings.bwrap_binary,
            directory: env.root().join("agent-host"),
            role: "operator-fixed-role".into(),
            inference: LocalInference::register("127.0.0.1:1".parse().unwrap(), "mock".into()).unwrap(),
            budget: WorkerBudget {
                wall_time: Duration::from_secs(5),
                output_bytes: 128 * 1024,
                reported_tokens: 1000,
            },
            max_live,
            max_tasks,
            journal_bytes: 64 * 1024,
        };
        (env, Arc::new(RpcAgent::new(config).unwrap()))
    }
    fn params(id: &str, text: &str, task: Option<&Task>) -> SendParams {
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
    fn task(event: Event) -> Task {
        match event {
            Event::Task(t) => t,
            _ => panic!("expected task"),
        }
    }
    fn wait(client: &mut Client, id: &str, want: TaskState) -> Task {
        let end = Instant::now() + Duration::from_secs(5);
        loop {
            let t = client.get(id).unwrap();
            if t.status.state == want {
                return t;
            }
            assert!(Instant::now() < end, "task did not reach expected state");
            thread::sleep(Duration::from_millis(2));
        }
    }
    #[test]
    fn authenticated_host_retains_idempotent_completion_and_attributed_truncation() {
        let (_env, backend) = fixture(2, 2);
        let server = Server::start(backend.clone(), ServerLimits::default()).unwrap();
        let mut client = server.client(Limits::default()).unwrap();
        let first = task(client.send(params("first", "normal", None)).unwrap());
        let duplicate = task(client.send(params("first", "normal", None)).unwrap());
        assert_eq!(first.id, duplicate.id);
        assert!(client.send(params("first", "changed", None)).is_err());
        let complete = wait(&mut client, &first.id, TaskState::Completed);
        assert!(complete.artifacts.iter().any(|a| a
            .parts
            .iter()
            .any(|p| matches!(p,Part::Text{text} if text=="reply:normal"))));
        assert_eq!(backend.report(&first.id).unwrap().cleanup, Cleanup::Stopped);
        let second = task(client.send(params("second", "length", None)).unwrap());
        let truncated = wait(&mut client, &second.id, TaskState::Failed);
        assert!(truncated.artifacts.iter().any(|a| a
            .parts
            .iter()
            .any(|p| matches!(p,Part::Text{text} if text=="attributed partial"))));
        assert_eq!(backend.report(&second.id).unwrap().outcome, Outcome::Truncated);
        assert!(client.send(params("third", "normal", None)).is_err());
        assert_eq!(backend.state.lock().unwrap().tasks.len(), 2);
    }
    #[test]
    fn steering_preserves_context_policy_and_history_without_duplicate_execution() {
        let (_env, backend) = fixture(1, 2);
        let server = Server::start(backend.clone(), ServerLimits::default()).unwrap();
        let mut client = server.client(Limits::default()).unwrap();
        let mut input = params("initial", "wait-for-steer", None);
        let MessageObject::Message(message) = &mut input.message;
        message.parts.push(Part::Data {
            data: BTreeMap::from([
                ("model".into(), json!("untrusted-model")),
                ("role".into(), json!("untrusted-role")),
            ]),
        });
        let first = task(client.send(input).unwrap());
        let working = wait(&mut client, &first.id, TaskState::Working);
        let mut wrong = params("wrong", "continue", Some(&working));
        let MessageObject::Message(m) = &mut wrong.message;
        m.context_id = Some("wrong-context".into());
        assert!(client.send(wrong).is_err());
        let steering = params("steering", "continue", Some(&working));
        client.send(steering.clone()).unwrap();
        let complete = wait(&mut client, &first.id, TaskState::Completed);
        client.send(steering).unwrap();
        assert_eq!(backend.report(&first.id).unwrap().observed_tokens, Some(19));
        let history = backend
            .get(TaskParams {
                id: first.id.clone(),
                history_length: Some(128),
            })
            .unwrap();
        assert_eq!(history.history.len(), 2);
        assert!(complete.artifacts.iter().any(|a| a
            .parts
            .iter()
            .any(|p| matches!(p,Part::Text{text} if text.contains("continue")))));
        let journal =
            fs::read_to_string(backend.config.directory.join(format!("{}.jsonl", first.id))).unwrap();
        assert!(journal.contains(&format!("{:x}", Sha256::digest(b"operator-fixed-role"))));
        assert!(!journal.contains("untrusted-model"));
    }
    #[test]
    fn cancellation_and_server_shutdown_publish_terminal_state_only_after_cleanup() {
        let (_env, backend) = fixture(2, 2);
        let server = Server::start(backend.clone(), ServerLimits::default()).unwrap();
        let mut client = server.client(Limits::default()).unwrap();
        let first = task(client.send(params("first", "wait-for-steer", None)).unwrap());
        wait(&mut client, &first.id, TaskState::Working);
        assert_eq!(
            client.cancel(&first.id).unwrap().status.state,
            TaskState::Canceled
        );
        assert_eq!(backend.report(&first.id).unwrap().cleanup, Cleanup::Stopped);
        let second = task(client.send(params("second", "wait-for-steer", None)).unwrap());
        wait(&mut client, &second.id, TaskState::Working);
        drop(server);
        assert_eq!(backend.report(&second.id).unwrap().cleanup, Cleanup::Stopped);
        assert_eq!(
            backend
                .get(TaskParams {
                    id: second.id,
                    history_length: None
                })
                .unwrap()
                .status
                .state,
            TaskState::Canceled
        );
        assert!(backend.send(params("after", "normal", None)).is_err());
    }
    #[test]
    fn infrastructure_failure_stays_nonterminal_and_retains_capacity_without_false_cancel_ack() {
        let (_env, backend) = fixture(1, 2);
        fs::remove_dir(&backend.config.directory).unwrap();
        fs::write(&backend.config.directory, "owned fixture obstruction").unwrap();
        let server = Server::start(backend.clone(), ServerLimits::default()).unwrap();
        let mut client = server.client(Limits::default()).unwrap();
        let first = task(client.send(params("first", "normal", None)).unwrap());
        let unknown = wait(&mut client, &first.id, TaskState::Unknown);
        assert!(!unknown.status.state.is_terminal());
        assert!(client.cancel(&first.id).is_err());
        assert!(backend.report(&first.id).is_none());
        assert!(client.send(params("second", "normal", None)).is_err());
    }
    #[test]
    fn unconfirmed_steering_receipt_is_reserved_and_cannot_enqueue_on_retry() {
        let (_env, backend) = fixture(1, 2);
        let (controller, _inbox) = supervisor::controls();
        let t = Task {
            id: "existing".into(),
            context_id: "context".into(),
            status: TaskStatus {
                state: TaskState::Working,
                message: None,
                timestamp: None,
            },
            artifacts: vec![],
            history: vec![],
        };
        backend.state.lock().unwrap().tasks.insert(
            t.id.clone(),
            Entry {
                task: t.clone(),
                history: vec![],
                history_bytes: 0,
                messages: BTreeMap::new(),
                canceled: false,
                controls: controller.clone(),
                cleanup_confirmed: false,
                thread: None,
                finished: false,
                steering: false,
                report: None,
            },
        );
        let message = params("uncertain", "queued steer", Some(&t));
        assert!(backend.send(message.clone()).is_err());
        assert!(backend.send(message).is_err());
        for _ in 0..31 {
            controller.cancel("worker".into()).unwrap();
        }
        assert_eq!(
            controller.cancel("worker".into()).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        let state = backend.state.lock().unwrap();
        assert_eq!(state.tasks[&t.id].messages.len(), 1);
    }
}
