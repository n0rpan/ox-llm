//! Opt-in, sequential unattended task execution. Worktrees are retained on every outcome.
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

mod worker;
pub use worker::{WorkerOutcome, WorkerResult};

pub type Result<T> = std::result::Result<T, String>;

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

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Done,
    NothingToDo,
    Red,
    Blocked,
}

#[derive(Debug, Serialize)]
pub struct Record {
    pub id: String,
    pub branch: String,
    pub worktree: PathBuf,
    pub outcome: Outcome,
    pub detail: String,
    pub worker: Option<WorkerResult>,
    pub session_id: Option<String>,
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
fn git(repo: &Path, args: &[&str]) -> Command {
    let mut c = Command::new("git");
    c.current_dir(repo).args(args);
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
pub fn issue_task(repo: &Path, number: u64, model: String) -> Result<Task> {
    if number == 0 {
        return Err("issue number must be positive".into());
    }
    let mut c = Command::new("gh");
    c.current_dir(repo)
        .args(["issue", "view", &number.to_string(), "--json", "title,body"]);
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

/// Acceptance commands are frozen from the base before the agent can edit settings.
fn done_commands(root: &Path) -> Result<Vec<String>> {
    let path = root.join(".oxa/settings.json");
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e.to_string()),
    };
    let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    match value.get("done") {
        None => Ok(vec![]),
        Some(value) => serde_json::from_value(value.clone()).map_err(|e| format!("done commands: {e}")),
    }
}
/// Private runner data stays outside every workspace and writable Git bind.
pub struct RunPolicy {
    pub offline: bool,
    pub storage: PathBuf,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceRequest {
    pub source: PathBuf,
    pub commands: Vec<String>,
}

fn acceptance(
    binary: &Path,
    root: &Path,
    source: &Path,
    commands: &[String],
    offline: bool,
    lock: &fs::File,
) -> Result<String> {
    if commands.is_empty() {
        return Ok(String::new());
    }
    let request = AcceptanceRequest {
        source: source.to_path_buf(),
        commands: commands.to_vec(),
    };
    let mut command = Command::new(binary);
    if offline {
        command.arg("--offline");
    }
    let out = command
        .current_dir(root)
        .args([
            "work",
            "--acceptance",
            &serde_json::to_string(&request).map_err(|e| e.to_string())?,
        ])
        // The deterministic supervisor retains ownership if this runner dies.
        .stdin(lock.try_clone().map_err(|e| e.to_string())?)
        .output()
        .map_err(|e| format!("acceptance supervisor: {e}"))?;
    let diagnostic = String::from_utf8_lossy(&out.stderr).into_owned();
    match out.status.code() {
        Some(0) => Ok(String::new()),
        Some(1) => Ok(diagnostic),
        _ => Err(format!("acceptance could not run: {diagnostic}")),
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
    }
}
pub fn run_task_with_policy(repo: &Path, binary: &Path, task: &Task, policy: &RunPolicy) -> Result<Record> {
    validate(task)?;
    let root = checked(git(repo, &["rev-parse", "--show-toplevel"]))?;
    let root = PathBuf::from(root);
    checked(git(&root, &["check-ref-format", "--branch", &task.branch]))?;
    let common = checked(git(
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
    if receipt.exists() {
        return Err("task already has a receipt; retained for inspection".into());
    }
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
    let mut record = Record {
        id: task.id.clone(),
        branch: task.branch.clone(),
        worktree: tree.clone(),
        outcome: Outcome::Blocked,
        detail: String::new(),
        worker: None,
        session_id: None,
    };
    let result = (|| -> Result<Outcome> {
        if tree.exists() {
            return Err("task worktree already exists; retained for inspection".into());
        }
        let tree_arg = tree.to_str().ok_or("non-UTF8 worktree path")?;
        checked(git(
            &root,
            &["worktree", "add", "-b", &task.branch, tree_arg, &task.base],
        ))?;
        let tree = tree.canonicalize().map_err(|e| e.to_string())?;
        let initial = checked(git(&tree, &["rev-parse", "HEAD"]))?;
        let commands = done_commands(&tree)?;
        let first = worker::run(
            binary,
            &tree,
            task,
            &task.prompt,
            policy.offline,
            &lock_file,
            None,
        )?;
        record.worker = Some(first.result.clone());
        record.session_id = first.session.clone();
        let failures = acceptance(binary, &tree, &root, &commands, policy.offline, &lock_file)?;
        if let Some(error) = first.failure() {
            return Err(format!("{error}\nAcceptance checks:\n{failures}"));
        }
        if !failures.is_empty() {
            let repair = worker::run(
                binary,
                &tree,
                task,
                &format!("External acceptance checks failed. Repair once:\n{failures}"),
                policy.offline,
                &lock_file,
                first.session.as_deref(),
            )?;
            record.worker = Some(repair.result.clone());
            record.session_id = repair.session.clone();
            let failures = acceptance(binary, &tree, &root, &commands, policy.offline, &lock_file)?;
            if let Some(error) = repair.failure() {
                return Err(format!("{error}\nAcceptance checks:\n{failures}"));
            }
            if !failures.is_empty() {
                record.detail = failures;
                return Ok(Outcome::Red);
            }
        }
        let head = checked(git(&tree, &["rev-parse", "HEAD"]))?;
        let dirty = checked(git(&tree, &["status", "--porcelain"]))?;
        Ok(if head == initial && dirty.is_empty() {
            Outcome::NothingToDo
        } else {
            Outcome::Done
        })
    })();
    match result {
        Ok(outcome) => record.outcome = outcome,
        Err(e) => record.detail = e,
    }
    // Separate receipts avoid overwriting another task's result. Never remove its worktree.
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(receipt)
        .map_err(|e| e.to_string())?;
    file.write_all(&serde_json::to_vec_pretty(&record).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    Ok(record)
}
