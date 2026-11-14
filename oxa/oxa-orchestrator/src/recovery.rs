//! Bounded journal inspection and explicit reconciliation with an original
//! registered local A2A agent. Never replay input or signal a recorded PID.
use crate::{
    a2a_run::{LocalAgent, StopConfirmation},
    Outcome,
};
use oxa_a2a::{
    collect::Collector,
    loopback::{rpc_error_code, Client, Limits, ResponseBudget},
    wire::{Event, Task, TaskState},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::time::{Duration, Instant};

const MAX_JOURNAL: usize = 16 * 1024 * 1024;
const MAX_LINE: usize = 64 * 1024;
const MAX_RECORDS: usize = 100_000;

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    LocalRpc,
    A2a,
}
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Queued,
    Started,
    Bound,
    Finished,
}
#[derive(Clone, Debug, Serialize)]
pub struct Binding {
    pub task_id: String,
    pub context_id: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct WorkerSnapshot {
    pub worker: String,
    pub phase: Phase,
    pub binding: Option<Binding>,
    pub agent_url_sha256: Option<String>,
    pub recorded_outcome: Option<Outcome>,
    pub recorded_stop: Option<StopConfirmation>,
    pub recorded_task_state: Option<TaskState>,
    /// Historical data only. Never used to identify or signal a live process.
    pub recorded_process_id: Option<u32>,
    pub result_sha256: Option<String>,
    pub checkpoint_sha256: Option<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct Snapshot {
    pub version: u32,
    pub run: String,
    pub transport: Transport,
    pub journal_sha256: String,
    pub expected_workers: usize,
    pub unlisted_workers: usize,
    pub records: usize,
    pub truncated_tail: bool,
    pub recorded_run_finished: bool,
    /// New writers hold an exclusive kernel lock for the journal's lifetime.
    pub exclusive_ownership_supported: bool,
    pub workers: Vec<WorkerSnapshot>,
}
pub struct InspectedRun {
    snapshot: Snapshot,
}

/// Exclusive recovery authority over a cooperative writer's inactive journal
/// inode. Keep this value alive for the entire recovery operation.
/// It does not prove that a remote task or native descendant has stopped.
pub struct ExclusiveRun {
    inspected: InspectedRun,
    _journal: File,
}
impl ExclusiveRun {
    pub fn claim(path: &Path) -> io::Result<Self> {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let _namespace = crate::namespace::protected_directory(parent)?;
        let handle = open_input(path, MAX_JOURNAL)?;
        let metadata = handle.metadata()?;
        // SAFETY: geteuid only returns the effective user identity.
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
            return Err(invalid(
                "exclusive recovery requires an operator-owned private journal",
            ));
        }
        let journal = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(format!("/proc/self/fd/{}", handle.as_raw_fd()))?;
        crate::ownership::lock(&journal)?;
        let inspected = InspectedRun::from_bytes(read_handle(&journal, MAX_JOURNAL)?)?;
        if !inspected.snapshot.exclusive_ownership_supported {
            return Err(invalid(
                "legacy journal does not establish cooperative controller ownership",
            ));
        }
        Ok(Self {
            inspected,
            _journal: journal,
        })
    }
    pub fn inspected(&self) -> &InspectedRun {
        &self.inspected
    }
}
#[derive(Deserialize)]
struct Record {
    schema: u32,
    sequence: u64,
    #[serde(rename = "elapsedMs")]
    elapsed_ms: u64,
    worker: Option<String>,
    event: String,
    details: Value,
}

impl InspectedRun {
    pub fn inspect(path: &Path) -> io::Result<Self> {
        Self::from_bytes(read_journal(path)?)
    }
    fn from_bytes(bytes: Vec<u8>) -> io::Result<Self> {
        let truncated_tail = bytes.last() != Some(&b'\n');
        let end = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
        if end == 0 {
            return Err(invalid("journal has no complete records"));
        }
        if bytes.len() - end > MAX_LINE {
            return Err(invalid("incomplete journal line exceeds its bound"));
        }
        let mut workers = BTreeMap::<String, WorkerSnapshot>::new();
        let mut run = None;
        let mut transport = Transport::LocalRpc;
        let mut expected = 0;
        let mut elapsed = 0;
        let mut records = 0;
        let mut finished = false;
        let mut exclusive_ownership_supported = false;
        for line in bytes[..end - 1].split(|b| *b == b'\n') {
            if line.len() > MAX_LINE || records >= MAX_RECORDS {
                return Err(invalid("journal record bound exceeded"));
            }
            let record: Record =
                serde_json::from_slice(line).map_err(|_| invalid("invalid complete journal record"))?;
            if record.schema != 1
                || record.sequence != records as u64
                || record.elapsed_ms < elapsed
                || !record.details.is_object()
                || finished
            {
                return Err(invalid("journal schema, sequence or lifecycle mismatch"));
            }
            elapsed = record.elapsed_ms;
            records += 1;
            if run.is_none() {
                if record.event != "run_started" || record.worker.is_some() {
                    return Err(invalid("journal must begin with run_started"));
                }
                let id = text(&record.details, "run")?;
                if !valid_id(id) {
                    return Err(invalid("invalid journal run identity"));
                }
                run = Some(id.to_owned());
                expected = record.details["workers"]
                    .as_u64()
                    .filter(|n| *n > 0 && *n <= 128)
                    .ok_or_else(|| invalid("invalid recorded worker count"))?
                    as usize;
                transport = match record.details.get("transport") {
                    None => Transport::LocalRpc,
                    Some(Value::String(value)) if value == "A2A-0.3.0" => Transport::A2a,
                    _ => return Err(invalid("unsupported recorded transport")),
                };
                exclusive_ownership_supported = match record.details.get("ownership") {
                    None => false,
                    Some(Value::String(value)) if value == crate::ownership::PROTOCOL => true,
                    _ => return Err(invalid("unsupported controller ownership protocol")),
                };
                continue;
            }
            match record.event.as_str() {
                "run_started" => return Err(invalid("duplicate run start")),
                "queued" => {
                    let id = record
                        .worker
                        .as_deref()
                        .filter(|id| valid_id(id))
                        .ok_or_else(|| invalid("invalid queued worker identity"))?;
                    if workers.len() >= expected
                        || workers.contains_key(id)
                        || workers.values().any(|w| w.phase != Phase::Queued)
                    {
                        return Err(invalid("invalid queued worker manifest"));
                    }
                    let route = optional_hash(&record.details, "agentUrlSha256")?;
                    workers.insert(
                        id.into(),
                        WorkerSnapshot {
                            worker: id.into(),
                            phase: Phase::Queued,
                            binding: None,
                            agent_url_sha256: route,
                            recorded_outcome: None,
                            recorded_stop: None,
                            recorded_task_state: None,
                            recorded_process_id: None,
                            result_sha256: None,
                            checkpoint_sha256: None,
                        },
                    );
                }
                "starting" => {
                    if workers.len() != expected {
                        return Err(invalid("worker started before its manifest was complete"));
                    }
                    let worker = worker_mut(&mut workers, &record)?;
                    if worker.phase != Phase::Queued {
                        return Err(invalid("worker started twice"));
                    }
                    worker.phase = Phase::Started;
                }
                "spawned" => {
                    if transport != Transport::LocalRpc {
                        return Err(invalid("process record in A2A journal"));
                    }
                    let worker = worker_mut(&mut workers, &record)?;
                    if worker.phase != Phase::Started || worker.recorded_process_id.is_some() {
                        return Err(invalid("invalid process lifecycle"));
                    }
                    worker.recorded_process_id = Some(
                        record.details["processId"]
                            .as_u64()
                            .filter(|p| *p > 0 && *p <= u32::MAX as u64)
                            .ok_or_else(|| invalid("invalid recorded process ID"))?
                            as u32,
                    );
                }
                "task_bound" => {
                    if transport != Transport::A2a {
                        return Err(invalid("task binding in local RPC journal"));
                    }
                    let worker = worker_mut(&mut workers, &record)?;
                    if worker.phase != Phase::Started || worker.binding.is_some() {
                        return Err(invalid("duplicate or out-of-order task binding"));
                    }
                    let id = text(&record.details, "taskId")?;
                    let context = text(&record.details, "contextId")?;
                    if !valid_task_id(id) || !valid_task_id(context) {
                        return Err(invalid("invalid recorded A2A identity"));
                    }
                    worker.binding = Some(Binding {
                        task_id: id.into(),
                        context_id: context.into(),
                    });
                    worker.phase = Phase::Bound;
                }
                "finished" => {
                    let worker = worker_mut(&mut workers, &record)?;
                    if worker.phase == Phase::Finished {
                        return Err(invalid("duplicate worker finish"));
                    }
                    worker.recorded_outcome = Some(
                        serde_json::from_value(record.details["outcome"].clone())
                            .map_err(|_| invalid("invalid recorded outcome"))?,
                    );
                    if worker.phase == Phase::Queued
                        && !matches!(
                            worker.recorded_outcome,
                            Some(Outcome::Canceled | Outcome::Deadline)
                        )
                    {
                        return Err(invalid("unstarted worker has an impossible outcome"));
                    }
                    worker.recorded_stop = if transport == Transport::A2a {
                        Some(
                            serde_json::from_value(record.details["stop"].clone())
                                .map_err(|_| invalid("invalid recorded stop evidence"))?,
                        )
                    } else {
                        None
                    };
                    worker.recorded_task_state = match record.details.get("taskState") {
                        None | Some(Value::Null) => None,
                        Some(value) => Some(
                            serde_json::from_value(value.clone())
                                .map_err(|_| invalid("invalid recorded task state"))?,
                        ),
                    };
                    worker.result_sha256 = optional_hash(&record.details, "resultSha256")?;
                    worker.checkpoint_sha256 = optional_hash(&record.details, "checkpointSha256")?;
                    if transport != Transport::A2a && worker.checkpoint_sha256.is_some() {
                        return Err(invalid("A2A checkpoint in native journal"));
                    }
                    worker.phase = Phase::Finished;
                }
                "run_finished" => {
                    if record.worker.is_some()
                        || workers.len() != expected
                        || workers.values().any(|w| w.phase != Phase::Finished)
                        || record.details["workers"].as_u64() != Some(expected as u64)
                        || record.details["completed"].as_u64()
                            != Some(
                                workers
                                    .values()
                                    .filter(|w| w.recorded_outcome == Some(Outcome::Completed))
                                    .count() as u64,
                            )
                    {
                        return Err(invalid("invalid recorded run completion"));
                    }
                    finished = true;
                }
                "control_rejected" => {}
                "cancel_all"
                    if transport == Transport::LocalRpc
                        && record.worker.is_none()
                        && workers.len() == expected => {}
                "workspace" | "workspace_tool" | "workspace_review" | "stopping" | "policy_verified"
                | "working" | "usage" | "permission_denied" | "steered" | "steer_requested"
                | "steer_admitted" | "steer_unconfirmed" | "status" => {
                    let worker = worker_mut(&mut workers, &record)?;
                    if matches!(worker.phase, Phase::Queued | Phase::Finished) {
                        return Err(invalid("worker metadata outside its active lifecycle"));
                    }
                }
                _ => return Err(invalid("unsupported journal event")),
            }
        }
        let unlisted_workers = expected - workers.len();
        Ok(Self {
            snapshot: Snapshot {
                version: 1,
                run: run.unwrap(),
                transport,
                journal_sha256: hash(&bytes),
                expected_workers: expected,
                unlisted_workers,
                records,
                truncated_tail,
                recorded_run_finished: finished,
                exclusive_ownership_supported,
                workers: workers.into_values().collect(),
            },
        })
    }
    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    /// Load one explicitly selected checkpoint. Only a complete finished record
    /// can bind it; orphan/pending files never authorize recovery or task replay.
    pub fn load_checkpoint(&self, worker: &str, path: &Path) -> io::Result<crate::a2a_run::A2AWorkerReport> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Envelope {
            version: u32,
            run: String,
            report: crate::a2a_run::A2AWorkerReport,
        }
        let worker = self
            .snapshot
            .workers
            .iter()
            .find(|w| w.worker == worker)
            .ok_or_else(|| invalid("worker is absent from inspected journal"))?;
        let expected = worker
            .checkpoint_sha256
            .as_ref()
            .filter(|_| self.snapshot.transport == Transport::A2a && worker.phase == Phase::Finished)
            .ok_or_else(|| invalid("no durable worker checkpoint binding"))?;
        let bytes = read_bounded(path, crate::checkpoint::MAX_WORKER)?;
        if &hash(&bytes) != expected {
            return Err(invalid("checkpoint hash does not match the journal"));
        }
        let checkpoint: Envelope =
            serde_json::from_slice(&bytes).map_err(|_| invalid("invalid worker checkpoint"))?;
        let report = checkpoint.report;
        if checkpoint.version != 1
            || checkpoint.run != self.snapshot.run
            || report.worker != worker.worker
            || Some(&report.outcome) != worker.recorded_outcome.as_ref()
            || Some(report.stop) != worker.recorded_stop
        {
            return Err(invalid("checkpoint does not match recorded worker lifecycle"));
        }
        match (&report.task, &worker.binding) {
            (Some(task), Some(binding)) => {
                validate_task(task, binding, worker, crate::checkpoint::MAX_WORKER)
                    .map_err(|_| invalid("checkpoint task does not match recorded evidence"))?;
                if Some(&task.status.state) != worker.recorded_task_state.as_ref() {
                    return Err(invalid("checkpoint task state differs from the journal"));
                }
            }
            (None, None) => {}
            _ => return Err(invalid("checkpoint task binding differs from the journal")),
        }
        if report.task.is_some() && report.message.is_some() {
            return Err(invalid(
                "checkpoint cannot contain both a task and direct message",
            ));
        }
        Ok(report)
    }

    /// The operator re-registers the ORIGINAL agent instance/capability. An
    /// endpoint hash prevents accidental route changes; it is not authentication.
    /// Only discover/get/cancel are used, never a send or replay of task input.
    pub fn reconcile(
        &self,
        worker: &str,
        agent: LocalAgent,
        options: ReconcileOptions,
    ) -> io::Result<Reconciliation> {
        if self.snapshot.transport != Transport::A2a
            || options.timeout.is_zero()
            || options.timeout > Duration::from_secs(5)
            || !(1024..=16 * 1024 * 1024).contains(&options.response_bytes)
        {
            return Err(invalid("invalid reconciliation transport or limits"));
        }
        let worker = self
            .snapshot
            .workers
            .iter()
            .find(|w| w.worker == worker)
            .ok_or_else(|| invalid("worker is absent from inspected journal"))?;
        let binding = worker
            .binding
            .as_ref()
            .ok_or_else(|| invalid("no durable task identity; input must not be replayed"))?;
        let mut client = agent.into_client();
        if worker.agent_url_sha256.as_deref() != Some(hash(client.url().as_bytes()).as_str()) {
            return Err(invalid("original registered endpoint does not match the journal"));
        }
        let budget = ResponseBudget::new(options.response_bytes);
        client = client.with_response_budget(budget.clone());
        let deadline = Instant::now() + options.timeout;
        let limits = Limits {
            timeout: options.timeout,
            body_bytes: options.response_bytes,
            event_bytes: options.response_bytes,
        };
        client.constrain(limits)?;
        let mut result = Reconciliation {
            journal_sha256: self.snapshot.journal_sha256.clone(),
            worker: worker.worker.clone(),
            recorded_outcome: worker.recorded_outcome.clone(),
            disposition: Disposition::Unavailable,
            remote_stop: StopConfirmation::Unconfirmed,
            task: None,
            response_bytes: 0,
        };
        if client.card().is_err() {
            result.disposition = failure(&budget);
            result.response_bytes = budget.used();
            return Ok(result);
        }
        if !constrain(&mut client, limits, deadline)? {
            result.response_bytes = budget.used();
            return Ok(result);
        }
        let task = match client.get(&binding.task_id) {
            Ok(task) => task,
            Err(error) => {
                result.disposition = if rpc_error_code(&error) == Some(-32001) {
                    Disposition::NotFound
                } else {
                    failure(&budget)
                };
                result.response_bytes = budget.used();
                return Ok(result);
            }
        };
        if let Err(disposition) = validate_task(&task, binding, worker, options.response_bytes) {
            result.disposition = disposition;
            result.response_bytes = budget.used();
            return Ok(result);
        }
        let terminal = task.status.state.is_terminal();
        result.task = Some(task);
        result.disposition = Disposition::Observed;
        if terminal {
            result.remote_stop = StopConfirmation::Confirmed;
        }
        if options.action == Action::Cancel && !terminal {
            result.disposition = Disposition::CancellationUnconfirmed;
            if constrain(&mut client, limits, deadline)? {
                if let Ok(task) = client.cancel(&binding.task_id) {
                    match validate_task(&task, binding, worker, options.response_bytes) {
                        Ok(()) => {
                            result.remote_stop = if task.status.state.is_terminal() {
                                StopConfirmation::Confirmed
                            } else {
                                StopConfirmation::Unconfirmed
                            };
                            result.task = Some(task);
                        }
                        Err(disposition) => result.disposition = disposition,
                    }
                }
            }
            // A lost cancel response may still have stopped the task. Query
            // once within the SAME absolute deadline and aggregate allowance.
            if result.remote_stop != StopConfirmation::Confirmed
                && result.disposition == Disposition::CancellationUnconfirmed
                && constrain(&mut client, limits, deadline)?
            {
                if let Ok(task) = client.get(&binding.task_id) {
                    match validate_task(&task, binding, worker, options.response_bytes) {
                        Ok(()) => {
                            if task.status.state.is_terminal() {
                                result.remote_stop = StopConfirmation::Confirmed;
                            }
                            result.task = Some(task);
                        }
                        Err(disposition) => result.disposition = disposition,
                    }
                }
            }
            if result.remote_stop == StopConfirmation::Confirmed {
                result.disposition = Disposition::Observed;
            } else if budget.remaining() == 0 {
                result.disposition = Disposition::OutputBudget;
            }
        }
        result.response_bytes = budget.used();
        Ok(result)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Observe,
    Cancel,
}
#[derive(Clone, Copy, Debug)]
pub struct ReconcileOptions {
    pub action: Action,
    pub timeout: Duration,
    pub response_bytes: usize,
}
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    Observed,
    Unavailable,
    NotFound,
    IdentityMismatch,
    InvalidResponse,
    ResultMismatch,
    OutputBudget,
    CancellationUnconfirmed,
}
#[derive(Debug, Serialize)]
pub struct Reconciliation {
    pub journal_sha256: String,
    pub worker: String,
    pub recorded_outcome: Option<Outcome>,
    pub disposition: Disposition,
    pub remote_stop: StopConfirmation,
    #[serde(serialize_with = "crate::a2a_run::optional_task")]
    pub task: Option<Task>,
    pub response_bytes: usize,
}
fn validate_task(
    task: &Task,
    binding: &Binding,
    worker: &WorkerSnapshot,
    max: usize,
) -> Result<(), Disposition> {
    if task.id != binding.task_id || task.context_id != binding.context_id {
        return Err(Disposition::IdentityMismatch);
    }
    if worker
        .recorded_task_state
        .as_ref()
        .is_some_and(|state| state.is_terminal() && state != &task.status.state)
        || (worker.recorded_outcome == Some(Outcome::Completed) && task.status.state != TaskState::Completed)
        || (worker.recorded_stop == Some(StopConfirmation::Confirmed) && !task.status.state.is_terminal())
    {
        return Err(Disposition::ResultMismatch);
    }
    let mut collector = Collector::new(max);
    collector
        .apply(Event::Task(task.clone()))
        .map_err(|_| Disposition::InvalidResponse)?;
    let recorded_terminal = worker
        .recorded_task_state
        .as_ref()
        .is_some_and(TaskState::is_terminal)
        || worker.recorded_stop == Some(StopConfirmation::Confirmed)
        || worker.recorded_outcome == Some(Outcome::Completed);
    if recorded_terminal && task.status.state.is_terminal() {
        if let Some(expected) = &worker.result_sha256 {
            let actual = serde_json::to_vec(&task.artifacts).map_err(|_| Disposition::InvalidResponse)?;
            if expected != &hash(&actual) {
                return Err(Disposition::ResultMismatch);
            }
        }
    }
    Ok(())
}
fn constrain(client: &mut Client, limits: Limits, deadline: Instant) -> io::Result<bool> {
    if Instant::now() >= deadline {
        return Ok(false);
    }
    client.constrain(Limits {
        timeout: deadline
            .saturating_duration_since(Instant::now())
            .max(Duration::from_nanos(1)),
        ..limits
    })?;
    Ok(true)
}
fn failure(budget: &ResponseBudget) -> Disposition {
    if budget.remaining() == 0 {
        Disposition::OutputBudget
    } else {
        Disposition::Unavailable
    }
}
fn worker_mut<'a>(
    workers: &'a mut BTreeMap<String, WorkerSnapshot>,
    record: &Record,
) -> io::Result<&'a mut WorkerSnapshot> {
    record
        .worker
        .as_ref()
        .and_then(|id| workers.get_mut(id))
        .ok_or_else(|| invalid("event has no queued worker"))
}
fn text<'a>(value: &'a Value, key: &str) -> io::Result<&'a str> {
    value[key]
        .as_str()
        .ok_or_else(|| invalid("missing recorded identity"))
}
fn optional_hash(value: &Value, key: &str) -> io::Result<Option<String>> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(hash))
            if hash.len() == 64
                && hash
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) =>
        {
            Ok(Some(hash.clone()))
        }
        _ => Err(invalid("invalid recorded hash")),
    }
}
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
fn valid_task_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control)
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn read_journal(path: &Path) -> io::Result<Vec<u8>> {
    read_bounded(path, MAX_JOURNAL)
}
fn read_bounded(path: &Path, max: usize) -> io::Result<Vec<u8>> {
    let handle = open_input(path, max)?;
    read_handle(&handle, max)
}
fn open_input(path: &Path, max: usize) -> io::Result<File> {
    let handle = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let before = handle.metadata()?;
    if !before.is_file() || before.nlink() != 1 || before.len() > max as u64 {
        return Err(invalid("input must be a bounded plain single-link file"));
    }
    Ok(handle)
}
fn read_handle(handle: &File, max: usize) -> io::Result<Vec<u8>> {
    let before = handle.metadata()?;
    if !before.is_file() || before.nlink() != 1 || before.len() > max as u64 {
        return Err(invalid("input must be a bounded plain single-link file"));
    }
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(format!("/proc/self/fd/{}", handle.as_raw_fd()))?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(max as u64 + 1)
        .read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    if bytes.len() > max
        || (
            before.len(),
            before.mtime(),
            before.mtime_nsec(),
            before.ctime(),
            before.ctime_nsec(),
            before.nlink(),
        ) != (
            after.len(),
            after.mtime(),
            after.mtime_nsec(),
            after.ctime(),
            after.ctime_nsec(),
            after.nlink(),
        )
    {
        return Err(invalid("input changed during capture or exceeded its bound"));
    }
    Ok(bytes)
}
