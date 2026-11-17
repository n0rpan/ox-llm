//! Continue linked worktrees with ordinary sandboxed merges; retain every conflict and edit.
use super::{
    checked, dirty_worktree, git, output, supervise, Result, RunPolicy, SupervisorOperation,
    SupervisorRequest, Task,
};
use std::fs;
use std::path::{Path, PathBuf};

pub(crate) struct Prepared {
    pub initial: String,
    pub prompt: String,
}

fn git_path(policy: &RunPolicy, tree: &Path, name: &str) -> Result<PathBuf> {
    checked(git(
        policy,
        tree,
        &["rev-parse", "--path-format=absolute", "--git-path", name],
    ))
    .map(PathBuf::from)
}

fn merging(policy: &RunPolicy, tree: &Path) -> Result<bool> {
    Ok(git_path(policy, tree, "MERGE_HEAD")?.exists())
}

fn ref_exists(policy: &RunPolicy, repo: &Path, reference: &str) -> Result<bool> {
    let out = output(git(policy, repo, &["show-ref", "--verify", "--quiet", reference]))?;
    match out.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(String::from_utf8_lossy(&out.stderr).into()),
    }
}

/// The local branch a task base names, following Git's own precedence: a
/// literal branch first, then `<remote>/<branch>` for a configured remote.
fn local_branch_for_base(policy: &RunPolicy, repo: &Path, base: &str) -> Result<Option<String>> {
    let branch = base.strip_prefix("refs/heads/").unwrap_or(base);
    if ref_exists(policy, repo, &format!("refs/heads/{branch}"))? {
        return Ok(Some(branch.to_owned()));
    }
    let mut remotes: Vec<String> = checked(git(policy, repo, &["remote"]))?
        .lines()
        .filter(|remote| !remote.is_empty())
        .map(str::to_owned)
        .collect();
    // Prefer the longest remote name so a slashed remote wins over its prefix.
    remotes.sort_by_key(|remote| std::cmp::Reverse(remote.len()));
    for remote in remotes {
        let Some(branch) = base.strip_prefix(&format!("{remote}/")) else {
            continue;
        };
        if ref_exists(policy, repo, &format!("refs/heads/{branch}"))? {
            return Ok(Some(branch.to_owned()));
        }
    }
    Ok(None)
}

fn branch_checked_out(policy: &RunPolicy, repo: &Path, reference: &str) -> Result<bool> {
    let listing = checked(git(policy, repo, &["worktree", "list", "--porcelain"]))?;
    Ok(listing.lines().any(|line| line == format!("branch {reference}")))
}

/// A fresh task worktree shares the source repository's refs, so a local base
/// branch that lags the frozen base makes the task's own diff look like the
/// whole repository divergence. Advance that branch when it cannot lose work:
/// a strict fast-forward that no worktree has checked out.
fn fast_forward_stale_base(policy: &RunPolicy, repo: &Path, task: &Task, base: &str) -> Result<()> {
    let Some(branch) = local_branch_for_base(policy, repo, &task.base)? else {
        return Ok(());
    };
    let reference = format!("refs/heads/{branch}");
    let old = checked(git(policy, repo, &["rev-parse", "--verify", &reference]))?;
    if old == base || branch_checked_out(policy, repo, &reference)? {
        return Ok(());
    }
    if !output(git(policy, repo, &["merge-base", "--is-ancestor", &old, base]))?
        .status
        .success()
    {
        return Ok(());
    }
    // Compare-and-swap: a branch that moved in the meantime keeps its new tip.
    // A normalization failure leaves the checkout as it was and never blocks.
    let _ = output(git(policy, repo, &["update-ref", &reference, base, &old]))?;
    Ok(())
}

fn merge_prompt(
    policy: &RunPolicy,
    tree: &Path,
    task: &Task,
    base: &str,
    diagnostic: &str,
) -> Result<String> {
    // Index-only, no helpers or worktree content conversion. Stat output is
    // bounded to twenty paths instead of retaining an unbounded name list.
    let paths = checked(git(
        policy,
        tree,
        &[
            "diff",
            "--cached",
            "--stat",
            "--stat-count=20",
            "--stat-width=120",
            "--stat-name-width=100",
            "--no-ext-diff",
            "--no-textconv",
            "--diff-filter=U",
        ],
    ))?;
    let pending = checked(git(policy, tree, &["rev-parse", "MERGE_HEAD"]))?;
    Ok(format!("{}\n\nThere is a merge in progress from base {} at commit {pending}. The current base for this run is {base}. Resolve the conflicts, stage the resolutions, and finish the merge commit before completing the task. Then run `git merge --no-edit {base}` to integrate the captured current base as well, resolving and committing any further conflicts. Preserve the task's existing work.\nConflicted paths (at most twenty; use git diff --name-only --diff-filter=U for the full list):\n{paths}\nMerge output:\n{diagnostic}", task.prompt, task.base))
}

pub(crate) fn validate(policy: &RunPolicy, repo: &Path, tree: &Path, task: &Task) -> Result<()> {
    let canonical = tree.canonicalize().map_err(|e| e.to_string())?;
    if !fs::symlink_metadata(tree.join(".git")).is_ok_and(|m| m.is_file())
        || canonical == repo.canonicalize().map_err(|e| e.to_string())?
    {
        return Err("retained directory is not a separate linked task worktree".into());
    }
    let actual = PathBuf::from(checked(git(policy, tree, &["rev-parse", "--show-toplevel"]))?);
    let common = |p: &Path| -> Result<PathBuf> {
        PathBuf::from(checked(git(
            policy,
            p,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        ))?)
        .canonicalize()
        .map_err(|e| e.to_string())
    };
    if actual.canonicalize().map_err(|e| e.to_string())? != canonical || common(tree)? != common(repo)? {
        return Err("retained directory is not this repository's task worktree".into());
    }
    if checked(git(policy, tree, &["symbolic-ref", "--quiet", "HEAD"]))?
        != format!("refs/heads/{}", task.branch)
    {
        return Err("retained worktree is on a different branch".into());
    }
    Ok(())
}

pub(crate) fn prepare(
    repo: &Path,
    tree: &Path,
    task: &Task,
    base: &str,
    binary: &Path,
    policy: &RunPolicy,
    lock: &fs::File,
) -> Result<Prepared> {
    let retained = tree.exists();
    if !retained {
        let branch = format!("refs/heads/{}", task.branch);
        let exists = output(git(policy, repo, &["show-ref", "--verify", "--quiet", &branch]))?;
        let create = match exists.status.code() {
            Some(0) => false,
            Some(1) => true,
            _ => return Err(String::from_utf8_lossy(&exists.stderr).into()),
        };
        let tree_arg = tree.to_str().ok_or("non-UTF8 worktree path")?;
        let mut args = vec![
            "-c",
            "core.hooksPath=/dev/null",
            "worktree",
            "add",
            "--no-checkout",
        ];
        if create {
            args.push("--detach");
        }
        args.push(tree_arg);
        args.push(if create { base } else { &task.branch });
        // Only metadata on the host. Git still refuses a branch checked out
        // elsewhere; no task-branch update or checkout callback runs here.
        checked(git(policy, repo, &args))?;
        let checkout = supervise(
            binary,
            tree,
            SupervisorRequest {
                source: repo.to_path_buf(),
                operation: SupervisorOperation::Checkout {
                    branch: task.branch.clone(),
                    create,
                },
            },
            policy,
            lock,
        )?;
        if !checkout.status.success() {
            return Err(format!(
                "sandboxed worktree checkout failed: {}",
                String::from_utf8_lossy(&checkout.stderr)
            ));
        }
        fast_forward_stale_base(policy, repo, task, base)?;
    }
    validate(policy, repo, tree, task)?;
    for operation in ["rebase-merge", "rebase-apply", "CHERRY_PICK_HEAD", "REVERT_HEAD"] {
        if git_path(policy, tree, operation)?.exists() {
            return Err(format!("retained worktree has {operation} in progress"));
        }
    }
    let initial = checked(git(policy, tree, &["rev-parse", "HEAD"]))?;
    if merging(policy, tree)? {
        return Ok(Prepared {
            initial,
            prompt: merge_prompt(policy, tree, task, base, "Resuming the retained merge.")?,
        });
    }
    if retained && dirty_worktree(binary, tree, repo, policy, lock)? {
        return Err("retained worktree has uncommitted changes; worker was not restarted".into());
    }
    let merged = supervise(
        binary,
        tree,
        SupervisorRequest {
            source: repo.to_path_buf(),
            operation: SupervisorOperation::Merge { base: base.into() },
        },
        policy,
        lock,
    )?;
    if !merged.status.success() {
        let diagnostic = String::from_utf8_lossy(&merged.stderr);
        if merged.status.code() != Some(1) || !merging(policy, tree)? {
            return Err(format!("base merge failed: {diagnostic}"));
        }
        return Ok(Prepared {
            initial,
            prompt: merge_prompt(policy, tree, task, base, &diagnostic)?,
        });
    }
    ensure_merged(policy, tree, base, &task.branch)?;
    // A clean base-only merge is preparation, not new worker work.
    Ok(Prepared {
        initial: checked(git(policy, tree, &["rev-parse", "HEAD"]))?,
        prompt: task.prompt.clone(),
    })
}

pub(crate) fn ensure_merged(policy: &RunPolicy, tree: &Path, base: &str, branch: &str) -> Result<()> {
    if checked(git(policy, tree, &["symbolic-ref", "--quiet", "HEAD"]))? != format!("refs/heads/{branch}") {
        return Err("agent left the worktree on a different branch".into());
    }
    let unmerged = output(git(
        policy,
        tree,
        &[
            "diff",
            "--cached",
            "--quiet",
            "--no-ext-diff",
            "--no-textconv",
            "--diff-filter=U",
        ],
    ))?;
    match unmerged.status.code() {
        Some(0) => {}
        Some(1) => return Err("merge remains in progress; retained for resolution".into()),
        _ => {
            return Err(format!(
                "cannot inspect merge index: {}",
                String::from_utf8_lossy(&unmerged.stderr)
            ))
        }
    }
    if merging(policy, tree)? {
        return Err("merge remains in progress; retained for resolution".into());
    }
    let ancestor = output(git(policy, tree, &["merge-base", "--is-ancestor", base, "HEAD"]))?;
    match ancestor.status.code() {
        Some(0) => Ok(()),
        Some(1) => Err("base is not merged into the task branch; retained for continuation".into()),
        _ => Err(format!(
            "cannot verify captured base: {}",
            String::from_utf8_lossy(&ancestor.stderr)
        )),
    }
}
