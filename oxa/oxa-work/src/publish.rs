//! Explicit publishing of the accepted commit; existing queued PRs are never pushed.
use super::{checked, state, tree, CheckpointObserver, Outcome, Record, Result, RunPolicy, Task};
use state::Step;
use std::os::unix::fs::OpenOptionsExt;
use std::{
    fs::File,
    path::{Path, PathBuf},
    process::Command,
};

fn context(
    task: &Task,
    policy: &RunPolicy,
    observer: &dyn CheckpointObserver,
) -> Result<(state::Writer, File, state::State)> {
    super::validate(task)?;
    let path = policy.storage.join(format!("{}.json", task.id));
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .mode(0o600)
        .open(path.with_extension("lock"))
        .map_err(|e| e.to_string())?;
    lock.try_lock()
        .map_err(|e| format!("task is already running: {e}"))?;
    let value = state::load(&path)?;
    if value.task != *task {
        return Err("task specification changed".into());
    }
    let mut writer = state::Writer::new(path, true);
    writer.replay(&value, observer)?;
    Ok((writer, lock, value))
}
fn gh(binary: &Path, policy: &RunPolicy, repo: &Path, lock: &File, args: &[&str]) -> Result<String> {
    let mut command = Command::new(binary);
    super::isolate_git_environment(&mut command);
    command
        .env("PATH", &policy.host_path)
        .env_remove("GH_REPO")
        .env_remove("GH_HOST")
        .current_dir(repo)
        .args(args);
    command.stdin(lock.try_clone().map_err(|e| e.to_string())?);
    let output = command.output().map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err("GitHub publication request failed".into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().into())
}
fn json(text: &str) -> Result<serde_json::Value> {
    serde_json::from_str(text).map_err(|e| e.to_string())
}
fn pr_base(repo: &Path, task: &Task, policy: &RunPolicy) -> Result<String> {
    let reference = checked(super::git(
        policy,
        repo,
        &["rev-parse", "--symbolic-full-name", &task.base],
    ))?;
    reference
        .strip_prefix("refs/heads/")
        .or_else(|| reference.strip_prefix("refs/remotes/origin/"))
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "publishing requires a named local or origin base branch".into())
}
pub fn publish(
    repo: &Path,
    binary: &Path,
    task: &Task,
    policy: &RunPolicy,
    gh_binary: &Path,
    observer: &dyn CheckpointObserver,
    confirm: &mut dyn FnMut(&str) -> Option<bool>,
) -> Result<Record> {
    let (mut writer, lock, mut value) = context(task, policy, observer)?;
    let accepted = value.record.acceptance_outcome.unwrap_or(value.record.outcome);
    if !matches!(accepted, Outcome::Done | Outcome::NothingToDo) {
        return Ok(value.record);
    }
    value.record.acceptance_outcome = Some(accepted);
    writer.save(&mut value, Step::Publishing, observer)?;
    let git = |repo: &Path, args: &[&str]| super::git(policy, repo, args);
    let gh = |repo: &Path, lock: &File, args: &[&str]| gh(gh_binary, policy, repo, lock, args);
    let result = (|| -> Result<(String, bool)> {
        if policy.offline {
            return Err("publishing is unavailable in offline mode".into());
        }
        let tree = &value.record.worktree;
        tree::validate(policy, repo, tree, task)?;
        if super::dirty_worktree(binary, tree, repo, policy, &lock)? {
            return Err("uncommitted work must be saved before publishing".into());
        }
        let head = checked(git(tree, &["rev-parse", "HEAD"]))?;
        if value.record.head_sha.as_deref() != Some(head.as_str()) {
            return Err("task branch changed after acceptance; use --rerun before publishing".into());
        }
        let base = pr_base(tree, task, policy)?;
        if task.branch == base {
            return Err("task branch must differ from its base branch".into());
        }
        let remote = checked(git(repo, &["remote", "get-url", "--push", "--all", "origin"]))?;
        if remote.lines().count() != 1 {
            return Err("publishing requires one origin push URL".into());
        }
        let repository = json(&gh(
            tree,
            &lock,
            &["repo", "view", &remote, "--json", "nameWithOwner"],
        )?)?;
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
                "number,url,isDraft,headRepositoryOwner",
            ],
        )?)?;
        let prs = prs.as_array().ok_or("invalid PR list")?;
        if prs.len() > 1 {
            return Err("multiple open PRs match the task branch".into());
        }
        if prs
            .first()
            .is_some_and(|pr| pr["headRepositoryOwner"]["login"].as_str() != Some(owner))
        {
            return Err("PR head repository does not match origin".into());
        }
        if accepted == Outcome::NothingToDo
            && prs.is_empty()
            && value.record.head_sha == value.record.base_sha
        {
            return Err("nothing to publish".into());
        }
        if confirm(&format!(
            "Push accepted commit {head} as {} to origin and create/update its PR in {name} (base {base})? Type yes to publish: ",
            task.branch
        )) != Some(true) {
            return Err("publication requires confirmation through the controlling terminal".into());
        }
        if remote != checked(git(repo, &["remote", "get-url", "--push", "--all", "origin"]))?
            || head != checked(git(tree, &["rev-parse", "HEAD"]))?
        {
            return Err("publication destination or task head changed during confirmation".into());
        }
        // Confirmation may remain open indefinitely: queue admission is checked
        // only after it returns, immediately before the push.
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
            &[
                "-c",
                "protocol.https.allow=always",
                "-c",
                "protocol.ssh.allow=always",
                "-c",
                "protocol.file.allow=always",
                "push",
                "origin",
                &format!("{head}:refs/heads/{}", task.branch),
            ],
        );
        push.env("GIT_TERMINAL_PROMPT", "0")
            .stdin(lock.try_clone().map_err(|e| e.to_string())?);
        let pushed = push.output().map_err(|e| e.to_string())?;
        if !pushed.status.success() {
            return Err("accepted commit push failed".into());
        }
        let draft = false;
        let body = policy.storage.join(format!("{}.pr-body.md", task.id));
        let text = format!("T2 — task `{}` executed by `oxa work`.\n\nWhat/why: explicitly requested task; review its changes against the base.\n\nChanges: accepted commit `{head}`; worktree retained.\n\nRemoves: inspect the diff before promotion; no deletion clearance is inferred.\n\nValidation: frozen local acceptance passed. CI: not yet qualified.\n", task.id);
        std::fs::write(&body, text).map_err(|e| e.to_string())?;
        let body_arg = body.to_str().ok_or("non-UTF8 PR body path")?;
        let result = if let Some(pr) = prs.first() {
            let number = pr["number"].as_u64().ok_or("PR number missing")?.to_string();
            gh(
                tree,
                &lock,
                &["pr", "edit", &number, "--repo", name, "--base", &base],
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
            writer.save(&mut value, Step::Finished, observer)?;
        }
        Err(error) => {
            value.record.outcome = Outcome::Blocked;
            value.record.detail = format!("publishing: {error}");
            writer.save(&mut value, Step::Publishing, observer)?;
        }
    }
    Ok(value.record)
}
pub fn report_notes(
    task: &Task,
    policy: &RunPolicy,
    observer: &dyn CheckpointObserver,
    reporter: &mut dyn FnMut(&super::ProblemNote, &File, bool) -> Result<super::ReportRecord>,
) -> Result<Record> {
    let (mut writer, lock, mut value) = context(task, policy, observer)?;
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
        writer.save(&mut value, Step::Reporting, observer)?;
        let report =
            reporter(&value.record.problem_notes[index], &lock, policy.offline).unwrap_or_else(|detail| {
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
        writer.save(&mut value, Step::Reporting, observer)?;
    }
    writer.save(
        &mut value,
        if prior_step == Step::Publishing {
            Step::Publishing
        } else {
            Step::Finished
        },
        observer,
    )?;
    Ok(value.record)
}
