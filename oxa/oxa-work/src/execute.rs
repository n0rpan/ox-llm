//! Deterministic step recovery; every next action follows a durable checkpoint.
use crate::{acceptance, checked, dirty_worktree, git, output, state, supervise, tree, worker};
use crate::{
    Outcome, Record, Result, RunError, RunPolicy, SupervisorOperation, SupervisorRequest, WorkerTokens,
};
use serde::{Deserialize, Serialize};
use state::{CheckpointKind, CheckpointObserver, State, Step, Writer};
use std::fs::File;
use std::path::Path;
use std::time::Duration;

/// Known task-wide token totals, collected only from settled worker processes.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TaskTokens {
    /// Dispatches admitted by a durable checkpoint, including ones with no receipt.
    pub attempts: u64,
    /// Accepted worker results with complete, validated process usage.
    pub complete_attempts: u64,
    pub prompt: Option<u64>,
    pub completion: Option<u64>,
    /// Known cached input, already included in prompt when both are reported.
    pub cached: Option<u64>,
    pub complete: bool,
}

impl TaskTokens {
    pub(crate) fn valid(&self) -> bool {
        self.complete_attempts <= self.attempts
            && self.complete == self.full_coverage()
            && (!self.complete
                || (self.completion.is_some()
                    && self
                        .prompt
                        .zip(self.cached)
                        .is_some_and(|(prompt, cached)| cached <= prompt)))
    }

    fn reserve(&mut self) -> Option<()> {
        self.attempts = self.attempts.checked_add(1)?;
        self.complete = false;
        Some(())
    }

    fn settle(&mut self, tokens: Option<&WorkerTokens>, accepted: bool) -> Option<()> {
        if let Some(tokens) = tokens {
            add_known(&mut self.prompt, tokens.prompt)?;
            add_known(&mut self.completion, tokens.completion)?;
            let cached_valid = tokens
                .cached
                .zip(tokens.prompt)
                .is_none_or(|(cached, prompt)| cached <= prompt);
            if cached_valid {
                add_known(&mut self.cached, tokens.cached)?;
            }
            if accepted
                && tokens.complete
                && tokens.unreported_requests == Some(0)
                && tokens.prompt.is_some()
                && tokens.completion.is_some()
                && tokens.cached.is_some()
                && cached_valid
            {
                self.complete_attempts += 1;
            }
        }
        self.complete = self.full_coverage();
        Some(())
    }

    fn full_coverage(&self) -> bool {
        self.attempts > 0 && self.attempts == self.complete_attempts
    }
}

fn add_known(total: &mut Option<u64>, measured: Option<u64>) -> Option<()> {
    if let Some(measured) = measured {
        *total = Some(total.unwrap_or(0).checked_add(measured)?);
    }
    Some(())
}

fn worker_time(state: &mut State) -> std::result::Result<Duration, RunError> {
    let remaining = Duration::from_millis(state.deadline_at_ms.saturating_sub(state::now_ms()));
    if remaining.is_zero() {
        state.record.outcome = Outcome::Interrupted;
        state.record.interruption = Some(crate::WorkerInterruption::Deadline);
        return Err(RunError::Blocked(
            "task deadline exhausted before another worker; worktree retained".into(),
        ));
    }
    Ok(remaining)
}

pub(crate) struct Runner<'a> {
    pub repo: &'a Path,
    pub binary: &'a Path,
    pub policy: &'a RunPolicy,
    pub lock: &'a File,
    pub writer: &'a mut Writer,
    pub observer: &'a dyn CheckpointObserver,
    pub scrub_note: &'a (dyn Fn(&crate::ProblemNote) -> Option<crate::ProblemNote> + Sync),
    pub base_alias: Option<&'a tree::BaseAlias>,
    pub native: bool,
    #[cfg(target_os = "linux")]
    pub managed_steering: Option<&'a crate::managed_steering::Launch>,
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
                        (&base, self.base_alias),
                        self.binary,
                        self.policy,
                        self.lock,
                        state.native_binding.as_ref(),
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
                    worker_time(state)?;
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
                    if let Some(tokens) = state.record.task_tokens.as_mut() {
                        if tokens.reserve().is_none() {
                            state.record.task_tokens = None;
                        }
                        self.advance(state, step)?;
                    }
                    let remaining = worker_time(state)?;
                    // The session-header callback can advance the revision after spawn.
                    let launch_revision = state.record.checkpoint_revision;
                    let result = worker::run(
                        self.binary,
                        &tree,
                        &task,
                        &prompt,
                        self.policy,
                        self.lock,
                        session.as_deref(),
                        remaining,
                        #[cfg(target_os = "linux")]
                        self.managed_steering,
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
                            if state.record.task_tokens.as_mut().is_some_and(|usage| {
                                usage
                                    .settle(worker.result.tokens.as_ref(), worker.result.accepted())
                                    .is_none()
                            }) {
                                state.record.task_tokens = None;
                            }
                            state.record.worker = Some(worker.result.clone());
                            state.record.worker_launch_revision = self.native.then_some(launch_revision);
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
                            state.record.worker_launch_revision = None;
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
                        // A lane task has one repair round in total. A local
                        // acceptance failure spends it, so a later lane failure
                        // cannot start a second worker.
                        if state.task.lane_ci && state.record.lane_repairs > 0 {
                            state.record.outcome = Outcome::Red;
                            state.record.detail = failures;
                            self.advance(state, Step::Saving)?;
                        } else {
                            if state.task.lane_ci {
                                state.record.lane_repairs = 1;
                            }
                            state
                                .execution
                                .as_mut()
                                .ok_or("missing task execution checkpoint")?
                                .failures = failures;
                            self.advance(state, Step::Repair)?;
                        }
                    } else {
                        state.record.outcome = Outcome::Red;
                        state.record.detail = failures;
                        self.advance(state, Step::Saving)?;
                    }
                }
                // Lane acceptance runs after the saved commit exists; its own
                // checkpoint step is observed by the caller.
                Step::Saving | Step::Lane | Step::Finished | Step::Publishing | Step::Reporting => {
                    return Ok(())
                }
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

#[cfg(test)]
mod token_tests {
    use super::*;

    #[test]
    fn partial_usage_is_retained_without_claiming_complete_coverage() {
        let mut total = TaskTokens::default();
        total.reserve().unwrap();
        total
            .settle(
                Some(&WorkerTokens {
                    prompt: None,
                    completion: Some(6),
                    cached: None,
                    complete: false,
                    unreported_requests: Some(1),
                }),
                false,
            )
            .unwrap();
        assert_eq!(total.prompt, None);
        assert_eq!(total.completion, Some(6));
        assert_eq!(total.complete_attempts, 0);
        assert!(!total.complete);
        assert!(total.valid());
    }

    #[test]
    fn cached_input_is_a_subset_and_a_failed_result_leaves_coverage_incomplete() {
        let mut total = TaskTokens::default();
        let usage = WorkerTokens {
            prompt: Some(10),
            completion: Some(3),
            cached: Some(7),
            complete: true,
            unreported_requests: Some(0),
        };

        total.reserve().unwrap();
        total.settle(Some(&usage), true).unwrap();
        assert_eq!(total.complete_attempts, 1);
        assert!(total.complete);

        total.reserve().unwrap();
        total.settle(Some(&usage), false).unwrap();
        assert_eq!(total.attempts, 2);
        assert_eq!(total.complete_attempts, 1);
        assert_eq!(total.prompt, Some(20));
        assert_eq!(total.completion, Some(6));
        assert_eq!(total.cached, Some(14));
        assert!(!total.complete);
        assert!(total.valid());
    }

    #[test]
    fn overflowing_a_reported_dimension_invalidates_the_aggregate() {
        let mut total = TaskTokens {
            completion: Some(u64::MAX),
            ..TaskTokens::default()
        };
        total.reserve().unwrap();
        assert!(total
            .settle(
                Some(&WorkerTokens {
                    prompt: Some(2),
                    completion: Some(1),
                    cached: Some(1),
                    complete: true,
                    unreported_requests: Some(0),
                }),
                true,
            )
            .is_none());
    }
}
