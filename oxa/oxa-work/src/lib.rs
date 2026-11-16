//! Opt-in, sequential unattended task execution. Worktrees are retained on every outcome.
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

mod tree;
mod worker;
pub use worker::{WorkerOutcome, WorkerResult};

pub type Result<T> = std::result::Result<T, String>;

// Lead facts consume public observations and never open runner task-state files.
pub mod events;
mod facts_io;
pub mod incidents;
pub mod situation;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    pub id: String,
    pub model: String,
    pub branch: String,
    #[serde(default = "default_base")]
    pub base: String,
    pub prompt: String,
}
fn default_base() -> String {
    "main".into()
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Done,
    NothingToDo,
    Red,
    Blocked,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Record {
    pub id: String,
    pub branch: String,
    pub worktree: PathBuf,
    pub outcome: Outcome,
    pub detail: String,
    pub worker: Option<WorkerResult>,
    pub session_id: Option<String>,
    pub base_sha: Option<String>,
}

/// An immutable task/base/check snapshot, built before any task in its batch runs.
pub struct PlannedTask {
    task: Task,
    base_sha: String,
    commands: Vec<String>,
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
    })
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
fn validate(task: &Task) -> Result<()> {
    if task.id.is_empty()
        || !task
            .id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
    {
        return Err("task id must contain only letters, digits, - or _".into());
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
        "title,body",
    ]);
    let text = checked(c)?;
    let issue: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let title = issue["title"].as_str().ok_or("issue title missing")?;
    let body = issue["body"].as_str().ok_or("issue body missing")?;
    Ok(Task {
        id: format!("issue-{number}"),
        model,
        branch: format!("agent/{number}-work"),
        base: default_base(),
        prompt: format!("Implement issue #{number}: {title}\n\n{body}"),
    })
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
pub struct RunPolicy {
    /// Absolute tool path approved by the front end's trusted-executable checks.
    pub git_binary: PathBuf,
    /// Installed tools only; repository code is invoked inside the sandbox.
    pub host_path: std::ffi::OsString,
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

fn worker_and_checks(
    worker: &Result<worker::Worker>,
    merge: Result<()>,
    checks: Result<String>,
) -> Result<String> {
    let mut errors = Vec::new();
    match worker {
        Ok(worker) => errors.extend(worker.failure()),
        Err(error) => errors.push(format!("worker: {error}")),
    }
    if let Err(error) = merge {
        errors.push(format!("merge verification: {error}"));
    }
    let failures = match checks {
        Ok(failures) => failures,
        Err(error) => {
            errors.push(error);
            String::new()
        }
    };
    if errors.is_empty() {
        return Ok(failures);
    }
    if !failures.is_empty() {
        errors.push(format!("Acceptance checks:\n{failures}"));
    }
    Err(errors.join("\n"))
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
    }
}
pub fn run_task_with_policy(
    repo: &Path,
    binary: &Path,
    planned: &PlannedTask,
    policy: &RunPolicy,
) -> Result<Record> {
    let task = &planned.task;
    validate(task)?;
    let root = checked(git(policy, repo, &["rev-parse", "--show-toplevel"]))?;
    let root = PathBuf::from(root);
    let common = checked(git(
        policy,
        &root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    ))?;
    let common = PathBuf::from(common);
    let storage = &policy.storage;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(storage)
        .map_err(|e| e.to_string())?;
    let lock_path = storage.join(format!("{}.lock", task.id));
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
    // Keep the inode so a crashed runner leaves no stale ownership and concurrent
    // runners cannot acquire locks on two different files with the same name.
    let receipt = storage.join(format!("{}.json", task.id));
    let replacing = receipt.exists();
    // The common git directory must not be an ancestor of the child cwd: the sandbox
    // deliberately refuses writable git binds with that layout.
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
    fs::create_dir_all(&trees).map_err(|e| e.to_string())?;
    let tree = trees.join(&task.id);
    tree::check_receipt(&receipt, &tree, task)?;
    let mut record = Record {
        id: task.id.clone(),
        branch: task.branch.clone(),
        worktree: tree.clone(),
        outcome: Outcome::Blocked,
        detail: String::new(),
        worker: None,
        session_id: None,
        base_sha: Some(planned.base_sha.clone()),
    };
    let result = (|| -> Result<Outcome> {
        let base = &planned.base_sha;
        let commands = &planned.commands;
        let prepared = tree::prepare(&root, &tree, task, base, binary, policy, &lock_file)?;
        let tree = tree.canonicalize().map_err(|e| e.to_string())?;
        let first = worker::run(binary, &tree, task, &prepared.prompt, policy, &lock_file, None);
        if let Ok(worker) = &first {
            record.worker = Some(worker.result.clone());
            record.session_id = worker.session.clone();
        }
        let failures = worker_and_checks(
            &first,
            tree::ensure_merged(policy, &tree, base, &task.branch),
            acceptance(binary, &tree, &root, commands, policy, &lock_file),
        )?;
        let first = first?;
        if !failures.is_empty() {
            let repair = worker::run(
                binary,
                &tree,
                task,
                &format!("External acceptance checks failed. Repair once:\n{failures}"),
                policy,
                &lock_file,
                first.session.as_deref(),
            );
            if let Ok(worker) = &repair {
                record.worker = Some(worker.result.clone());
                record.session_id = worker.session.clone();
            }
            let failures = worker_and_checks(
                &repair,
                tree::ensure_merged(policy, &tree, base, &task.branch),
                acceptance(binary, &tree, &root, commands, policy, &lock_file),
            )?;
            if !failures.is_empty() {
                record.detail = failures;
                return Ok(Outcome::Red);
            }
        }
        let head = checked(git(policy, &tree, &["rev-parse", "HEAD"]))?;
        let dirty = dirty_worktree(binary, &tree, &root, policy, &lock_file)?;
        Ok(if head == prepared.initial && !dirty {
            Outcome::NothingToDo
        } else {
            Outcome::Done
        })
    })();
    match result {
        Ok(outcome) => record.outcome = outcome,
        Err(e) => record.detail = e,
    }
    save_receipt(&receipt, &record, replacing)?;
    Ok(record)
}

fn save_receipt(path: &Path, record: &Record, replacing: bool) -> Result<()> {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = path.with_extension(format!("tmp-{}-{nonce}", std::process::id()));
    let result = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|e| e.to_string())?;
        file.write_all(&serde_json::to_vec_pretty(record).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        if replacing {
            // Only the task's already validated receipt is replaced under its lock.
            fs::rename(&temporary, path).map_err(|e| e.to_string())?;
        } else {
            fs::hard_link(&temporary, path).map_err(|e| e.to_string())?;
            fs::remove_file(&temporary).map_err(|e| e.to_string())?;
        }
        fs::File::open(path.parent().ok_or("receipt has no parent")?)
            .and_then(|parent| parent.sync_all())
            .map_err(|e| e.to_string())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}
