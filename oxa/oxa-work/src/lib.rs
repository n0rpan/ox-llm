//! Opt-in, sequential unattended task execution. Worktrees are retained on every outcome.
mod execute;
mod notes;
mod publish;
pub use notes::{ProblemNote, ReportRecord};
pub use publish::{publish, report_notes};
mod state;
mod tree;
use serde::{Deserialize, Serialize};
pub use state::Step;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

pub type Result<T> = std::result::Result<T, String>;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
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
    Running,
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
    pub step: Step,
    pub progress_at: u64,
    #[serde(default)]
    pub head: Option<String>,
    #[serde(default)]
    pub pull_request: Option<String>,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub problem_notes: Vec<ProblemNote>,
    #[serde(default)]
    pub reports: Vec<ReportRecord>,
}

impl Record {
    pub fn failed(&self) -> bool {
        matches!(self.outcome, Outcome::Red | Outcome::Blocked)
            || self.reports.iter().any(|r| r.disposition == "failed")
    }
}

fn output(mut command: Command) -> Result<Output> {
    command.stdin(Stdio::null()).output().map_err(|e| e.to_string())
}
fn checked(command: Command) -> Result<String> {
    checked_output(output(command)?)
}
fn checked_output(out: Output) -> Result<String> {
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

fn acceptance(root: &Path, commands: &[String], lock: &fs::File, offline: bool) -> Result<String> {
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
        c.current_dir(dir)
            .args(["-c", shell])
            .stdin(lock.try_clone().map_err(|e| e.to_string())?);
        if offline {
            c.env("OXA_OFFLINE", "1");
        }
        let out = c.output().map_err(|e| e.to_string())?;
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
fn agent(
    binary: &Path,
    root: &Path,
    task: &Task,
    prompt: &str,
    lock: &fs::File,
    offline: bool,
) -> Result<AgentReply> {
    let mut c = Command::new(binary);
    c.current_dir(root)
        .args(["-p", "--model", &task.model, "--no-session"]);
    if offline {
        c.arg("--offline");
    }
    // Stdin was null for unattended runs. An empty locked file still supplies EOF,
    // and fd 0 keeps the shared kernel lock alive if the runner dies before its child.
    c.args(["--", prompt])
        .stdin(lock.try_clone().map_err(|e| e.to_string())?);
    let output = c.output().map_err(|e| e.to_string())?;
    let notes = notes::parse(&String::from_utf8_lossy(&output.stdout));
    Ok(AgentReply {
        notes,
        error: checked_output(output).err(),
    })
}

struct AgentReply {
    notes: Vec<ProblemNote>,
    error: Option<String>,
}
pub fn blocked_record(task: &Task, detail: String) -> Record {
    Record {
        id: task.id.clone(),
        branch: task.branch.clone(),
        worktree: PathBuf::new(),
        outcome: Outcome::Blocked,
        detail,
        step: Step::Finished,
        progress_at: state::now(),
        head: None,
        pull_request: None,
        draft: false,
        problem_notes: vec![],
        reports: vec![],
    }
}
pub fn run_task_with_policy(repo: &Path, binary: &Path, task: &Task, offline: bool) -> Result<Record> {
    run_task_with_options(repo, binary, task, false, offline)
}
pub fn run_task(repo: &Path, binary: &Path, task: &Task) -> Result<Record> {
    run_task_with_options(repo, binary, task, false, false)
}
pub fn run_task_with_options(
    repo: &Path,
    binary: &Path,
    task: &Task,
    rerun: bool,
    offline: bool,
) -> Result<Record> {
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
    let lock = state::lock(&lock_path)?;
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
    let mut state = if receipt.exists() {
        let old = state::read(&receipt)?;
        if old.task != *task
            || old.record.worktree != tree
            || old.record.id != task.id
            || old.record.branch != task.branch
        {
            return Err("task specification changed; use a new task id".into());
        }
        if matches!(old.record.step, Step::Publishing | Step::Reporting) && !rerun {
            return Ok(old.record);
        }
        if !matches!(
            old.record.step,
            Step::Finished | Step::Publishing | Step::Reporting
        ) {
            old
        } else if !rerun && matches!(old.record.outcome, Outcome::Done | Outcome::NothingToDo) {
            return Ok(old.record);
        } else {
            new_state(task, tree, offline || old.offline)
        }
    } else {
        new_state(task, tree, offline)
    };
    state.offline |= offline;
    let step = state.record.step;
    state::save(&receipt, &mut state, step)?;
    if let Err(error) = execute::execute(&root, binary, task, &receipt, &mut state, &lock) {
        state.record.outcome = Outcome::Blocked;
        state.record.detail = error;
    }
    state::save(&receipt, &mut state, Step::Saving)?;
    if let Err(error) = execute::save_work(&root, &state.record.worktree, task, &lock) {
        state.record.outcome = Outcome::Blocked;
        if !state.record.detail.is_empty() {
            state.record.detail.push('\n');
        }
        state.record.detail.push_str(&format!("save: {error}"));
    }
    if state.record.worktree.exists() && state.record.outcome != Outcome::Blocked {
        state.record.head = Some(checked(git(&state.record.worktree, &["rev-parse", "HEAD"]))?);
    }
    state::save(&receipt, &mut state, Step::Finished)?;
    Ok(state.record)
}
fn new_state(task: &Task, worktree: PathBuf, offline: bool) -> state::State {
    state::State {
        record: Record {
            id: task.id.clone(),
            branch: task.branch.clone(),
            worktree,
            outcome: Outcome::Running,
            detail: String::new(),
            step: Step::Preparing,
            progress_at: state::now(),
            head: None,
            pull_request: None,
            draft: false,
            problem_notes: vec![],
            reports: vec![],
        },
        task: task.clone(),
        checkpoint: None,
        offline,
        acceptance_outcome: None,
    }
}
pub fn status(repo: &Path) -> Result<Vec<Record>> {
    let common = checked(git(
        repo,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    ))?;
    let directory = PathBuf::from(common).join("oxa-work");
    if !directory.exists() {
        return Ok(vec![]);
    }
    let mut records = vec![];
    for entry in fs::read_dir(directory).map_err(|e| e.to_string())? {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path.extension().is_some_and(|s| s == "json") {
            let mut value = state::read(&path)?;
            if value.record.step != Step::Finished {
                let file = fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(path.with_extension("lock"));
                let busy = match file {
                    Ok(file) => match file.try_lock() {
                        Ok(()) => {
                            value = state::read(&path)?;
                            false
                        }
                        Err(std::fs::TryLockError::WouldBlock) => true,
                        Err(error) => return Err(error.to_string()),
                    },
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                    Err(error) => return Err(error.to_string()),
                };
                if value.record.step != Step::Finished {
                    value.record.outcome = if busy { Outcome::Running } else { Outcome::Blocked };
                    if !busy {
                        value.record.detail = "task was interrupted; rerun its task file to resume".into();
                    }
                }
            }
            records.push(value.record);
        }
    }
    records.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(records)
}
