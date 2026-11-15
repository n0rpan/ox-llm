//! Explicit publishing of the accepted commit; existing queued PRs are never pushed.
use super::{checked, git, state, tree, Outcome, Record, Result, Task};
use state::Step;
use std::{
    fs::File,
    path::{Path, PathBuf},
    process::Command,
};

fn context(repo: &Path, task: &Task) -> Result<(PathBuf, File, state::State)> {
    let common = PathBuf::from(checked(git(
        repo,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    ))?);
    let path = common.join("oxa-work").join(format!("{}.json", task.id));
    let lock = state::lock(&path.with_extension("lock"))?;
    let value = state::read(&path)?;
    if value.task != *task {
        return Err("task specification changed".into());
    }
    Ok((path, lock, value))
}
fn gh(repo: &Path, lock: &File, args: &[&str]) -> Result<String> {
    let mut command = Command::new("gh");
    command.current_dir(repo).args(args);
    command.stdin(lock.try_clone().map_err(|e| e.to_string())?);
    super::checked_output(command.output().map_err(|e| e.to_string())?)
}
fn json(text: &str) -> Result<serde_json::Value> {
    serde_json::from_str(text).map_err(|e| e.to_string())
}
fn pr_base(repo: &Path, task: &Task) -> Result<String> {
    let reference = checked(git(repo, &["rev-parse", "--symbolic-full-name", &task.base]))?;
    reference
        .strip_prefix("refs/heads/")
        .or_else(|| reference.strip_prefix("refs/remotes/origin/"))
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "publishing requires a named local or origin base branch".into())
}
pub fn publish(repo: &Path, task: &Task, offline: bool) -> Result<Record> {
    let (path, lock, mut value) = context(repo, task)?;
    let accepted = value.acceptance_outcome.unwrap_or(value.record.outcome);
    if !matches!(accepted, Outcome::Done | Outcome::Red) {
        return Ok(value.record);
    }
    value.acceptance_outcome = Some(accepted);
    state::save(&path, &mut value, Step::Publishing)?;
    let result = (|| -> Result<(String, bool)> {
        if offline {
            return Err("publishing is unavailable in offline mode".into());
        }
        let tree = &value.record.worktree;
        tree::validate(repo, tree, task)?;
        if !checked(git(tree, &["status", "--porcelain"]))?.is_empty() {
            return Err("uncommitted work must be saved before publishing".into());
        }
        let head = checked(git(tree, &["rev-parse", "HEAD"]))?;
        if value.record.head.as_deref() != Some(head.as_str()) {
            return Err("task branch changed after acceptance; use --rerun before publishing".into());
        }
        let base = pr_base(tree, task)?;
        let repository = json(&gh(tree, &lock, &["repo", "view", "--json", "nameWithOwner"])?)?;
        let name = repository["nameWithOwner"]
            .as_str()
            .ok_or("repository name missing")?;
        let (owner, repo_name) = name.split_once('/').ok_or("invalid repository name")?;
        let prs = json(&gh(
            tree,
            &lock,
            &[
                "pr",
                "list",
                "--repo",
                name,
                "--state",
                "open",
                "--head",
                &task.branch,
                "--json",
                "number,url,isDraft",
            ],
        )?)?;
        let prs = prs.as_array().ok_or("invalid PR list")?;
        if prs.len() > 1 {
            return Err("multiple open PRs match the task branch".into());
        }
        if let Some(pr) = prs.first() {
            let number = pr["number"].as_u64().ok_or("PR number missing")?.to_string();
            let query = "query($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name){pullRequest(number:$number){mergeQueueEntry{id}}}}";
            let data = json(&gh(
                tree,
                &lock,
                &[
                    "api",
                    "graphql",
                    "-f",
                    &format!("query={query}"),
                    "-F",
                    &format!("owner={owner}"),
                    "-F",
                    &format!("name={repo_name}"),
                    "-F",
                    &format!("number={number}"),
                ],
            )?)?;
            let pull = &data["data"]["repository"]["pullRequest"];
            if !pull.is_object() || pull.get("mergeQueueEntry").is_none() {
                return Err("cannot determine whether PR is in the merge queue".into());
            }
            if !pull["mergeQueueEntry"].is_null() {
                return Err("PR is in the merge queue; use a new task branch for follow-up work".into());
            }
        }
        let mut push = git(
            repo,
            &["push", "origin", &format!("{head}:refs/heads/{}", task.branch)],
        );
        push.env("GIT_TERMINAL_PROMPT", "0")
            .stdin(lock.try_clone().map_err(|e| e.to_string())?);
        super::checked_output(push.output().map_err(|e| e.to_string())?)?;
        let draft = accepted == Outcome::Red;
        let body = path.with_extension("pr-body.md");
        let text = format!("Task `{}` executed by `oxa work`.\n\nAcceptance result: `{}`.\n\nThe runner retained the task worktree and saved its changes.\n", task.id, if draft { "red" } else { "done" });
        std::fs::write(&body, text).map_err(|e| e.to_string())?;
        let body_arg = body.to_str().ok_or("non-UTF8 PR body path")?;
        let result = if let Some(pr) = prs.first() {
            let number = pr["number"].as_u64().ok_or("PR number missing")?.to_string();
            gh(
                tree,
                &lock,
                &["pr", "edit", &number, "--repo", name, "--body-file", body_arg],
            )?;
            let was_draft = pr["isDraft"].as_bool().ok_or("PR draft state missing")?;
            if was_draft != draft {
                let mut args = vec!["pr", "ready", &number, "--repo", name];
                if draft {
                    args.push("--undo");
                }
                gh(tree, &lock, &args)?;
            }
            pr["url"].as_str().ok_or("PR URL missing")?.to_string()
        } else {
            let title = format!("oxa work: {}", task.id);
            let mut args = vec![
                "pr",
                "create",
                "--repo",
                name,
                "--head",
                &task.branch,
                "--base",
                &base,
                "--title",
                &title,
                "--body-file",
                body_arg,
            ];
            if draft {
                args.push("--draft");
            }
            gh(tree, &lock, &args)?
        };
        let _ = std::fs::remove_file(body);
        Ok((result.trim().to_string(), draft))
    })();
    match result {
        Ok((url, draft)) => {
            value.record.pull_request = Some(url);
            value.record.draft = draft;
            value.record.outcome = accepted;
            value.record.detail.clear();
            state::save(&path, &mut value, Step::Finished)?;
        }
        Err(error) => {
            value.record.outcome = Outcome::Blocked;
            value.record.detail = format!("publishing: {error}");
            state::save(&path, &mut value, Step::Publishing)?;
        }
    }
    Ok(value.record)
}
pub fn report_notes(
    repo: &Path,
    task: &Task,
    offline: bool,
    reporter: &mut dyn FnMut(&super::ProblemNote, &File, bool) -> Result<super::ReportRecord>,
) -> Result<Record> {
    let (path, lock, mut value) = context(repo, task)?;
    let prior_step = value.record.step;
    for index in 0..value.record.problem_notes.len() {
        if value
            .record
            .reports
            .get(index)
            .is_some_and(|r| !matches!(r.disposition.as_str(), "local" | "failed"))
        {
            continue;
        }
        state::save(&path, &mut value, Step::Reporting)?;
        let report = reporter(&value.record.problem_notes[index], &lock, offline).unwrap_or_else(|detail| {
            super::ReportRecord {
                disposition: "failed".into(),
                draft: PathBuf::new(),
                url: None,
                detail,
            }
        });
        if index < value.record.reports.len() {
            value.record.reports[index] = report;
        } else {
            value.record.reports.push(report);
        }
        state::save(&path, &mut value, Step::Reporting)?;
    }
    state::save(
        &path,
        &mut value,
        if prior_step == Step::Publishing {
            Step::Publishing
        } else {
            Step::Finished
        },
    )?;
    Ok(value.record)
}
