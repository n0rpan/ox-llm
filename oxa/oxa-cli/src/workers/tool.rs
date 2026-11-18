//! K5a: the `worker` tool. `start` launches a background worker (a child `oxa` process in its own
//! git worktree, see `launch.rs`), `list` reads the registry, `stop` ends one. The workers of a
//! session are recorded per parent session id (`registry.rs`); children are spawned through the
//! process supervisor (`proc.rs`).
//!
//! * Not built in a worker child: `OXA_WORKER_DEPTH` set means nested (see [`built`]).
//! * `start` checks, in order: the name, the goal, the parent session id, a worker of that name in
//!   this session, `oxa.workers.max` live workers, an existing branch `agent/<name>`. Then it makes
//!   the worktree (`git worktree add`), writes the goal file (mode 0600), and spawns the child.
//! * The child runs under `env -C <worktree>`: its cwd is the worktree and its environment is
//!   `launch::child_env`. Only the removed names and the changed or new values reach argv (the
//!   worker marker, never a value the parent already has), so no secret is listed in `ps`.
//! * `Hooks::shutdown` stops every worker through the supervisor.
//! * K5b: `send {name, text}` publishes `text` to the worker's session inbox (the writer behind
//!   `oxa send`) and reports the delivery and the worker's phase. The child id is read from the
//!   registry record of `name`. `poll_event("worker", {name})` is done when the worker finished
//!   or its process is gone. On `session_start` (startup, resume) each dead, unfinished worker is
//!   relaunched through the same launch path (`resume.rs`) unless `oxa.workers.resumeOnStart`
//!   is false.
use super::launch::{child_argv, child_env, valid_name, worktree_argv, WorkerMode, WorkerSpec};
use super::proc::Supervisor;
use super::registry::{self, WorkerRecord, WorkerState};
use super::resume;
use oxa_proto::{
    Access, Autonomy, CommandSpec, Exposure, HookPoint, HookReply, Hooks, Host, HostAction, Level, Mode,
    OxaError, Source, Tool, ToolClass, ToolCtx, ToolOutput, ToolSpec, Ui, Value, YoloSwitch,
};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

/// The name the model calls.
pub const TOOL_NAME: &str = "worker";
/// `Hooks::name` and the source of the tool.
const HOOK_NAME: &str = "builtin:worker";
/// The command the permission layer sees for this tool.
const COMMAND: &str = "oxa worker";
/// Setting: the switch (`oxa-config/src/schema.rs`).
pub const SETTING_ENABLED: &str = "oxa.workers.enabled";
/// Setting: the most live workers one session runs.
pub const SETTING_MAX: &str = "oxa.workers.max";
/// Env: set in a worker child (`launch::child_env` sets it to `1`). Any value, even empty, means
/// nested; only an unset variable lets the hook set be built.
pub const ENV_WORKER_DEPTH: &str = "OXA_WORKER_DEPTH";

const DESCRIPTION: &str = "Background workers: each is a separate oxa process that runs one goal in its own git worktree on branch agent/<name>. Actions: start {name, goal, model?, mode?} starts one (name: lowercase letters, digits and dashes, at most 40 characters); list shows this session's workers and their state; stop {name} stops one; send {name, text} delivers a follow-up message to a worker's session inbox and reports its phase. mode is plan, ask, workspace, auto or yolo, never wider than this session.";

const SCHEMA: &str = r#"{"type":"object","properties":{"action":{"type":"string","enum":["start","list","stop","send"]},"name":{"type":"string"},"goal":{"type":"string"},"text":{"type":"string"},"model":{"type":"string"},"mode":{"type":"string","enum":["plan","ask","workspace","auto","yolo"]}},"required":["action"]}"#;

/// Whether the worker hook set is built: not in a worker child (`worker_depth` is the value of
/// `OXA_WORKER_DEPTH`, `None` when unset) and the setting is not false.
pub fn built(worker_depth: Option<&str>, enabled: bool) -> bool {
    worker_depth.is_none() && enabled
}

/// What [`WorkerHooks::new`] needs.
pub struct WorkerConfig {
    /// The binary a worker runs: this executable in production.
    pub oxa_bin: PathBuf,
    pub agent_dir: PathBuf,
    pub max: usize,
    /// The process supervisor (`proc::start`: the trusted `kill` helper and
    /// `oxa.workers.stopGraceSeconds` are already resolved).
    pub supervisor: Supervisor,
    /// The directory the child writes its sessions to (and so its `<session>.status.json`).
    pub sessions_dir: PathBuf,
    /// The process environment of this session; the child environment is made from it.
    pub parent_env: Vec<(String, String)>,
    /// The parent's token allowance, tool exclusions and resolved session directory.
    pub parent_scope: ParentScope,
    /// The process's autonomy switch (the parent's level sets the worker's ceiling).
    pub yolo: YoloSwitch,
    /// `oxa.workers.resumeOnStart`: relaunch dead, unfinished workers when the session opens.
    pub resume_on_start: bool,
}

/// What the parent session allows a worker to have.
#[derive(Debug, Clone, Default)]
pub struct ParentScope {
    pub max_tokens: Option<u64>,
    pub exclude_tools: Vec<String>,
    /// Set when the parent's session directory is not the child's own default.
    pub session_dir: Option<PathBuf>,
}

/// State shared by the tool and the hook set.
pub struct WorkerCore {
    oxa_bin: PathBuf,
    agent_dir: PathBuf,
    max: usize,
    sessions_dir: PathBuf,
    parent_env: Vec<(String, String)>,
    parent_scope: ParentScope,
    yolo: YoloSwitch,
    resume_on_start: bool,
    /// Parent session ids whose workers were already considered for resume in this process.
    resumed: Mutex<Vec<String>>,
    supervisor: Supervisor,
    /// Serializes start and stop: the registry is read, changed and written back.
    gate: Mutex<()>,
    /// (session id, model) of the parent session, as the host last reported them.
    session: Mutex<(String, String)>,
    host: Mutex<Option<Arc<dyn Host>>>,
    /// The parent's table mode as the host last reported it (`None` until known).
    table_mode: Mutex<Option<Mode>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// The worktree of a worker `name` for the repository at `repo`: the place
/// `launch::worktree_argv` creates it.
pub fn worktree_path(repo: &Path, name: &str) -> Option<PathBuf> {
    if !valid_name(name) {
        return None;
    }
    let repo_name = repo.file_name()?;
    Some(repo.parent()?.join(".oxa-worktrees").join(repo_name).join(name))
}

pub(super) fn parse_mode(text: &str) -> Option<WorkerMode> {
    match text {
        "plan" => Some(WorkerMode::Plan),
        "ask" => Some(WorkerMode::Ask),
        "workspace" => Some(WorkerMode::Workspace),
        "auto" => Some(WorkerMode::Auto),
        "yolo" => Some(WorkerMode::Yolo),
        _ => None,
    }
}

/// Whether `git <args>` run in `dir` succeeds; its output is dropped.
fn git_succeeds(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Writes `text` to `path`, creating `dir` first: a fresh mode-0600 file beside it, renamed over
/// the name. An existing goal file is replaced, never followed (a symlink there stays a symlink
/// and its target is untouched), and broader permissions cannot survive the replace.
fn write_private(dir: &Path, path: &Path, text: &str) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let leaf = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "the goal file has no name"))?;
    let temp = dir.join(format!(".{}.{}.tmp", leaf.to_string_lossy(), std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    let written = file.write_all(text.as_bytes()).and_then(|()| file.sync_all());
    drop(file);
    if let Err(e) = written {
        let _ = fs::remove_file(&temp);
        return Err(e);
    }
    fs::rename(&temp, path).inspect_err(|_| {
        let _ = fs::remove_file(&temp);
    })
}

/// The argv prefix that runs the child in `cwd` with environment `child`, given the process
/// environment `parent`: `-u NAME` for each name the child lacks, `NAME=value` for each new or
/// changed one. Everything else is inherited.
fn env_wrapper(
    program: &Path,
    cwd: &Path,
    parent: &[(String, String)],
    child: &[(String, String)],
) -> Vec<String> {
    let mut argv = vec![
        program.to_string_lossy().into_owned(),
        "-C".to_string(),
        cwd.to_string_lossy().into_owned(),
    ];
    for (name, _) in parent {
        if !child.iter().any(|(c, _)| c == name) {
            argv.push("-u".to_string());
            argv.push(name.clone());
        }
    }
    for (name, value) in child {
        if !parent.iter().any(|(p, v)| p == name && v == value) {
            argv.push(format!("{name}={value}"));
        }
    }
    argv
}

/// A single safe path component for a parent session id (`workers/<slug>/`): session ids are
/// alphanumeric with dashes, and anything else becomes a dash so no path escapes the directory.
fn artifact_slug(parent: &str) -> String {
    let slug: String = parent
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if slug.is_empty() {
        "session".to_string()
    } else {
        slug
    }
}

/// The status file of the worker session `session_id` that ran in `worktree`: the sibling
/// `<session>.status.json` of its session file (`<stamp>_<id>.jsonl`, written by the status writer
/// as `session_file.with_extension("status.json")`), in the session directory of the worktree.
/// The time stamp is the child's, so the file is found by its suffix; `None` until it exists.
fn status_path(sessions_dir: &Path, worktree: &Path, session_id: &str) -> Option<PathBuf> {
    let dir = sessions_dir.join(oxa_session::session_dir_name(worktree));
    let suffix = format!("_{session_id}.status.json");
    let mut found: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(&suffix))
        .map(|e| e.path())
        .collect();
    found.sort();
    found.pop()
}

/// The state of a worker from its status file and its process group. The status file is found
/// under `sessions_dir` (see [`status_path`]); `rec.status_file` is the fallback.
fn live_state(rec: &WorkerRecord, sessions_dir: &Path) -> WorkerState {
    let path =
        status_path(sessions_dir, &rec.worktree, &rec.session_id).unwrap_or_else(|| rec.status_file.clone());
    let status: Option<Value> = fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    let alive = PathBuf::from(format!("/proc/{}", rec.pid)).exists();
    registry::state(rec, status.as_ref(), alive)
}

/// How many log lines a finished worker's probe text carries.
const LOG_LINES: usize = 20;
/// The most bytes read from the end of a log for its tail.
const LOG_TAIL_BYTES: u64 = 64 * 1024;

/// The last [`LOG_LINES`] lines of the log at `path` (empty text when it cannot be read).
fn log_tail(path: &Path) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = fs::File::open(path) else {
        return String::new();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(LOG_TAIL_BYTES);
    let mut bytes = Vec::new();
    if file.seek(SeekFrom::Start(start)).is_err()
        || file.take(LOG_TAIL_BYTES).read_to_end(&mut bytes).is_err()
    {
        return String::new();
    }
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(LOG_LINES)..].join("\n")
}

fn is_live(rec: &WorkerRecord, sessions_dir: &Path) -> bool {
    matches!(
        live_state(rec, sessions_dir),
        WorkerState::Starting | WorkerState::Working | WorkerState::Idle
    )
}

fn label(state: &WorkerState) -> String {
    match state {
        WorkerState::Starting => "starting".to_string(),
        WorkerState::Working => "working".to_string(),
        WorkerState::Idle => "idle".to_string(),
        WorkerState::Finished(outcome) => format!("finished ({outcome})"),
        WorkerState::Dead => "dead".to_string(),
        WorkerState::Stopped => "stopped".to_string(),
    }
}

fn describe(rec: &WorkerRecord, sessions_dir: &Path) -> String {
    format!(
        "{} session {} branch {} {}",
        rec.name,
        rec.session_id,
        rec.branch,
        label(&live_state(rec, sessions_dir))
    )
}

impl WorkerCore {
    fn new(cfg: WorkerConfig) -> WorkerCore {
        WorkerCore {
            oxa_bin: cfg.oxa_bin,
            agent_dir: cfg.agent_dir,
            max: cfg.max,
            sessions_dir: cfg.sessions_dir,
            parent_env: cfg.parent_env,
            parent_scope: cfg.parent_scope,
            yolo: cfg.yolo,
            resume_on_start: cfg.resume_on_start,
            resumed: Mutex::new(Vec::new()),
            supervisor: cfg.supervisor,
            gate: Mutex::new(()),
            session: Mutex::new((String::new(), String::new())),
            host: Mutex::new(None),
            table_mode: Mutex::new(None),
        }
    }

    fn session(&self) -> (String, String) {
        lock(&self.session).clone()
    }

    /// A host program the worker launch runs, from a trusted PATH directory only.
    fn trusted_program(&self, program: &str, cwd: &Path) -> Option<PathBuf> {
        let value = |name: &str| {
            self.parent_env
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        };
        let path = value("PATH")
            .map(std::ffi::OsString::from)
            .or_else(|| std::env::var_os("PATH"));
        let home = value("HOME").map(PathBuf::from);
        crate::netguard::trusted_executable_for(program, path.as_deref(), Some(cwd), home.as_deref())
    }

    /// Learn the parent session id and model through the host (`HostAction::EngineLimits`).
    fn learn(&self) {
        let Some(host) = lock(&self.host).clone() else {
            return;
        };
        if let Ok(v) = host.call(HostAction::EngineLimits) {
            let id = v.get("sessionId").and_then(Value::as_str).unwrap_or("");
            if let Some(mode) = v.get("mode").and_then(Value::as_str).and_then(Mode::parse) {
                *lock(&self.table_mode) = Some(mode);
            }
            if !id.is_empty() {
                let model = v.get("model").and_then(Value::as_str).unwrap_or("");
                *lock(&self.session) = (id.to_string(), model.to_string());
            }
        }
    }

    /// `start`: see the module docs for the order of the checks.
    pub fn start(&self, args: &Value, cwd: &Path, mode: Mode, autonomy: Autonomy) -> ToolOutput {
        let _gate = lock(&self.gate);
        let name = str_arg(args, "name").unwrap_or("");
        if !valid_name(name) {
            return ToolOutput::error(format!(
                "worker: invalid name {name:?}; use lowercase letters, digits and dashes, at most 40 characters"
            ));
        }
        let goal = str_arg(args, "goal").unwrap_or("");
        if goal.trim().is_empty() {
            return ToolOutput::error("worker: start needs a goal");
        }
        let wanted = match str_arg(args, "mode") {
            None => None,
            Some(text) => match parse_mode(text) {
                Some(m) => Some(m),
                None => return ToolOutput::error(format!("worker: unknown mode {text:?}")),
            },
        };
        let (parent, session_model) = self.session();
        if parent.is_empty() {
            return ToolOutput::error("worker: the session id is not known yet");
        }
        // A worker runs this session's model: the model argument cannot pick another one, so a
        // tool call never selects a cloud or metered transport the owner did not choose.
        if let Some(requested) = str_arg(args, "model").map(str::trim).filter(|m| !m.is_empty()) {
            if session_model.is_empty() || requested != session_model {
                return ToolOutput::error(format!(
                    "worker: a worker runs this session's model ({}); start it without a model",
                    if session_model.is_empty() {
                        "not known yet"
                    } else {
                        session_model.as_str()
                    }
                ));
            }
        }
        let model = session_model;
        if model.is_empty() {
            return ToolOutput::error("worker: the session model is not known yet");
        }
        let mut records = match registry::load(&self.agent_dir, &parent) {
            Ok(records) => records,
            Err(e) => return ToolOutput::error(format!("worker: {e}")),
        };
        if records.iter().any(|r| r.name == name) {
            return ToolOutput::error(format!(
                "worker: a worker named {name} already exists in this session; stop it or choose another name"
            ));
        }
        let live = records.iter().filter(|r| is_live(r, &self.sessions_dir)).count();
        if live >= self.max {
            return ToolOutput::error(format!(
                "worker: {live} live workers already; the limit is {} (oxa.workers.max)",
                self.max
            ));
        }
        let branch = format!("agent/{name}");
        let refname = format!("refs/heads/{branch}");
        if git_succeeds(cwd, &["rev-parse", "--verify", "--quiet", refname.as_str()]) {
            return ToolOutput::error(format!(
                "worker: branch {branch} already exists; another owner has this name"
            ));
        }
        let Some(worktree) = worktree_path(cwd, name) else {
            return ToolOutput::error(
                "worker: this directory has no worktree location; start from a git repository",
            );
        };
        if worktree.exists() {
            return ToolOutput::error(format!("worker: {} already exists", worktree.display()));
        }
        let steps = worktree_argv(cwd, name, "HEAD");
        if steps.is_empty() {
            return ToolOutput::error(
                "worker: cannot plan the worktree; the directory must be a git repository with a UTF-8 path",
            );
        }
        for step in &steps {
            let Some((program, rest)) = step.split_first() else {
                continue;
            };
            match Command::new(program)
                .args(rest)
                .current_dir(cwd)
                .stdin(Stdio::null())
                .output()
            {
                Ok(out) if out.status.success() => {}
                Ok(out) => {
                    return ToolOutput::error(format!(
                        "worker: git worktree add failed: {}",
                        String::from_utf8_lossy(&out.stderr).trim()
                    ))
                }
                Err(e) => return ToolOutput::error(format!("worker: cannot run git: {e}")),
            }
        }

        // Artifacts are per parent session: two sessions of one agent directory may start a
        // worker with the same name and must not share a goal file or a log.
        let dir = self.agent_dir.join("workers").join(artifact_slug(&parent));
        let goal_file = dir.join(format!("{name}.goal.md"));
        if let Err(e) = write_private(&dir, &goal_file, goal) {
            return ToolOutput::error(format!("worker: cannot write the goal file: {e}"));
        }

        let started = now_ms();
        let session_id = format!("{parent}-{name}-{started}");
        let spec = WorkerSpec {
            name: name.to_string(),
            session_id: session_id.clone(),
            model: model.clone(),
            goal_file: goal_file.to_string_lossy().into_owned(),
            mode: wanted,
            max_tokens: self.parent_scope.max_tokens,
            exclude_tools: self.parent_scope.exclude_tools.clone(),
            session_dir: self.parent_scope.session_dir.clone(),
        };
        let (argv, notice) = match child_argv(&spec, mode, autonomy) {
            Ok(v) => v,
            Err(e) => return ToolOutput::error(format!("worker: {e}")),
        };
        let env = child_env(&self.parent_env, oxa_proto::offline());
        // `env` runs outside the sandbox: resolve it like every other host tool, never from a
        // project-controlled PATH entry.
        let Some(env_bin) = self.trusted_program("env", &worktree) else {
            return ToolOutput::error("worker: env was not found in a trusted PATH directory");
        };
        let mut command = env_wrapper(&env_bin, &worktree, &self.parent_env, &env);
        command.push(self.oxa_bin.to_string_lossy().into_owned());
        command.extend(argv.iter().cloned());
        let refs: Vec<&str> = command.iter().map(String::as_str).collect();
        let log = dir.join(format!("{name}.log"));
        let pgid = match self.supervisor.spawn(&refs, &log) {
            Ok(pgid) => pgid,
            Err(e) => return ToolOutput::error(format!("worker: cannot start the process: {e}")),
        };
        let effective = argv
            .iter()
            .position(|a| a == "--permission-mode")
            .and_then(|i| argv.get(i + 1))
            .cloned()
            .unwrap_or_else(|| "workspace".to_string());

        records.push(WorkerRecord {
            name: name.to_string(),
            session_id: session_id.clone(),
            pid: pgid,
            pgid,
            worktree: worktree.clone(),
            branch: branch.clone(),
            model,
            mode: effective,
            goal_file,
            // The child's real file carries a time stamp (see `status_path`); `live_state`
            // looks for it by session id and falls back to this name.
            status_file: self
                .sessions_dir
                .join(oxa_session::session_dir_name(&worktree))
                .join(format!("{session_id}.status.json")),
            log_file: log,
            started_ms: started,
            stopped: false,
        });
        if let Err(e) = registry::save(&self.agent_dir, &parent, &records) {
            // Not recorded, so nothing would ever stop it: stop it now.
            let _ = self.supervisor.stop(pgid);
            return ToolOutput::error(format!("worker: {e}"));
        }
        let mut text = format!(
            "started worker {name}: session {session_id}, branch {branch}, worktree {}",
            worktree.display()
        );
        if let Some(notice) = notice {
            text.push('\n');
            text.push_str(&notice);
        }
        ToolOutput::ok(text)
    }

    /// `list`: one line per worker of this session.
    pub fn list(&self) -> ToolOutput {
        let (parent, _) = self.session();
        if parent.is_empty() {
            return ToolOutput::error("worker: the session id is not known yet");
        }
        let records = match registry::load(&self.agent_dir, &parent) {
            Ok(records) => records,
            Err(e) => return ToolOutput::error(format!("worker: {e}")),
        };
        if records.is_empty() {
            return ToolOutput::ok("no workers in this session");
        }
        let lines: Vec<String> = records.iter().map(|r| describe(r, &self.sessions_dir)).collect();
        ToolOutput::ok(lines.join("\n"))
    }

    /// `stop {name}`: stop the worker's process group and mark it stopped.
    pub fn stop(&self, args: &Value) -> ToolOutput {
        let _gate = lock(&self.gate);
        let name = str_arg(args, "name").unwrap_or("");
        let (parent, _) = self.session();
        if parent.is_empty() {
            return ToolOutput::error("worker: the session id is not known yet");
        }
        let mut records = match registry::load(&self.agent_dir, &parent) {
            Ok(records) => records,
            Err(e) => return ToolOutput::error(format!("worker: {e}")),
        };
        let Some(index) = records.iter().position(|r| r.name == name) else {
            return ToolOutput::error(format!("worker: no worker named {name:?}"));
        };
        if records[index].stopped {
            return ToolOutput::ok(format!("worker {name} is already stopped"));
        }
        if let Err(e) = self.supervisor.stop(records[index].pgid) {
            return ToolOutput::error(format!("worker: cannot stop {name}: {e}"));
        }
        records[index].stopped = true;
        if let Err(e) = registry::save(&self.agent_dir, &parent, &records) {
            return ToolOutput::error(format!("worker: {e}"));
        }
        ToolOutput::ok(format!("stopped worker {name}"))
    }

    /// `send {name, text}`: publish `text` to the inbox of the worker's session. The session id is
    /// the one in the registry record, never a value from the model. Acceptance means the file was
    /// published; the child takes it up when its headless goal run watches the inbox.
    pub fn send(&self, args: &Value) -> ToolOutput {
        let name = str_arg(args, "name").unwrap_or("");
        let text = str_arg(args, "text").unwrap_or("");
        if text.trim().is_empty() {
            return ToolOutput::error("worker: send needs text");
        }
        let (parent, _) = self.session();
        if parent.is_empty() {
            return ToolOutput::error("worker: the session id is not known yet");
        }
        let records = match registry::load(&self.agent_dir, &parent) {
            Ok(records) => records,
            Err(e) => return ToolOutput::error(format!("worker: {e}")),
        };
        let Some(rec) = records.iter().find(|r| r.name == name) else {
            return ToolOutput::error(format!("worker: no worker named {name:?}"));
        };
        if let Err(e) = oxa_session::send_inbox(&self.agent_dir, &rec.session_id, text) {
            return ToolOutput::error(format!("worker: cannot deliver to {name}: {e}"));
        }
        ToolOutput::ok(format!(
            "delivered to worker {name}: file accepted in the inbox of session {}; worker phase: {}",
            rec.session_id,
            label(&live_state(rec, &self.sessions_dir))
        ))
    }

    /// The wait probe for `worker {name}`: `Ok(None)` while the worker runs, `Ok(Some(text))` when
    /// it finished or its process is gone, with the outcome and the last 20 log lines.
    pub fn probe(&self, args: &Value) -> Result<Option<String>, String> {
        let name = str_arg(args, "name").unwrap_or("");
        let (parent, _) = self.session();
        if parent.is_empty() {
            return Err("worker: the session id is not known yet".to_string());
        }
        let records = registry::load(&self.agent_dir, &parent).map_err(|e| format!("worker: {e}"))?;
        let rec = records
            .iter()
            .find(|r| r.name == name)
            .ok_or_else(|| format!("worker: no worker named {name:?}"))?;
        let state = live_state(rec, &self.sessions_dir);
        let outcome = match &state {
            WorkerState::Finished(outcome) => format!("finished, outcome {outcome}"),
            WorkerState::Dead => "process exited without a finished outcome".to_string(),
            WorkerState::Stopped => "stopped".to_string(),
            _ => return Ok(None),
        };
        Ok(Some(format!(
            "worker {name}: {outcome}\nlast {LOG_LINES} log lines:\n{}",
            log_tail(&rec.log_file)
        )))
    }

    /// Relaunch each dead, unfinished worker of the parent session (once per session id in this
    /// process): same launch path as `start` (env wrapper, worktree cwd, supervisor), argv from
    /// `resume::resume_argv`. Returns one notice per worker.
    pub fn resume(&self) -> Vec<String> {
        if !self.resume_on_start {
            return Vec::new();
        }
        let _gate = lock(&self.gate);
        let (parent, _) = self.session();
        if parent.is_empty() {
            return Vec::new();
        }
        {
            let mut seen = lock(&self.resumed);
            if seen.contains(&parent) {
                return Vec::new();
            }
            seen.push(parent.clone());
        }
        let mut records = match registry::load(&self.agent_dir, &parent) {
            Ok(records) => records,
            Err(e) => return vec![format!("worker: cannot resume workers: {e}")],
        };
        let autonomy = self.yolo.level();
        // An unknown table mode is taken as the most restrictive one, never a wider one.
        let table_mode = (*lock(&self.table_mode)).unwrap_or(Mode::Plan);
        let env = child_env(&self.parent_env, oxa_proto::offline());
        let mut live = records.iter().filter(|r| is_live(r, &self.sessions_dir)).count();
        let mut notices = Vec::new();
        let mut changed = false;
        for rec in records.iter_mut() {
            if !resume::due(&live_state(rec, &self.sessions_dir)) {
                continue;
            }
            if live >= self.max {
                notices.push(format!(
                    "worker {}: not resumed, {live} live workers already (oxa.workers.max)",
                    rec.name
                ));
                continue;
            }
            let (argv, notice) = match resume::resume_argv(rec, table_mode, autonomy, &self.parent_scope) {
                Ok(v) => v,
                Err(e) => {
                    notices.push(format!("worker {}: not resumed: {e}", rec.name));
                    continue;
                }
            };
            if !rec.worktree.is_dir() {
                notices.push(format!(
                    "worker {}: not resumed, worktree {} is gone",
                    rec.name,
                    rec.worktree.display()
                ));
                continue;
            }
            let Some(env_bin) = self.trusted_program("env", &rec.worktree) else {
                notices.push(format!(
                    "worker {}: not resumed, env was not found in a trusted PATH directory",
                    rec.name
                ));
                continue;
            };
            let mut command = env_wrapper(&env_bin, &rec.worktree, &self.parent_env, &env);
            command.push(self.oxa_bin.to_string_lossy().into_owned());
            command.extend(argv);
            let refs: Vec<&str> = command.iter().map(String::as_str).collect();
            match self.supervisor.spawn(&refs, &rec.log_file) {
                Ok(pgid) => {
                    rec.pid = pgid;
                    rec.pgid = pgid;
                    changed = true;
                    live += 1;
                    notices.push(format!(
                        "worker {}: relaunched with --continue={}",
                        rec.name, rec.session_id
                    ));
                    notices.extend(notice);
                }
                Err(e) => notices.push(format!("worker {}: cannot relaunch: {e}", rec.name)),
            }
        }
        if changed {
            if let Err(e) = registry::save(&self.agent_dir, &parent, &records) {
                notices.push(format!("worker: relaunched workers were not recorded: {e}"));
            }
        }
        notices
    }

    /// The `workers` prompt section: one line per live worker of this session.
    fn prompt_lines(&self) -> Vec<(String, String)> {
        let (parent, _) = self.session();
        if parent.is_empty() {
            return Vec::new();
        }
        let records = registry::load(&self.agent_dir, &parent).unwrap_or_default();
        let lines: Vec<String> = records
            .iter()
            .filter(|r| is_live(r, &self.sessions_dir))
            .map(|r| describe(r, &self.sessions_dir))
            .collect();
        if lines.is_empty() {
            Vec::new()
        } else {
            vec![("workers".to_string(), lines.join("\n"))]
        }
    }
}

/// The `worker` tool.
struct WorkerTool {
    spec: ToolSpec,
    core: Arc<WorkerCore>,
}

impl Tool for WorkerTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    /// `start` runs `git worktree add` and a child session, so it asks for the command with the
    /// network on and for the worktree it writes. `list` and `stop` only name the command.
    fn access(&self, args: &Value, cx: &ToolCtx) -> Result<Vec<Access>, String> {
        let mut out = vec![Access::Exec {
            command: COMMAND.to_string(),
            net: true,
        }];
        if str_arg(args, "action") == Some("start") {
            let name = str_arg(args, "name").unwrap_or("");
            let Some(path) = worktree_path(cx.cwd, name) else {
                return Err(format!("worker: invalid name {name:?}"));
            };
            out.push(Access::Write(path));
        }
        Ok(out)
    }

    fn run(&self, args: &Value, cx: &ToolCtx, _live: &mut dyn FnMut(&str)) -> ToolOutput {
        match str_arg(args, "action") {
            Some("start") => self.core.start(args, cx.cwd, cx.mode, self.core.yolo.level()),
            Some("list") => self.core.list(),
            Some("stop") => self.core.stop(args),
            Some("send") => self.core.send(args),
            _ => ToolOutput::error("worker: action must be start, list, stop or send"),
        }
    }

    fn default_selected(&self) -> bool {
        true
    }
}

/// The hook set: the `worker` tool, the session's learned id and model, and the shutdown.
pub struct WorkerHooks {
    core: Arc<WorkerCore>,
    tool: Arc<dyn Tool>,
}

impl WorkerHooks {
    /// The hook set.
    pub fn new(cfg: WorkerConfig) -> WorkerHooks {
        let core = Arc::new(WorkerCore::new(cfg));
        let tool: Arc<dyn Tool> = Arc::new(WorkerTool {
            spec: ToolSpec {
                name: TOOL_NAME.to_string(),
                description: DESCRIPTION.to_string(),
                schema_json: SCHEMA.to_string(),
                class: ToolClass::Exec,
                exposure: Exposure::Direct,
                source: Source::Extension(HOOK_NAME.to_string()),
            },
            core: core.clone(),
        });
        WorkerHooks { core, tool }
    }
}

impl Hooks for WorkerHooks {
    fn fire(&self, p: HookPoint, payload: &Value, ui: &mut dyn Ui) -> HookReply {
        if matches!(p, HookPoint::SessionStart | HookPoint::ModelSelect) {
            self.core.learn();
        }
        // A session that is opened (a new process or `/resume`) takes its workers back up.
        if p == HookPoint::SessionStart
            && matches!(
                payload.get("reason").and_then(Value::as_str),
                Some("startup" | "resume")
            )
        {
            for notice in self.core.resume() {
                ui.notify(&notice, Level::Info);
            }
        }
        HookReply::Continue
    }
    fn poll_event(&self, kind: &str, args: &Value) -> Option<Result<Option<String>, String>> {
        (kind == TOOL_NAME).then(|| self.core.probe(args))
    }
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![self.tool.clone()]
    }
    fn commands(&self) -> Vec<CommandSpec> {
        Vec::new()
    }
    fn run_command(&self, name: &str, _args: &str, _ui: &mut dyn Ui) -> Result<(), OxaError> {
        Err(OxaError::new(
            oxa_proto::codes::NOT_FOUND,
            format!("unknown command: /{name}"),
        ))
    }
    fn prompt_sections(&self) -> Vec<(String, String)> {
        self.core.prompt_lines()
    }
    fn wants(&self, p: HookPoint) -> bool {
        matches!(p, HookPoint::SessionStart | HookPoint::ModelSelect)
    }
    fn name(&self) -> &str {
        HOOK_NAME
    }
    fn attach(&self, host: Arc<dyn Host>) {
        *lock(&self.core.host) = Some(host);
        self.core.learn();
    }
    /// Stops every live worker (SIGINT, then the group). Idempotent.
    fn shutdown(&self) {
        let _ = self.core.supervisor.stop_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    /// A fresh directory under the system temp directory.
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("oxa-worker-{tag}-{}-{}", std::process::id(), now_ms()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// (root, repo, agent dir, fake oxa): a git repository with one commit, and a fake `oxa` that
    /// sleeps (the child argv is ignored).
    fn fixture(tag: &str) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
        let root = scratch(tag);
        let repo = root.join("repo");
        let agent = root.join("agent");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&agent).unwrap();
        let git = |args: &[&str]| {
            let ok = Command::new("git")
                .arg("-c")
                .arg("user.name=test")
                .arg("-c")
                .arg("user.email=test@example.invalid")
                .args(args)
                .current_dir(&repo)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?} failed");
        };
        git(&["init", "-q"]);
        git(&["commit", "-q", "--allow-empty", "-m", "init"]);
        let fake = root.join("fake-oxa");
        fs::write(&fake, "#!/bin/sh\nsleep 30\n").unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        (root, repo, agent, fake)
    }

    fn hooks(fake: &Path, agent: &Path) -> WorkerHooks {
        let kill = ["/bin/kill", "/usr/bin/kill"]
            .into_iter()
            .map(PathBuf::from)
            .find(|p| p.is_file())
            .expect("a kill executable");
        WorkerHooks::new(WorkerConfig {
            oxa_bin: fake.to_path_buf(),
            agent_dir: agent.to_path_buf(),
            max: 4,
            supervisor: Supervisor::new(Duration::from_secs(1), kill).expect("supervisor"),
            sessions_dir: agent.join("sessions"),
            parent_env: Vec::new(),
            parent_scope: ParentScope::default(),
            yolo: YoloSwitch::new(Autonomy::Standard),
            resume_on_start: true,
        })
    }

    fn set_table_mode(hooks: &WorkerHooks, mode: Mode) {
        *lock(&hooks.core.table_mode) = Some(mode);
    }

    fn set_session(hooks: &WorkerHooks, id: &str) {
        *lock(&hooks.core.session) = (id.to_string(), "model-x".to_string());
    }

    #[test]
    fn worker_start_writes_registry_goal_file_and_worktree() {
        let (root, repo, agent, fake) = fixture("start");
        let hooks = hooks(&fake, &agent);
        set_session(&hooks, "parent-1");
        let out = hooks.core.start(
            &json!({"action": "start", "name": "alpha", "goal": "write the thing"}),
            &repo,
            Mode::Workspace,
            Autonomy::Standard,
        );
        assert!(!out.is_error, "{}", out.text);
        let records = registry::load(&agent, "parent-1").unwrap();
        assert_eq!(records.len(), 1);
        let rec = &records[0];
        assert_eq!(rec.name, "alpha");
        assert_eq!(rec.branch, "agent/alpha");
        assert_eq!(rec.worktree, worktree_path(&repo, "alpha").unwrap());
        assert!(rec.worktree.is_dir());
        assert_eq!(
            rec.goal_file,
            agent.join("workers").join("parent-1").join("alpha.goal.md")
        );
        assert_eq!(fs::read_to_string(&rec.goal_file).unwrap(), "write the thing");
        assert!(!rec.stopped);
        let stopped = hooks.core.stop(&json!({"action": "stop", "name": "alpha"}));
        assert!(!stopped.is_error, "{}", stopped.text);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn worker_start_refuses_second_owner_for_same_name() {
        let (root, repo, agent, fake) = fixture("dup");
        let hooks = hooks(&fake, &agent);
        set_session(&hooks, "parent-1");
        let args = json!({"action": "start", "name": "alpha", "goal": "one"});
        let first = hooks
            .core
            .start(&args, &repo, Mode::Workspace, Autonomy::Standard);
        assert!(!first.is_error, "{}", first.text);
        let second = hooks
            .core
            .start(&args, &repo, Mode::Workspace, Autonomy::Standard);
        assert!(second.is_error);
        assert!(second.text.contains("already exists"), "{}", second.text);
        assert_eq!(registry::load(&agent, "parent-1").unwrap().len(), 1);
        let _ = hooks.core.stop(&json!({"action": "stop", "name": "alpha"}));
        let _ = fs::remove_dir_all(&root);
    }

    fn start_alpha(hooks: &WorkerHooks, repo: &Path) {
        let out = hooks.core.start(
            &json!({"action": "start", "name": "alpha", "goal": "g"}),
            repo,
            Mode::Workspace,
            Autonomy::Standard,
        );
        assert!(!out.is_error, "{}", out.text);
    }

    #[test]
    fn worker_send_lands_in_child_inbox_dir() {
        let (root, repo, agent, fake) = fixture("send");
        let hooks = hooks(&fake, &agent);
        set_session(&hooks, "parent-1");
        start_alpha(&hooks, &repo);
        let rec = registry::load(&agent, "parent-1").unwrap().remove(0);
        let out = hooks
            .core
            .send(&json!({"action": "send", "name": "alpha", "text": "please add tests"}));
        assert!(!out.is_error, "{}", out.text);
        assert!(out.text.contains("file accepted"), "{}", out.text);
        assert!(out.text.contains("worker phase"), "{}", out.text);
        let inbox = agent.join("inbox").join(&rec.session_id);
        let texts: Vec<String> = fs::read_dir(&inbox)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".msg"))
            .map(|e| fs::read_to_string(e.path()).unwrap())
            .collect();
        assert_eq!(texts, vec!["please add tests".to_string()]);
        // An unknown worker and empty text are refused; nothing else lands anywhere.
        assert!(hooks.core.send(&json!({"name": "nope", "text": "x"})).is_error);
        assert!(hooks.core.send(&json!({"name": "alpha", "text": " "})).is_error);
        let _ = hooks.core.stop(&json!({"action": "stop", "name": "alpha"}));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn worker_probe_reports_finished_outcome() {
        let (root, repo, agent, fake) = fixture("probe");
        let hooks = hooks(&fake, &agent);
        set_session(&hooks, "parent-1");
        start_alpha(&hooks, &repo);
        let rec = registry::load(&agent, "parent-1").unwrap().remove(0);
        let args = json!({"kind": "worker", "name": "alpha"});
        // Running: still pending.
        assert_eq!(hooks.poll_event("worker", &args), Some(Ok(None)));
        // Another kind is not ours.
        assert_eq!(hooks.poll_event("pr", &args), None);
        // The worker reports its outcome; the log has more than 20 lines.
        let status = json!({"session_id": rec.session_id, "pid": rec.pid, "phase": "finished",
            "outcome": {"type": "oxa_result", "outcome": "done", "error_class": null, "exit_code": 0}});
        fs::create_dir_all(rec.status_file.parent().unwrap()).unwrap();
        fs::write(&rec.status_file, status.to_string()).unwrap();
        let log: Vec<String> = (1..=25).map(|n| format!("line {n}")).collect();
        fs::write(&rec.log_file, log.join("\n")).unwrap();
        let text = hooks.poll_event("worker", &args).unwrap().unwrap().expect("done");
        assert!(text.contains("outcome done"), "{text}");
        assert!(text.contains("line 25") && text.contains("line 6"), "{text}");
        assert!(!text.contains("line 5\n") && !text.ends_with("line 5"), "{text}");
        // An unknown worker is a probe failure.
        assert!(hooks
            .poll_event("worker", &json!({"name": "nope"}))
            .unwrap()
            .is_err());
        let _ = hooks.core.stop(&json!({"action": "stop", "name": "alpha"}));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn reopen_relaunches_unfinished_worker_with_continue() {
        let (root, repo, agent, _fake) = fixture("resume");
        // A fake oxa that records its argv next to itself, then sleeps.
        let fake = root.join("argv-oxa");
        fs::write(
            &fake,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$(dirname \"$0\")/argv.txt\"\nsleep 30\n",
        )
        .unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        let hooks = hooks(&fake, &agent);
        set_session(&hooks, "parent-1");
        set_table_mode(&hooks, Mode::Workspace);
        start_alpha(&hooks, &repo);
        let before = registry::load(&agent, "parent-1").unwrap().remove(0);
        // The parent goes away: the worker is killed but not marked stopped.
        hooks.core.supervisor.stop(before.pgid).unwrap();
        assert!(!registry::load(&agent, "parent-1").unwrap()[0].stopped);
        let argv_file = root.join("argv.txt");
        let _ = fs::remove_file(&argv_file);
        // The parent opens again.
        let notices = hooks.core.resume();
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(
            notices[0].contains("alpha") && notices[0].contains("--continue="),
            "{notices:?}"
        );
        let mut argv = String::new();
        for _ in 0..100 {
            argv = fs::read_to_string(&argv_file).unwrap_or_default();
            if argv.contains("--continue=") {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            argv.lines()
                .any(|l| l == format!("--continue={}", before.session_id)),
            "{argv}"
        );
        assert!(!argv.lines().any(|l| l == "--goal"), "{argv}");
        let after = registry::load(&agent, "parent-1").unwrap().remove(0);
        assert_ne!(after.pid, before.pid);
        // A second open in the same process does not launch it again, and a stopped worker is
        // never relaunched.
        assert!(hooks.core.resume().is_empty());
        let _ = hooks.core.stop(&json!({"action": "stop", "name": "alpha"}));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn worker_start_refuses_another_model() {
        let (root, repo, agent, fake) = fixture("model");
        let hooks = hooks(&fake, &agent);
        set_session(&hooks, "parent-1");
        let out = hooks.core.start(
            &json!({"action": "start", "name": "alpha", "goal": "one", "model": "cloud/expensive"}),
            &repo,
            Mode::Workspace,
            Autonomy::Standard,
        );
        assert!(out.is_error, "{}", out.text);
        assert!(out.text.contains("session's model"), "{}", out.text);
        assert!(registry::load(&agent, "parent-1").unwrap().is_empty());
        assert!(!worktree_path(&repo, "alpha").unwrap().exists());
        // The session's own model is accepted and passed through.
        let ok = hooks.core.start(
            &json!({"action": "start", "name": "alpha", "goal": "one", "model": "model-x"}),
            &repo,
            Mode::Workspace,
            Autonomy::Standard,
        );
        assert!(!ok.is_error, "{}", ok.text);
        assert_eq!(registry::load(&agent, "parent-1").unwrap()[0].model, "model-x");
        let _ = hooks.core.stop(&json!({"action": "stop", "name": "alpha"}));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn saved_worker_still_auto_resumes_with_default_setting() {
        // `resume_on_start` is what `oxa.workers.resumeOnStart` (default true) sets.
        let key = oxa_config::schema()
            .iter()
            .find(|k| k.key == resume::SETTING_RESUME)
            .expect("the setting is in the schema");
        assert_eq!(key.default, "true");
        let (root, repo, agent, fake) = fixture("auto-resume");
        let hooks = hooks(&fake, &agent);
        set_session(&hooks, "parent-1");
        set_table_mode(&hooks, Mode::Workspace);
        start_alpha(&hooks, &repo);
        let rec = registry::load(&agent, "parent-1").unwrap().remove(0);
        hooks.core.supervisor.stop(rec.pgid).unwrap();
        let notices = hooks.core.resume();
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(notices[0].contains("relaunched"), "{notices:?}");
        let _ = hooks.core.stop(&json!({"action": "stop", "name": "alpha"}));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn worker_tool_absent_in_worker_child() {
        // A worker child sets OXA_WORKER_DEPTH: the hook set (and so the tool) is not built,
        // whatever the setting says. Outside a worker it is built unless switched off.
        assert!(built(None, true));
        assert!(!built(Some("1"), true));
        assert!(!built(Some(""), true));
        assert!(!built(None, false));
    }
}
