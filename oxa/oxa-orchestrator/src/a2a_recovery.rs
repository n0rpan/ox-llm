//! A2A inspection and explicit lease-bound reconciliation. No replay or PID authority.
pub use crate::a2a_reconcile::{Action, Disposition, ReconcileOptions, Reconciliation};
use crate::a2a_run::StopConfirmation;
use crate::supervisor::Outcome;
use oxa_a2a::wire::TaskState;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io;
use std::path::Path;

const MAX_LINE: usize = 64 * 1024;
const MAX_RECORDS: usize = 100_000;
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
    pub agent_url_sha256: String,
    pub recorded_outcome: Option<Outcome>,
    pub recorded_stop: Option<StopConfirmation>,
    pub recorded_task_state: Option<TaskState>,
    pub result_sha256: Option<String>,
    pub checkpoint_sha256: Option<String>,
    pub resume_binding: Option<crate::resume::WorkerBinding>,
}
#[derive(Clone, Debug, Serialize)]
pub struct Snapshot {
    pub version: u32,
    pub run: String,
    pub journal_sha256: String,
    pub expected_workers: usize,
    pub unlisted_workers: usize,
    pub records: usize,
    pub truncated_tail: bool,
    /// The local scheduler finished; this does NOT prove remote task cleanup.
    pub recorded_run_finished: bool,
    pub exclusive_ownership_supported: bool,
    pub persist_results: bool,
    pub workers: Vec<WorkerSnapshot>,
    pub resume_binding: Option<crate::resume::RunBinding>,
}
pub struct InspectedRun {
    snapshot: Snapshot,
}
/// Exclusive authority over a cooperative journal inode, never a remote task.
/// Keep this guard alive throughout original-agent reconciliation.
pub struct ExclusiveRun {
    _journal: crate::ownership::Lease,
    inspected: InspectedRun,
}
impl ExclusiveRun {
    pub fn claim(path: &Path) -> io::Result<Self> {
        let file = crate::recovery::open_input(path)?;
        let journal = crate::ownership::Lease::acquire(file)?;
        let inspected = InspectedRun::from_bytes(crate::recovery::read_checked(&journal)?)?;
        if !inspected.snapshot.exclusive_ownership_supported {
            return Err(invalid("legacy journal does not establish cooperative ownership"));
        }
        Ok(Self {
            _journal: journal,
            inspected,
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
        Self::from_bytes(crate::recovery::read_checked(&crate::recovery::open_input(
            path,
        )?)?)
    }
    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }
    /// Read one explicitly selected checkpoint bound by a complete finished
    /// record. Orphan/pending data never grants replay or recovery authority.
    pub fn load_checkpoint(&self, worker: &str, path: &Path) -> io::Result<crate::a2a_run::A2AWorkerReport> {
        use oxa_a2a::{collect::Collector, wire::Event};
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
            .filter(|_| worker.phase == Phase::Finished && self.snapshot.persist_results)
            .ok_or_else(|| invalid("no durable worker checkpoint binding"))?;
        let bytes = crate::recovery::read_bounded(path, crate::checkpoint::MAX_WORKER)?;
        if &format!("{:x}", Sha256::digest(&bytes)) != expected {
            return Err(invalid("checkpoint hash does not match journal"));
        }
        let checkpoint: Envelope =
            serde_json::from_slice(&bytes).map_err(|_| invalid("invalid worker checkpoint"))?;
        let report = checkpoint.report;
        if checkpoint.version != 1
            || checkpoint.run != self.snapshot.run
            || report.worker != worker.worker
            || Some(report.outcome) != worker.recorded_outcome
            || Some(report.stop) != worker.recorded_stop
            || (report.task.is_some() && report.message.is_some())
        {
            return Err(invalid("checkpoint disagrees with recorded lifecycle"));
        }
        let mut collector = Collector::new(crate::checkpoint::MAX_WORKER);
        let digest = match (&report.task, &report.message, &worker.binding) {
            (Some(task), None, Some(binding)) => {
                if task.id != binding.task_id
                    || task.context_id != binding.context_id
                    || Some(&task.status.state) != worker.recorded_task_state.as_ref()
                {
                    return Err(invalid("checkpoint task identity/state mismatch"));
                }
                collector
                    .apply(Event::Task(task.clone()))
                    .map_err(|_| invalid("invalid checkpoint task"))?;
                Some(serde_json::to_vec(&task.artifacts)?)
            }
            (None, Some(message), None) => {
                let oxa_a2a::wire::MessageObject::Message(message_value) = message;
                collector
                    .apply(Event::Message(message_value.clone()))
                    .map_err(|_| invalid("invalid checkpoint message"))?;
                Some(serde_json::to_vec(message)?)
            }
            (None, None, None) => None,
            _ => return Err(invalid("checkpoint task binding differs from journal")),
        }
        .map(|bytes| format!("{:x}", Sha256::digest(bytes)));
        if digest != worker.result_sha256 {
            return Err(invalid("checkpoint result digest differs from journal"));
        }
        Ok(report)
    }
    fn from_bytes(bytes: Vec<u8>) -> io::Result<Self> {
        let end = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
        if end == 0 || bytes.len() - end > MAX_LINE {
            return Err(invalid("missing or oversized complete journal prefix"));
        }
        let mut workers = BTreeMap::<String, WorkerSnapshot>::new();
        let mut run = None;
        let mut expected = 0;
        let mut elapsed = 0;
        let mut records = 0;
        let mut finished = false;
        let mut exclusive = false;
        let mut persist_results = false;
        let mut resume_binding = None;
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
            records += 1;
            elapsed = record.elapsed_ms;
            let details = &record.details;
            if run.is_none() {
                if record.event != "run_started"
                    || record.worker.is_some()
                    || details["transport"] != "A2A-0.3.0"
                {
                    return Err(invalid("unsupported A2A journal start or resume binding"));
                }
                let id = text(details, "run")?;
                if !valid_id(id) {
                    return Err(invalid("invalid run identity"));
                }
                run = Some(id.to_owned());
                expected = details["workers"]
                    .as_u64()
                    .filter(|n| (1..=128).contains(n))
                    .ok_or_else(|| invalid("invalid worker manifest size"))?
                    as usize;
                exclusive = match details.get("ownership") {
                    None => false,
                    Some(Value::String(v)) if v == crate::ownership::PROTOCOL => true,
                    _ => return Err(invalid("unsupported controller ownership protocol")),
                };
                persist_results = match details.get("persistResults") {
                    None => false,
                    Some(Value::Bool(v)) => *v,
                    _ => return Err(invalid("invalid checkpoint consent metadata")),
                };
                resume_binding = match details.get("resumeBinding") {
                    None => None,
                    Some(value) => {
                        let binding: crate::resume::RunBinding = serde_json::from_value(value.clone())
                            .map_err(|_| invalid("invalid run budget binding"))?;
                        binding.validate()?;
                        if !exclusive
                            || details["parallelism"].as_u64() != Some(binding.parallelism as u64)
                            || binding.persist_results != persist_results
                        {
                            return Err(invalid("run binding disagrees with journal"));
                        }
                        Some(binding)
                    }
                };
                continue;
            }
            match record.event.as_str() {
                "queued" => {
                    let id = record
                        .worker
                        .as_deref()
                        .filter(|s| valid_id(s))
                        .ok_or_else(|| invalid("invalid queued identity"))?;
                    if workers.len() >= expected
                        || workers.contains_key(id)
                        || workers.values().any(|w| w.phase != Phase::Queued)
                    {
                        return Err(invalid("invalid queued worker manifest"));
                    }
                    required_hash(details, "roleSha256")?;
                    required_hash(details, "taskSha256")?;
                    if !details["reservedBytes"]
                        .as_u64()
                        .is_some_and(|n| (1..=16 * 1024 * 1024).contains(&n))
                    {
                        return Err(invalid("invalid worker response reservation"));
                    }
                    let worker_binding = match details.get("resumeBinding") {
                        None if resume_binding.is_none() => None,
                        Some(value) if resume_binding.is_some() => {
                            let binding: crate::resume::WorkerBinding = serde_json::from_value(value.clone())
                                .map_err(|_| invalid("invalid worker plan binding"))?;
                            binding.validate()?;
                            if binding.endpoint_sha256 != required_hash(details, "agentUrlSha256")?
                                || binding.role_sha256 != required_hash(details, "roleSha256")?
                                || binding.task_sha256 != required_hash(details, "taskSha256")?
                                || details["reservedBytes"].as_u64() != Some(binding.response_bytes as u64)
                            {
                                return Err(invalid("worker binding disagrees with journal"));
                            }
                            Some(binding)
                        }
                        _ => return Err(invalid("incomplete resume bindings")),
                    };
                    workers.insert(
                        id.into(),
                        WorkerSnapshot {
                            worker: id.into(),
                            phase: Phase::Queued,
                            binding: None,
                            agent_url_sha256: required_hash(details, "agentUrlSha256")?,
                            recorded_outcome: None,
                            recorded_stop: None,
                            recorded_task_state: None,
                            result_sha256: None,
                            checkpoint_sha256: None,
                            resume_binding: worker_binding,
                        },
                    );
                }
                "starting" => {
                    if workers.len() != expected {
                        return Err(invalid("worker started before complete manifest"));
                    }
                    let worker = worker_mut(&mut workers, &record)?;
                    if worker.phase != Phase::Queued {
                        return Err(invalid("duplicate worker start"));
                    }
                    worker.phase = Phase::Started;
                }
                "task_bound" => {
                    let worker = worker_mut(&mut workers, &record)?;
                    if worker.phase != Phase::Started {
                        return Err(invalid("duplicate or out-of-order task binding"));
                    }
                    let task_id = text(details, "taskId")?;
                    let context_id = text(details, "contextId")?;
                    if !valid_task_id(task_id) || !valid_task_id(context_id) {
                        return Err(invalid("invalid task identity"));
                    }
                    worker.binding = Some(Binding {
                        task_id: task_id.into(),
                        context_id: context_id.into(),
                    });
                    worker.phase = Phase::Bound;
                }
                "finished" => {
                    if workers.len() != expected {
                        return Err(invalid("worker finished before complete manifest"));
                    }
                    let worker = worker_mut(&mut workers, &record)?;
                    if worker.phase == Phase::Finished {
                        return Err(invalid("duplicate worker finish"));
                    }
                    let outcome: Outcome = serde_json::from_value(details["outcome"].clone())
                        .map_err(|_| invalid("invalid recorded outcome"))?;
                    let stop: StopConfirmation = serde_json::from_value(details["stop"].clone())
                        .map_err(|_| invalid("invalid stop evidence"))?;
                    let state: Option<TaskState> = serde_json::from_value(details["taskState"].clone())
                        .map_err(|_| invalid("invalid task state"))?;
                    let result_sha256 = optional_hash(details, "resultSha256")?;
                    let checkpoint_sha256 = optional_hash(details, "checkpointSha256")?;
                    if (checkpoint_sha256.is_some() != persist_results)
                        || (worker.phase == Phase::Queued
                            && (!matches!(outcome, Outcome::Canceled | Outcome::Deadline)
                                || stop != StopConfirmation::NotNeeded
                                || result_sha256.is_some()))
                        || (worker.binding.is_none()
                            && (state.is_some() || stop == StopConfirmation::Confirmed))
                        || (worker.binding.is_some() && (state.is_none() || result_sha256.is_none()))
                        || (stop == StopConfirmation::Confirmed
                            && !state.as_ref().is_some_and(TaskState::is_terminal))
                        || (worker.binding.is_some()
                            && stop == StopConfirmation::NotNeeded
                            && !state.as_ref().is_some_and(TaskState::is_terminal))
                        || (outcome == Outcome::Completed
                            && (stop == StopConfirmation::Unconfirmed
                                || result_sha256.is_none()
                                || state.as_ref().is_some_and(|s| *s != TaskState::Completed)))
                    {
                        return Err(invalid("finish disagrees with recorded lifecycle evidence"));
                    }
                    worker.recorded_outcome = Some(outcome);
                    worker.recorded_stop = Some(stop);
                    worker.recorded_task_state = state;
                    worker.result_sha256 = result_sha256;
                    worker.checkpoint_sha256 = checkpoint_sha256;
                    worker.phase = Phase::Finished;
                }
                "run_finished" => {
                    if record.worker.is_some()
                        || workers.len() != expected
                        || workers.values().any(|w| w.phase != Phase::Finished)
                        || details["workers"].as_u64() != Some(expected as u64)
                        || details["completed"].as_u64()
                            != Some(
                                workers
                                    .values()
                                    .filter(|w| w.recorded_outcome == Some(Outcome::Completed))
                                    .count() as u64,
                            )
                    {
                        return Err(invalid("invalid run completion record"));
                    }
                    finished = true;
                }
                "control_rejected" => {
                    if workers.len() != expected || record.worker.as_deref().is_some_and(|id| !valid_id(id)) {
                        return Err(invalid("invalid rejected control metadata"));
                    }
                }
                "steer_requested" | "steer_admitted" | "steer_unconfirmed" | "status" => {
                    let worker = worker_mut(&mut workers, &record)?;
                    if !matches!(worker.phase, Phase::Started | Phase::Bound) {
                        return Err(invalid("metadata outside active lifecycle"));
                    }
                    optional_hash(details, "textSha256")?;
                }
                _ => return Err(invalid("unsupported A2A journal event")),
            }
        }
        if let Some(binding) = &resume_binding {
            let reserved = workers.values().try_fold(0usize, |sum, w| {
                sum.checked_add(w.resume_binding.as_ref().unwrap().response_bytes)
                    .ok_or_else(|| invalid("worker reservation overflow"))
            })?;
            if reserved > binding.response_reservation {
                return Err(invalid("worker reservations exceed run binding"));
            }
        }
        Ok(Self {
            snapshot: Snapshot {
                version: 1,
                run: run.unwrap(),
                journal_sha256: format!("{:x}", Sha256::digest(&bytes)),
                expected_workers: expected,
                unlisted_workers: expected - workers.len(),
                records,
                truncated_tail: end != bytes.len(),
                recorded_run_finished: finished,
                exclusive_ownership_supported: exclusive,
                persist_results,
                resume_binding,
                workers: workers.into_values().collect(),
            },
        })
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
        .ok_or_else(|| invalid("event names an unlisted worker"))
}
fn text<'a>(value: &'a Value, key: &str) -> io::Result<&'a str> {
    value[key]
        .as_str()
        .ok_or_else(|| invalid("missing journal field"))
}
fn required_hash(value: &Value, key: &str) -> io::Result<String> {
    optional_hash(value, key)?.ok_or_else(|| invalid("missing recorded hash"))
}
fn optional_hash(value: &Value, key: &str) -> io::Result<Option<String>> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(v))
            if v.len() == 64
                && v.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) =>
        {
            Ok(Some(v.clone()))
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
fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn row(event: &str, worker: Option<&str>, details: Value) -> Value {
        json!({"event":event,"worker":worker,"details":details})
    }
    fn fixture() -> Vec<Value> {
        let hash = "a".repeat(64);
        vec![
            row(
                "run_started",
                None,
                json!({"run":"fixture","transport":"A2A-0.3.0","workers":1,"ownership":crate::ownership::PROTOCOL,"persistResults":true}),
            ),
            row(
                "queued",
                Some("worker"),
                json!({"roleSha256":hash,"taskSha256":hash,"agentUrlSha256":hash,"reservedBytes":1024}),
            ),
            row("starting", Some("worker"), json!({})),
            row(
                "task_bound",
                Some("worker"),
                json!({"taskId":"task","contextId":"context"}),
            ),
            row(
                "finished",
                Some("worker"),
                json!({"outcome":"completed","stop":"not_needed","taskState":"completed","resultSha256":hash,"checkpointSha256":hash}),
            ),
            row("run_finished", None, json!({"workers":1,"completed":1})),
        ]
    }
    fn encode(rows: Vec<Value>) -> Vec<u8> {
        rows.into_iter()
            .enumerate()
            .flat_map(|(i, mut r)| {
                r["schema"] = json!(1);
                r["sequence"] = json!(i);
                r["elapsedMs"] = json!(i);
                let mut b = serde_json::to_vec(&r).unwrap();
                b.push(b'\n');
                b
            })
            .collect()
    }
    #[test]
    fn complete_prefix_tail_fingerprint_and_legacy_ownership_are_distinct() {
        let rows = fixture();
        let full = encode(rows.clone());
        let inspected = InspectedRun::from_bytes(full.clone()).unwrap();
        let snapshot = inspected.snapshot();
        assert!(
            snapshot.recorded_run_finished
                && snapshot.exclusive_ownership_supported
                && snapshot.persist_results
        );
        assert_eq!(snapshot.records, 6);
        assert_eq!(snapshot.workers[0].binding.as_ref().unwrap().task_id, "task");
        assert_eq!(
            snapshot.workers[0].recorded_stop,
            Some(StopConfirmation::NotNeeded)
        );
        let mut tail = encode(rows[..4].to_vec());
        tail.extend(b"{partial");
        let partial = InspectedRun::from_bytes(tail.clone()).unwrap();
        assert!(partial.snapshot.truncated_tail);
        assert!(!partial.snapshot.recorded_run_finished);
        assert_eq!(partial.snapshot.workers[0].phase, Phase::Bound);
        let digest = partial.snapshot.journal_sha256;
        tail.push(b'x');
        assert_ne!(
            InspectedRun::from_bytes(tail).unwrap().snapshot.journal_sha256,
            digest
        );
        let mut legacy = rows;
        legacy[0]["details"].as_object_mut().unwrap().remove("ownership");
        assert!(
            !InspectedRun::from_bytes(encode(legacy))
                .unwrap()
                .snapshot
                .exclusive_ownership_supported
        );
        assert!(InspectedRun::from_bytes(b"{incomplete".to_vec()).is_err());
        let mut oversized = full;
        oversized.extend(vec![b'x'; MAX_LINE + 1]);
        assert!(InspectedRun::from_bytes(oversized).is_err());
    }
    #[test]
    fn inconsistent_lifecycle_identity_consent_hashes_and_cleanup_fail_closed() {
        for mutation in 0..22 {
            let mut rows = fixture();
            match mutation {
                0 => rows[0]["details"]["transport"] = json!("native"),
                1 => rows[0]["details"]["workers"] = json!(2),
                2 => rows[1]["worker"] = json!("../invalid"),
                3 => rows[1]["details"]["agentUrlSha256"] = json!("BAD"),
                4 => rows[2]["event"] = json!("spawned"),
                5 => rows[3]["details"]["taskId"] = json!("bad\nidentity"),
                6 => rows[4]["details"]["stop"] = json!("unconfirmed"),
                7 => rows[4]["details"]["taskState"] = json!("working"),
                8 => rows[4]["details"]["resultSha256"] = Value::Null,
                9 => rows[0]["details"]["persistResults"] = json!(false),
                10 => rows[4]["details"]["checkpointSha256"] = json!("a".repeat(63)),
                11 => rows[5]["details"]["completed"] = json!(0),
                12 => rows.push(row("status", Some("worker"), json!({}))),
                13 => {
                    rows.insert(4, rows[3].clone());
                }
                14 => {
                    rows.insert(5, rows[4].clone());
                }
                15 => {
                    rows.insert(2, row("status", Some("worker"), json!({})));
                }
                16 => rows[0]["details"]["resumeBinding"] = json!({}),
                17 => rows[1]["details"]["reservedBytes"] = json!(0),
                18 => rows[0]["details"]["ownership"] = json!("unknown"),
                19 => rows[4]["details"]["checkpointSha256"] = Value::Null,
                20 => rows[0]["details"]["persistResults"] = json!("true"),
                _ => rows[1]["details"]["roleSha256"] = Value::Null,
            }
            assert!(
                InspectedRun::from_bytes(encode(rows)).is_err(),
                "mutation {mutation}"
            );
        }
        let mut rows = fixture();
        rows[4]["details"]["outcome"] = json!("deadline");
        rows[4]["details"]["stop"] = json!("unconfirmed");
        rows[4]["details"]["taskState"] = json!("working");
        rows[5]["details"]["completed"] = json!(0);
        let snapshot = InspectedRun::from_bytes(encode(rows)).unwrap().snapshot;
        assert!(snapshot.recorded_run_finished);
        assert_eq!(
            snapshot.workers[0].recorded_stop,
            Some(StopConfirmation::Unconfirmed)
        );
        // A local run_finished record is never proof of remote cleanup.
    }
    #[test]
    fn record_count_schema_sequence_and_complete_line_bounds_are_enforced() {
        let rows = fixture();
        let mut malformed = encode(rows.clone());
        let boundary = malformed.iter().position(|b| *b == b'\n').unwrap();
        malformed[boundary] = b'x';
        assert!(InspectedRun::from_bytes(malformed).is_err());
        let mut decreasing = encode(rows.clone());
        let text = String::from_utf8(decreasing.clone())
            .unwrap()
            .replace("\"sequence\":3", "\"sequence\":2");
        decreasing = text.into_bytes();
        assert!(InspectedRun::from_bytes(decreasing).is_err());
        let mut oversized = rows.clone();
        oversized[3]["details"]["extra"] = json!("x".repeat(MAX_LINE));
        assert!(InspectedRun::from_bytes(encode(oversized)).is_err());
        let mut many = rows[..4].to_vec();
        many.resize(MAX_RECORDS, row("status", Some("worker"), json!({})));
        assert_eq!(
            InspectedRun::from_bytes(encode(many.clone()))
                .unwrap()
                .snapshot
                .records,
            MAX_RECORDS
        );
        many.push(row("status", Some("worker"), json!({})));
        assert!(InspectedRun::from_bytes(encode(many)).is_err());
    }
    #[test]
    fn claims_refuse_live_writers_and_legacy_records_and_release_on_error() {
        use oxa_fake::TestEnv;
        use std::fs;
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        let env = TestEnv::new();
        let parent = env.root().join("private");
        fs::DirBuilder::new().mode(0o700).create(&parent).unwrap();
        let path = parent.join("transcript.jsonl");
        let mut writer = crate::journal::Journal::create(&path, 64 * 1024).unwrap();
        for row in fixture().into_iter().take(4) {
            writer
                .record(
                    row["worker"].as_str(),
                    row["event"].as_str().unwrap(),
                    row["details"].clone(),
                )
                .unwrap();
        }
        assert_eq!(
            InspectedRun::inspect(&path).unwrap().snapshot.workers[0].phase,
            Phase::Bound
        );
        assert_eq!(
            ExclusiveRun::claim(&path).err().unwrap().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(writer);
        let claim = ExclusiveRun::claim(&path).unwrap();
        assert_eq!(claim.inspected().snapshot().workers[0].phase, Phase::Bound);
        drop(claim);
        let legacy = parent.join("legacy.jsonl");
        let mut rows = fixture();
        rows[0]["details"].as_object_mut().unwrap().remove("ownership");
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&legacy)
            .unwrap();
        file.write_all(&encode(rows)).unwrap();
        drop(file);
        assert!(ExclusiveRun::claim(&legacy).is_err());
        fs::write(&legacy, encode(fixture())).unwrap();
        assert!(ExclusiveRun::claim(&legacy).is_ok());
    }
    #[test]
    fn resume_bindings_match_the_manifest_and_cannot_widen_recorded_policy() {
        use crate::resume::{RunBinding, WorkerBinding};
        let mut rows = fixture();
        let run = RunBinding {
            version: 1,
            boot_sha256: "a".repeat(64),
            started_ns: 100,
            deadline_ns: 200,
            parallelism: 1,
            response_reservation: 1024,
            journal_bytes: 1024,
            request_timeout_ns: 1,
            poll_interval_ns: 1_000_000,
            cancel_grace_ns: 1,
            persist_results: true,
        };
        let worker = WorkerBinding {
            role_sha256: "a".repeat(64),
            task_sha256: "a".repeat(64),
            endpoint_sha256: "a".repeat(64),
            wall_time_ns: 1,
            response_bytes: 1024,
            reported_tokens: Some(1),
        };
        rows[0]["details"]["parallelism"] = json!(1);
        rows[0]["details"]["resumeBinding"] = serde_json::to_value(&run).unwrap();
        rows[1]["details"]["resumeBinding"] = serde_json::to_value(&worker).unwrap();
        let inspected = InspectedRun::from_bytes(encode(rows.clone())).unwrap();
        assert_eq!(inspected.snapshot.resume_binding.as_ref(), Some(&run));
        assert_eq!(
            inspected.snapshot.workers[0].resume_binding.as_ref(),
            Some(&worker)
        );
        for mutation in 0..13 {
            let mut changed = rows.clone();
            match mutation {
                0 => changed[0]["details"]["resumeBinding"]["response_reservation"] = json!(1023),
                1 => changed[0]["details"]["resumeBinding"]["parallelism"] = json!(2),
                2 => changed[0]["details"]["resumeBinding"]["persist_results"] = json!(false),
                3 => {
                    changed[1]["details"]
                        .as_object_mut()
                        .unwrap()
                        .remove("resumeBinding");
                }
                4 => {
                    changed[0]["details"]
                        .as_object_mut()
                        .unwrap()
                        .remove("resumeBinding");
                }
                5 => changed[1]["details"]["resumeBinding"]["role_sha256"] = json!("b".repeat(64)),
                6 => changed[1]["details"]["resumeBinding"]["response_bytes"] = json!(2048),
                7 => changed[1]["details"]["resumeBinding"]["reported_tokens"] = json!(0),
                8 => changed[0]["details"]["resumeBinding"]["journal_bytes"] = json!(16 * 1024 * 1024 + 1),
                9 => changed[0]["details"]["resumeBinding"]["unknown_policy"] = json!(true),
                10 => changed[1]["details"]["resumeBinding"]["unknown_policy"] = json!(true),
                11 => changed[0]["details"]["resumeBinding"]["deadline_ns"] = json!(100),
                _ => changed[0]["details"]["resumeBinding"]["boot_sha256"] = json!("bad"),
            }
            assert!(
                InspectedRun::from_bytes(encode(changed)).is_err(),
                "mutation {mutation}"
            );
        }
    }
}
