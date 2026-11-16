//! Record a refused new attempt without replacing live or unsupported work.
use crate::{
    state, tree, CheckpointKind, CheckpointObserver, Outcome, Record, RunError, RunPolicy, Step, Task,
};
use std::fs::{self, File};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

pub fn checkpoint_plan_refusal(
    common: &Path,
    task: &Task,
    detail: String,
    policy: &RunPolicy,
    rerun: bool,
    observer: &dyn CheckpointObserver,
) -> std::result::Result<Record, RunError> {
    crate::validate(task)?;
    let mut record = crate::blocked_record(task, detail);
    let lock = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(policy.storage.join(format!("{}.lock", task.id)))
        .map_err(|_| "cannot open refused task lock")?;
    if lock.try_lock().is_err() {
        return Ok(record);
    }
    let path = policy.storage.join(format!("{}.json", task.id));
    let mut writer = state::Writer::new(path.clone(), false);
    let mut offline = policy.offline;
    let worktree = tree::location(common, &task.id)?;
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Ok(record),
        Ok(metadata) => {
            if !metadata.is_file() {
                return Ok(record);
            }
            let Ok(old) = state::load(&path) else {
                return Ok(record);
            };
            if old.task != *task
                || old.record.worktree != worktree
                || old.record.step != Step::Finished
                || (!rerun && matches!(old.record.outcome, Outcome::Done | Outcome::NothingToDo))
            {
                return Ok(record);
            }
            writer = state::Writer::new(path, true);
            writer.replay(&old, observer).map_err(RunError::Checkpoint)?;
            record.checkpoint_revision = old.record.checkpoint_revision;
            offline |= old.offline;
        }
    }
    record.worktree = worktree;
    record.failure_step = Some(Step::Preparing);
    let mut state = state::State::new(task.clone(), record, offline);
    state.record.outcome = Outcome::Blocked;
    writer
        .save_kind(&mut state, Step::Finished, CheckpointKind::Blocked, observer)
        .map_err(RunError::Checkpoint)?;
    writer
        .save(&mut state, Step::Finished, observer)
        .map_err(RunError::Checkpoint)?;
    Ok(state.record)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FactObserver, Result};
    use oxa_proto::work::Fact;
    use std::os::unix::fs::DirBuilderExt;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Fixture(PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn fixture() -> (Fixture, RunPolicy, FactObserver, Task, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "oxa-refused-plan-{}-{}-{}",
            std::process::id(),
            state::now_ms(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        let policy = RunPolicy {
            git_binary: "/usr/bin/git".into(),
            host_path: "/usr/bin:/bin".into(),
            terminate_binary: None,
            offline: true,
            storage: root.clone(),
            status_reader: None,
        };
        let facts = FactObserver::for_run(&root).unwrap();
        let task = Task {
            id: "test".into(),
            branch: "agent/test".into(),
            base: "main".into(),
            model: "fake".into(),
            prompt: "Synthetic task".into(),
            max_time: 2700,
        };
        let common = root.join("project/.git");
        (Fixture(root), policy, facts, task, common)
    }
    #[test]
    fn a_new_refusal_is_durable_without_a_worktree_or_execution() -> Result<()> {
        let (_fixture, policy, facts, task, common) = fixture();
        let record = checkpoint_plan_refusal(&common, &task, "raw diagnostic".into(), &policy, false, &facts)
            .map_err(|error| error.to_string())?;
        assert!(!record.worktree.exists());
        let saved = state::load(&policy.storage.join("test.json"))?;
        assert!(saved.execution.is_none());
        assert_eq!(saved.record.outcome, Outcome::Blocked);
        assert_eq!(saved.record.step, Step::Finished);
        let snapshot = facts.situation()?;
        assert!(snapshot.running.is_empty());
        assert_eq!(snapshot.blocked[0].code, "setup_failed");
        let events = facts.events_after(0)?;
        assert!(matches!(events.events[0].fact, Fact::Blocked(_)));
        assert!(matches!(events.events[1].fact, Fact::TaskTransition(_)));
        assert!(!String::from_utf8(events.to_json().unwrap())
            .unwrap()
            .contains("raw diagnostic"));
        Ok(())
    }
    #[test]
    fn existing_live_unsupported_and_different_tasks_are_preserved() {
        for kind in ["live", "unsupported", "different", "locked"] {
            let (_fixture, policy, facts, task, common) = fixture();
            let path = policy.storage.join("test.json");
            let mut record = crate::blocked_record(&task, String::new());
            record.worktree = tree::location(&common, &task.id).unwrap();
            let mut saved = state::State::new(task.clone(), record, true);
            let mut writer = state::Writer::new(path.clone(), false);
            writer.save(&mut saved, Step::Preparing, &facts).unwrap();
            if kind == "unsupported" {
                saved.version += 1;
                fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();
            }
            let mut input = task;
            if kind == "different" {
                input.prompt = "Different task".into();
            }
            let lock = File::options()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(policy.storage.join("test.lock"))
                .unwrap();
            if kind == "locked" {
                lock.try_lock().unwrap();
            }
            let before = fs::read(&path).unwrap();
            let seq = facts.situation().unwrap().as_of_seq;
            let refused =
                checkpoint_plan_refusal(&common, &input, "refused".into(), &policy, true, &facts).unwrap();
            assert_eq!(refused.checkpoint_revision, 0);
            assert_eq!(fs::read(&path).unwrap(), before);
            assert_eq!(facts.situation().unwrap().as_of_seq, seq);
        }
    }
}
