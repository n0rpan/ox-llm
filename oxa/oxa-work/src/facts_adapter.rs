//! Runner-owned checkpoint adapter. Facts storage never opens task state.
use crate::{events::Facts, CheckpointKind, CheckpointObserver, Outcome, Record, Result, Step};
use oxa_proto::work::{
    Blocked, Checkpoint, EventPage, Fact, Running, Snapshot, StepReason, Subject, TaskStep, TaskTransition,
    WorkerFinished, WorkerOutcome,
};
use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

pub struct FactObserver {
    storage: PathBuf,
    facts: Facts,
}
impl FactObserver {
    /// Reads require a real store. Missing facts are never treated as an empty history.
    pub fn open(storage: &Path) -> Result<Self> {
        Ok(Self {
            storage: storage.into(),
            facts: Facts::open(&storage.join("facts"))?,
        })
    }

    /// Initialize facts before a repository's first task checkpoint, under a short init lock.
    pub fn for_run(storage: &Path) -> Result<Self> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(storage)
            .map_err(|_| "cannot create protected runner root")?;
        let protected = crate::facts_io::Root::open(storage, false)?;
        let _init = protected.lock(".facts-init.lock")?;
        let root = storage.join("facts");
        let facts = match fs::symlink_metadata(&root) {
            Ok(_) => Facts::open(&root)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut has_state = false;
                for entry in fs::read_dir(storage).map_err(|_| "cannot inspect runner root")? {
                    let path = entry.map_err(|_| "cannot inspect runner entry")?.path();
                    has_state |= path.extension().is_some_and(|extension| extension == "json");
                }
                if has_state
                    || protected.exists(".checkpoint-initialized")?
                    || protected.exists(".checkpoint-counter")?
                {
                    return Err("facts are missing for an initialized runner; history was not reset".into());
                }
                Facts::initialize(&root)?
            }
            Err(_) => return Err("cannot inspect runner facts".into()),
        };
        let observer = Self {
            storage: storage.into(),
            facts,
        };
        observer.recover()?;
        Ok(observer)
    }

    fn recover(&self) -> Result<()> {
        crate::recover_checkpoints(&self.storage, self).map_err(|error| error.to_string())
    }
    pub fn situation(&self) -> Result<Snapshot> {
        self.recover()?;
        self.facts.situation()
    }
    pub fn events_after(&self, after: u64) -> Result<EventPage> {
        self.recover()?;
        self.facts.events_after(after, 64)
    }
}
impl CheckpointObserver for FactObserver {
    fn checkpoint(&self, record: &Record) -> Result<()> {
        self.facts
            .append(
                Checkpoint {
                    source: "runner".into(),
                    revision: record.checkpoint_revision,
                },
                record.checkpoint_at_ms,
                observation(record)?,
            )
            .map(|_| ())
    }
}

fn observation(record: &Record) -> Result<Fact> {
    Ok(match record.checkpoint_kind {
        CheckpointKind::WorkerStalled => {
            if record.stall.as_ref().is_none_or(|stall| {
                !matches!(record.step, Step::Worker | Step::Repair)
                    || record.outcome != Outcome::Running
                    || record.session_id.as_deref() != Some(stall.session_id.as_str())
                    || record.worker_pid != Some(stall.pid)
                    || stall.deadline_ms > record.checkpoint_at_ms
            }) {
                return Err("stall checkpoint identity or phase is inconsistent".into());
            }
            let stall = record
                .stall
                .as_ref()
                .ok_or("stall checkpoint has no observation")?;
            Fact::Incident(
                crate::incidents::Stall {
                    session_id: stall.session_id.clone(),
                    pid: stall.pid,
                    last_progress_ms: stall.last_progress_ms,
                    deadline_ms: stall.deadline_ms,
                }
                .incident(&record.id),
            )
        }
        CheckpointKind::Unblocked => Fact::Unblocked(Subject::Task(record.id.clone())),
        CheckpointKind::Blocked => {
            if matches!(
                record.outcome,
                Outcome::Running | Outcome::Done | Outcome::NothingToDo
            ) {
                return Err("blocked checkpoint has no terminal failure".into());
            }
            Fact::Blocked(Blocked {
                subject: Subject::Task(record.id.clone()),
                code: reason_code(reason(record)).into(),
                ask_id: None,
            })
        }
        CheckpointKind::WorkerFinished => {
            let outcome = match record.worker.as_ref().map(|worker| worker.outcome.clone()) {
                Some(crate::WorkerOutcome::Done) => WorkerOutcome::Done,
                Some(crate::WorkerOutcome::NothingToDo) => WorkerOutcome::NothingToDo,
                Some(crate::WorkerOutcome::Failed) => WorkerOutcome::Failed,
                Some(crate::WorkerOutcome::Interrupted) => WorkerOutcome::Interrupted,
                Some(crate::WorkerOutcome::Blocked) | None => WorkerOutcome::Blocked,
            };
            let error_class = match outcome {
                WorkerOutcome::Done | WorkerOutcome::NothingToDo => None,
                WorkerOutcome::Blocked => Some("worker_blocked".into()),
                WorkerOutcome::Failed => Some("worker_failed".into()),
                WorkerOutcome::Interrupted => Some("worker_interrupted".into()),
            };
            Fact::WorkerFinished(WorkerFinished {
                task: record.id.clone(),
                session_id: record.session_id.clone(),
                outcome,
                error_class,
            })
        }
        CheckpointKind::TaskTransition if record.step == Step::Preparing => Fact::TaskStarted(Running {
            task: record.id.clone(),
            host: "local".into(),
            branch: record.branch.clone(),
            worktree: record
                .worktree
                .to_str()
                .ok_or("task worktree is not UTF-8")?
                .into(),
            session_id: record.session_id.clone(),
            step: TaskStep::Preparing,
        }),
        CheckpointKind::TaskTransition
            if record.step == Step::Finished && record.outcome == Outcome::Running =>
        {
            return Err("finished checkpoint has a running outcome".into());
        }
        CheckpointKind::TaskTransition => Fact::TaskTransition(TaskTransition {
            task: record.id.clone(),
            step: task_step(record.step),
            session_id: record.session_id.clone(),
            reason: (record.step == Step::Finished).then(|| reason(record)),
        }),
    })
}

fn task_step(step: Step) -> TaskStep {
    match step {
        Step::Preparing => TaskStep::Preparing,
        Step::Worker => TaskStep::Worker,
        Step::Acceptance => TaskStep::Acceptance,
        Step::Repair => TaskStep::Repair,
        Step::Recheck => TaskStep::Recheck,
        Step::Saving => TaskStep::Saving,
        Step::Finished => TaskStep::Finished,
    }
}
fn reason(record: &Record) -> StepReason {
    if record.interruption == Some(crate::WorkerInterruption::Deadline) {
        return StepReason::Deadline;
    }
    match record.outcome {
        Outcome::Done | Outcome::NothingToDo => StepReason::Saved,
        Outcome::Red => StepReason::ChecksFailed,
        Outcome::Interrupted => StepReason::Interrupted,
        Outcome::Blocked if record.failure_step == Some(Step::Saving) => StepReason::SaveFailed,
        Outcome::Blocked => match record.worker.as_ref().map(|worker| worker.outcome.clone()) {
            Some(crate::WorkerOutcome::Failed) => StepReason::WorkerFailed,
            Some(crate::WorkerOutcome::Blocked) => StepReason::WorkerBlocked,
            _ => match record.failure_step {
                Some(Step::Acceptance | Step::Recheck) => StepReason::ChecksFailed,
                Some(Step::Repair) => StepReason::RepairFailed,
                _ => StepReason::SetupFailed,
            },
        },
        Outcome::Running => StepReason::Resumed,
    }
}
fn reason_code(reason: StepReason) -> &'static str {
    match reason {
        StepReason::Deadline => "deadline",
        StepReason::Interrupted => "interrupted",
        StepReason::SaveFailed => "save_failed",
        StepReason::WorkerFailed => "worker_failed",
        StepReason::WorkerBlocked => "worker_blocked",
        StepReason::ChecksFailed => "checks_failed",
        StepReason::RepairFailed => "repair_failed",
        _ => "setup_failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{State, Writer};
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "oxa-fact-adapter-{}-{}-{}",
                std::process::id(),
                crate::state::now_ms(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn state() -> State {
        let task = crate::Task {
            id: "test".into(),
            branch: "agent/test".into(),
            base: "main".into(),
            model: "fake".into(),
            prompt: "raw-model-output".into(),
            max_time: 2700,
        };
        let mut record = crate::blocked_record(&task, "raw-model-output".into());
        record.worktree = "/tmp/synthetic-task".into();
        State::new(task, record, false)
    }

    #[test]
    fn a_lost_append_ack_recovers_once_before_a_situation_read() {
        let fixture = Fixture::new();
        let facts = FactObserver::for_run(&fixture.0).unwrap();
        let mut state = state();
        let mut writer = Writer::new(fixture.0.join("test.json"), false);
        assert!(writer
            .save(&mut state, Step::Preparing, &|record: &Record| {
                facts.checkpoint(record)?;
                Err("synthetic lost acknowledgement".into())
            })
            .is_err());
        let recovered = FactObserver::open(&fixture.0).unwrap();
        let snapshot = recovered.situation().unwrap();
        assert_eq!(snapshot.as_of_seq, 1);
        assert_eq!(snapshot.running.len(), 1);
        let page = recovered.events_after(0).unwrap();
        assert_eq!(page.events.len(), 1);
        assert!(!String::from_utf8(page.to_json().unwrap())
            .unwrap()
            .contains("raw-model-output"));
        assert_eq!(recovered.situation().unwrap().as_of_seq, 1);
    }

    #[test]
    fn blocked_and_finished_get_distinct_revisions_and_no_acceptance_or_landing() {
        let fixture = Fixture::new();
        let facts = FactObserver::for_run(&fixture.0).unwrap();
        let mut state = state();
        let mut writer = Writer::new(fixture.0.join("test.json"), false);
        writer.save(&mut state, Step::Preparing, &facts).unwrap();
        state.record.outcome = Outcome::Red;
        writer
            .save_kind(&mut state, Step::Saving, CheckpointKind::Blocked, &facts)
            .unwrap();
        writer.save(&mut state, Step::Finished, &facts).unwrap();
        let snapshot = facts.situation().unwrap();
        assert!(snapshot.running.is_empty());
        assert_eq!(snapshot.blocked.len(), 1);
        assert_eq!(snapshot.blocked[0].code, "checks_failed");
        assert!(snapshot.local_accepted.is_empty() && snapshot.landed.is_empty());
        let page = facts.events_after(0).unwrap();
        assert_eq!(
            page.events
                .iter()
                .map(|event| event.checkpoint.revision)
                .collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert!(
            matches!(&page.events[2].fact, Fact::TaskTransition(change) if change.reason == Some(StepReason::ChecksFailed))
        );
        writer
            .save_kind(&mut state, Step::Preparing, CheckpointKind::Unblocked, &facts)
            .unwrap();
        writer.save(&mut state, Step::Preparing, &facts).unwrap();
        assert!(facts.situation().unwrap().blocked.is_empty());
    }

    #[test]
    fn missing_facts_of_an_initialized_runner_are_not_recreated() {
        let fixture = Fixture::new();
        FactObserver::for_run(&fixture.0).unwrap();
        fs::remove_dir_all(fixture.0.join("facts")).unwrap();
        assert!(FactObserver::for_run(&fixture.0).is_err());
        assert!(!fixture.0.join("facts").exists());
    }

    #[test]
    fn a_stall_is_an_incident_while_the_task_remains_running() {
        let fixture = Fixture::new();
        let facts = FactObserver::for_run(&fixture.0).unwrap();
        let mut state = state();
        let mut writer = Writer::new(fixture.0.join("test.json"), false);
        writer.save(&mut state, Step::Preparing, &facts).unwrap();
        state.record.session_id = Some("saved".into());
        state.record.worker_pid = Some(123);
        writer.save(&mut state, Step::Worker, &facts).unwrap();
        state.record.stall = Some(crate::StallObservation {
            session_id: "saved".into(),
            pid: 123,
            last_progress_ms: 100,
            deadline_ms: 900100,
        });
        writer
            .save_kind(&mut state, Step::Worker, CheckpointKind::WorkerStalled, &facts)
            .unwrap();
        let snapshot = facts.situation().unwrap();
        assert_eq!(snapshot.running.len(), 1);
        assert_eq!(snapshot.running[0].step, TaskStep::Worker);
        assert!(snapshot.blocked.is_empty());
        assert_eq!(snapshot.incidents.len(), 1);
        assert_eq!(
            snapshot.incidents[0].kind,
            oxa_proto::work::IncidentKind::WorkerStalled
        );
        assert_eq!(state.record.outcome, Outcome::Running);
    }
}
