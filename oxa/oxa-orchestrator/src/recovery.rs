//! Bounded native journal inspection. History is never live process authority.
//! There is no network access, replay, deletion, task submission or PID signal.
use crate::supervisor::{Cleanup, Outcome};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
const MAX_JOURNAL: usize = 16 * 1024 * 1024;
const MAX_LINE: usize = 64 * 1024;
const MAX_RECORDS: usize = 100_000;
const PROFILE_V1: &str = "tool-free-rpc-v1";
const PROFILE_V2: &str = "tool-free-rpc-v2";
const PROFILE_WORKSPACE: &str = "workspace-rpc-v1";
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Queued,
    Started,
    Finished,
    Unconfirmed,
}
#[derive(Clone, Debug, Serialize)]
pub struct WorkerSnapshot {
    pub worker: String,
    pub phase: Phase,
    pub recorded_outcome: Option<Outcome>,
    pub recorded_cleanup: Option<Cleanup>,
    /// Historical only. Never identifies a process that recovery may signal.
    pub recorded_process_id: Option<u32>,
    pub result_sha256: Option<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct Snapshot {
    pub version: u32,
    pub run: String,
    pub profile: Option<String>,
    pub journal_sha256: String,
    pub expected_workers: usize,
    pub unlisted_workers: usize,
    pub records: usize,
    pub truncated_tail: bool,
    pub recorded_run_finished: bool,
    pub exclusive_ownership_supported: bool,
    pub workers: Vec<WorkerSnapshot>,
}
pub struct InspectedRun {
    snapshot: Snapshot,
}
/// Exclusive authority over one cooperative journal inode, not a worker process.
/// Holding this guard does not prove a local process or provider has stopped.
/// No recovery action is implemented by this inspection-only slice.
pub struct ExclusiveRun {
    _journal: crate::ownership::Lease,
    inspected: InspectedRun,
}
impl ExclusiveRun {
    pub fn claim(path: &Path) -> io::Result<Self> {
        let file = open_input(path)?;
        let journal = crate::ownership::Lease::acquire(file)?;
        let inspected = InspectedRun::from_bytes(read_checked(&journal)?)?;
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
        Self::from_bytes(read_checked(&open_input(path)?)?)
    }
    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }
    fn from_bytes(bytes: Vec<u8>) -> io::Result<Self> {
        if bytes.len() > MAX_JOURNAL {
            return Err(invalid("journal size bound exceeded"));
        }
        let end = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
        if end == 0 || bytes.len() - end > MAX_LINE {
            return Err(invalid("journal has no complete record or an oversized tail"));
        }
        let mut workers = BTreeMap::<String, WorkerSnapshot>::new();
        let mut run = None;
        let mut profile = None;
        let mut expected = 0;
        let mut elapsed = 0;
        let mut records = 0;
        let mut finished = false;
        let mut closed = false;
        let mut exclusive = false;
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
                || closed
            {
                return Err(invalid("journal schema, sequence or lifecycle mismatch"));
            }
            records += 1;
            elapsed = record.elapsed_ms;
            if run.is_none() {
                if record.event != "run_started" || record.worker.is_some() {
                    return Err(invalid("journal must begin with run_started"));
                }
                let id = text(&record.details, "run")?;
                if !valid_id(id) {
                    return Err(invalid("invalid recorded run identity"));
                }
                run = Some(id.to_owned());
                expected = record.details["workers"]
                    .as_u64()
                    .filter(|n| *n > 0 && *n <= 128)
                    .ok_or_else(|| invalid("invalid worker manifest size"))?
                    as usize;
                if record.details.get("transport").is_some() || record.details.get("resumeBinding").is_some()
                {
                    return Err(invalid(
                        "agent reconciliation and resume are not supported by native inspection",
                    ));
                }
                profile = match record.details.get("profile") {
                    None => None,
                    Some(Value::String(v))
                        if v == PROFILE_V1 || v == PROFILE_V2 || v == PROFILE_WORKSPACE =>
                    {
                        Some(v.clone())
                    }
                    _ => return Err(invalid("unsupported native journal profile")),
                };
                exclusive = match record.details.get("ownership") {
                    None => false,
                    Some(Value::String(v)) if v == crate::ownership::PROTOCOL => true,
                    _ => return Err(invalid("unsupported journal ownership protocol")),
                };
                continue;
            }
            match record.event.as_str() {
                "queued" => {
                    let id = record
                        .worker
                        .as_deref()
                        .filter(|s| valid_id(s))
                        .ok_or_else(|| invalid("invalid queued worker identity"))?;
                    if workers.len() >= expected
                        || workers.contains_key(id)
                        || workers.values().any(|w| w.phase != Phase::Queued)
                    {
                        return Err(invalid("invalid queued worker manifest"));
                    }
                    for name in ["roleSha256", "taskSha256"] {
                        optional_hash(&record.details, name)?;
                    }
                    workers.insert(
                        id.into(),
                        WorkerSnapshot {
                            worker: id.into(),
                            phase: Phase::Queued,
                            recorded_outcome: None,
                            recorded_cleanup: None,
                            recorded_process_id: None,
                            result_sha256: None,
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
                "spawned" => {
                    let worker = worker_mut(&mut workers, &record)?;
                    if worker.phase != Phase::Started || worker.recorded_process_id.is_some() {
                        return Err(invalid("invalid recorded process lifecycle"));
                    }
                    worker.recorded_process_id = Some(
                        record.details["processId"]
                            .as_u64()
                            .filter(|n| *n > 0 && *n <= u32::MAX as u64)
                            .ok_or_else(|| invalid("invalid recorded process ID"))?
                            as u32,
                    );
                }
                "finished" => {
                    if workers.len() != expected {
                        return Err(invalid("worker settled before complete manifest"));
                    }
                    let worker = worker_mut(&mut workers, &record)?;
                    if matches!(worker.phase, Phase::Finished | Phase::Unconfirmed) {
                        return Err(invalid("duplicate worker finish"));
                    }
                    let outcome: Outcome = serde_json::from_value(record.details["outcome"].clone())
                        .map_err(|_| invalid("invalid recorded outcome"))?;
                    if worker.phase == Phase::Queued
                        && !matches!(
                            outcome,
                            Outcome::Canceled
                                | Outcome::Deadline
                                | Outcome::TokenBudget
                                | Outcome::OutputBudget
                        )
                    {
                        return Err(invalid("unstarted worker has impossible outcome"));
                    }
                    let cleanup = match record.details.get("cleanup") {
                        None if !matches!(profile.as_deref(), Some(PROFILE_V2 | PROFILE_WORKSPACE)) => {
                            if profile.as_deref() == Some(PROFILE_V1)
                                && record.details["processStopped"] == true
                            {
                                Some(Cleanup::Stopped)
                            } else if profile.as_deref() == Some(PROFILE_V1)
                                && record.details["processStopped"] == false
                                && worker.phase == Phase::Queued
                            {
                                Some(Cleanup::NotStarted)
                            } else {
                                None
                            }
                        }
                        Some(value) => Some(
                            serde_json::from_value::<Cleanup>(value.clone())
                                .map_err(|_| invalid("invalid cleanup evidence"))?,
                        ),
                        _ => return Err(invalid("profile requires explicit cleanup evidence")),
                    };
                    if let Some(cleanup) = cleanup {
                        if cleanup == Cleanup::Unconfirmed
                            || record.details["processStopped"].as_bool() != Some(cleanup == Cleanup::Stopped)
                            || (cleanup == Cleanup::Stopped && worker.recorded_process_id.is_none())
                            || (cleanup == Cleanup::NotStarted && worker.recorded_process_id.is_some())
                            || (outcome == Outcome::Completed && cleanup != Cleanup::Stopped)
                        {
                            return Err(invalid("finish disagrees with recorded cleanup"));
                        }
                    }
                    worker.recorded_outcome = Some(outcome);
                    worker.recorded_cleanup = cleanup;
                    worker.result_sha256 = optional_hash(&record.details, "resultSha256")?;
                    worker.phase = Phase::Finished;
                }
                "cleanup_unconfirmed" => {
                    if record.worker.is_some() {
                        let worker = worker_mut(&mut workers, &record)?;
                        if worker.phase != Phase::Started
                            || worker.recorded_process_id.is_none()
                            || record.details["outcome"] != "lost"
                            || (record.details["cleanup"] != "unconfirmed"
                                && (matches!(profile.as_deref(), Some(PROFILE_V2 | PROFILE_WORKSPACE))
                                    || record.details.get("cleanup").is_some()))
                            || record.details["processStopped"] != false
                        {
                            return Err(invalid("invalid unconfirmed cleanup record"));
                        }
                        worker.phase = Phase::Unconfirmed;
                        worker.recorded_outcome = Some(Outcome::Lost);
                        worker.recorded_cleanup = Some(Cleanup::Unconfirmed);
                        worker.result_sha256 = optional_hash(&record.details, "resultSha256")?;
                    } else {
                        if workers.len() != expected
                            || !workers.values().any(|w| w.phase == Phase::Unconfirmed)
                            || workers
                                .values()
                                .any(|w| matches!(w.phase, Phase::Queued | Phase::Started))
                        {
                            return Err(invalid("invalid unconfirmed run closure"));
                        }
                        closed = true;
                    }
                }
                "run_finished" => {
                    let completed = workers
                        .values()
                        .filter(|w| w.recorded_outcome == Some(Outcome::Completed))
                        .count();
                    if record.worker.is_some()
                        || workers.len() != expected
                        || workers.values().any(|w| w.phase != Phase::Finished)
                        || record.details["workers"].as_u64() != Some(expected as u64)
                        || record.details["completed"].as_u64() != Some(completed as u64)
                        || record
                            .details
                            .get("succeeded")
                            .is_some_and(|v| v.as_bool() != Some(completed == expected))
                    {
                        return Err(invalid("invalid run completion record"));
                    }
                    finished = true;
                    closed = true;
                }
                "control_rejected" | "cancel_all" => {
                    if record.worker.is_some() || workers.len() != expected {
                        return Err(invalid("invalid global control metadata"));
                    }
                }
                "workspace" | "workspace_tool" | "workspace_review" | "stopping" | "policy_verified"
                | "working" | "usage" | "permission_denied" | "steered" | "steer_requested"
                | "steer_admitted" | "steer_unconfirmed" | "status" => {
                    if worker_mut(&mut workers, &record)?.phase != Phase::Started {
                        return Err(invalid("metadata outside active worker lifecycle"));
                    }
                }
                _ => return Err(invalid("unsupported journal event")),
            }
        }
        Ok(Self {
            snapshot: Snapshot {
                version: 1,
                run: run.unwrap(),
                profile,
                journal_sha256: format!("{:x}", Sha256::digest(&bytes)),
                expected_workers: expected,
                unlisted_workers: expected - workers.len(),
                records,
                truncated_tail: end != bytes.len(),
                recorded_run_finished: finished,
                exclusive_ownership_supported: exclusive,
                workers: workers.into_values().collect(),
            },
        })
    }
}
fn worker_mut<'a>(
    workers: &'a mut BTreeMap<String, WorkerSnapshot>,
    record: &Record,
) -> io::Result<&'a mut WorkerSnapshot> {
    workers
        .get_mut(
            record
                .worker
                .as_deref()
                .ok_or_else(|| invalid("record needs a worker"))?,
        )
        .ok_or_else(|| invalid("record names an unlisted worker"))
}
fn text<'a>(value: &'a Value, key: &str) -> io::Result<&'a str> {
    value[key]
        .as_str()
        .ok_or_else(|| invalid("missing journal field"))
}
fn optional_hash(value: &Value, key: &str) -> io::Result<Option<String>> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s))
            if s.len() == 64
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) =>
        {
            Ok(Some(s.clone()))
        }
        _ => Err(invalid("invalid journal hash")),
    }
}
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}
fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
pub(crate) fn open_input(path: &Path) -> io::Result<File> {
    open_input_limit(path, MAX_JOURNAL)
}
fn open_input_limit(path: &Path, max: usize) -> io::Result<File> {
    let parent = path.parent().ok_or_else(|| invalid("journal needs a parent"))?;
    let directory = crate::namespace::protected_directory(parent)?;
    let metadata = directory.metadata()?;
    // SAFETY: geteuid only returns this process's effective identity.
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
        return Err(invalid("journal parent must be private and operator-owned"));
    }
    let name = path
        .file_name()
        .ok_or_else(|| invalid("journal needs a file name"))?;
    let handle = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(
            Path::new("/proc/self/fd")
                .join(directory.as_raw_fd().to_string())
                .join(name),
        )?;
    let metadata = handle.metadata()?;
    // SAFETY: geteuid only returns this process's effective identity.
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.len() > max as u64
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
        || metadata.dev() != directory.metadata()?.dev()
    {
        return Err(invalid(
            "journal must be a private bounded plain single-link file",
        ));
    }
    // Reopen the pinned validated inode; no attacker-replaceable path is read.
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOCTTY)
        .open(format!("/proc/self/fd/{}", handle.as_raw_fd()))
}
pub(crate) fn read_checked(file: &File) -> io::Result<Vec<u8>> {
    read_checked_limit(file, MAX_JOURNAL)
}
pub(crate) fn read_bounded(path: &Path, max: usize) -> io::Result<Vec<u8>> {
    read_checked_limit(&open_input_limit(path, max)?, max)
}
fn read_checked_limit(file: &File, max: usize) -> io::Result<Vec<u8>> {
    let before = file.metadata()?;
    // SAFETY: geteuid only returns this process's effective identity.
    if !before.is_file()
        || before.nlink() != 1
        || before.len() > max as u64
        || before.mode() & 0o077 != 0
        || before.uid() != unsafe { libc::geteuid() }
    {
        return Err(invalid("journal changed before capture"));
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut &*file)
        .take(max as u64 + 1)
        .read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    if bytes.len() > max
        || (
            before.dev(),
            before.ino(),
            before.len(),
            before.mtime(),
            before.mtime_nsec(),
            before.ctime(),
            before.ctime_nsec(),
            before.nlink(),
            before.mode(),
            before.uid(),
        ) != (
            after.dev(),
            after.ino(),
            after.len(),
            after.mtime(),
            after.mtime_nsec(),
            after.ctime(),
            after.ctime_nsec(),
            after.nlink(),
            after.mode(),
            after.uid(),
        )
    {
        return Err(invalid("journal changed during capture or exceeded its bound"));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::Journal;
    use oxa_fake::TestEnv;
    use serde_json::json;
    use std::fs;
    use std::os::unix::fs::{symlink, DirBuilderExt, PermissionsExt};
    fn row(event: &str, worker: Option<&str>, details: Value) -> Value {
        json!({"event":event,"worker":worker,"details":details})
    }
    fn encode(rows: Vec<Value>) -> Vec<u8> {
        let mut bytes = Vec::new();
        for (i, mut row) in rows.into_iter().enumerate() {
            let fields = row.as_object_mut().unwrap();
            fields.entry("schema").or_insert(json!(1));
            fields.entry("sequence").or_insert(json!(i));
            fields.entry("elapsedMs").or_insert(json!(i));
            serde_json::to_writer(&mut bytes, &row).unwrap();
            bytes.push(b'\n');
        }
        bytes
    }
    fn complete() -> Vec<Value> {
        vec![
            row(
                "run_started",
                None,
                json!({"run":"fixture","workers":1,"profile":PROFILE_V2,"ownership":crate::ownership::PROTOCOL}),
            ),
            row("queued", Some("worker"), json!({})),
            row("starting", Some("worker"), json!({})),
            row("spawned", Some("worker"), json!({"processId":424242})),
            row(
                "finished",
                Some("worker"),
                json!({"outcome":"completed","cleanup":"stopped","processStopped":true}),
            ),
            row(
                "run_finished",
                None,
                json!({"workers":1,"completed":1,"succeeded":true}),
            ),
        ]
    }
    fn private(env: &TestEnv) -> std::path::PathBuf {
        let directory = env.root().join("private");
        fs::DirBuilder::new().mode(0o700).create(&directory).unwrap();
        directory.join("audit.jsonl")
    }
    fn write(path: &Path, bytes: &[u8]) {
        fs::write(path, bytes).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[test]
    fn actual_supervisor_complete_and_truncated_lanes_are_inspected_without_replay() {
        let (_env, settings, plan) = crate::supervisor::tests::fixture(&["normal", "length"]);
        let path = settings.journal.clone();
        let (_controller, inbox) = crate::supervisor::controls();
        let report = crate::supervisor::run(settings, plan, inbox, |_| {}).unwrap();
        assert!(!report.succeeded());
        assert!(report.cleanup_confirmed());
        let inspected = InspectedRun::inspect(&path).unwrap();
        let snapshot = inspected.snapshot();
        assert!(snapshot.recorded_run_finished);
        assert!(!snapshot.truncated_tail);
        assert!(snapshot.exclusive_ownership_supported);
        assert_eq!(snapshot.profile.as_deref(), Some(PROFILE_V2));
        assert_eq!(snapshot.unlisted_workers, 0);
        assert_eq!(snapshot.workers[0].recorded_outcome, Some(Outcome::Completed));
        assert_eq!(snapshot.workers[1].recorded_outcome, Some(Outcome::Truncated));
        assert!(snapshot
            .workers
            .iter()
            .all(|w| w.recorded_cleanup == Some(Cleanup::Stopped)));
        let rendered = serde_json::to_string(snapshot).unwrap();
        assert!(!rendered.contains("attributed partial"));
        assert!(!rendered.contains("synthetic-private-role"));
        let claimed = ExclusiveRun::claim(&path).unwrap();
        assert!(claimed.inspected().snapshot().recorded_run_finished);
        assert_eq!(
            ExclusiveRun::claim(&path).err().unwrap().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(claimed);
        assert!(ExclusiveRun::claim(&path).is_ok());
    }
    #[test]
    fn a_live_or_poisoned_writer_excludes_recovery_until_its_lease_is_released() {
        let env = TestEnv::new();
        let path = private(&env);
        let mut writer = Journal::create(&path, 1024).unwrap();
        writer
            .record(
                None,
                "run_started",
                json!({"run":"fixture","workers":1,"profile":PROFILE_V2}),
            )
            .unwrap();
        writer.record(Some("worker"), "queued", json!({})).unwrap();
        assert!(
            !InspectedRun::inspect(&path)
                .unwrap()
                .snapshot()
                .recorded_run_finished
        );
        assert_eq!(
            ExclusiveRun::claim(&path).err().unwrap().kind(),
            io::ErrorKind::WouldBlock
        );
        assert!(writer
            .record(Some("worker"), "status", json!({"large":"x".repeat(2000)}))
            .is_err());
        assert_eq!(
            ExclusiveRun::claim(&path).err().unwrap().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(writer);
        let claimed = ExclusiveRun::claim(&path).unwrap();
        assert_eq!(claimed.inspected().snapshot().records, 2);
        assert!(!claimed.inspected().snapshot().recorded_run_finished);
    }
    #[test]
    fn incomplete_tail_is_fingerprinted_but_never_promoted_to_a_record() {
        let rows = complete();
        let prefix = encode(rows[..2].to_vec());
        let mut bytes = prefix.clone();
        bytes.extend_from_slice(br#"{"event":"run_finished","synthetic-private-tail":"do not export"}"#);
        let snapshot = InspectedRun::from_bytes(bytes.clone()).unwrap().snapshot;
        assert!(snapshot.truncated_tail);
        assert_eq!(snapshot.records, 2);
        assert!(!snapshot.recorded_run_finished);
        assert_eq!(snapshot.journal_sha256, format!("{:x}", Sha256::digest(&bytes)));
        assert_ne!(
            snapshot.journal_sha256,
            InspectedRun::from_bytes(prefix.clone())
                .unwrap()
                .snapshot
                .journal_sha256
        );
        assert!(!serde_json::to_string(&snapshot)
            .unwrap()
            .contains("synthetic-private-tail"));
        bytes.push(b'\n');
        assert!(InspectedRun::from_bytes(bytes).is_err());
        let mut rows = complete()[..4].to_vec();
        rows.push(row(
            "status",
            Some("worker"),
            json!({"padding":"x".repeat(MAX_LINE)}),
        ));
        assert!(InspectedRun::from_bytes(encode(rows)).is_err());
        let mut oversized = prefix;
        oversized.extend(std::iter::repeat_n(b'x', MAX_LINE + 1));
        assert!(InspectedRun::from_bytes(oversized).is_err());
    }
    #[test]
    fn schema_sequence_identity_and_terminal_mutations_fail_closed() {
        for kind in 0..16 {
            let mut rows = complete();
            match kind {
                0 => rows[1]["schema"] = json!(2),
                1 => rows[2]["sequence"] = json!(8),
                2 => rows[3]["elapsedMs"] = json!(0),
                3 => rows[2]["worker"] = json!("unlisted"),
                4 => rows.insert(2, rows[1].clone()),
                5 => rows[3]["details"]["processId"] = json!(0),
                6 => rows[4]["details"]["cleanup"] = json!("not_started"),
                7 => rows[4]["details"]["processStopped"] = json!(false),
                8 => rows[5]["details"]["completed"] = json!(0),
                9 => rows.push(row("control_rejected", None, json!({}))),
                10 => rows[0]["details"]["workers"] = json!(2),
                11 => rows[4]["details"]["resultSha256"] = json!("not-a-digest"),
                12 => rows[0]["details"]["profile"] = json!("unsupported-profile"),
                13 => rows[0]["details"]["ownership"] = json!("unsupported-lease"),
                14 => rows[0]["details"]["transport"] = json!("A2A-0.3.0"),
                _ => rows[0]["details"]["resumeBinding"] = json!({}),
            }
            assert!(InspectedRun::from_bytes(encode(rows)).is_err(), "mutation {kind}");
        }
    }
    #[test]
    fn legacy_and_v1_history_do_not_invent_cleanup_evidence() {
        let mut legacy = complete();
        legacy[0]["details"].as_object_mut().unwrap().remove("profile");
        legacy[0]["details"].as_object_mut().unwrap().remove("ownership");
        legacy[4]["details"].as_object_mut().unwrap().remove("cleanup");
        legacy[4]["details"]
            .as_object_mut()
            .unwrap()
            .remove("processStopped");
        let env = TestEnv::new();
        let path = private(&env);
        write(&path, &encode(legacy));
        let snapshot = InspectedRun::inspect(&path).unwrap().snapshot;
        assert!(snapshot.recorded_run_finished);
        assert!(!snapshot.exclusive_ownership_supported);
        assert_eq!(snapshot.workers[0].recorded_cleanup, None);
        assert!(ExclusiveRun::claim(&path).is_err());
        let mut v1 = complete();
        v1[0]["details"]["profile"] = json!(PROFILE_V1);
        v1.remove(3);
        v1[3]["details"] = json!({"outcome":"failed","processStopped":false});
        v1[4]["details"] = json!({"workers":1,"completed":0,"succeeded":false});
        assert_eq!(
            InspectedRun::from_bytes(encode(v1.clone()))
                .unwrap()
                .snapshot
                .workers[0]
                .recorded_cleanup,
            None
        );
        v1[0]["details"]["profile"] = json!(PROFILE_V2);
        assert!(InspectedRun::from_bytes(encode(v1)).is_err());
    }
    #[test]
    fn unconfirmed_cleanup_never_becomes_run_finished_or_process_authority() {
        let mut rows = complete();
        rows.truncate(4);
        rows.push(row(
            "cleanup_unconfirmed",
            Some("worker"),
            json!({"outcome":"lost","cleanup":"unconfirmed","processStopped":false}),
        ));
        rows.push(row("cleanup_unconfirmed", None, json!({})));
        let env = TestEnv::new();
        let path = private(&env);
        write(&path, &encode(rows.clone()));
        let claimed = ExclusiveRun::claim(&path).unwrap();
        let snapshot = claimed.inspected().snapshot();
        assert!(!snapshot.recorded_run_finished);
        assert_eq!(snapshot.workers[0].phase, Phase::Unconfirmed);
        assert_eq!(snapshot.workers[0].recorded_cleanup, Some(Cleanup::Unconfirmed));
        rows.push(row("run_finished", None, json!({"workers":1,"completed":0})));
        assert!(InspectedRun::from_bytes(encode(rows)).is_err());
    }
    #[test]
    fn private_path_checks_refuse_links_special_files_and_oversized_inputs() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let env = TestEnv::new();
        let path = private(&env);
        write(&path, &encode(complete()));
        let alias = path.with_file_name("alias");
        symlink(&path, &alias).unwrap();
        assert!(InspectedRun::inspect(&alias).is_err());
        fs::remove_file(&alias).unwrap();
        fs::hard_link(&path, &alias).unwrap();
        assert!(InspectedRun::inspect(&path).is_err());
        fs::remove_file(&alias).unwrap();
        let fifo = path.with_file_name("fifo");
        let name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: a test-owned NUL-terminated path; no reader/writer is launched.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(InspectedRun::inspect(&fifo).is_err());
        let parent_alias = env.root().join("parent-alias");
        symlink(path.parent().unwrap(), &parent_alias).unwrap();
        assert!(InspectedRun::inspect(&parent_alias.join("audit.jsonl")).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(InspectedRun::inspect(&path).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(InspectedRun::inspect(&path).is_err());
        fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
        OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(MAX_JOURNAL as u64 + 1)
            .unwrap();
        assert!(InspectedRun::inspect(&path).is_err());
    }
    #[test]
    fn a_finite_record_count_bounds_even_small_valid_metadata_events() {
        let mut bytes = encode(complete()[..4].to_vec());
        for i in 4..MAX_RECORDS {
            let value = json!({"schema":1,"sequence":i,"elapsedMs":i,"event":"status","worker":"worker","details":{}});
            serde_json::to_writer(&mut bytes, &value).unwrap();
            bytes.push(b'\n');
        }
        assert!(bytes.len() < MAX_JOURNAL);
        assert_eq!(
            InspectedRun::from_bytes(bytes.clone()).unwrap().snapshot.records,
            MAX_RECORDS
        );
        serde_json::to_writer(&mut bytes,&json!({"schema":1,"sequence":MAX_RECORDS,"elapsedMs":MAX_RECORDS,"event":"status","worker":"worker","details":{}})).unwrap();
        bytes.push(b'\n');
        assert!(InspectedRun::from_bytes(bytes).is_err());
    }
}
