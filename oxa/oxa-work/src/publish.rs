//! Explicit publishing of the accepted commit; existing queued PRs are never pushed.
use super::{checked, state, tree, CheckpointObserver, LockedTask, Outcome, Record, Result, RunPolicy, Task};
use state::Step;
mod landing;
pub use landing::{
    Admission, AdmissionAcknowledgment, AdmissionReceipt, DeliveryReceipt, LandingPublisher, NoticeIntent,
    RejectedLanding, RetryIntent,
};
use std::os::{fd::AsRawFd, unix::process::CommandExt};
use std::{
    fs::File,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

fn action_context(
    locked: &LockedTask<'_>,
    observer: &dyn CheckpointObserver,
) -> Result<(state::Writer, state::State)> {
    let path = locked.policy.storage.join(format!("{}.json", locked.task.id));
    let value = state::load(&path)?;
    if value.task != *locked.task {
        return Err("task specification changed".into());
    }
    let mut writer = state::Writer::new(path, true);
    writer.replay(&value, observer)?;
    Ok((writer, value))
}
const MAX_PUBLICATION_RESPONSE_BYTES: usize = 1024 * 1024;

pub(crate) fn isolate_github_environment(command: &mut Command) {
    // The destination comes from origin. Let the CLI choose its stored
    // login for that host, independently of host-agnostic shell overrides.
    for key in [
        "GH_HOST",
        "GH_REPO",
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "GH_ENTERPRISE_TOKEN",
        "GITHUB_ENTERPRISE_TOKEN",
    ] {
        command.env_remove(key);
    }
}

pub(crate) fn gh(
    binary: &Path,
    policy: &RunPolicy,
    repo: &Path,
    lock: &File,
    args: &[&str],
) -> Result<String> {
    let mut command = Command::new(binary);
    super::isolate_git_environment(&mut command);
    isolate_github_environment(&mut command);
    command
        .env("PATH", &policy.host_path)
        .env("GH_PROMPT_DISABLED", "1")
        .current_dir(repo)
        .args(args);
    match super::command_runner_with_stdin(
        command,
        Some(MAX_PUBLICATION_RESPONSE_BYTES),
        Stdio::from(lock.try_clone().map_err(|e| e.to_string())?),
        super::COMMAND_DEADLINE,
    ) {
        Some((0, text)) => Ok(text.trim().into()),
        _ => Err("GitHub publication request failed".into()),
    }
}
fn json(text: &str) -> Result<serde_json::Value> {
    serde_json::from_str(text).map_err(|e| e.to_string())
}
pub(crate) struct Repository {
    pub(crate) qualified: String,
    pub(crate) host: String,
    pub(crate) owner: String,
    pub(crate) name: String,
}
pub(crate) fn repository(value: &serde_json::Value) -> Result<Repository> {
    let identity = value["nameWithOwner"]
        .as_str()
        .ok_or("repository identity missing")?;
    let (owner, name) = identity.split_once('/').ok_or("invalid repository identity")?;
    let url = value["url"].as_str().ok_or("repository URL missing")?;
    let (host, path) = url
        .strip_prefix("https://")
        .and_then(|url| url.split_once('/'))
        .ok_or("repository URL must identify its HTTPS host")?;
    if owner.is_empty()
        || name.is_empty()
        || name.contains('/')
        || path.trim_end_matches('/') != identity
        || host.is_empty()
        || !host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b".-:".contains(&byte))
    {
        return Err("repository URL disagrees with its identity".into());
    }
    Ok(Repository {
        qualified: format!("{host}/{identity}"),
        host: host.into(),
        owner: owner.into(),
        name: name.into(),
    })
}
/// Resolve the single origin destination through the same validated `gh` read.
pub(crate) fn origin_repository(
    repo: &Path,
    gh_binary: &Path,
    policy: &RunPolicy,
    lock: &File,
) -> Result<Repository> {
    let remote = checked(super::git(
        policy,
        repo,
        &["remote", "get-url", "--push", "--all", "origin"],
    ))?;
    if remote.lines().count() != 1 {
        return Err("publishing requires one origin push URL".into());
    }
    repository(&json(&gh(
        gh_binary,
        policy,
        repo,
        lock,
        &["repo", "view", &remote, "--json", "nameWithOwner,url"],
    )?)?)
}

/// Push one exact accepted commit. Metadata probes deny every transport;
/// only this explicit, confirmed publication may use normal GitHub/file transports.
pub(crate) fn push_head(
    policy: &RunPolicy,
    repo: &Path,
    head: &str,
    branch: &str,
    lock: &File,
) -> Result<()> {
    let mut push = super::git(
        policy,
        repo,
        &[
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "protocol.https.allow=always",
            "-c",
            "protocol.ssh.allow=always",
            "-c",
            "protocol.file.allow=always",
            "push",
            "origin",
            &format!("{head}:refs/heads/{branch}"),
        ],
    );
    isolate_github_environment(&mut push);
    push.env("GIT_TERMINAL_PROMPT", "0")
        .env("GH_PROMPT_DISABLED", "1")
        .env("GIT_ALLOW_PROTOCOL", "https:ssh:file");
    if !matches!(
        super::command_runner_with_stdin(
            push,
            Some(MAX_PUBLICATION_RESPONSE_BYTES),
            Stdio::from(lock.try_clone().map_err(|e| e.to_string())?),
            super::COMMAND_DEADLINE,
        ),
        Some((0, _))
    ) {
        return Err("accepted commit push failed".into());
    }
    Ok(())
}

const BODY_BEGIN: &str = "<!-- oxa-work-acceptance -->";
const BODY_END: &str = "<!-- /oxa-work-acceptance -->";
fn body(
    task: &Task,
    head: &str,
    lane: Option<&crate::lane::LaneVerdict>,
    existing: Option<&str>,
) -> Result<String> {
    let ci = match lane {
        Some(verdict) if verdict.head == head && verdict.passed() => {
            format!("CI: lane run {} passed at this commit", verdict.run_id)
        }
        Some(verdict) => format!(
            "CI: lane run {} concluded {} at {}",
            verdict.run_id, verdict.conclusion, verdict.head
        ),
        None => "CI: not qualified for this commit".into(),
    };
    let section =
        format!("{BODY_BEGIN}\nAccepted commit: `{head}`\nLocal acceptance: passed\n{ci}\n{BODY_END}");
    let validation = if lane.is_some() {
        "Validation: local acceptance and the lane run passed at the accepted commit."
    } else {
        "Validation: frozen local acceptance passed; CI qualification is separate."
    };
    let default = format!("T2 — task `{}` executed by `oxa work`.\n\nWhat/why: explicitly requested task; review its changes against the base.\n\nChanges: accepted worktree retained.\n\nRemoves: inspect the diff before promotion; no deletion clearance is inferred.\n\n{validation}\n", task.id);
    let original = existing.unwrap_or(&default);
    match (
        original.matches(BODY_BEGIN).count(),
        original.matches(BODY_END).count(),
    ) {
        (0, 0) => Ok(format!("{original}\n\n{section}\n")),
        (1, 1) => {
            let (before, rest) = original
                .split_once(BODY_BEGIN)
                .ok_or("invalid managed PR section")?;
            let (_, after) = rest.split_once(BODY_END).ok_or("invalid managed PR section")?;
            Ok(format!("{before}{section}{after}"))
        }
        _ => Err("ambiguous managed PR section; preserve the owner description".into()),
    }
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
    LockedTask::open(task, policy, false)?.publish(repo, binary, gh_binary, observer, confirm)
}

fn publish_locked(
    repo: &Path,
    binary: &Path,
    locked: &LockedTask<'_>,
    gh_binary: &Path,
    observer: &dyn CheckpointObserver,
    confirm: &mut dyn FnMut(&str) -> Option<bool>,
) -> Result<Record> {
    let (mut writer, mut value) = action_context(locked, observer)?;
    let (task, policy, lock) = (locked.task, locked.policy, &locked.lock);
    let accepted = value.record.acceptance_outcome.unwrap_or(value.record.outcome);
    if !super::completed_execution(value.record.step)
        || !matches!(accepted, Outcome::Done | Outcome::NothingToDo)
    {
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
        if task.lane_ci
            && !value.record.lane.as_ref().is_some_and(|verdict| {
                Some(verdict.head.as_str()) == value.record.head_sha.as_deref() && verdict.passed()
            })
        {
            return Err("publication requires a passing lane CI run at the saved head".into());
        }
        let tree = &value.record.worktree;
        tree::validate(policy, repo, tree, task)?;
        if super::dirty_worktree(binary, tree, repo, policy, lock)? {
            return Err("uncommitted work must be saved before publishing".into());
        }
        let head = checked(git(tree, &["rev-parse", "HEAD"]))?;
        if value.record.head_sha.as_deref() != Some(head.as_str()) {
            return Err("task branch changed after acceptance; use --rerun before publishing".into());
        }
        let proof = super::committed::verify_commit(repo, binary, policy, &value, lock)?;
        let accepted_base = value
            .record
            .base_sha
            .as_deref()
            .ok_or("saved base is unavailable")?;
        let base = pr_base(tree, task, policy)?;
        if task.branch == base {
            return Err("task branch must differ from its base branch".into());
        }
        let remote = checked(git(repo, &["remote", "get-url", "--push", "--all", "origin"]))?;
        let repository = origin_repository(tree, gh_binary, policy, lock)?;
        let name = repository.qualified.as_str();
        let owner = repository.owner.as_str();
        let repo_name = repository.name.as_str();
        let prs = json(&gh(
            tree,
            lock,
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
                "number,url,isDraft,headRepositoryOwner,isCrossRepository",
            ],
        )?)?;
        let prs = prs.as_array().ok_or("invalid PR list")?;
        if prs.len() > 1 {
            return Err("multiple open PRs match the task branch".into());
        }
        if prs.first().is_some_and(|pr| {
            pr["headRepositoryOwner"]["login"].as_str() != Some(owner)
                || pr["isCrossRepository"].as_bool() != Some(false)
        }) {
            return Err("PR head repository does not match origin".into());
        }
        if accepted == Outcome::NothingToDo
            && prs.is_empty()
            && value.record.head_sha == value.record.base_sha
        {
            return Err("nothing to publish".into());
        }
        let existing_body = if let Some(pr) = prs.first() {
            let number = pr["number"].as_u64().ok_or("PR number missing")?.to_string();
            let value = json(&gh(
                tree,
                lock,
                &["pr", "view", &number, "--repo", name, "--json", "body"],
            )?)?;
            Some(value["body"].as_str().ok_or("PR body missing")?.to_owned())
        } else {
            None
        };
        let text = body(task, &head, value.record.lane.as_ref(), existing_body.as_deref())?;
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
        proof.verify(policy, &head, accepted_base)?;
        // Confirmation may remain open indefinitely: queue admission is checked
        // only after it returns, immediately before the push.
        if let Some(pr) = prs.first() {
            let number = pr["number"].as_u64().ok_or("PR number missing")?.to_string();
            let query = "query($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name){pullRequest(number:$number){mergeQueueEntry{id}}}}";
            let data = json(&gh(
                tree,
                lock,
                &[
                    "api",
                    "graphql",
                    "--hostname",
                    &repository.host,
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
        push_head(policy, repo, &head, &task.branch, lock)?;
        let draft = false;
        let body = policy.storage.join(format!("{}.pr-body.md", task.id));
        crate::checkpoint::write_atomic(&body, text.as_bytes(), body.exists())?;
        let body_arg = body.to_str().ok_or("non-UTF8 PR body path")?;
        let result = if let Some(pr) = prs.first() {
            let number = pr["number"].as_u64().ok_or("PR number missing")?.to_string();
            gh(
                tree,
                lock,
                &[
                    "pr",
                    "edit",
                    &number,
                    "--repo",
                    name,
                    "--base",
                    &base,
                    "--body-file",
                    body_arg,
                ],
            )?;
            let was_draft = pr["isDraft"].as_bool().ok_or("PR draft state missing")?;
            if was_draft != draft {
                let mut args = vec!["pr", "ready", &number, "--repo", name];
                if draft {
                    args.push("--undo");
                }
                gh(tree, lock, &args)?;
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
            gh(tree, lock, &args)?
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
/// Keep a report's per-user lease held if the controller dies during a host command.
pub fn inherit_report_lease(command: &mut Command, gate: &File) -> std::io::Result<()> {
    let gate = gate.try_clone()?;
    // SAFETY: the closure owns the descriptor, and only changes its flags
    // in this child. fcntl is async-signal-safe.
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(gate.as_raw_fd(), libc::F_SETFD, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(())
}

pub fn report_notes(
    task: &Task,
    policy: &RunPolicy,
    observer: &dyn CheckpointObserver,
    reporter: &mut impl FnMut(&super::ProblemNote, &Record, &File, bool) -> Result<super::ReportRecord>,
) -> Result<Record> {
    LockedTask::open(task, policy, false)?.report_notes(observer, reporter)
}

fn report_locked(
    locked: &LockedTask<'_>,
    observer: &dyn CheckpointObserver,
    reporter: &mut impl FnMut(&super::ProblemNote, &Record, &File, bool) -> Result<super::ReportRecord>,
) -> Result<Record> {
    let (mut writer, mut value) = action_context(locked, observer)?;
    let (policy, lock) = (locked.policy, &locked.lock);
    let prior_step = value.record.step;
    // Reporting is ancillary: a failed save/publication remains recoverable even
    // if reporting is interrupted between durable report receipts.
    let reporting_step = match prior_step {
        Step::Saving | Step::Publishing => prior_step,
        _ => Step::Reporting,
    };
    for index in 0..value.record.problem_notes.len() {
        if value
            .record
            .reports
            .get(index)
            .is_some_and(|r| !matches!(r.disposition.as_str(), "local" | "failed"))
        {
            continue;
        }
        writer.save(&mut value, reporting_step, observer)?;
        let report = reporter(
            &value.record.problem_notes[index],
            &value.record,
            lock,
            policy.offline || value.offline,
        )
        .unwrap_or_else(|detail| super::ReportRecord {
            disposition: "failed".into(),
            draft: PathBuf::new(),
            url: None,
            detail,
        });
        if index < value.record.reports.len() {
            value.record.reports[index] = report;
        } else {
            value.record.reports.push(report);
        }
        writer.save(&mut value, reporting_step, observer)?;
    }
    writer.save(
        &mut value,
        if matches!(prior_step, Step::Saving | Step::Publishing) {
            prior_step
        } else {
            Step::Finished
        },
        observer,
    )?;
    Ok(value.record)
}

impl LockedTask<'_> {
    pub fn publish(
        &mut self,
        repo: &Path,
        binary: &Path,
        gh_binary: &Path,
        observer: &dyn CheckpointObserver,
        confirm: &mut dyn FnMut(&str) -> Option<bool>,
    ) -> Result<Record> {
        publish_locked(repo, binary, self, gh_binary, observer, confirm)
    }

    pub fn report_notes(
        &mut self,
        observer: &dyn CheckpointObserver,
        reporter: &mut impl FnMut(&super::ProblemNote, &Record, &File, bool) -> Result<super::ReportRecord>,
    ) -> Result<Record> {
        report_locked(self, observer, reporter)
    }
}
