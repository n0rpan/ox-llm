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

/// One part of the commits a publication would push: the name of a file that a
/// commit adds (`line` is `None`, `text` is empty) or one added line.
pub struct PushedContent<'a> {
    pub path: &'a str,
    pub line: Option<usize>,
    pub text: &'a str,
}

/// Why pushed content must not be published. `label` never contains the content;
/// `secret_path` withholds a path that itself contains secret text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContentFinding {
    pub label: &'static str,
    pub secret_path: bool,
}

/// Classifies pushed content before publication.
pub type ContentScan<'a> = &'a dyn Fn(&PushedContent<'_>) -> Option<ContentFinding>;

fn diff_path(header: &str) -> Option<String> {
    let path = header.trim_end();
    if path == "/dev/null" {
        return None;
    }
    let path = path
        .strip_prefix('"')
        .and_then(|quoted| quoted.strip_suffix('"'))
        .unwrap_or(path);
    Some(path.strip_prefix("b/").unwrap_or(path).to_string())
}

fn refusal(path: &str, line: Option<usize>, finding: ContentFinding) -> String {
    let path: String = if finding.secret_path {
        "a file whose path".into()
    } else {
        path.chars()
            .map(|ch| if ch.is_control() { '?' } else { ch })
            .take(200)
            .collect()
    };
    let at = line.map_or(String::new(), |line| format!(":{line}"));
    format!(
        "publishing refused: {path}{at} adds {}; remove it from the task branch history before publishing",
        finding.label
    )
}

/// The commits a push can add: reachable from `head` and from none of `known`.
fn pushed_revisions<'a>(head: &'a str, known: &'a [String]) -> Vec<&'a str> {
    let mut revisions = vec!["--ignore-missing", head, "--not"];
    revisions.extend(known.iter().map(String::as_str));
    revisions
}

/// The branch and tag tips the publication destination itself advertises. Local
/// remote-tracking refs are writable by the task and may describe a different
/// fetch URL, so only the destination's own answer may exclude history.
fn destination_tips(policy: &RunPolicy, repo: &Path, url: &str) -> Result<Vec<String>> {
    if url.starts_with('-') {
        return Err("publishing refused: invalid publication destination".into());
    }
    let mut command = super::git(
        policy,
        repo,
        &[
            "-c",
            "protocol.https.allow=always",
            "-c",
            "protocol.ssh.allow=always",
            "-c",
            "protocol.file.allow=always",
            "ls-remote",
            "--heads",
            "--tags",
            "--refs",
            url,
        ],
    );
    isolate_github_environment(&mut command);
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GH_PROMPT_DISABLED", "1")
        .env("GIT_ALLOW_PROTOCOL", "https:ssh:file");
    let text = checked(command)
        .map_err(|_| "publishing refused: cannot read the publication destination's branches".to_string())?;
    let mut tips: Vec<String> = text
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .map(|(oid, _)| oid)
        .filter(|oid| matches!(oid.len(), 40 | 64) && oid.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .map(str::to_string)
        .collect();
    tips.sort();
    tips.dedup();
    Ok(tips)
}

/// Scan the commits `revisions` select: each commit's metadata, its own changes and
/// the lines a merge commit introduces itself. Any failure refuses publication.
fn scan_pushed(policy: &RunPolicy, tree: &Path, revisions: &[&str], scan: ContentScan<'_>) -> Result<()> {
    let unreadable = |_: String| "publishing refused: cannot scan the commits to publish".to_string();
    let log = |options: &[&str]| {
        let mut args = vec!["-c", "core.quotePath=false", "log"];
        args.extend_from_slice(options);
        args.extend_from_slice(revisions);
        args.push("--");
        checked(super::git(policy, tree, &args)).map_err(unreadable)
    };
    // Messages and identities are pushed with the commits.
    let metadata = log(&["--format=%x00%H%n%an%n%ae%n%cn%n%ce%n%B", "--no-show-signature"])?;
    scan_metadata(&metadata, scan)?;
    let text = log(&[
        "--reverse",
        "--format=%x00%H",
        "--patch",
        "--cc",
        "--text",
        "--unified=0",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "--no-renames",
        "--no-relative",
        "--no-show-signature",
        "--src-prefix=a/",
        "--dst-prefix=b/",
    ])?;
    scan_patch(&text, scan)?;
    // An empty added file has no content header in the patch, so the names of all
    // added files are listed separately and checked as well.
    let names = log(&[
        "--format=",
        "--name-only",
        "-z",
        "--diff-filter=A",
        "--cc",
        "--no-renames",
        "--no-relative",
    ])?;
    for name in names.split('\0').map(str::trim).filter(|name| !name.is_empty()) {
        let item = PushedContent {
            path: name,
            line: None,
            text: "",
        };
        if let Some(finding) = scan(&item) {
            return Err(refusal(name, None, finding));
        }
    }
    Ok(())
}

/// Pass each line of every commit's author, committer and message to `scan`.
fn scan_metadata(text: &str, scan: ContentScan<'_>) -> Result<()> {
    for record in text.split('\0').filter(|record| !record.trim().is_empty()) {
        let mut lines = record.lines();
        let commit = lines.next().unwrap_or_default();
        let place = format!("commit {}", commit.get(..12).unwrap_or(commit));
        for (index, text) in lines.enumerate() {
            let item = PushedContent {
                path: &place,
                line: Some(index + 1),
                text,
            };
            if let Some(finding) = scan(&item) {
                return Err(refusal(&place, Some(index + 1), finding));
            }
        }
    }
    Ok(())
}

/// Pass every added file name and added line of `git log --patch --cc -U0` output to `scan`.
fn scan_patch(text: &str, scan: ContentScan<'_>) -> Result<()> {
    let (mut path, mut added, mut hunk, mut parents, mut line) = (None::<String>, false, false, 1, 0);
    for row in text.lines() {
        if row.starts_with('\0') || row.starts_with("diff --") {
            parents = if row.starts_with("diff --cc ") || row.starts_with("diff --combined ") {
                2
            } else {
                1
            };
            (path, added, hunk) = (None, false, false);
            continue;
        }
        if row.starts_with("@@") {
            let marks = row.bytes().take_while(|byte| *byte == b'@').count();
            parents = marks.saturating_sub(1).max(1);
            line = row[marks..]
                .split_ascii_whitespace()
                .find_map(|range| range.strip_prefix('+'))
                .and_then(|range| range.split(',').next())
                .and_then(|start| start.parse().ok())
                .ok_or("publishing refused: cannot scan the commits to publish")?;
            hunk = true;
            continue;
        }
        if !hunk {
            if row.starts_with("new file mode ") {
                added = true;
            } else if let Some(header) = row.strip_prefix("+++ ") {
                path = diff_path(header);
                if let Some(name) = path.as_deref().filter(|_| added) {
                    let item = PushedContent {
                        path: name,
                        line: None,
                        text: "",
                    };
                    if let Some(finding) = scan(&item) {
                        return Err(refusal(name, None, finding));
                    }
                }
            }
            continue;
        }
        let Some(name) = path.as_deref() else {
            continue;
        };
        let Some((marks, rest)) = row.split_at_checked(parents) else {
            continue;
        };
        if marks.contains('-') || row.starts_with('\\') {
            continue;
        }
        if marks.contains('+') {
            let item = PushedContent {
                path: name,
                line: Some(line),
                text: rest,
            };
            if let Some(finding) = scan(&item) {
                return Err(refusal(name, Some(line), finding));
            }
        }
        line += 1;
    }
    Ok(())
}

fn isolate_github_environment(command: &mut Command) {
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

fn gh(binary: &Path, policy: &RunPolicy, repo: &Path, lock: &File, args: &[&str]) -> Result<String> {
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
struct Repository {
    qualified: String,
    host: String,
    owner: String,
    name: String,
}
fn repository(value: &serde_json::Value) -> Result<Repository> {
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
const BODY_BEGIN: &str = "<!-- oxa-work-acceptance -->";
const BODY_END: &str = "<!-- /oxa-work-acceptance -->";
fn body(task: &Task, head: &str, existing: Option<&str>) -> Result<String> {
    let section = format!("{BODY_BEGIN}\nAccepted commit: `{head}`\nLocal acceptance: passed\nCI: not yet qualified for this commit\n{BODY_END}");
    let default = format!("T2 — task `{}` executed by `oxa work`.\n\nWhat/why: explicitly requested task; review its changes against the base.\n\nChanges: accepted worktree retained.\n\nRemoves: inspect the diff before promotion; no deletion clearance is inferred.\n\nValidation: frozen local acceptance passed; CI qualification is separate.\n", task.id);
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
    scan: ContentScan<'_>,
    confirm: &mut dyn FnMut(&str) -> Option<bool>,
) -> Result<Record> {
    LockedTask::open(task, policy, false)?.publish(repo, binary, gh_binary, observer, scan, confirm)
}

fn publish_locked(
    repo: &Path,
    binary: &Path,
    locked: &LockedTask<'_>,
    gh_binary: &Path,
    observer: &dyn CheckpointObserver,
    scan: ContentScan<'_>,
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
        // The owner confirmation shows only metadata, so content is checked here,
        // before any remote request, against what the local origin refs record.
        // The destination's own tips are checked again after confirmation.
        scan_pushed(policy, tree, &[head.as_str(), "--not", "--remotes=origin"], scan)?;
        let remote = checked(git(repo, &["remote", "get-url", "--push", "--all", "origin"]))?;
        if remote.lines().count() != 1 {
            return Err("publishing requires one origin push URL".into());
        }
        let repository = repository(&json(&gh(
            tree,
            lock,
            &["repo", "view", &remote, "--json", "nameWithOwner,url"],
        )?)?)?;
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
        let text = body(task, &head, existing_body.as_deref())?;
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
        // Every commit the destination lacks is pushed, so the exclusions come from
        // the push URL's own advertised tips rather than from local refs.
        let known = destination_tips(policy, repo, &remote)?;
        scan_pushed(policy, tree, &pushed_revisions(&head, &known), scan)?;
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
        let mut push = git(
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
                &format!("{head}:refs/heads/{}", task.branch),
            ],
        );
        isolate_github_environment(&mut push);
        push.env("GIT_TERMINAL_PROMPT", "0")
            .env("GH_PROMPT_DISABLED", "1")
            // Metadata probes deny every transport. Only this explicit,
            // confirmed publication may use the normal GitHub/file transports.
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
        scan: ContentScan<'_>,
        confirm: &mut dyn FnMut(&str) -> Option<bool>,
    ) -> Result<Record> {
        publish_locked(repo, binary, self, gh_binary, observer, scan, confirm)
    }

    pub fn report_notes(
        &mut self,
        observer: &dyn CheckpointObserver,
        reporter: &mut impl FnMut(&super::ProblemNote, &Record, &File, bool) -> Result<super::ReportRecord>,
    ) -> Result<Record> {
        report_locked(self, observer, reporter)
    }
}

#[cfg(test)]
mod scan_tests {
    use super::*;

    fn items(text: &str) -> Vec<(String, Option<usize>, String)> {
        let seen = std::cell::RefCell::new(vec![]);
        scan_patch(text, &|item| {
            seen.borrow_mut()
                .push((item.path.to_string(), item.line, item.text.to_string()));
            None
        })
        .unwrap();
        seen.into_inner()
    }

    #[test]
    fn added_names_and_lines_are_scanned_in_ordinary_and_merge_commits() {
        let patch = "\0aaaa\n\ndiff --git a/new.txt b/new.txt\nnew file mode 100644\nindex 0000000..1111111\n--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1,2 @@\n+first\n+++ looks like a header\ndiff --git a/old.txt b/old.txt\nindex 1111111..2222222 100644\n--- a/old.txt\n+++ b/old.txt\n@@ -3 +3 @@\n-before\n+after\n\0bbbb\n\ndiff --cc merged.txt\nindex 1111111,2222222..3333333\n--- a/merged.txt\n+++ b/merged.txt\n@@@ -4,1 -4,1 +4,2 @@@\n- ours\n -theirs\n++introduced by the merge\n+ kept from the first parent\n";
        let seen = items(patch);
        let expected = [
            ("new.txt", None, ""),
            ("new.txt", Some(1), "first"),
            ("new.txt", Some(2), "++ looks like a header"),
            ("old.txt", Some(3), "after"),
            ("merged.txt", Some(4), "introduced by the merge"),
            ("merged.txt", Some(5), "kept from the first parent"),
        ];
        let expected: Vec<_> = expected
            .iter()
            .map(|(p, l, t)| (p.to_string(), *l, t.to_string()))
            .collect();
        assert_eq!(seen, expected);
    }

    #[test]
    fn a_secret_path_is_withheld_from_the_refusal() {
        let patch =
            "\0aaaa\n\ndiff --git a/hidden b/hidden\nnew file mode 100644\n--- /dev/null\n+++ b/hidden\n";
        let error = scan_patch(patch, &|_| {
            Some(ContentFinding {
                label: "a known access token format",
                secret_path: true,
            })
        })
        .unwrap_err();
        assert!(!error.contains("hidden"), "{error}");
    }
}
