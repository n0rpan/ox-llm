//! Opt-in, sequential unattended task execution. Worktrees are retained on every outcome.
mod tree;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

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

fn acceptance(root: &Path, commands: &[String]) -> Result<String> {
    let mut failures = String::new();
    for entry in commands {
        let (dir, shell) = match entry.strip_prefix("in ").and_then(|s| s.split_once(':')) {
            Some((dir, cmd)) => (root.join(dir.trim()), cmd.trim()),
            None => (root.to_path_buf(), entry.as_str()),
        };
        let dir = dir.canonicalize().map_err(|e| e.to_string())?;
        if !dir.starts_with(root) {
            return Err("done directory escapes worktree".into());
        }
        let mut c = Command::new("sh");
        c.current_dir(dir).args(["-c", shell]);
        let out = output(c)?;
        if !out.status.success() {
            failures.push_str(&format!(
                "{entry}: {}\n{}\n{}\n",
                out.status,
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ));
        }
    }
    Ok(failures)
}
fn agent(binary: &Path, root: &Path, task: &Task, prompt: &str, offline: bool) -> Result<()> {
    let mut c = Command::new(binary);
    c.current_dir(root)
        .args(["-p", "--model", &task.model, "--no-session"]);
    if offline {
        c.arg("--offline");
    }
    c.args(["--", prompt]);
    checked(c).map(|_| ())
}

pub fn blocked_record(task: &Task, detail: String) -> Record {
    Record {
        id: task.id.clone(),
        branch: task.branch.clone(),
        worktree: PathBuf::new(),
        outcome: Outcome::Blocked,
        detail,
    }
}
pub fn run_task(repo: &Path, binary: &Path, task: &Task) -> Result<Record> {
    run_task_with_policy(repo, binary, task, false)
}
pub fn run_task_with_policy(repo: &Path, binary: &Path, task: &Task, offline: bool) -> Result<Record> {
    validate(task)?;
    let root = checked(git(repo, &["rev-parse", "--show-toplevel"]))?;
    let root = PathBuf::from(root);
    checked(git(&root, &["check-ref-format", "--branch", &task.branch]))?;
    let common = checked(git(
        &root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    ))?;
    let common = PathBuf::from(common);
    let storage = common.join("oxa-work");
    fs::create_dir_all(&storage).map_err(|e| e.to_string())?;
    let lock_path = storage.join(format!("{}.lock", task.id));
    let lock_file = fs::OpenOptions::new()
        .write(true)
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
    };
    let result = (|| -> Result<Outcome> {
        let prepared = tree::prepare(&root, &tree, task)?;
        let tree = tree.canonicalize().map_err(|e| e.to_string())?;
        let commands = tree::done_from_base(&tree, &prepared.base)?;
        let first_agent = agent(binary, &tree, task, &prepared.prompt, offline);
        tree::ensure_merged(&tree, &prepared.base, &task.branch)?;
        let failures = acceptance(&tree, &commands)?;
        if !failures.is_empty() {
            let repair_agent = agent(
                binary,
                &tree,
                task,
                &format!(
                    "{}\n\nExternal acceptance checks failed. Repair once:\n{failures}",
                    prepared.prompt
                ),
                offline,
            );
            tree::ensure_merged(&tree, &prepared.base, &task.branch)?;
            let failures = acceptance(&tree, &commands)?;
            if !failures.is_empty() {
                if let Err(error) = repair_agent {
                    return Err(format!("{error}\nAcceptance checks:\n{failures}"));
                }
                record.detail = failures;
                return Ok(Outcome::Red);
            }
            repair_agent?;
        } else {
            first_agent?;
        }
        let head = checked(git(&tree, &["rev-parse", "HEAD"]))?;
        let dirty = checked(git(&tree, &["status", "--porcelain"]))?;
        Ok(if head == prepared.initial && dirty.is_empty() {
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
    fs::write(
        receipt,
        serde_json::to_vec_pretty(&record).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    Ok(record)
}
