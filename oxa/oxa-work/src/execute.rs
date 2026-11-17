//! Deterministic step recovery; every next action follows a durable checkpoint.
use crate::{acceptance, checked, dirty_worktree, git, output, state, supervise, tree, worker};
use crate::{Outcome, Record, Result, RunError, RunPolicy, SupervisorOperation, SupervisorRequest};
use state::{CheckpointKind, CheckpointObserver, State, Step, Writer};
use std::fs::File;
use std::path::Path;

pub(crate) struct Runner<'a> {
    pub repo: &'a Path,
    pub binary: &'a Path,
    pub policy: &'a RunPolicy,
    pub lock: &'a File,
    pub writer: &'a mut Writer,
    pub observer: &'a dyn CheckpointObserver,
    pub scrub_note: &'a (dyn Fn(&crate::ProblemNote) -> Option<crate::ProblemNote> + Sync),
}
impl Runner<'_> {
    fn advance(&mut self, state: &mut State, step: Step) -> std::result::Result<(), RunError> {
        self.writer
            .save(state, step, self.observer)
            .map_err(RunError::Checkpoint)
    }

    pub(crate) fn drive(&mut self, state: &mut State) -> std::result::Result<(), RunError> {
        let tree = state.record.worktree.clone();
        loop {
            match state.record.step {
                Step::Preparing => {
                    let base = point(state)?.base.clone();
                    let prepared = tree::prepare(
                        self.repo,
                        &tree,
                        &state.task,
                        &base,
                        self.binary,
                        self.policy,
                        self.lock,
                    )?;
                    let point = state
                        .execution
                        .as_mut()
                        .ok_or("missing task execution checkpoint")?;
                    point.initial = Some(prepared.initial);
                    point.prompt = prepared.prompt;
                    self.advance(state, Step::Worker)?;
                }
                Step::Worker | Step::Repair => {
                    tree::verify_objects(self.policy, self.repo, &point(state)?.base)?;
                    tree::validate(self.policy, self.repo, &tree, &state.task)?;
                    let step = state.record.step;
                    if step == Step::Worker && state.record.session_id.is_none() {
                        if let Some(reviewed) = state.task.reviewed_head() {
                            let actual = checked(git(self.policy, &tree, &["rev-parse", "HEAD"]))?;
                            if actual != reviewed {
                                return Err("pull request branch moved before starting the worker".into());
                            }
                        }
                    }
                    let remaining = std::time::Duration::from_millis(
                        state.deadline_at_ms.saturating_sub(state::now_ms()),
                    );
                    if remaining.is_zero() {
                        state.record.outcome = Outcome::Interrupted;
                        state.record.interruption = Some(crate::WorkerInterruption::Deadline);
                        return Err(RunError::Blocked(
                            "task deadline exhausted before another worker; worktree retained".into(),
                        ));
                    }
                    let point = point(state)?;
                    let prompt = if step == Step::Repair {
                        format!(
                            "External acceptance checks failed. Repair once:\n{}",
                            point.failures
                        )
                    } else if state.record.session_id.is_some() {
                        "Continue the interrupted task from this session; preserve the retained work.".into()
                    } else {
                        point.prompt.clone()
                    };
                    let prompt = format!("{prompt}{}", crate::notes::INSTRUCTIONS);
                    let session = state.record.session_id.clone();
                    let task = state.task.clone();
                    let result = worker::run(
                        self.binary,
                        &tree,
                        &task,
                        &prompt,
                        self.policy,
                        self.lock,
                        session.as_deref(),
                        remaining,
                        &mut |id| {
                            state.record.session_id = Some(id.to_owned());
                            self.writer.save(state, step, self.observer)
                        },
                    );
                    // Session persistence fails while the child is active. Its reader
                    // requests immediate termination; no merge/check/save follows it.
                    if self.writer.failed() {
                        return Err(RunError::Checkpoint(
                            result
                                .err()
                                .unwrap_or_else(|| "worker session checkpoint failed".into()),
                        ));
                    }
                    let error = match &result {
                        Ok(worker) => {
                            if matches!(worker.result.outcome, crate::WorkerOutcome::Interrupted)
                                && state::now_ms() >= state.deadline_at_ms
                            {
                                state.record.interruption = Some(crate::WorkerInterruption::Deadline);
                            } else {
                                state.record.interruption = worker.result.interruption;
                            }
                            state.record.worker = Some(worker.result.clone());
                            for note in worker.notes.iter().filter_map(self.scrub_note) {
                                if state.record.problem_notes.len() < 5
                                    && !state.record.problem_notes.contains(&note)
                                {
                                    state.record.problem_notes.push(note);
                                }
                            }
                            state.record.session_id = worker.session.clone();
                            worker.failure()
                        }
                        Err(error) => {
                            state.record.worker = None;
                            Some(format!("worker: {error}"))
                        }
                    };
                    let point = state
                        .execution
                        .as_mut()
                        .ok_or("missing task execution checkpoint")?;
                    point.worker_error = error;
                    point.merge_error =
                        tree::ensure_merged(self.policy, &tree, &point.base, &task.branch).err();
                    self.writer
                        .save_kind(
                            state,
                            if step == Step::Worker {
                                Step::Acceptance
                            } else {
                                Step::Recheck
                            },
                            CheckpointKind::WorkerFinished,
                            self.observer,
                        )
                        .map_err(RunError::Checkpoint)?;
                }
                Step::Acceptance | Step::Recheck => {
                    if state.record.checkpoint_kind == CheckpointKind::WorkerFinished {
                        self.advance(state, state.record.step)?;
                    }
                    tree::validate(self.policy, self.repo, &tree, &state.task)?;
                    let point = point(state)?;
                    let merge =
                        tree::ensure_merged(self.policy, &tree, &point.base, &state.task.branch).err();
                    let checks = acceptance(
                        self.binary,
                        &tree,
                        self.repo,
                        &point.commands,
                        self.policy,
                        self.lock,
                    );
                    let mut errors: Vec<_> = point.worker_error.iter().cloned().collect();
                    errors.extend(
                        point
                            .merge_error
                            .iter()
                            .chain(merge.as_ref())
                            .map(|error| format!("merge verification: {error}")),
                    );
                    errors.dedup();
                    let failures = match checks {
                        Ok(failures) => failures,
                        Err(error) => {
                            errors.push(error);
                            String::new()
                        }
                    };
                    if !errors.is_empty() {
                        if !failures.is_empty() {
                            errors.push(format!("Acceptance checks:\n{failures}"));
                        }
                        return Err(RunError::Blocked(errors.join("\n")));
                    }
                    if failures.is_empty() {
                        let initial = point.initial.as_ref().ok_or("missing initial task commit")?;
                        let head = checked(git(self.policy, &tree, &["rev-parse", "HEAD"]))?;
                        let dirty = dirty_worktree(self.binary, &tree, self.repo, self.policy, self.lock)?;
                        state.record.outcome = if head == *initial && !dirty {
                            Outcome::NothingToDo
                        } else {
                            Outcome::Done
                        };
                        self.advance(state, Step::Saving)?;
                    } else if state.record.step == Step::Acceptance {
                        state
                            .execution
                            .as_mut()
                            .ok_or("missing task execution checkpoint")?
                            .failures = failures;
                        self.advance(state, Step::Repair)?;
                    } else {
                        state.record.outcome = Outcome::Red;
                        state.record.detail = failures;
                        self.advance(state, Step::Saving)?;
                    }
                }
                Step::Saving | Step::Finished | Step::Publishing | Step::Reporting => return Ok(()),
            }
        }
    }
}

fn point(state: &State) -> Result<&state::Execution> {
    state
        .execution
        .as_ref()
        .ok_or_else(|| "missing task execution checkpoint".into())
}

pub(crate) fn save_work(
    repo: &Path,
    binary: &Path,
    task: &crate::Task,
    policy: &RunPolicy,
    lock: &File,
    record: &mut Record,
) -> Result<()> {
    let tree = &record.worktree;
    if !tree.exists() {
        return Ok(());
    }
    tree::validate(policy, repo, tree, task)?;
    let merging = output(git(
        policy,
        tree,
        &["rev-parse", "--verify", "--quiet", "MERGE_HEAD"],
    ))?;
    match merging.status.code() {
        Some(0) => return Err("unresolved merge retained in progress; files were not discarded".into()),
        Some(1) => {}
        _ => return Err("cannot inspect pending task merge".into()),
    }
    let saved = supervise(
        binary,
        tree,
        SupervisorRequest {
            source: repo.to_path_buf(),
            operation: SupervisorOperation::Save {
                task_id: task.id.clone(),
            },
        },
        policy,
        lock,
    )?;
    if !saved.status.success() {
        return Err(format!(
            "sandboxed save failed: {}",
            String::from_utf8_lossy(&saved.stderr)
        ));
    }
    if dirty_worktree(binary, tree, repo, policy, lock)? {
        return Err("uncommitted changes remain after saving; retained for inspection".into());
    }
    record.head_sha = Some(checked(git(policy, tree, &["rev-parse", "HEAD"]))?);
    record.tree_sha = Some(checked(git(policy, tree, &["rev-parse", "HEAD^{tree}"]))?);
    Ok(())
}
