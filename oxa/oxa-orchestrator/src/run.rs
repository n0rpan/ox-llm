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
use std::sync::mpsc::{Receiver, TryRecvError};
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

/// Operator registration, never a property learned from an agent card. The
/// caller must establish that this route cannot forward to a paid/cloud model.
/// Literal loopback by itself does not establish that guarantee.
#[derive(Clone, Debug)]
pub struct LocalInference {
    address: SocketAddr,
    model: String,
}

impl LocalInference {
    pub fn register_verified_local(address: SocketAddr, model: String) -> io::Result<Self> {
        if !address.ip().is_loopback()
            || address.port() == 0
            || model.is_empty()
            || model.len() > 256
            || model.chars().any(char::is_control)
        {
            return Err(invalid("invalid verified local inference route"));
        }
        Ok(Self { address, model })
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
    Steer { worker: String, text: String },
    Cancel { worker: String },
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
    let name = settings
        .directory
        .file_name()
        .ok_or_else(|| invalid("missing run directory name"))?;
    let directory = parent.join(name);
    DirBuilder::new().mode(0o700).create(&directory)?;
    let mut journal = Journal::create(&directory.join("transcript.jsonl"), settings.journal_bytes)?;
    journal.record(None, "run_started", json!({"run":settings.id,"workers":plan.len(),"parallelism":settings.parallelism,"mode":settings.mode,"offline":true,"tools":[]}))?;
    for spec in &plan {
        journal.record(Some(&spec.id), "queued", json!({"roleSha256":hash(&spec.role),"taskSha256":hash(&spec.task),"route":"verified-local","reservedTokens":spec.budget.reported_tokens,"reservedBytes":spec.budget.output_bytes}))?;
        observe(status(spec, "queued", None, 0));
    }
    let mut pending = VecDeque::from(plan);
    let mut active: Vec<Worker> = Vec::new();
    let mut reports = Vec::new();
    let mut cancel_all = false;
    while !pending.is_empty() || !active.is_empty() {
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
                    } else if let Some(worker) = active.iter_mut().find(|w| w.spec.id == worker) {
                        worker.canceled = true;
                    } else {
                        journal.record(None, "control_rejected", json!({"reason":"worker_not_live"}))?;
                    }
                }
                Ok(Control::Steer { worker, text }) => {
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
                            }
                            journal.record(
                                Some(&worker.spec.id),
                                "steered",
                                json!({"textSha256":hash(&text)}),
                            )?;
                        }
                    } else {
                        journal.record(None, "control_rejected", json!({"reason":"worker_not_running"}))?;
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
        while active.len() < settings.parallelism && !cancel_all && Instant::now() < run_deadline {
            let Some(spec) = pending.pop_front() else {
                break;
            };
            journal.record(Some(&spec.id), "starting", json!({}))?;
            observe(status(&spec, "starting", None, 0));
            let endpoint = format!("http://{}", spec.inference.address);
            let worker_deadline = (Instant::now() + spec.budget.wall_time).min(run_deadline);
            match Process::spawn(
                &worker_binary,
                &bwrap_binary,
                &endpoint,
                &spec.inference.model,
                &spec.role,
                settings.mode.as_str(),
                spec.budget.output_bytes,
            ) {
                Ok(mut process) => {
                    let queued = process.queue(&json!({"id":"check","type":"get_state"}));
                    journal.record(Some(&spec.id), "spawned", json!({"processId":process.pid()}))?;
                    active.push(Worker {
                        deadline: worker_deadline,
                        spec,
                        process,
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
                let report = worker.report(&settings.id, outcome);
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
        if !active.is_empty() {
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

struct Worker {
    spec: WorkerSpec,
    process: Process,
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
}

impl Worker {
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
        if self.settled.is_some() && self.prompt_acked {
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
                        json!({"mode":mode,"offline":true,"tools":[]}),
                    )?;
                } else if self.configuration.remove(id) {
                    if self.configuration.is_empty() {
                        self.running = true;
                        self.process.queue(
                            &json!({"id":"prompt","type":"prompt","message":task_text(&self.spec.task)}),
                        )?;
                        journal.record(Some(&self.spec.id), "working", json!({}))?;
                    }
                } else if id == "prompt" && self.running {
                    self.prompt_acked = true;
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
                        if part["type"] == "toolCall" {
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
            "tool_execution_start" => return Ok(Some(Outcome::PolicyViolation)),
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

fn validate(settings: &RunSettings, plan: &[WorkerSpec]) -> io::Result<()> {
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
        {
            return Err(invalid("invalid worker plan"));
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
