//! Private durable task checkpoints. Observers never receive or mutate State.
use crate::{checkpoint, Outcome, Record, Result, Task};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const VERSION: u64 = 1;
const MAX_STATE_BYTES: u64 = 4 * 1024 * 1024;

pub(crate) fn lock(path: &Path) -> Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .map_err(|error| error.to_string())?;
    file.try_lock()
        .map_err(|error| format!("queue is already running: {error}"))?;
    Ok(file)
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    Preparing,
    Worker,
    Acceptance,
    Repair,
    Recheck,
    Saving,
    #[default]
    Finished,
    Publishing,
    Reporting,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointKind {
    #[default]
    TaskTransition,
    WorkerFinished,
}

/// Called after the checkpoint and directory are synced, and again on recovery.
/// The consumer deduplicates by repository and checkpoint_revision (source runner).
/// An error must stop the runner before another action or checkpoint.
pub trait CheckpointObserver: Sync {
    fn checkpoint(&self, record: &Record) -> Result<()>;
}
impl<F: Fn(&Record) -> Result<()> + Sync> CheckpointObserver for F {
    fn checkpoint(&self, record: &Record) -> Result<()> {
        self(record)
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Execution {
    pub initial: Option<String>,
    pub base: String,
    pub prompt: String,
    pub commands: Vec<String>,
    pub worker_error: Option<String>,
    pub merge_error: Option<String>,
    pub failures: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct State {
    pub version: u64,
    pub record: Record,
    pub task: Task,
    pub execution: Option<Execution>,
    pub offline: bool,
    pub deadline_at_ms: u64,
}
impl State {
    pub(crate) fn new(task: Task, mut record: Record, offline: bool) -> Self {
        record.outcome = Outcome::Running;
        record.step = Step::Preparing;
        let deadline_at_ms = now_ms().saturating_add(task.max_time.saturating_mul(1000));
        Self {
            version: VERSION,
            record,
            task,
            execution: None,
            offline,
            deadline_at_ms,
        }
    }
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
pub(crate) fn read(path: &Path) -> Result<State> {
    load(path)
}

pub(crate) fn load(path: &Path) -> Result<State> {
    let file = File::open(path).map_err(|e| format!("task checkpoint: {e}"))?;
    let mut bytes = Vec::new();
    file.take(MAX_STATE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_STATE_BYTES {
        return Err("task checkpoint exceeds capacity".into());
    }
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|e| format!("task checkpoint: {e}"))?;
    let fields = value.as_object().ok_or("invalid task checkpoint")?;
    if fields.len() != 6
        || [
            "version",
            "record",
            "task",
            "execution",
            "offline",
            "deadline_at_ms",
        ]
        .iter()
        .any(|key| !fields.contains_key(*key))
    {
        return Err("incomplete or unknown task checkpoint fields".into());
    }
    // Every field is explicit in the current format. Missing metadata cannot
    // silently turn an unfinished task into Finished.
    let keys = [
        "id",
        "branch",
        "worktree",
        "outcome",
        "detail",
        "worker",
        "session_id",
        "base_sha",
        "checkpoint_revision",
        "checkpoint_kind",
        "checkpoint_at_ms",
        "step",
        "head_sha",
        "tree_sha",
        "interruption",
    ];
    let record = value
        .get("record")
        .and_then(serde_json::Value::as_object)
        .ok_or("task checkpoint record is missing")?;
    let optional = [
        "pull_request",
        "draft",
        "problem_notes",
        "reports",
        "acceptance_outcome",
    ];
    if keys.iter().any(|key| !record.contains_key(*key))
        || record
            .keys()
            .any(|key| !keys.contains(&key.as_str()) && !optional.contains(&key.as_str()))
    {
        return Err("incomplete or unknown task checkpoint record fields".into());
    }
    let state: State = serde_json::from_value(value).map_err(|e| format!("task checkpoint: {e}"))?;
    if state.version != VERSION || state.record.checkpoint_revision == 0 || state.deadline_at_ms == 0 {
        return Err("unsupported or invalid task checkpoint".into());
    }
    if state.record.id != state.task.id || state.record.branch != state.task.branch {
        return Err("task checkpoint identity disagrees with its task".into());
    }
    Ok(state)
}

/// A writer is constructed only after validating an existing file under its task lock.
pub(crate) struct Writer {
    path: PathBuf,
    present: bool,
    failed: bool,
}
impl Writer {
    pub(crate) fn new(path: PathBuf, present: bool) -> Self {
        Self {
            path,
            present,
            failed: false,
        }
    }

    pub(crate) fn failed(&self) -> bool {
        self.failed
    }

    pub(crate) fn replay(&mut self, state: &State, observer: &dyn CheckpointObserver) -> Result<()> {
        if self.failed {
            return Err("checkpoint persistence is blocked; restart to replay".into());
        }
        let result =
            checkpoint::Repository::open(self.path.parent().ok_or("checkpoint has no parent")?, observer)
                .and_then(|repository| repository.verify(&state.record));
        self.failed = result.is_err();
        result
    }

    pub(crate) fn save(
        &mut self,
        state: &mut State,
        step: Step,
        observer: &dyn CheckpointObserver,
    ) -> Result<()> {
        self.save_kind(state, step, CheckpointKind::TaskTransition, observer)
    }

    pub(crate) fn save_kind(
        &mut self,
        state: &mut State,
        step: Step,
        kind: CheckpointKind,
        observer: &dyn CheckpointObserver,
    ) -> Result<()> {
        if self.failed {
            return Err("checkpoint persistence is blocked; restart to replay".into());
        }
        let result = self.persist(state, step, kind, observer);
        self.failed = result.is_err();
        result
    }

    fn persist(
        &mut self,
        state: &mut State,
        step: Step,
        kind: CheckpointKind,
        observer: &dyn CheckpointObserver,
    ) -> Result<()> {
        if self
            .path
            .file_name()
            .is_none_or(|name| name != std::ffi::OsStr::new(&format!("{}.json", state.record.id)))
        {
            return Err("task checkpoint path disagrees with its identity".into());
        }
        let mut repository =
            checkpoint::Repository::open(self.path.parent().ok_or("checkpoint has no parent")?, observer)?;
        let revision = repository.reserve(&state.record)?;
        state.record.checkpoint_revision = revision;
        state.record.checkpoint_at_ms = now_ms();
        state.record.step = step;
        state.record.checkpoint_kind = kind;
        let bytes = serde_json::to_vec(state).map_err(|e| e.to_string())?;
        if bytes.len() as u64 > MAX_STATE_BYTES {
            return Err("task checkpoint exceeds capacity".into());
        }
        checkpoint::write_atomic(&self.path, &bytes, self.present)?;
        self.present = true;
        observer
            .checkpoint(&state.record)
            .map_err(|e| format!("checkpoint event append: {e}"))?;
        repository.acknowledge()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, OpenOptions};
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    use std::sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    };

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "oxa-checkpoint-{}-{}",
                std::process::id(),
                SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
            ));
            fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
            Self(path)
        }
        fn path(&self) -> PathBuf {
            self.0.join("test.json")
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn state() -> State {
        let task = Task {
            id: "test".into(),
            branch: "agent/test".into(),
            base: "main".into(),
            model: "fake".into(),
            prompt: "Synthetic task".into(),
            max_time: crate::default_max_time(),
        };
        let record = crate::blocked_record(&task, String::new());
        State::new(task, record, false)
    }

    #[test]
    fn recovery_planning_does_not_need_current_refs_or_settings() {
        for step in [Step::Worker, Step::Saving, Step::Finished] {
            let fixture = Fixture::new();
            let mut state = state();
            let base = "1".repeat(40);
            state.record.base_sha = Some(base.clone());
            state.record.outcome = if step == Step::Finished {
                Outcome::Done
            } else {
                Outcome::Running
            };
            state.execution = Some(Execution {
                initial: Some(base.clone()),
                base: base.clone(),
                prompt: "saved prompt".into(),
                commands: vec!["saved command".into()],
                worker_error: None,
                merge_error: None,
                failures: String::new(),
            });
            Writer::new(fixture.path(), false)
                .save(&mut state, step, &|_: &Record| Ok(()))
                .unwrap();
            let before = fs::read(fixture.path()).unwrap();
            let policy = crate::RunPolicy {
                git_binary: fixture.0.join("unavailable-git"),
                host_path: "/usr/bin:/bin".into(),
                terminate_binary: None,
                offline: true,
                storage: fixture.0.clone(),
            };
            for rerun in [false, true] {
                let planned = crate::plan_task_for_run(&fixture.0, state.task.clone(), &policy, rerun);
                if step == Step::Finished && rerun {
                    assert!(planned.is_err(), "explicit new attempts require a fresh plan");
                } else {
                    let planned = planned.unwrap();
                    assert_eq!(planned.base_sha, base);
                    assert_eq!(planned.commands, ["saved command"]);
                    assert_eq!(
                        planned.checkpoint_revision,
                        Some(state.record.checkpoint_revision)
                    );
                }
            }
            assert_eq!(fs::read(fixture.path()).unwrap(), before);
        }
    }

    #[test]
    fn observer_sees_durable_private_checkpoint_and_monotonic_revisions() {
        let fixture = Fixture::new();
        let revisions = Mutex::new(Vec::new());
        let observer = |record: &Record| -> Result<()> {
            let stored = read(&fixture.path())?;
            assert_eq!(stored.record.checkpoint_revision, record.checkpoint_revision);
            assert_eq!(stored.record.step, record.step);
            assert_eq!(
                fs::metadata(fixture.path()).unwrap().permissions().mode() & 0o777,
                0o600
            );
            revisions.lock().unwrap().push(record.checkpoint_revision);
            Ok(())
        };
        let mut state = state();
        let mut writer = Writer::new(fixture.path(), false);
        writer.save(&mut state, Step::Preparing, &observer).unwrap();
        writer.save(&mut state, Step::Worker, &observer).unwrap();
        let mut recovered = read(&fixture.path()).unwrap();
        let mut writer = Writer::new(fixture.path(), true);
        writer.replay(&recovered, &observer).unwrap();
        writer.save(&mut recovered, Step::Acceptance, &observer).unwrap();
        assert_eq!(*revisions.lock().unwrap(), [1, 2, 3]);
    }

    #[test]
    fn append_failure_leaves_latest_checkpoint_replayable_without_new_revision() {
        let fixture = Fixture::new();
        let mut writer = Writer::new(fixture.path(), false);
        let mut state = state();
        let unavailable = |_: &Record| Err("synthetic append failure".into());
        assert!(writer.save(&mut state, Step::Worker, &unavailable).is_err());
        let recovered = read(&fixture.path()).unwrap();
        assert_eq!(recovered.record.step, Step::Worker);
        assert_eq!(recovered.record.checkpoint_revision, 1);
        let replayed = AtomicU64::new(0);
        let mut writer = Writer::new(fixture.path(), true);
        writer
            .replay(&recovered, &|record: &Record| {
                replayed.store(record.checkpoint_revision, Ordering::Relaxed);
                Ok(())
            })
            .unwrap();
        assert_eq!(replayed.load(Ordering::Relaxed), 1);
        assert_eq!(read(&fixture.path()).unwrap().record.checkpoint_revision, 1);
    }

    #[test]
    fn failed_append_refuses_another_checkpoint_until_restart_and_replay() {
        let fixture = Fixture::new();
        let mut writer = Writer::new(fixture.path(), false);
        let mut state = state();
        assert!(writer
            .save(&mut state, Step::Worker, &|_: &Record| {
                Err("synthetic append failure".into())
            })
            .is_err());
        let durable = fs::read(fixture.path()).unwrap();
        let called = AtomicU64::new(0);
        assert!(writer
            .save(&mut state, Step::Saving, &|_: &Record| {
                called.fetch_add(1, Ordering::Relaxed);
                Ok(())
            })
            .is_err());
        assert_eq!(fs::read(fixture.path()).unwrap(), durable);
        assert_eq!(called.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_new_checkpoint_cannot_replace_an_unexpected_existing_file() {
        let fixture = Fixture::new();
        fs::write(fixture.path(), b"existing").unwrap();
        let mut writer = Writer::new(fixture.path(), false);
        let called = AtomicU64::new(0);
        assert!(writer
            .save(&mut state(), Step::Preparing, &|_: &Record| {
                called.fetch_add(1, Ordering::Relaxed);
                Ok(())
            })
            .is_err());
        assert_eq!(fs::read(fixture.path()).unwrap(), b"existing");
        assert_eq!(called.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn revision_exhaustion_preserves_the_previous_file_and_never_calls_observer() {
        let fixture = Fixture::new();
        let mut state = state();
        state.record.checkpoint_revision = u64::MAX;
        let bytes = serde_json::to_vec(&state).unwrap();
        fs::write(fixture.path(), &bytes).unwrap();
        fs::write(
            fixture.0.join(".checkpoint-counter"),
            serde_json::to_vec(&serde_json::json!({"version":1,"revision":u64::MAX,"pending":null})).unwrap(),
        )
        .unwrap();
        let mut writer = Writer::new(fixture.path(), true);
        assert!(writer
            .save(&mut state, Step::Worker, &|_: &Record| -> Result<()> {
                panic!("exhausted revision was emitted")
            })
            .unwrap_err()
            .contains("revision exhausted"));
        assert_eq!(fs::read(fixture.path()).unwrap(), bytes);
    }

    #[test]
    fn missing_corrupt_oversized_and_future_checkpoints_are_errors() {
        let fixture = Fixture::new();
        assert!(read(&fixture.path()).is_err());
        fs::write(fixture.path(), b"{").unwrap();
        assert!(read(&fixture.path()).is_err());
        let mut state = state();
        state.version = VERSION + 1;
        state.record.checkpoint_revision = 1;
        fs::write(fixture.path(), serde_json::to_vec(&state).unwrap()).unwrap();
        assert!(read(&fixture.path()).is_err());
        OpenOptions::new()
            .write(true)
            .open(fixture.path())
            .unwrap()
            .set_len(MAX_STATE_BYTES + 1)
            .unwrap();
        assert!(read(&fixture.path()).unwrap_err().contains("capacity"));
    }

    #[test]
    fn durable_records_require_explicit_metadata_fields() {
        let fixture = Fixture::new();
        let mut state = state();
        let mut writer = Writer::new(fixture.path(), false);
        writer
            .save(&mut state, Step::Preparing, &|_: &Record| Ok(()))
            .unwrap();
        let complete: serde_json::Value = serde_json::from_slice(&fs::read(fixture.path()).unwrap()).unwrap();
        for field in ["step", "checkpoint_revision", "base_sha"] {
            let mut damaged = complete.clone();
            damaged["record"].as_object_mut().unwrap().remove(field);
            fs::write(fixture.path(), serde_json::to_vec(&damaged).unwrap()).unwrap();
            assert!(read(&fixture.path()).is_err(), "missing {field} was accepted");
        }
    }

    #[test]
    fn repository_recovery_emits_pending_other_task_before_allocating_again() {
        let fixture = Fixture::new();
        let mut first = state();
        let mut writer = Writer::new(fixture.path(), false);
        assert!(writer
            .save(&mut first, Step::Worker, &|_: &Record| Err("unavailable".into()))
            .is_err());
        let mut second = state();
        second.task.id = "second".into();
        second.record.id = "second".into();
        second.task.branch = "agent/second".into();
        second.record.branch = "agent/second".into();
        let second_path = fixture.0.join("second.json");
        let mut still_blocked = Writer::new(second_path.clone(), false);
        assert!(still_blocked
            .save(&mut second, Step::Preparing, &|_: &Record| Err(
                "unavailable".into()
            ))
            .is_err());
        assert!(!second_path.exists());
        let events = Mutex::new(Vec::new());
        let observe = |record: &Record| {
            events
                .lock()
                .unwrap()
                .push((record.id.clone(), record.checkpoint_revision));
            Ok(())
        };
        let mut restarted = Writer::new(second_path, false);
        restarted.save(&mut second, Step::Preparing, &observe).unwrap();
        assert_eq!(
            *events.lock().unwrap(),
            [("test".into(), 1), ("second".into(), 2)]
        );
        // An acknowledged older task has no replay: the one emitter high-water mark suffices.
        Writer::new(fixture.path(), true)
            .replay(&first, &observe)
            .unwrap();
        assert_eq!(events.lock().unwrap().len(), 2);
    }

    #[test]
    fn a_reservation_without_a_task_rename_creates_only_a_revision_gap() {
        let fixture = Fixture::new();
        let state = state();
        {
            let mut repository = checkpoint::Repository::open(&fixture.0, &|_: &Record| Ok(())).unwrap();
            assert_eq!(repository.reserve(&state.record).unwrap(), 1);
        }
        assert!(!fixture.path().exists());
        let revisions = Mutex::new(Vec::new());
        let mut writer = Writer::new(fixture.path(), false);
        let mut state = state;
        writer
            .save(&mut state, Step::Preparing, &|record: &Record| {
                revisions.lock().unwrap().push(record.checkpoint_revision);
                Ok(())
            })
            .unwrap();
        assert_eq!(*revisions.lock().unwrap(), [2]);
    }

    #[test]
    fn missing_or_corrupt_repository_counter_cannot_reset_revisions() {
        let fixture = Fixture::new();
        let mut state = state();
        Writer::new(fixture.path(), false)
            .save(&mut state, Step::Preparing, &|_: &Record| Ok(()))
            .unwrap();
        let durable = fs::read(fixture.path()).unwrap();
        fs::remove_file(fixture.0.join(".checkpoint-counter")).unwrap();
        let mut writer = Writer::new(fixture.path(), true);
        assert!(writer
            .save(&mut state, Step::Worker, &|_: &Record| -> Result<()> {
                panic!("missing counter must not emit")
            })
            .is_err());
        assert_eq!(fs::read(fixture.path()).unwrap(), durable);
        fs::write(fixture.0.join(".checkpoint-counter"), b"{").unwrap();
        let mut writer = Writer::new(fixture.path(), true);
        assert!(writer
            .save(&mut state, Step::Worker, &|_: &Record| -> Result<()> {
                panic!("corrupt counter must not emit")
            })
            .is_err());
        assert_eq!(fs::read(fixture.path()).unwrap(), durable);
        fs::write(
            fixture.0.join(".checkpoint-counter"),
            br#"{"version":1,"revision":1}"#,
        )
        .unwrap();
        let mut writer = Writer::new(fixture.path(), true);
        assert!(
            writer
                .save(&mut state, Step::Worker, &|_: &Record| Ok(()))
                .is_err(),
            "a missing acknowledgement field must not become an empty pending slot"
        );
        assert_eq!(fs::read(fixture.path()).unwrap(), durable);
    }

    #[test]
    fn completed_tasks_do_not_grow_the_repository_replay_cursor() {
        let fixture = Fixture::new();
        let mut last = 0;
        for number in 0..140 {
            let id = format!("task-{number}");
            let mut state = state();
            state.task.id = id.clone();
            state.record.id = id.clone();
            state.task.branch = format!("agent/{id}");
            state.record.branch = state.task.branch.clone();
            Writer::new(fixture.0.join(format!("{id}.json")), false)
                .save(&mut state, Step::Preparing, &|_: &Record| Ok(()))
                .unwrap();
            assert!(state.record.checkpoint_revision > last);
            last = state.record.checkpoint_revision;
            assert!(fs::metadata(fixture.0.join(".checkpoint-counter")).unwrap().len() < 256);
        }
        assert_eq!(last, 140);
    }

    #[test]
    fn worker_result_and_acceptance_entry_are_distinct_durable_observations() {
        let fixture = Fixture::new();
        let mut state = state();
        let mut writer = Writer::new(fixture.path(), false);
        let seen = Mutex::new(Vec::new());
        let observe = |record: &Record| {
            let durable = read(&fixture.path()).unwrap();
            assert_eq!(durable.record.checkpoint_kind, record.checkpoint_kind);
            seen.lock()
                .unwrap()
                .push((record.checkpoint_revision, record.checkpoint_kind, record.step));
            Ok(())
        };
        writer
            .save_kind(
                &mut state,
                Step::Acceptance,
                CheckpointKind::WorkerFinished,
                &observe,
            )
            .unwrap();
        writer.save(&mut state, Step::Acceptance, &observe).unwrap();
        assert_eq!(
            *seen.lock().unwrap(),
            [
                (1, CheckpointKind::WorkerFinished, Step::Acceptance),
                (2, CheckpointKind::TaskTransition, Step::Acceptance)
            ]
        );
    }
}
