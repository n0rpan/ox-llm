use super::{acceptance, agent, checked, git, state, tree, Outcome, Result, Task};
use state::{Checkpoint, State, Step};
use std::fs::File;
use std::path::Path;

fn complete(tree: &Path, state: &mut State) -> Result<()> {
    let point = state.checkpoint.as_ref().ok_or("missing task checkpoint")?;
    if let Some(error) = &point.agent_error {
        return Err(error.clone());
    }
    let head = checked(git(tree, &["rev-parse", "HEAD"]))?;
    let dirty = checked(git(tree, &["status", "--porcelain"]))?;
    state.record.outcome = if head == point.initial && dirty.is_empty() {
        Outcome::NothingToDo
    } else {
        Outcome::Done
    };
    Ok(())
}
pub(crate) fn execute(
    repo: &Path,
    binary: &Path,
    task: &Task,
    path: &Path,
    state: &mut State,
    lock: &File,
) -> Result<()> {
    let tree = state.record.worktree.clone();
    loop {
        match state.record.step {
            Step::Preparing => {
                let prepared = tree::prepare(repo, &tree, task, lock)?;
                let commands = tree::done_from_base(&tree, &prepared.base)?;
                state.checkpoint = Some(Checkpoint {
                    initial: prepared.initial,
                    base: prepared.base,
                    prompt: prepared.prompt + super::notes::INSTRUCTIONS,
                    commands,
                    agent_error: None,
                    failures: String::new(),
                });
                state::save(path, state, Step::Agent)?;
            }
            Step::Agent | Step::Repair => {
                tree::validate(repo, &tree, task)?;
                let step = state.record.step;
                let point = state.checkpoint.as_ref().ok_or("missing task checkpoint")?;
                let prompt = if step == Step::Repair {
                    format!(
                        "{}\n\nExternal acceptance checks failed. Repair once:\n{}",
                        point.prompt, point.failures
                    )
                } else {
                    point.prompt.clone()
                };
                let result = agent(binary, &tree, task, &prompt, lock, state.offline);
                let error = match result {
                    Ok(reply) => {
                        for note in reply.notes {
                            if state.record.problem_notes.len() < 5
                                && !state.record.problem_notes.contains(&note)
                            {
                                state.record.problem_notes.push(note);
                            }
                        }
                        reply.error
                    }
                    Err(error) => Some(error),
                };
                tree::ensure_merged(&tree, &point.base, &task.branch)?;
                state
                    .checkpoint
                    .as_mut()
                    .ok_or("missing task checkpoint")?
                    .agent_error = error;
                state::save(
                    path,
                    state,
                    if step == Step::Agent {
                        Step::Acceptance
                    } else {
                        Step::Recheck
                    },
                )?;
            }
            Step::Acceptance | Step::Recheck => {
                tree::validate(repo, &tree, task)?;
                let point = state.checkpoint.as_ref().ok_or("missing task checkpoint")?;
                tree::ensure_merged(&tree, &point.base, &task.branch)?;
                let failures = acceptance(
                    &tree.canonicalize().map_err(|e| e.to_string())?,
                    &point.commands,
                    lock,
                    state.offline,
                )?;
                if failures.is_empty() {
                    complete(&tree, state)?;
                    state::save(path, state, Step::Saving)?;
                } else if state.record.step == Step::Acceptance {
                    state
                        .checkpoint
                        .as_mut()
                        .ok_or("missing task checkpoint")?
                        .failures = failures;
                    state::save(path, state, Step::Repair)?;
                } else {
                    state.record.outcome = Outcome::Red;
                    state.record.detail = failures;
                    state::save(path, state, Step::Saving)?;
                }
            }
            Step::Saving | Step::Finished | Step::Publishing | Step::Reporting => return Ok(()),
        }
    }
}
/// Save all staged, unstaged and untracked files. Conflicts are kept in progress for the agent.
pub(crate) fn save_work(repo: &Path, tree: &Path, task: &Task, lock: &File) -> Result<()> {
    if !tree.exists() {
        return Ok(());
    }
    tree::validate(repo, tree, task)?;
    let run = |args: &[&str]| -> Result<String> {
        let mut command = git(tree, args);
        command.stdin(lock.try_clone().map_err(|e| e.to_string())?);
        super::checked_output(command.output().map_err(|e| e.to_string())?)
    };
    if tree::merging(tree)? || !run(&["diff", "--name-only", "--diff-filter=U"])?.is_empty() {
        return Err("unresolved merge retained in progress; files were not discarded".into());
    }
    if run(&["status", "--porcelain"])?.is_empty() {
        return Ok(());
    }
    run(&["add", "--all"])?;
    run(&["commit", "-m", &format!("oxa work: save task {}", task.id)])?;
    if !run(&["status", "--porcelain"])?.is_empty() {
        return Err("uncommitted changes remain after saving; retained for inspection".into());
    }
    Ok(())
}
