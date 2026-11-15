//! An A2A backend that owns bounded, isolated oxa RPC workers. Endpoint/model,
//! tools, permissions, budgets and executable paths are fixed by the operator;
//! A2A messages only provide task data and bounded steering text.
use crate::{
    Control, LocalInference, Outcome, PermissionMode, RunSettings, WorkerBudget, WorkerReport, WorkerSpec,
};
use oxa_a2a::server::{error, Backend};
use oxa_a2a::wire::*;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{DirBuilder, File};
use std::io::{self, Read};
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{self, SyncSender},
    Arc, Mutex,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct RpcAgentConfig {
    pub worker_binary: PathBuf,
    pub bwrap_binary: PathBuf,
    /// A new private directory, not an existing runtime/session directory.
    pub directory: PathBuf,
    pub role: String,
    pub inference: LocalInference,
    pub mode: PermissionMode,
    pub workspace: Option<Arc<crate::workspace::Grant>>,
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
    workspaces: Option<Arc<Mutex<crate::git_worktrees::Pool>>>,
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
    messages: BTreeMap<String, String>,
    cancel: Arc<AtomicBool>,
    controls: SyncSender<Control>,
    thread: Option<JoinHandle<()>>,
    finished: bool,
    steering: bool,
    report: Option<WorkerReport>,
}

impl RpcAgent {
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
        crate::run::validate(&settings, &[spec(&config, "synthetic preflight".into())])?;
        config.worker_binary = crate::process::checked_program(&config.worker_binary)?;
        config.bwrap_binary = crate::process::checked_program(&config.bwrap_binary)?;
        let parent = config
            .directory
            .parent()
            .ok_or_else(|| io::Error::other("missing agent directory parent"))?
            .canonicalize()?;
        let _namespace = crate::namespace::protected_directory(&parent)?;
        let name = config
            .directory
            .file_name()
            .ok_or_else(|| io::Error::other("missing agent directory name"))?;
        config.directory = parent.join(name);
        let mut random = [0u8; 16];
        File::open("/dev/urandom")?.read_exact(&mut random)?;
        let prefix = random.iter().map(|b| format!("{b:02x}")).collect();
        DirBuilder::new().mode(0o700).create(&config.directory)?;
        let workspaces = if config.workspace.as_ref().is_some_and(|w| w.uses_git_worktrees()) {
            Some(Arc::new(Mutex::new(crate::git_worktrees::Pool::new(
                config.directory.join("git-workspaces"),
            )?)))
        } else {
            None
        };
        Ok(Self {
            shutdown_gate: Mutex::new(()),
            workspaces,
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
            || state.tasks.values().filter(|e| !e.finished).count() >= self.config.max_live
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
        let (controls, receiver) = mpsc::sync_channel(32);
        let cancel = Arc::new(AtomicBool::new(false));
        state.tasks.insert(
            id.clone(),
            Entry {
                task: task.clone(),
                history: vec![MessageObject::Message(message.clone())],
                history_bytes: text.len(),
                messages: BTreeMap::from([(message.message_id.clone(), fingerprint.clone())]),
                cancel: cancel.clone(),
                controls,
                thread: None,
                finished: false,
                steering: false,
                report: None,
            },
        );
        let shared = self.state.clone();
        let workspaces = self.workspaces.clone();
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
                    crate::run::run_with_workspaces(
                        worker_settings,
                        plan,
                        receiver,
                        cancel,
                        workspaces,
                        |status| {
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
                        },
                    )
                }));
                let mut state = shared.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(entry) = state.tasks.get_mut(&task_id) {
                    match outcome {
                        Ok(Ok(mut report)) if report.workers.len() == 1 => {
                            let worker = report.workers.remove(0);
                            entry.task.status = worker.task.status.clone();
                            if worker.outcome == Outcome::Lost {
                                entry.task.status.state = TaskState::Failed;
                            }
                            entry.task.artifacts = worker.task.artifacts.clone();
                            entry.task.artifacts.push(Artifact {
                                artifact_id: format!("{task_id}-usage"),
                                name: Some("worker usage".into()),
                                description: None,
                                parts: vec![Part::Data {
                                    data: BTreeMap::from([
                                        ("reportedTokens".into(), json!(worker.reported_tokens)),
                                        ("outputBytes".into(), json!(worker.output_bytes)),
                                        ("outcome".into(), json!(worker.outcome)),
                                    ]),
                                }],
                            });
                            entry.report = Some(worker);
                        }
                        _ => {
                            entry.task.status.state = TaskState::Failed;
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
        let (sender, reply) = mpsc::sync_channel(1);
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
            if let Some(previous) = entry.messages.get(&message.message_id) {
                if previous != &fingerprint {
                    return Err(error(-32602, "Message ID reused with different data"));
                }
                return Ok(Event::Task(entry.task.clone()));
            }
            if entry.finished
                || entry.task.status.state != TaskState::Working
                || entry.steering
                || entry.cancel.load(Ordering::Acquire)
            {
                return Err(error(-32004, "Task is not accepting steering"));
            }
            if entry.history.len() >= 128 || text.len() > (64 * 1024usize).saturating_sub(entry.history_bytes)
            {
                return Err(error(-32004, "Task message limit reached"));
            }
            entry
                .controls
                .try_send(Control::SteerChecked {
                    worker: "worker".into(),
                    text: text.clone(),
                    admitted: sender,
                })
                .map_err(|_| error(-32004, "Task control queue unavailable"))?;
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
        if !admitted || entry.finished {
            return Err(error(-32004, "Task did not admit steering"));
        }
        entry.history_bytes += text.len();
        entry.messages.insert(message.message_id.clone(), fingerprint);
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
            let state = self
                .state
                .lock()
                .map_err(|_| error(-32603, "Agent state unavailable"))?;
            let entry = state
                .tasks
                .get(id)
                .ok_or_else(|| error(-32001, "Task not found"))?;
            if entry.finished {
                return Err(error(-32002, "Task not cancelable"));
            }
            entry.cancel.store(true, Ordering::Release);
        }
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            {
                let state = self
                    .state
                    .lock()
                    .map_err(|_| error(-32603, "Agent state unavailable"))?;
                let entry = &state.tasks[id];
                if entry.finished {
                    return Ok(entry.task.clone());
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
            for entry in state.tasks.values() {
                entry.cancel.store(true, Ordering::Release);
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
        directory: config.directory.join(id),
        parallelism: 1,
        mode: config.mode,
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
        budget: config.budget.clone(),
        workspace: config.workspace.clone(),
    }
}
