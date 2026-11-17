//! Opt-in, sequential unattended task execution. Worktrees are retained on every outcome.
use serde::{Deserialize, Serialize};
use std::fs;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

pub mod board;
mod checkpoint;
mod execute;
pub mod shadow;
mod state;
mod status;
pub use status::{status, Liveness, Status};
mod tree;
pub use state::{CheckpointKind, CheckpointObserver, Step};
mod worker;
mod worker_wait;
pub use worker::{WorkerInterruption, WorkerOutcome, WorkerResult};

pub type Result<T> = std::result::Result<T, String>;

// Lead facts consume public observations and never open runner task-state files.
pub mod events;
mod facts_io;
pub mod incidents;
pub mod situation;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Task {
    pub id: String,
    pub model: String,
    pub branch: String,
    #[serde(default = "default_base")]
    pub base: String,
    pub prompt: String,
    #[serde(default = "default_max_time")]
    pub max_time: u64,
}
fn default_max_time() -> u64 {
    45 * 60
}
fn default_base() -> String {
    "main".into()
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Running,
    Interrupted,
    Done,
    NothingToDo,
    Red,
    Blocked,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Record {
    pub id: String,
    pub branch: String,
    pub worktree: PathBuf,
    pub outcome: Outcome,
    pub detail: String,
    pub worker: Option<WorkerResult>,
    pub session_id: Option<String>,
    pub base_sha: Option<String>,
    pub checkpoint_revision: u64,
    pub checkpoint_kind: CheckpointKind,
    pub checkpoint_at_ms: u64,
    pub step: Step,
    pub head_sha: Option<String>,
    pub tree_sha: Option<String>,
    pub interruption: Option<WorkerInterruption>,
}

/// An immutable task/base/check snapshot, built before any task in its batch runs.
pub struct PlannedTask {
    task: Task,
    base_sha: String,
    commands: Vec<String>,
    checkpoint_revision: Option<u64>,
}
impl PlannedTask {
    pub fn task(&self) -> &Task {
        &self.task
    }
    pub fn base_sha(&self) -> &str {
        &self.base_sha
    }
}

pub fn plan_task(repo: &Path, task: Task, policy: &RunPolicy) -> Result<PlannedTask> {
    validate(&task)?;
    let branch = checked(git(policy, repo, &["check-ref-format", "--branch", &task.branch]))?;
    if branch != task.branch {
        return Err("branch aliases are not allowed; provide a literal branch name".into());
    }
    let base_sha = checked(git(
        policy,
        repo,
        &["rev-parse", "--verify", &format!("{}^{{commit}}", task.base)],
    ))?;
    let commands = done_commands(policy, repo, &base_sha)?;
    Ok(PlannedTask {
        task,
        base_sha,
        commands,
        checkpoint_revision: None,
    })
}

/// Recover immutable inputs before consulting today's mutable base/settings.
/// New attempts still freeze a fresh plan before any worker in the batch runs.
pub fn plan_task_for_run(repo: &Path, task: Task, policy: &RunPolicy, rerun: bool) -> Result<PlannedTask> {
    validate(&task)?;
    let path = policy.storage.join(format!("{}.json", task.id));
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return plan_task(repo, task, policy);
        }
        Err(error) => return Err(format!("task checkpoint: {error}")),
        Ok(metadata) if !metadata.is_file() => return Err("unsafe task checkpoint".into()),
        Ok(_) => {}
    }
    let saved = state::load(&path)?;
    if saved.task != task {
        return Err("task specification changed; use a new task id".into());
    }
    if saved.record.step != Step::Finished
        || (!rerun && matches!(saved.record.outcome, Outcome::Done | Outcome::NothingToDo))
    {
        let execution = saved.execution.ok_or("missing task execution checkpoint")?;
        if saved.record.base_sha.as_deref() != Some(&execution.base) {
            return Err("saved task base disagrees with execution checkpoint".into());
        }
        return Ok(PlannedTask {
            task,
            base_sha: execution.base,
            commands: execution.commands,
            checkpoint_revision: Some(saved.record.checkpoint_revision),
        });
    }
    plan_task(repo, task, policy)
}

fn output(mut command: Command) -> Result<Output> {
    command.stdin(Stdio::null()).output().map_err(|e| e.to_string())
}
fn checked(command: Command) -> Result<String> {
    let out = output(command)?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().into())
}
fn git(policy: &RunPolicy, repo: &Path, args: &[&str]) -> Command {
    metadata_git(&policy.git_binary, &policy.host_path, repo, args)
}

fn isolate_git_environment(command: &mut Command) {
    for (key, _) in std::env::vars_os() {
        if key.as_encoded_bytes().starts_with(b"GIT_") {
            command.env_remove(key);
        }
    }
    command.env("GIT_NO_REPLACE_OBJECTS", "1");
}

/// Host metadata and bootstrap commands never consult replace refs, invoke a
/// filesystem monitor, or fetch missing objects through a repository's helpers.
pub fn metadata_git(binary: &Path, path: &std::ffi::OsStr, repo: &Path, args: &[&str]) -> Command {
    let mut c = Command::new(binary);
    isolate_git_environment(&mut c);
    c.env("PATH", path)
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_ALLOW_PROTOCOL", "")
        .env("GIT_TERMINAL_PROMPT", "0")
        .current_dir(repo)
        .args([
            "--no-replace-objects",
            "--no-optional-locks",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "protocol.allow=never",
        ])
        .args(args);
    c
}

pub fn read_tasks(path: &Path) -> Result<Vec<Task>> {
    let tasks: Vec<Task> = serde_json::from_slice(&fs::read(path).map_err(|e| e.to_string())?)
        .map_err(|e| format!("task file: {e}"))?;
    if tasks.is_empty() {
        return Err("task file is empty".into());
    }
    let mut ids = std::collections::HashSet::new();
    let mut branches = std::collections::HashSet::new();
    for task in &tasks {
        validate(task)?;
        if !ids.insert(&task.id) || !branches.insert(&task.branch) {
            return Err("duplicate task id or branch".into());
        }
    }
    Ok(tasks)
}
fn valid_task_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
}
fn validate(task: &Task) -> Result<()> {
    if !valid_task_id(&task.id) {
        return Err("task id must contain 1..128 letters, digits, - or _".into());
    }
    let allowance =
        std::time::Duration::from_secs(task.max_time).checked_add(std::time::Duration::from_secs(70));
    if task.max_time == 0
        || allowance.is_none_or(|duration| std::time::Instant::now().checked_add(duration).is_none())
    {
        return Err("max_time must be a positive number of seconds that fits the runtime clock".into());
    }
    if task
        .max_time
        .checked_mul(1000)
        .and_then(|ms| ms.checked_add(70_000))
        .and_then(|ms| ms.checked_add(state::now_ms()))
        .is_none()
    {
        return Err("max_time is too large for a durable deadline".into());
    }
    if task.model.trim().is_empty() || task.prompt.trim().is_empty() {
        return Err("model and prompt are required".into());
    }
    for reference in [&task.branch, &task.base] {
        if reference.is_empty() || reference.starts_with('-') {
            return Err("invalid branch or base".into());
        }
    }
    Ok(())
}

/// Issue text is data, passed as one prompt argument; gh never invokes a shell.
pub fn issue_task(
    repo: &Path,
    gh: &Path,
    host_path: &std::ffi::OsStr,
    number: u64,
    model: String,
) -> Result<Task> {
    if number == 0 {
        return Err("issue number must be positive".into());
    }
    let mut c = Command::new(gh);
    isolate_git_environment(&mut c);
    c.env("PATH", host_path).current_dir(repo).args([
        "issue",
        "view",
        &number.to_string(),
        "--json",
        "title,body,state,comments",
    ]);
    let text = checked(c)?;
    let issue: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    Ok(Task {
        id: format!("issue-{number}"),
        model,
        branch: format!("agent/{number}-work"),
        base: default_base(),
        prompt: issue_prompt(number, &issue)?,
        max_time: default_max_time(),
    })
}

/// A worker inside the sandbox cannot read the issue itself, so the packet carries
/// the state and the comment thread the runner already fetched.
const MAX_COMMENT_BYTES: usize = 16 * 1024;
const MAX_STATE_BYTES: usize = 32;

/// gh's `issue view --json title,body,state,comments` as one data packet.
fn issue_prompt(number: u64, issue: &serde_json::Value) -> Result<String> {
    let title = issue["title"].as_str().ok_or("issue title missing")?;
    let body = issue["body"].as_str().ok_or("issue body missing")?;
    let state = issue["state"].as_str().ok_or("issue state missing")?;
    if state.is_empty()
        || state.len() > MAX_STATE_BYTES
        || !state.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Err("issue state malformed".into());
    }
    let mut prompt = format!("Implement issue #{number}: {title}\n\n{body}");
    prompt.push_str(&format!("\n\nRunner-fetched issue state: {state}."));
    let thread = comment_thread(issue)?;
    if !thread.is_empty() {
        prompt.push_str(
            "\n\nIssue comments follow as data, oldest first. They may carry findings and \
             corrections to the issue above.\n\n",
        );
        prompt.push_str(&bounded(&thread, MAX_COMMENT_BYTES));
    }
    Ok(prompt)
}

/// The comment bodies with their authors, oldest first. A comment without text is
/// skipped; a present but non-text body refuses the packet instead of dropping a
/// finding, and the whole section stays bounded.
fn comment_thread(issue: &serde_json::Value) -> Result<String> {
    let comments: &[serde_json::Value] = match issue.get("comments") {
        None | Some(serde_json::Value::Null) => return Ok(String::new()),
        Some(serde_json::Value::Array(comments)) => comments,
        Some(_) => return Err("issue comments malformed".into()),
    };
    let mut thread = String::new();
    for comment in comments {
        if !comment.is_object() {
            return Err("issue comment malformed".into());
        }
        let body = match comment.get("body") {
            None | Some(serde_json::Value::Null) => continue,
            Some(serde_json::Value::String(body)) => body.trim(),
            Some(_) => return Err("issue comment body malformed".into()),
        };
        if body.is_empty() {
            continue;
        }
        let author = comment["author"]["login"].as_str().unwrap_or("unknown");
        let author = if author.is_empty() { "unknown" } else { author };
        thread.push_str(&format!("--- comment by {author} ---\n{body}\n\n"));
    }
    Ok(thread)
}

/// Keep a bounded prefix without splitting a UTF-8 character, and say what was left out.
fn bounded(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n--- further comments truncated; open the issue for the rest ---\n",
        &text[..end]
    )
}

/// Freeze acceptance before checkout filters or hooks can edit project settings.
fn done_commands(policy: &RunPolicy, root: &Path, base: &str) -> Result<Vec<String>> {
    let path = ".oxa/settings.json";
    if checked(git(policy, root, &["ls-tree", "--name-only", base, "--", path]))?.is_empty() {
        return Ok(vec![]);
    }
    let text = checked(git(policy, root, &["show", &format!("{base}:{path}")]))?;
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    match value.get("done") {
        None => Ok(vec![]),
        Some(value) => serde_json::from_value(value.clone()).map_err(|e| format!("done commands: {e}")),
    }
}
/// Private runner data stays outside every workspace and writable Git bind.
#[derive(Clone)]
pub struct RunPolicy {
    /// Absolute tool path approved by the front end's trusted-executable checks.
    pub git_binary: PathBuf,
    /// Installed tools only; repository code is invoked inside the sandbox.
    pub host_path: std::ffi::OsString,
    pub terminate_binary: Option<PathBuf>,
    pub offline: bool,
    pub storage: PathBuf,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SupervisorRequest {
    pub source: PathBuf,
    pub operation: SupervisorOperation,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SupervisorOperation {
    Checkout { branch: String, create: bool },
    Merge { base: String },
    Save { task_id: String },
    Checks { commands: Vec<String> },
    Inspect,
}

fn supervise(
    binary: &Path,
    root: &Path,
    request: SupervisorRequest,
    policy: &RunPolicy,
    lock: &fs::File,
) -> Result<Output> {
    let mut command = Command::new(binary);
    isolate_git_environment(&mut command);
    if policy.offline {
        command.arg("--offline");
    }
    command
        .env("PATH", &policy.host_path)
        .current_dir(root)
        .args([
            "work",
            "--supervise",
            &serde_json::to_string(&request).map_err(|e| e.to_string())?,
        ])
        // The deterministic supervisor retains ownership if this runner dies.
        .stdin(lock.try_clone().map_err(|e| e.to_string())?)
        .output()
        .map_err(|e| format!("work supervisor: {e}"))
}

fn acceptance(
    binary: &Path,
    root: &Path,
    source: &Path,
    commands: &[String],
    policy: &RunPolicy,
    lock: &fs::File,
) -> Result<String> {
    if commands.is_empty() {
        return Ok(String::new());
    }
    let out = supervise(
        binary,
        root,
        SupervisorRequest {
            source: source.to_path_buf(),
            operation: SupervisorOperation::Checks {
                commands: commands.to_vec(),
            },
        },
        policy,
        lock,
    )?;
    let diagnostic = String::from_utf8_lossy(&out.stderr).into_owned();
    match out.status.code() {
        Some(0) => Ok(String::new()),
        Some(1) => Ok(diagnostic),
        _ => Err(format!("acceptance could not run: {diagnostic}")),
    }
}

fn dirty_worktree(
    binary: &Path,
    root: &Path,
    source: &Path,
    policy: &RunPolicy,
    lock: &fs::File,
) -> Result<bool> {
    let out = supervise(
        binary,
        root,
        SupervisorRequest {
            source: source.to_path_buf(),
            operation: SupervisorOperation::Inspect,
        },
        policy,
        lock,
    )?;
    match out.status.code() {
        Some(0) => Ok(false),
        Some(1) => Ok(true),
        _ => Err(format!(
            "worktree inspection could not run: {}",
            String::from_utf8_lossy(&out.stderr)
        )),
    }
}

#[derive(Debug)]
pub enum RunError {
    Blocked(String),
    Checkpoint(String),
}
impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Blocked(error) | Self::Checkpoint(error) => f.write_str(error),
        }
    }
}
impl std::error::Error for RunError {}
impl From<String> for RunError {
    fn from(value: String) -> Self {
        Self::Blocked(value)
    }
}
impl From<&str> for RunError {
    fn from(value: &str) -> Self {
        Self::Blocked(value.into())
    }
}

pub fn blocked_record(task: &Task, detail: String) -> Record {
    Record {
        id: task.id.clone(),
        branch: task.branch.clone(),
        worktree: PathBuf::new(),
        outcome: Outcome::Blocked,
        detail,
        worker: None,
        session_id: None,
        base_sha: None,
        checkpoint_revision: 0,
        checkpoint_kind: CheckpointKind::TaskTransition,
        checkpoint_at_ms: 0,
        step: Step::Finished,
        head_sha: None,
        tree_sha: None,
        interruption: None,
    }
}

/// Recover the bounded pending observation before opening a lead's facts view.
/// This never acquires a task lock, changes a task file or starts a worker.
pub fn recover_checkpoints(
    storage: &Path,
    observer: &dyn CheckpointObserver,
) -> std::result::Result<(), RunError> {
    match fs::metadata(storage) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(RunError::Checkpoint(error.to_string())),
        Ok(metadata) if !metadata.is_dir() => {
            return Err(RunError::Checkpoint("checkpoint root is not a directory".into()))
        }
        Ok(_) => {}
    }
    checkpoint::Repository::open(storage, observer)
        .map(|_| ())
        .map_err(RunError::Checkpoint)
}
pub fn run_task_with_policy(
    repo: &Path,
    binary: &Path,
    planned: &PlannedTask,
    policy: &RunPolicy,
) -> Result<Record> {
    run_task_with_observer(repo, binary, planned, policy, false, &|_: &Record| Ok(()))
        .map_err(|e| e.to_string())
}

/// Checkpoint/observer failures are fatal to the batch; ordinary task failures
/// remain per-task blocked records. No observer can read or write private State.
pub fn run_task_with_observer(
    repo: &Path,
    binary: &Path,
    planned: &PlannedTask,
    policy: &RunPolicy,
    rerun: bool,
    observer: &dyn CheckpointObserver,
) -> std::result::Result<Record, RunError> {
    let task = &planned.task;
    validate(task)?;
    let root = PathBuf::from(checked(git(policy, repo, &["rev-parse", "--show-toplevel"]))?);
    let common = PathBuf::from(checked(git(
        policy,
        &root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    ))?);
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&policy.storage)
        .map_err(|e| e.to_string())?;
    let lock_path = policy.storage.join(format!("{}.lock", task.id));
    let lock_file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .mode(0o600)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| format!("cannot lock task {}: {e}", task.id))?;
    lock_file
        .try_lock()
        .map_err(|e| format!("task {} is already running: {e}", task.id))?;
    // The inherited lock retains its inode; status never tries to acquire it.
    let receipt = policy.storage.join(format!("{}.json", task.id));
    let replacing = receipt.exists();
    if planned.checkpoint_revision.is_some() && !replacing {
        return Err("task checkpoint disappeared after planning; retry the task file".into());
    }
    let repo_dir = if common.file_name().is_some_and(|name| name == ".git") {
        common.parent().ok_or("git directory has no parent")?
    } else {
        &common
    };
    let parent = repo_dir
        .parent()
        .ok_or("repository has no parent for worktrees")?;
    let name = repo_dir.file_name().ok_or("repository has no directory name")?;
    let trees = parent.join(".oxa-worktrees").join(name);
    let tree = trees.join(&task.id);
    let fresh = |revision| {
        let mut record = blocked_record(task, String::new());
        record.worktree = tree.clone();
        record.base_sha = Some(planned.base_sha.clone());
        record.checkpoint_revision = revision;
        let mut state = state::State::new(task.clone(), record, policy.offline);
        state.execution = Some(state::Execution {
            initial: None,
            base: planned.base_sha.clone(),
            prompt: task.prompt.clone(),
            commands: planned.commands.clone(),
            worker_error: None,
            merge_error: None,
            failures: String::new(),
        });
        state
    };
    let mut writer = state::Writer::new(receipt.clone(), replacing);
    let mut state = if replacing {
        let old = state::load(&receipt)?;
        if planned
            .checkpoint_revision
            .is_some_and(|revision| revision != old.record.checkpoint_revision)
        {
            return Err("task checkpoint changed after planning; retry the task file".into());
        }
        if old.task != *task || old.record.worktree != tree {
            return Err("task specification changed; use a new task id".into());
        }
        writer.replay(&old, observer).map_err(RunError::Checkpoint)?;
        if old.record.step == Step::Finished {
            if !rerun && matches!(old.record.outcome, Outcome::Done | Outcome::NothingToDo) {
                return Ok(old.record);
            }
            let mut new = fresh(old.record.checkpoint_revision);
            new.offline |= old.offline;
            new
        } else {
            old
        }
    } else {
        fresh(0)
    };
    state.offline |= policy.offline;
    let policy = RunPolicy {
        offline: state.offline,
        ..policy.clone()
    };
    // Replaying the prior checkpoint precedes this first new checkpoint or action.
    let step = state.record.step;
    writer
        .save(&mut state, step, observer)
        .map_err(RunError::Checkpoint)?;
    match (execute::Runner {
        repo: &root,
        binary,
        policy: &policy,
        lock: &lock_file,
        writer: &mut writer,
        observer,
    })
    .drive(&mut state)
    {
        Ok(()) => {}
        Err(RunError::Checkpoint(error)) => return Err(RunError::Checkpoint(error)),
        Err(RunError::Blocked(error)) => {
            state.record.outcome = if state.record.outcome == Outcome::Interrupted
                || state
                    .record
                    .worker
                    .as_ref()
                    .is_some_and(|worker| matches!(worker.outcome, WorkerOutcome::Interrupted))
            {
                Outcome::Interrupted
            } else {
                Outcome::Blocked
            };
            state.record.detail = error;
        }
    }
    writer
        .save(&mut state, Step::Saving, observer)
        .map_err(RunError::Checkpoint)?;
    if let Err(error) = execute::save_work(&root, binary, task, &policy, &lock_file, &mut state.record) {
        if state.record.outcome != Outcome::Interrupted {
            state.record.outcome = Outcome::Blocked;
        }
        if !state.record.detail.is_empty() {
            state.record.detail.push('\n');
        }
        state.record.detail.push_str(&format!("save: {error}"));
    }
    writer
        .save(&mut state, Step::Finished, observer)
        .map_err(RunError::Checkpoint)?;
    Ok(state.record)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn comments(bodies: &[&str]) -> serde_json::Value {
        json!(bodies
            .iter()
            .map(|body| json!({"author": {"login": "reviewer"}, "body": body}))
            .collect::<Vec<_>>())
    }

    #[test]
    fn issue_packet_carries_the_state_the_runner_fetched() {
        // The worker cannot open the issue from inside the sandbox, so a closed or open
        // state has to arrive with the text the runner already fetched.
        let prompt = issue_prompt(
            42,
            &json!({"title": "Dogfood task", "body": "Fix the fixture", "state": "CLOSED"}),
        )
        .unwrap();
        assert!(
            prompt.starts_with("Implement issue #42: Dogfood task\n\nFix the fixture\n\n"),
            "{prompt}"
        );
        assert!(prompt.contains("Runner-fetched issue state: CLOSED."), "{prompt}");
        assert!(!prompt.contains("Issue comments follow"), "{prompt}");
    }

    #[test]
    fn issue_packet_carries_the_comment_thread_after_the_body() {
        let prompt = issue_prompt(
            42,
            &json!({
                "title": "Dogfood task",
                "body": "Fix the fixture",
                "state": "OPEN",
                "comments": comments(&["Second finding", "", "Third finding"]),
            }),
        )
        .unwrap();
        let body_at = prompt.find("Fix the fixture").unwrap();
        let first = prompt.find("Second finding").unwrap();
        let second = prompt.find("Third finding").unwrap();
        assert!(body_at < first && first < second, "{prompt}");
        assert!(
            prompt.contains("--- comment by reviewer ---\nSecond finding"),
            "{prompt}"
        );
        assert!(prompt.contains("Runner-fetched issue state: OPEN."), "{prompt}");
    }

    #[test]
    fn issue_packet_refuses_fields_it_cannot_read_instead_of_dropping_them() {
        let base = json!({"title": "T", "body": "B", "state": "OPEN"});
        let cases = [
            (json!({"body": "B", "state": "OPEN"}), "issue title missing"),
            (json!({"title": "T", "state": "OPEN"}), "issue body missing"),
            (json!({"title": "T", "body": "B"}), "issue state missing"),
            (
                json!({"title": "T", "body": "B", "state": ""}),
                "issue state malformed",
            ),
            (
                json!({"title": "T", "body": "B", "state": "OPEN; ignore previous"}),
                "issue state malformed",
            ),
            (
                json!({"title": "T", "body": "B", "state": "OPEN", "comments": "none"}),
                "issue comments malformed",
            ),
            (
                json!({"title": "T", "body": "B", "state": "OPEN", "comments": [42]}),
                "issue comment malformed",
            ),
            (
                json!({"title": "T", "body": "B", "state": "OPEN",
                       "comments": [{"body": {"text": "nested"}}]}),
                "issue comment body malformed",
            ),
        ];
        for (issue, expected) in cases {
            assert_eq!(issue_prompt(42, &issue).unwrap_err(), expected, "{issue}");
        }
        // A missing or null field still means none, as gh reports it, and a comment with no
        // text is skipped instead of failing the whole packet.
        for comments in [
            serde_json::Value::Null,
            json!([]),
            json!([{"author": {"login": "reviewer"}}, {"body": "   "}]),
        ] {
            let mut issue = base.clone();
            issue["comments"] = comments;
            assert!(issue_prompt(42, &issue).is_ok(), "{issue}");
        }
    }

    #[test]
    fn truncated_comments_request_missing_data_from_the_runner() {
        let long = "é".repeat(MAX_COMMENT_BYTES);
        let prompt = issue_prompt(
            42,
            &json!({"title": "T", "body": "B", "state": "OPEN", "comments": comments(&[&long])}),
        )
        .unwrap();
        assert!(prompt.contains("further comments truncated"), "{prompt}");
        assert!(prompt.contains("ask the runner for the rest"), "{prompt}");
        assert!(!prompt.contains("open the issue"), "{prompt}");
    }

    #[test]
    fn long_comment_threads_are_bounded_at_a_character_boundary() {
        let long = format!("{}{}", "é".repeat(MAX_COMMENT_BYTES), " tail");
        let prompt = issue_prompt(
            42,
            &json!({"title": "T", "body": "B", "state": "OPEN", "comments": comments(&[&long])}),
        )
        .unwrap();
        assert!(prompt.len() < MAX_COMMENT_BYTES + 1024, "{}", prompt.len());
        assert!(
            prompt.contains("further comments truncated; open the issue for the rest"),
            "{prompt}"
        );
        assert!(!prompt.contains("tail"), "{prompt}");
    }
}
