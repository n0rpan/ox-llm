//! Worktree continuation uses ordinary merges; no reset, stash or force checkout.
use super::{checked, git, output, Result, Task};
use std::fs;
use std::path::{Path, PathBuf};

pub(crate) struct Prepared {
    pub initial: String,
    pub base: String,
    pub prompt: String,
}
fn git_path(tree: &Path, name: &str) -> Result<PathBuf> {
    checked(git(
        tree,
        &["rev-parse", "--path-format=absolute", "--git-path", name],
    ))
    .map(PathBuf::from)
}
fn merging(tree: &Path) -> Result<bool> {
    Ok(git_path(tree, "MERGE_HEAD")?.exists())
}
fn merge_prompt(tree: &Path, task: &Task, output: &str) -> Result<String> {
    let paths = checked(git(tree, &["diff", "--name-only", "--diff-filter=U"]))?;
    Ok(format!("{}\n\nThere is a merge in progress from base {}. Resolve the conflicts, stage the resolutions, and finish the merge commit before completing the task. Preserve the task's existing work.\nConflicted paths:\n{paths}\nMerge output:\n{output}", task.prompt, task.base))
}
pub(crate) fn prepare(repo: &Path, tree: &Path, task: &Task) -> Result<Prepared> {
    let base = checked(git(
        repo,
        &["rev-parse", "--verify", &format!("{}^{{commit}}", task.base)],
    ))?;
    if !tree.exists() {
        let branch = format!("refs/heads/{}", task.branch);
        let exists = output(git(repo, &["show-ref", "--verify", "--quiet", &branch]))?;
        let tree_arg = tree.to_str().ok_or("non-UTF8 worktree path")?;
        match exists.status.code() {
            Some(0) => {
                checked(git(repo, &["worktree", "add", tree_arg, &task.branch]))?;
            }
            Some(1) => {
                checked(git(
                    repo,
                    &["worktree", "add", "-b", &task.branch, tree_arg, &base],
                ))?;
            }
            _ => return Err(String::from_utf8_lossy(&exists.stderr).into()),
        }
    }
    let canonical = tree.canonicalize().map_err(|e| e.to_string())?;
    let actual = PathBuf::from(checked(git(tree, &["rev-parse", "--show-toplevel"]))?);
    let common = |p: &Path| -> Result<PathBuf> {
        PathBuf::from(checked(git(
            p,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        ))?)
        .canonicalize()
        .map_err(|e| e.to_string())
    };
    if actual.canonicalize().map_err(|e| e.to_string())? != canonical || common(tree)? != common(repo)? {
        return Err("retained directory is not this repository's task worktree".into());
    }
    let branch = checked(git(tree, &["symbolic-ref", "--quiet", "HEAD"]))?;
    if branch != format!("refs/heads/{}", task.branch) {
        return Err("retained worktree is on a different branch".into());
    }
    for operation in ["rebase-merge", "rebase-apply", "CHERRY_PICK_HEAD", "REVERT_HEAD"] {
        if git_path(tree, operation)?.exists() {
            return Err(format!("retained worktree has {operation} in progress"));
        }
    }
    let initial = checked(git(tree, &["rev-parse", "HEAD"]))?;
    if merging(tree)? {
        return Ok(Prepared {
            initial,
            base,
            prompt: merge_prompt(tree, task, "Resuming the retained merge.")?,
        });
    }
    if !checked(git(tree, &["status", "--porcelain"]))?.is_empty() {
        return Err("retained worktree has uncommitted changes; commit them before continuing".into());
    }
    let merged = output(git(tree, &["merge", "--no-edit", &base]))?;
    if !merged.status.success() {
        if !merging(tree)? {
            return Err(format!(
                "base merge failed: {}\n{}",
                String::from_utf8_lossy(&merged.stdout),
                String::from_utf8_lossy(&merged.stderr)
            ));
        }
        return Ok(Prepared {
            initial,
            base,
            prompt: merge_prompt(
                tree,
                task,
                &format!(
                    "{}\n{}",
                    String::from_utf8_lossy(&merged.stdout),
                    String::from_utf8_lossy(&merged.stderr)
                ),
            )?,
        });
    }
    // A clean base-only merge is preparation, not new task work.
    Ok(Prepared {
        initial: checked(git(tree, &["rev-parse", "HEAD"]))?,
        base,
        prompt: task.prompt.clone(),
    })
}
pub(crate) fn ensure_merged(tree: &Path, base: &str, branch: &str) -> Result<()> {
    if checked(git(tree, &["symbolic-ref", "--quiet", "HEAD"]))? != format!("refs/heads/{branch}") {
        return Err("agent left the worktree on a different branch".into());
    }
    if merging(tree)? || !checked(git(tree, &["diff", "--name-only", "--diff-filter=U"]))?.is_empty() {
        return Err("merge remains in progress; retained for resolution".into());
    }
    if !output(git(tree, &["merge-base", "--is-ancestor", base, "HEAD"]))?
        .status
        .success()
    {
        return Err("base is not merged into the task branch; retained for continuation".into());
    }
    Ok(())
}
/// Read acceptance settings from the captured base commit, even on an older task branch.
pub(crate) fn done_from_base(repo: &Path, base: &str) -> Result<Vec<String>> {
    let path = ".oxa/settings.json";
    if checked(git(repo, &["ls-tree", "--name-only", base, "--", path]))?.is_empty() {
        return Ok(vec![]);
    }
    let text = checked(git(repo, &["show", &format!("{base}:{path}")]))?;
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    match value.get("done") {
        None => Ok(vec![]),
        Some(value) => serde_json::from_value(value.clone()).map_err(|e| format!("done commands: {e}")),
    }
}
pub(crate) fn check_receipt(receipt: &Path, tree: &Path, task: &Task) -> Result<()> {
    if receipt.exists() {
        let record: super::Record = serde_json::from_slice(&fs::read(receipt).map_err(|e| e.to_string())?)
            .map_err(|e| format!("retained receipt: {e}"))?;
        if record.id != task.id || record.branch != task.branch || record.worktree != tree {
            return Err("retained receipt belongs to a different task branch or worktree".into());
        }
    }
    Ok(())
}
