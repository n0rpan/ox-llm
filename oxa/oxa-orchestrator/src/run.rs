use crate::journal::Journal;
use crate::process::{checked_program, invalid, Process};
use oxa_a2a::wire::{Artifact, Part, Task, TaskState, TaskStatus};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, VecDeque};
use std::fs::DirBuilder;
use std::io;
use std::net::SocketAddr;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

mod task_wire {
    use oxa_a2a::wire::{Event, Task};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(task: &Task, serializer: S) -> Result<S::Ok, S::Error> {
        Event::Task(task.clone()).serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Task, D::Error> {
        match Event::deserialize(deserializer)? {
            Event::Task(task) => Ok(task),
            _ => Err(serde::de::Error::custom("expected A2A task")),
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PermissionMode {
    Plan,
    Workspace,
    Ask,
}

impl PermissionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Workspace => "workspace",
            Self::Ask => "ask",
        }
    }
}

/// Operator registration, never a property learned from an agent card.
/// The public local constructor requires the caller to establish that the route
/// cannot forward to paid inference; literal loopback alone is insufficient.
/// It always launches offline. The historical type name is retained
/// for API compatibility; only private operator profiles can grant a gateway.
#[derive(Clone, Debug)]
pub struct LocalInference {
    address: SocketAddr,
    model: String,
    local_only: bool,
}

impl LocalInference {
    pub fn register_verified_local(address: SocketAddr, model: String) -> io::Result<Self> {
        Self::validate_route(address, &model)?;
        Ok(Self {
            address,
            model,
            local_only: true,
        })
    }
    pub(crate) fn validate_route(address: SocketAddr, model: &str) -> io::Result<()> {
        if !address.ip().is_loopback()
            || address.port() == 0
            || model.is_empty()
            || model.len() > 256
            || model.chars().any(char::is_control)
        {
            return Err(invalid("invalid verified local inference route"));
        }
        Ok(())
    }
    #[cfg(feature = "interactive-control")]
    pub(crate) fn register_operator_gateway(address: SocketAddr, model: String) -> io::Result<Self> {
        if oxa_proto::offline() {
            return Err(invalid("offline mode refuses cloud worker inference"));
        }
        Self::validate_route(address, &model)?;
        Ok(Self {
            address,
            model,
            local_only: false,
        })
    }
}

#[derive(Clone, Debug)]
pub struct WorkerBudget {
    /// Includes startup, queueing within the child, steering and shutdown.
    pub wall_time: Duration,
    /// Hard bound on accepted stdout + stderr bytes, including protocol records.
    pub output_bytes: usize,
    /// Observed cumulative usage cap. This cannot preempt inference before the
    /// engine reports usage, and must not be used as a hard monetary ceiling.
    pub reported_tokens: u64,
}

#[derive(Clone, Debug)]
pub struct WorkerSpec {
    pub id: String,
    pub role: String,
    pub task: String,
    pub inference: LocalInference,
    pub budget: WorkerBudget,
    /// Immutable operator-approved snapshot; never derived from task messages.
    pub workspace: Option<Arc<crate::workspace::Grant>>,
}

#[derive(Clone, Debug)]
pub struct RunSettings {
    pub id: String,
    pub worker_binary: PathBuf,
    pub bwrap_binary: PathBuf,
    /// Must not already exist; private audit storage is created here.
    pub directory: PathBuf,
    pub parallelism: usize,
    pub mode: PermissionMode,
    pub wall_time: Duration,
    pub output_reservation: usize,
    pub token_reservation: u64,
    pub journal_bytes: usize,
}

#[derive(Clone, Debug)]
pub enum Control {
    Steer {
        worker: String,
        text: String,
    },
    /// Acknowledge admission to the bounded RPC queue, not model consumption.
    SteerChecked {
        worker: String,
        text: String,
        admitted: SyncSender<bool>,
    },
    Cancel {
        worker: String,
    },
    CancelAll,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Completed,
    Failed,
    Canceled,
    Deadline,
    OutputBudget,
    TokenBudget,
    PolicyViolation,
    Lost,
}

impl Outcome {
    fn task_state(&self) -> TaskState {
        match self {
            Self::Completed => TaskState::Completed,
            Self::Canceled | Self::Deadline | Self::OutputBudget | Self::TokenBudget => TaskState::Canceled,
            Self::PolicyViolation => TaskState::Rejected,
            Self::Failed => TaskState::Failed,
            Self::Lost => TaskState::Unknown,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkerReport {
    pub worker: String,
    pub outcome: Outcome,
    #[serde(with = "task_wire")]
    pub task: Task,
    pub reported_tokens: Option<u64>,
    pub output_bytes: usize,
    pub process_id: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunReport {
    pub run: String,
    pub workers: Vec<WorkerReport>,
}

impl RunReport {
    /// All-required fan-in; callers can separately inspect every partial result.
    pub fn succeeded(&self) -> bool {
        !self.workers.is_empty() && self.workers.iter().all(|w| w.outcome == Outcome::Completed)
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct WorkerStatus {
    pub worker: String,
    pub state: String,
    pub reported_tokens: Option<u64>,
    pub output_bytes: usize,
}

/// Fan out through bounded, supervised processes; fan in without hiding partial
/// failures. `observe` is a trusted, nonblocking status callback, never model
/// code. Controls should come from a bounded sync_channel owned by the caller.
/// No cloud route, filesystem tools, parent transcript or parent grants are
/// exposed by this initial profile. Offline is always enabled in every child.
pub fn run(
    settings: RunSettings,
    plan: Vec<WorkerSpec>,
    controls: Receiver<Control>,
    observe: impl FnMut(WorkerStatus),
) -> io::Result<RunReport> {
    run_cancellable(
        settings,
        plan,
        controls,
        Arc::new(AtomicBool::new(false)),
        observe,
    )
}

/// An independent cancellation flag cannot be starved by a full control queue.
pub fn run_cancellable(
    settings: RunSettings,
    plan: Vec<WorkerSpec>,
    controls: Receiver<Control>,
    canceled: Arc<AtomicBool>,
    observe: impl FnMut(WorkerStatus),
) -> io::Result<RunReport> {
    run_with_workspaces(settings, plan, controls, canceled, None, observe)
}
pub(crate) fn run_with_workspaces(
    settings: RunSettings,
    plan: Vec<WorkerSpec>,
    controls: Receiver<Control>,
    canceled: Arc<AtomicBool>,
    shared_workspaces: Option<Arc<std::sync::Mutex<crate::git_worktrees::Pool>>>,
    mut observe: impl FnMut(WorkerStatus),
) -> io::Result<RunReport> {
    validate(&settings, &plan)?;
    let worker_binary = checked_program(&settings.worker_binary)?;
    let bwrap_binary = checked_program(&settings.bwrap_binary)?;
    let started = Instant::now();
    let run_deadline = started
        .checked_add(settings.wall_time)
        .ok_or_else(|| invalid("invalid run deadline"))?;
    let parent = settings
        .directory
        .parent()
        .ok_or_else(|| invalid("missing run directory parent"))?
        .canonicalize()?;
    let _namespace = crate::namespace::protected_directory(&parent)?;
    let name = settings
        .directory
        .file_name()
        .ok_or_else(|| invalid("missing run directory name"))?;
    let directory = parent.join(name);
    DirBuilder::new().mode(0o700).create(&directory)?;
    let mut journal = Journal::create(&directory.join("transcript.jsonl"), settings.journal_bytes)?;
    journal.record(None, "run_started", json!({"run":settings.id,"workers":plan.len(),"parallelism":settings.parallelism,"mode":settings.mode,"offline":true,"toolPolicy":"explicit-per-worker-grants"}))?;
    for spec in &plan {
        journal.record(Some(&spec.id), "queued", json!({"roleSha256":hash(&spec.role),"taskSha256":hash(&spec.task),"route":"verified-local","reservedTokens":spec.budget.reported_tokens,"reservedBytes":spec.budget.output_bytes,"tools":spec.workspace.as_ref().map(|w|w.tools()).unwrap_or(&[])}))?;
        observe(status(spec, "queued", None, 0));
    }
    let workspaces = if plan
        .iter()
        .any(|s| s.workspace.as_ref().is_some_and(|w| w.uses_git_worktrees()))
    {
        Some(match shared_workspaces {
            Some(pool) => pool,
            None => Arc::new(std::sync::Mutex::new(crate::git_worktrees::Pool::new(
                directory.join("git-workspaces"),
            )?)),
        })
    } else {
        None
    };
    let mut pending = VecDeque::from(plan);
    let mut preparing = Preparations(Vec::new());
    let mut ready = VecDeque::<Ready>::new();
    let mut active: Vec<Worker> = Vec::new();
    let mut reports = Vec::new();
    let mut cancel_all = false;
    while !pending.is_empty() || !active.is_empty() || !preparing.0.is_empty() || !ready.is_empty() {
        if canceled.load(Ordering::Acquire) && !cancel_all {
            cancel_all = true;
            journal.record(None, "cancel_all", json!({}))?;
        }
        for _ in 0..32 {
            match controls.try_recv() {
                Ok(Control::CancelAll) => {
                    cancel_all = true;
                    journal.record(None, "cancel_all", json!({}))?;
                }
                Ok(Control::Cancel { worker }) => {
                    if let Some(index) = pending.iter().position(|w| w.id == worker) {
                        let spec = pending.remove(index).unwrap();
                        let report = empty_report(&settings.id, &spec, Outcome::Canceled);
                        terminal(&mut journal, &report, &mut observe)?;
                        reports.push(report);
                    } else if let Some(job) = preparing.0.iter_mut().find(|j| j.spec.id == worker) {
                        job.forced = Some(Outcome::Canceled);
                        job.cancel.store(true, Ordering::Release);
                    } else if let Some(worker) = active.iter_mut().find(|w| w.spec.id == worker) {
                        worker.canceled = true;
                    } else {
                        journal.record(None, "control_rejected", json!({"reason":"worker_not_live"}))?;
                    }
                }
                Ok(control @ (Control::Steer { .. } | Control::SteerChecked { .. })) => {
                    let (worker, text, admitted) = match control {
                        Control::Steer { worker, text } => (worker, text, None),
                        Control::SteerChecked {
                            worker,
                            text,
                            admitted,
                        } => (worker, text, Some(admitted)),
                        _ => unreachable!(),
                    };
                    let mut accepted = false;
                    if let Some(worker) = active
                        .iter_mut()
                        .find(|w| w.spec.id == worker && w.running && !w.canceled)
                    {
                        if text.len() > 32 * 1024 || text.is_empty() {
                            journal.record(
                                Some(&worker.spec.id),
                                "control_rejected",
                                json!({"reason":"steer_size"}),
                            )?;
                        } else if worker.steering >= 128 {
                            journal.record(
                                Some(&worker.spec.id),
                                "control_rejected",
                                json!({"reason":"steer_limit"}),
                            )?;
                        } else {
                            worker.steering += 1;
                            let queued = worker.process.queue(&json!({"id":format!("steer-{}",worker.steering),"type":"steer","message":task_text(&text)}));
                            if queued.is_err() {
                                worker.forced = Some(Outcome::Failed);
                                journal.record(
                                    Some(&worker.spec.id),
                                    "control_rejected",
                                    json!({"reason":"rpc_input_limit"}),
                                )?;
                            } else {
                                accepted = true;
                                journal.record(
                                    Some(&worker.spec.id),
                                    "steered",
                                    json!({"textSha256":hash(&text)}),
                                )?;
                            }
                        }
                    } else {
                        journal.record(None, "control_rejected", json!({"reason":"worker_not_running"}))?;
                    }
                    if let Some(reply) = admitted {
                        let _ = reply.try_send(accepted);
                    }
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        let run_expired = Instant::now() >= run_deadline;
        if run_expired {
            cancel_all = true;
        }
        if cancel_all {
            for worker in &mut active {
                if !run_expired {
                    worker.canceled = true;
                }
            }
            while let Some(spec) = pending.pop_front() {
                let outcome = if Instant::now() >= run_deadline {
                    Outcome::Deadline
                } else {
                    Outcome::Canceled
                };
                let report = empty_report(&settings.id, &spec, outcome);
                terminal(&mut journal, &report, &mut observe)?;
                reports.push(report);
            }
        }
        let mut index = 0;
        while index < preparing.0.len() {
            let job = &mut preparing.0[index];
            if job.forced.is_none() && (cancel_all || Instant::now() >= job.deadline) {
                job.forced = Some(if run_expired || Instant::now() >= job.deadline {
                    Outcome::Deadline
                } else {
                    Outcome::Canceled
                });
                job.cancel.store(true, Ordering::Release);
            }
            if job.thread.as_ref().unwrap().is_finished() {
                let mut job = preparing.0.remove(index);
                let result = job
                    .thread
                    .take()
                    .unwrap()
                    .join()
                    .unwrap_or_else(|_| Err(invalid("workspace preparation failed")));
                ready.push_back(Ready {
                    spec: job.spec,
                    deadline: job.deadline,
                    workspace: result.map(Some),
                    forced: job.forced,
                });
            } else {
                index += 1;
            }
        }
        while active.len() + preparing.0.len() + ready.len() < settings.parallelism
            && !cancel_all
            && !canceled.load(Ordering::Acquire)
            && Instant::now() < run_deadline
        {
            let Some(spec) = pending.pop_front() else {
                break;
            };
            journal.record(Some(&spec.id), "starting", json!({}))?;
            observe(status(&spec, "starting", None, 0));
            let deadline = (Instant::now() + spec.budget.wall_time).min(run_deadline);
            if let Some(grant) = spec.workspace.clone() {
                let path = directory.join(format!("workspace-{}", spec.id));
                let pool = workspaces.clone();
                let flag = Arc::new(AtomicBool::new(false));
                let worker_flag = flag.clone();
                let task = std::thread::Builder::new()
                    .name("oxa-workspace-prepare".into())
                    .spawn(move || {
                        if grant.uses_git_worktrees() {
                            crate::git_worktrees::prepare(
                                pool.as_ref().unwrap(),
                                &grant,
                                &path,
                                deadline,
                                &worker_flag,
                            )
                        } else {
                            grant.materialize(&path)
                        }
                    });
                match task {
                    Ok(thread) => preparing.0.push(Preparation {
                        spec,
                        deadline,
                        cancel: flag,
                        forced: None,
                        thread: Some(thread),
                    }),
                    Err(_) => ready.push_back(Ready {
                        spec,
                        deadline,
                        workspace: Err(invalid("cannot start workspace preparation")),
                        forced: None,
                    }),
                }
            } else {
                ready.push_back(Ready {
                    spec,
                    deadline,
                    workspace: Ok(None),
                    forced: None,
                });
            }
        }
        while let Some(job) = ready.pop_front() {
            let spec = job.spec;
            let worker_deadline = job.deadline;
            let forced = job.forced.or_else(|| {
                if canceled.load(Ordering::Acquire) || cancel_all {
                    Some(if run_expired {
                        Outcome::Deadline
                    } else {
                        Outcome::Canceled
                    })
                } else if Instant::now() >= worker_deadline {
                    Some(Outcome::Deadline)
                } else {
                    None
                }
            });
            if let Some(outcome) = forced {
                let report = empty_report(&settings.id, &spec, outcome);
                terminal(&mut journal, &report, &mut observe)?;
                reports.push(report);
                continue;
            }
            let workspace = match job.workspace {
                Ok(workspace) => workspace,
                Err(error) => {
                    let outcome = match error.kind() {
                        io::ErrorKind::TimedOut => Outcome::Deadline,
                        io::ErrorKind::Interrupted => Outcome::Canceled,
                        _ => Outcome::Failed,
                    };
                    let report = empty_report(&settings.id, &spec, outcome);
                    terminal(&mut journal, &report, &mut observe)?;
                    reports.push(report);
                    continue;
                }
            };
            let endpoint = format!("http://{}", spec.inference.address);
            if let Some(workspace) = &workspace {
                journal.record(
                    Some(&spec.id),
                    "workspace",
                    json!({"manifestSha256":workspace.manifest(),"editing":workspace.editing(),"gitBaseCommit":workspace.base_commit()}),
                )?;
            }
            if canceled.load(Ordering::Acquire) || Instant::now() >= worker_deadline {
                let outcome = if canceled.load(Ordering::Acquire) {
                    Outcome::Canceled
                } else {
                    Outcome::Deadline
                };
                let report = empty_report(&settings.id, &spec, outcome);
                terminal(&mut journal, &report, &mut observe)?;
                reports.push(report);
                continue;
            }
            match Process::spawn(
                &worker_binary,
                &bwrap_binary,
                &endpoint,
                &spec.inference.model,
                &spec.role,
                settings.mode.as_str(),
                spec.budget.output_bytes,
                workspace.as_ref(),
                spec.inference.local_only,
            ) {
                Ok(mut process) => {
                    let queued = process.queue(&json!({"id":"check","type":"get_state"}));
                    journal.record(Some(&spec.id), "spawned", json!({"processId":process.pid()}))?;
                    active.push(Worker {
                        deadline: worker_deadline,
                        spec,
                        process,
                        workspace,
                        checking: true,
                        configuration: BTreeSet::new(),
                        running: false,
                        prompt_acked: false,
                        settled: None,
                        last_stop: None,
                        result: String::new(),
                        completed_tokens: 0,
                        pending_tokens: None,
                        usage_seen: false,
                        canceled: false,
                        forced: queued.err().map(|_| Outcome::Failed),
                        steering: 0,
                        events: 0,
                        pending_abort: false,
                        defer_prompt: false,
                    });
                }
                Err(_) => {
                    let report = empty_report(&settings.id, &spec, Outcome::Failed);
                    terminal(&mut journal, &report, &mut observe)?;
                    reports.push(report);
                }
            }
        }
        let mut i = 0;
        while i < active.len() {
            let worker = &mut active[i];
            let outcome = if worker.canceled {
                Some(Outcome::Canceled)
            } else if Instant::now() >= worker.deadline {
                Some(Outcome::Deadline)
            } else {
                worker.poll(settings.mode, &mut journal)?
            };
            if let Some(outcome) = outcome {
                let mut worker = active.remove(i);
                journal.record(Some(&worker.spec.id), "stopping", json!({"outcome":outcome}))?;
                worker.process.stop(
                    worker
                        .deadline
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(20)),
                );
                let (outcome, review) = match worker
                    .workspace
                    .as_ref()
                    .map(|w| w.collect(&worker.spec.id))
                    .transpose()
                {
                    Ok(review) => (outcome, review),
                    Err(error) => (
                        if error.kind() == io::ErrorKind::FileTooLarge {
                            Outcome::OutputBudget
                        } else {
                            Outcome::PolicyViolation
                        },
                        None,
                    ),
                };
                let mut report = worker.report(&settings.id, outcome);
                if let Some(review) = review.filter(|r| !r.changes.is_empty()) {
                    journal.record(
                        Some(&worker.spec.id),
                        "workspace_review",
                        json!({"manifestSha256":review.manifest_sha256,"changes":review.changes.len()}),
                    )?;
                    report.task.artifacts.push(Artifact {
                        artifact_id: format!("{}-review", worker.spec.id),
                        parts: vec![Part::Data {
                            data: std::collections::BTreeMap::from([
                                ("kind".into(), json!("oxa.workspace.review.v1")),
                                ("review".into(), json!(review)),
                            ]),
                        }],
                        name: Some("proposed workspace changes".into()),
                        description: None,
                    });
                }
                terminal(&mut journal, &report, &mut observe)?;
                reports.push(report);
            } else {
                let worker = &active[i];
                observe(status(
                    &worker.spec,
                    if worker.running { "working" } else { "starting" },
                    worker.tokens(),
                    worker.process.bytes,
                ));
                i += 1;
            }
        }
        if !active.is_empty() || !preparing.0.is_empty() {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    reports.sort_by(|a, b| a.worker.cmp(&b.worker));
    journal.record(None, "run_finished", json!({"workers":reports.len(),"completed":reports.iter().filter(|w|w.outcome == Outcome::Completed).count()}))?;
    Ok(RunReport {
        run: settings.id,
        workers: reports,
    })
}

struct Ready {
    spec: WorkerSpec,
    deadline: Instant,
    workspace: io::Result<Option<crate::workspace::WorkingCopy>>,
    forced: Option<Outcome>,
}
struct Preparation {
    spec: WorkerSpec,
    deadline: Instant,
    cancel: Arc<AtomicBool>,
    forced: Option<Outcome>,
    thread: Option<std::thread::JoinHandle<io::Result<crate::workspace::WorkingCopy>>>,
}
struct Preparations(Vec<Preparation>);
impl Drop for Preparations {
    fn drop(&mut self) {
        for job in &self.0 {
            job.cancel.store(true, Ordering::Release);
        }
        for job in &mut self.0 {
            if let Some(thread) = job.thread.take() {
                let _ = thread.join();
            }
        }
    }
}

struct Worker {
    spec: WorkerSpec,
    process: Process,
    workspace: Option<crate::workspace::WorkingCopy>,
    deadline: Instant,
    checking: bool,
    configuration: BTreeSet<String>,
    running: bool,
    prompt_acked: bool,
    settled: Option<bool>,
    last_stop: Option<String>,
    result: String,
    completed_tokens: u64,
    pending_tokens: Option<u64>,
    usage_seen: bool,
    canceled: bool,
    forced: Option<Outcome>,
    steering: usize,
    events: usize,
    pending_abort: bool,
    defer_prompt: bool,
}

impl Worker {
    fn tool_allowed(&self, name: Option<&str>) -> bool {
        self.workspace
            .as_ref()
            .is_some_and(|w| name.is_some_and(|name| w.tools().contains(&name)))
    }
    fn tokens(&self) -> Option<u64> {
        self.usage_seen.then(|| {
            self.completed_tokens
                .saturating_add(self.pending_tokens.unwrap_or(0))
        })
    }

    fn poll(&mut self, mode: PermissionMode, journal: &mut Journal) -> io::Result<Option<Outcome>> {
        if let Some(outcome) = self.forced.take() {
            return Ok(Some(outcome));
        }
        let records = match self.process.poll() {
            Ok(records) => records,
            Err(error) => {
                return Ok(Some(if error.to_string() == "worker output budget exceeded" {
                    Outcome::OutputBudget
                } else {
                    Outcome::Failed
                }))
            }
        };
        for record in records {
            self.events += 1;
            if self.events > 16_384 {
                return Ok(Some(Outcome::OutputBudget));
            }
            match self.record(record, mode, journal) {
                Ok(Some(outcome)) => return Ok(Some(outcome)),
                Ok(None) => {}
                Err(_) => return Ok(Some(Outcome::Failed)),
            }
            if self
                .tokens()
                .is_some_and(|n| n > self.spec.budget.reported_tokens)
            {
                return Ok(Some(Outcome::TokenBudget));
            }
        }
        if self.settled.is_some() && self.prompt_acked && !self.pending_abort {
            return Ok(Some(if self.settled == Some(true) {
                Outcome::Canceled
            } else if self.last_stop.as_deref() == Some("stop") && self.usage_seen {
                Outcome::Completed
            } else {
                Outcome::Failed
            }));
        }
        if self.process.ended().unwrap_or(true) {
            return Ok(Some(Outcome::Lost));
        }
        Ok(None)
    }

    fn record(
        &mut self,
        record: Value,
        mode: PermissionMode,
        journal: &mut Journal,
    ) -> io::Result<Option<Outcome>> {
        let kind = record
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("worker record has no type"))?;
        match kind {
            "response" => {
                let id = record
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("response has no ID"))?;
                let expected_command = match id {
                    "check" => "get_state",
                    "retry" => "set_auto_retry",
                    "compact" => "set_auto_compaction",
                    "prompt" => "prompt",
                    "goal-abort" if self.pending_abort => "abort",
                    _ if id
                        .strip_prefix("steer-")
                        .and_then(|s| s.parse::<usize>().ok())
                        .is_some_and(|n| n > 0 && n <= self.steering) =>
                    {
                        "steer"
                    }
                    _ => return Ok(Some(Outcome::Failed)),
                };
                if record["command"].as_str() != Some(expected_command) {
                    return Ok(Some(Outcome::Failed));
                }
                if record.get("success").and_then(Value::as_bool) != Some(true) {
                    return Ok(Some(Outcome::Failed));
                }
                if id == "check" && self.checking {
                    if record["data"]["mode"].as_str() != Some(mode.as_str())
                        || record["data"]["model"]["id"].as_str() != Some(&self.spec.inference.model)
                        || record["data"]["isStreaming"].as_bool() != Some(false)
                    {
                        return Ok(Some(Outcome::PolicyViolation));
                    }
                    self.checking = false;
                    for (id, command) in [("retry", "set_auto_retry"), ("compact", "set_auto_compaction")] {
                        self.configuration.insert(id.into());
                        self.process
                            .queue(&json!({"id":id,"type":command,"enabled":false}))?;
                    }
                    journal.record(
                        Some(&self.spec.id),
                        "policy_verified",
                        json!({"mode":mode,"offline":self.spec.inference.local_only,"toolPolicy":"explicit-per-worker-grants"}),
                    )?;
                } else if self.configuration.remove(id) {
                    if self.configuration.is_empty() && !self.defer_prompt {
                        self.running = true;
                        self.process.queue(
                            &json!({"id":"prompt","type":"prompt","message":task_text(&self.spec.task)}),
                        )?;
                        journal.record(Some(&self.spec.id), "working", json!({}))?;
                    }
                } else if id == "prompt" && self.running {
                    self.prompt_acked = true;
                } else if id == "goal-abort" && self.pending_abort {
                    self.pending_abort = false;
                } else if !id.starts_with("steer-") {
                    return Ok(Some(Outcome::Failed));
                }
            }
            "message_update" if self.running => {
                if let Some(usage) = record.get("usage").filter(|u| !u.is_null()) {
                    self.pending_tokens = Some(tokens(usage)?);
                    self.usage_seen = true;
                }
            }
            "message_end" if self.running => {
                let message = &record["message"];
                if message["role"] == "assistant" {
                    let total = tokens(&message["usage"])?;
                    self.completed_tokens = self
                        .completed_tokens
                        .checked_add(total)
                        .ok_or_else(|| invalid("usage overflow"))?;
                    self.pending_tokens = None;
                    self.usage_seen = true;
                    self.last_stop = message["stopReason"].as_str().map(str::to_owned);
                    let content = message["content"]
                        .as_array()
                        .ok_or_else(|| invalid("invalid assistant content"))?;
                    let mut text = String::new();
                    for part in content {
                        if part["type"] == "toolCall" && !self.tool_allowed(part["name"].as_str()) {
                            return Ok(Some(Outcome::PolicyViolation));
                        }
                        if part["type"] == "text" {
                            text.push_str(
                                part["text"]
                                    .as_str()
                                    .ok_or_else(|| invalid("invalid assistant text"))?,
                            );
                        }
                    }
                    self.result = text;
                    journal.record(
                        Some(&self.spec.id),
                        "usage",
                        json!({"reportedTokens":self.tokens(),"resultSha256":hash(&self.result)}),
                    )?;
                }
            }
            "agent_settled" if self.running => {
                self.settled = Some(
                    record["aborted"]
                        .as_bool()
                        .ok_or_else(|| invalid("invalid settled event"))?,
                );
            }
            "mode_changed" if record["mode"].as_str() != Some(mode.as_str()) => {
                return Ok(Some(Outcome::PolicyViolation))
            }
            "model_select" if record["model"].as_str() != Some(&self.spec.inference.model) => {
                return Ok(Some(Outcome::PolicyViolation))
            }
            "tool_execution_start" => {
                if !self.tool_allowed(record["toolName"].as_str()) {
                    return Ok(Some(Outcome::PolicyViolation));
                }
                journal.record(
                    Some(&self.spec.id),
                    "workspace_tool",
                    json!({"tool":record["toolName"],"argumentsSha256":hash(&record["args"].to_string())}),
                )?;
            }
            "extension_ui_request" => {
                if let Some(id) = record.get("id") {
                    self.process.queue(&json!({"type":"extension_ui_response","id":id,"value":false,"confirmed":false,"cancelled":true}))?;
                    journal.record(Some(&self.spec.id), "permission_denied", json!({}))?;
                }
            }
            _ => {} // Unknown optional events consume the same bounded event/byte budgets.
        }
        Ok(None)
    }

    fn report(&self, run_id: &str, outcome: Outcome) -> WorkerReport {
        let mut report = empty_report(run_id, &self.spec, outcome);
        if !self.result.is_empty() {
            report.task.artifacts.push(Artifact {
                artifact_id: format!("{}-result", self.spec.id),
                parts: vec![Part::Text {
                    text: self.result.clone(),
                }],
                name: Some("worker result".into()),
                description: None,
            });
        }
        report.reported_tokens = self.tokens();
        report.output_bytes = self.process.bytes;
        report.process_id = Some(self.process.pid());
        report
    }
}

/// The session adapter retains this SAME process, conversation and cumulative
/// counters between goal turns. The ordinary fan-out worker lifecycle is unchanged.
pub(crate) struct SessionWorker {
    inner: Worker,
    mode: PermissionMode,
}
impl SessionWorker {
    pub(crate) fn start(
        settings: &RunSettings,
        spec: WorkerSpec,
        directory: &std::path::Path,
        journal: &mut Journal,
    ) -> io::Result<Self> {
        validate(settings, std::slice::from_ref(&spec))?;
        let binary = checked_program(&settings.worker_binary)?;
        let bwrap = checked_program(&settings.bwrap_binary)?;
        let workspace = spec
            .workspace
            .as_ref()
            .map(|grant| grant.materialize(&directory.join("workspace")))
            .transpose()?;
        let mut process = Process::spawn(
            &binary,
            &bwrap,
            &format!("http://{}", spec.inference.address),
            &spec.inference.model,
            &spec.role,
            settings.mode.as_str(),
            spec.budget.output_bytes,
            workspace.as_ref(),
            spec.inference.local_only,
        )?;
        process.queue(&json!({"id":"check","type":"get_state"}))?;
        journal.record(Some(&spec.id), "spawned", json!({"processId":process.pid()}))?;
        Ok(Self {
            mode: settings.mode,
            inner: Worker {
                deadline: Instant::now() + settings.wall_time,
                spec,
                process,
                workspace,
                checking: true,
                configuration: BTreeSet::new(),
                running: false,
                prompt_acked: false,
                settled: None,
                last_stop: None,
                result: String::new(),
                completed_tokens: 0,
                pending_tokens: None,
                usage_seen: false,
                canceled: false,
                forced: None,
                steering: 0,
                events: 0,
                pending_abort: false,
                defer_prompt: false,
            },
        })
    }
    pub(crate) fn poll(&mut self, journal: &mut Journal) -> io::Result<Option<Outcome>> {
        if self.inner.defer_prompt
            && !self.inner.running
            && !self.inner.checking
            && self.inner.configuration.is_empty()
        {
            return Ok(Some(Outcome::Canceled)); // verified no prompt was ever sent
        }
        self.inner.poll(self.mode, journal)
    }
    pub(crate) fn continue_turn(&mut self, task: String, journal: &mut Journal) -> io::Result<()> {
        if self.inner.pending_abort
            || self.inner.checking
            || !self.inner.configuration.is_empty()
            || (self.inner.running && self.inner.settled.is_none())
        {
            return Err(invalid("native session has not settled"));
        }
        self.inner.spec.task = task;
        self.inner.result.clear();
        self.inner.last_stop = None;
        self.inner.pending_tokens = None;
        self.inner.prompt_acked = false;
        self.inner.settled = None;
        self.inner.defer_prompt = false;
        self.inner.running = true;
        self.inner
            .process
            .queue(&json!({"id":"prompt","type":"prompt","message":task_text(&self.inner.spec.task)}))?;
        journal.record(Some(&self.inner.spec.id), "working", json!({}))
    }
    pub(crate) fn pause(&mut self) -> io::Result<()> {
        if !self.inner.running {
            self.inner.defer_prompt = true;
            return Ok(());
        }
        if !self.inner.pending_abort {
            self.inner
                .process
                .queue(&json!({"id":"goal-abort","type":"abort"}))?;
            self.inner.pending_abort = true;
        }
        Ok(())
    }
    pub(crate) fn steer(&mut self, text: &str) -> io::Result<()> {
        if !self.inner.running
            || self.inner.settled.is_some()
            || self.inner.pending_abort
            || self.inner.steering >= 128
            || text.trim().is_empty()
            || text.len() > 16 * 1024
        {
            return Err(invalid("native session cannot admit steering"));
        }
        self.inner.steering += 1;
        self.inner.process.queue(
            &json!({"id":format!("steer-{}",self.inner.steering),"type":"steer","message":task_text(text)}),
        )
    }
    pub(crate) fn reply(&self) -> &str {
        &self.inner.result
    }
    pub(crate) fn tokens(&self) -> u64 {
        self.inner.completed_tokens
    }
    pub(crate) fn final_usage_known(&self, outcome: &Outcome) -> bool {
        *outcome == Outcome::Completed || (self.inner.defer_prompt && !self.inner.running)
    }
    pub(crate) fn pid(&self) -> u32 {
        self.inner.process.pid()
    }
    pub(crate) fn bytes(&self) -> usize {
        self.inner.process.bytes
    }
    pub(crate) fn stop(&mut self) {
        self.inner.process.stop(Duration::from_millis(20));
    }
    pub(crate) fn review(&self) -> io::Result<Option<crate::workspace::ReviewBundle>> {
        self.inner
            .workspace
            .as_ref()
            .map(|copy| copy.collect(&self.inner.spec.id))
            .transpose()
    }
}

fn tokens(usage: &Value) -> io::Result<u64> {
    let mut total = 0u64;
    for key in ["input", "output", "cacheRead", "cacheWrite"] {
        total = total
            .checked_add(
                usage[key]
                    .as_u64()
                    .ok_or_else(|| invalid("missing or invalid usage"))?,
            )
            .ok_or_else(|| invalid("usage overflow"))?;
    }
    Ok(total)
}

fn status(spec: &WorkerSpec, state: &str, reported_tokens: Option<u64>, output_bytes: usize) -> WorkerStatus {
    WorkerStatus {
        worker: spec.id.clone(),
        state: state.into(),
        reported_tokens,
        output_bytes,
    }
}

fn task_text(text: &str) -> String {
    format!("Delegated task data:\n{}", json!({"task":text}))
}

fn hash(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub(crate) fn validate(settings: &RunSettings, plan: &[WorkerSpec]) -> io::Result<()> {
    if !valid_id(&settings.id)
        || plan.is_empty()
        || plan.len() > 128
        || settings.parallelism == 0
        || settings.parallelism > 32
        || settings.wall_time.is_zero()
        || settings.wall_time > Duration::from_secs(86400)
        || settings.journal_bytes < 1024
    {
        return Err(invalid("invalid run settings"));
    }
    let mut ids = BTreeSet::new();
    let mut bytes = 0usize;
    let mut tokens = 0u64;
    for worker in plan {
        #[cfg(feature = "interactive-control")]
        if !worker.inference.local_only && oxa_proto::offline() {
            return Err(invalid("offline mode refuses cloud worker inference"));
        }
        if !valid_id(&worker.id)
            || !ids.insert(&worker.id)
            || worker.role.is_empty()
            || worker.role.len() > 8192
            || worker.task.is_empty()
            || worker.task.len() > 32 * 1024
            || worker.budget.wall_time.is_zero()
            || worker.budget.wall_time > Duration::from_secs(86400)
            || worker.budget.output_bytes == 0
            || worker.budget.reported_tokens == 0
            || worker
                .workspace
                .as_ref()
                .is_some_and(|w| w.is_editing() && settings.mode != PermissionMode::Workspace)
        {
            return Err(invalid("invalid worker plan"));
        }
        if let Some(workspace) = &worker.workspace {
            workspace.validate_backend()?;
        }
        bytes = bytes
            .checked_add(worker.budget.output_bytes)
            .ok_or_else(|| invalid("output reservation overflow"))?;
        tokens = tokens
            .checked_add(worker.budget.reported_tokens)
            .ok_or_else(|| invalid("token reservation overflow"))?;
    }
    if bytes > settings.output_reservation || tokens > settings.token_reservation {
        return Err(invalid("plan exceeds run reservations"));
    }
    Ok(())
}

fn empty_report(run_id: &str, spec: &WorkerSpec, outcome: Outcome) -> WorkerReport {
    WorkerReport {
        worker: spec.id.clone(),
        task: Task {
            id: format!("{run_id}:{}", spec.id),
            context_id: run_id.into(),
            status: TaskStatus {
                state: outcome.task_state(),
                message: None,
                timestamp: None,
            },
            artifacts: vec![],
            history: vec![],
        },
        outcome,
        reported_tokens: None,
        output_bytes: 0,
        process_id: None,
    }
}

fn terminal(
    journal: &mut Journal,
    report: &WorkerReport,
    observe: &mut impl FnMut(WorkerStatus),
) -> io::Result<()> {
    journal.record(Some(&report.worker), "finished", json!({"outcome":report.outcome,"reportedTokens":report.reported_tokens,"outputBytes":report.output_bytes,"processId":report.process_id}))?;
    observe(WorkerStatus {
        worker: report.worker.clone(),
        state: format!("{:?}", report.outcome).to_ascii_lowercase(),
        reported_tokens: report.reported_tokens,
        output_bytes: report.output_bytes,
    });
    Ok(())
}

#[cfg(test)]
mod preparation_tests {
    use super::*;
    use crate::workspace::{Grant, RelativePath, Snapshot};
    use oxa_fake::{FakeOpts, FakeServer, TestEnv};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn blocked_workspace_preparation_cannot_delay_a_running_workers_deadline() {
        let env = TestEnv::new();
        // Owned protocol stub only: it acknowledges startup and then waits.
        // No model, shell, source code or network request is executed.
        let binary = env.root().join("rpc-fixture.py");
        fs::write(
            &binary,
            r#"#!/usr/bin/python3
import sys, json
for line in sys.stdin:
    request = json.loads(line)
    command = request['type']
    if command == 'abort':
        break
    response = {'type':'response','id':request['id'],'command':command,'success':True}
    if command == 'get_state':
        response['data'] = {'mode':'workspace','model':{'id':'fake-model'},'isStreaming':False}
    print(json.dumps(response), flush=True)
"#,
        )
        .unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(env.root().join("input.txt"), "approved source\n").unwrap();
        let snapshot = Arc::new(
            Snapshot::approve(env.root(), vec![RelativePath::new("input.txt".into()).unwrap()]).unwrap(),
        );
        let grant = Arc::new(
            Grant::editing(snapshot, vec![])
                .unwrap()
                .with_git_worktrees()
                .unwrap(),
        );
        let engine = FakeServer::start(vec![], FakeOpts::default()).unwrap();
        let inference = LocalInference::register_verified_local(
            engine.url().trim_start_matches("http://").parse().unwrap(),
            "fake-model".into(),
        )
        .unwrap();
        let spec = |id: &str, duration: Duration, workspace| WorkerSpec {
            id: id.into(),
            role: "synthetic".into(),
            task: "synthetic".into(),
            inference: inference.clone(),
            workspace,
            budget: WorkerBudget {
                wall_time: duration,
                output_bytes: 64 * 1024,
                reported_tokens: 1000,
            },
        };
        let pool = Arc::new(std::sync::Mutex::new(
            crate::git_worktrees::Pool::new(env.root().join("pool")).unwrap(),
        ));
        let (locked, got_lock) = std::sync::mpsc::sync_channel(1);
        let (release, wait_release) = std::sync::mpsc::sync_channel(1);
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        let settings = RunSettings {
            id: "fairness".into(),
            worker_binary: binary,
            bwrap_binary: "/usr/bin/bwrap".into(),
            directory: env.root().join("run"),
            parallelism: 2,
            mode: PermissionMode::Workspace,
            wall_time: Duration::from_secs(4),
            output_reservation: 128 * 1024,
            token_reservation: 2000,
            journal_bytes: 128 * 1024,
        };
        let mut deadline_seen = None;
        let mut canceled_second = false;
        let started = Instant::now();
        let report = std::thread::scope(|scope| {
            let held_pool = pool.clone();
            scope.spawn(move || {
                let _held = held_pool.lock().unwrap();
                locked.send(()).unwrap();
                // Timeout is a test deadlock guard, not the normal release path.
                let _ = wait_release.recv_timeout(Duration::from_secs(2));
            });
            got_lock.recv().unwrap();
            run_with_workspaces(
                settings,
                vec![
                    spec("running", Duration::from_millis(100), None),
                    spec("preparing", Duration::from_secs(3), Some(grant)),
                ],
                rx,
                Arc::new(AtomicBool::new(false)),
                Some(pool.clone()),
                |status| {
                    if status.worker == "running" && status.state == "deadline" {
                        deadline_seen = Some(started.elapsed());
                        let _ = release.try_send(());
                    }
                    if status.worker == "preparing" && status.state == "working" && !canceled_second {
                        canceled_second = true;
                        tx.try_send(Control::Cancel {
                            worker: "preparing".into(),
                        })
                        .unwrap();
                    }
                },
            )
            .unwrap()
        });
        assert!(
            deadline_seen.unwrap() < Duration::from_millis(700),
            "active deadline must be serviced while the other preparation is blocked"
        );
        assert_eq!(
            report
                .workers
                .iter()
                .find(|w| w.worker == "running")
                .unwrap()
                .outcome,
            Outcome::Deadline
        );
        assert_eq!(
            report
                .workers
                .iter()
                .find(|w| w.worker == "preparing")
                .unwrap()
                .outcome,
            Outcome::Canceled
        );
        assert!(engine.requests().is_empty());
    }
}
