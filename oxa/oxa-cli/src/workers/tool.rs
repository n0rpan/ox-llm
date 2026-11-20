//! K5a: the `worker` tool. `start` launches a background worker (a child `oxa` process in its own
//! git worktree, see `launch.rs`), `list` reads the registry, `stop` ends one. The workers of a
//! session are recorded per parent session id (`registry.rs`); children are spawned through the
//! process supervisor (`proc.rs`).
//!
//! * Not built in a worker child: `OXA_WORKER_DEPTH` set means nested (see [`built`]).
//! * `start` checks, in order: the name, the goal, the parent session id, a worker of that name in
//!   this session, `oxa.workers.max` live workers, an existing branch `agent/<name>`. Then it makes
//!   the worktree (`git worktree add --no-checkout` on the host, then the checkout inside the
//!   sandbox through `oxa work --supervise`, so no repository filter runs on the host), writes the goal file (mode 0600), and spawns the child.
//! * The child runs under `env -C <worktree>`: its cwd is the worktree. Its environment is exactly
//!   `launch::child_env` with the trusted `PATH`, passed to the supervisor (which clears the
//!   environment first), so no value reaches argv and no secret is listed in `ps`.
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
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
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

/// `oxa.workers.enabled` from the user settings only (default true): a project file cannot turn
/// process launch on or off.
pub fn enabled(settings: &oxa_config::Settings) -> bool {
    settings.user_bool(SETTING_ENABLED, true)
}

/// `oxa.workers.max` from the user settings only (default 4): a project file cannot raise it.
pub fn max_workers(settings: &oxa_config::Settings) -> usize {
    usize::try_from(settings.user_u32(SETTING_MAX, 4)).unwrap_or(4)
}

/// Whether the worker hook set is built: not in a worker child (`worker_depth` is the value of
/// `OXA_WORKER_DEPTH`, `None` when unset) and the setting is not false.
pub fn built(worker_depth: Option<&str>, enabled: bool) -> bool {
    worker_depth.is_none() && enabled
}

/// What [`WorkerHooks::new`] needs.
pub struct WorkerConfig {
    /// The parent workspace used to resolve trusted host tools for resume.
    pub cwd: PathBuf,
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
    /// The parent's other narrowing switches.
    pub narrowing: super::launch::Narrowing,
}

/// State shared by the tool and the hook set.
pub struct WorkerCore {
    cwd: PathBuf,
    /// Private copy opened from the session executable before model-controlled work starts.
    oxa_bin: PathBuf,
    pin_error: Option<String>,
    /// The identity of the private copy; checked before each use as a defence in depth.
    bin_id: Option<BinId>,
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

/// The host tools a worker launch runs outside the sandbox: a `PATH` free of the project (and of
/// worker worktrees), the project root and the home directory they were resolved against.
struct HostTools {
    path: std::ffi::OsString,
    project: PathBuf,
    home: PathBuf,
}

impl HostTools {
    /// Resolved from the session `cwd` exactly like `oxa work`: the project is the Git root of
    /// the session directory and the `PATH` is trusted against the outermost one, so no
    /// repository-supplied shim is ever found.
    fn resolve(parent_env: &[(String, String)], cwd: &Path) -> Result<HostTools, String> {
        let value = |name: &str| {
            parent_env
                .iter()
                .rev()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        };
        let cwd_c = cwd
            .canonicalize()
            .map_err(|e| format!("cannot resolve {}: {e}", cwd.display()))?;
        let project = cwd_c
            .ancestors()
            .find(|p| p.join(".git").exists())
            .ok_or("this directory is not in a git repository")?
            .to_path_buf();
        let host_project = project
            .ancestors()
            .filter(|p| p.join(".git").exists())
            .last()
            .unwrap_or(&project)
            .to_path_buf();
        let home = value("HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
            .ok_or("HOME is not set")?;
        let path = value("PATH")
            .map(std::ffi::OsString::from)
            .or_else(|| std::env::var_os("PATH"))
            .unwrap_or_default();
        let path = crate::netguard::trusted_work_path(&path, &host_project, &home)
            .ok_or("cannot resolve a trusted PATH")?;
        Ok(HostTools { path, project, home })
    }

    fn program(&self, name: &str) -> Option<PathBuf> {
        crate::netguard::trusted_executable_for(
            name,
            Some(self.path.as_os_str()),
            Some(&self.project),
            Some(&self.home),
        )
    }
}

/// `git <args>` for the host: the trusted binary through `oxa_work::metadata_git` (no replace
/// objects, no filesystem monitor, `GIT_*` isolation, no protocols), run in `dir`.
fn host_git(tools: &HostTools, git: &Path, dir: &Path, args: &[&str]) -> Command {
    let mut command = oxa_work::metadata_git(git, &tools.path, dir, args);
    command.stdin(Stdio::null());
    command
}

/// The identity of the executable a worker runs. Any rename over it or write to it changes the
/// inode or the change time, so a binary replaced after this session started (the sandbox leaves
/// a workspace-built `oxa` writable) is never executed outside the sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BinId {
    dev: u64,
    ino: u64,
    len: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

impl BinId {
    fn of(path: &Path) -> io::Result<BinId> {
        let m = fs::metadata(path)?;
        Ok(BinId {
            dev: m.dev(),
            ino: m.ino(),
            len: m.len(),
            mtime: (m.mtime(), m.mtime_nsec()),
            ctime: (m.ctime(), m.ctime_nsec()),
        })
    }
}

/// Copy the running executable into owner-private storage before any model-controlled command can
/// replace a workspace-built binary. Worker launches use only this copy, never reopen the original
/// pathname. Comparing the source identity around the copy rejects a concurrent replacement.
fn pin_executable(source: &Path, agent_dir: &Path) -> Result<PathBuf, String> {
    let before = BinId::of(source).map_err(|e| format!("cannot pin the oxa executable: {e}"))?;
    let mut input = fs::File::open(source).map_err(|e| format!("cannot pin the oxa executable: {e}"))?;
    let dir = agent_dir.join("workers").join("launchers");
    fs::create_dir_all(&dir).map_err(|e| format!("cannot create the worker launcher directory: {e}"))?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("cannot protect the worker launcher directory: {e}"))?;
    let path = dir.join(format!("oxa-{}-{}", std::process::id(), now_ms()));
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o500)
        .open(&path)
        .map_err(|e| format!("cannot create the pinned worker executable: {e}"))?;
    let copied =
        io::copy(&mut input, &mut output).map_err(|e| format!("cannot copy the worker executable: {e}"))?;
    output
        .sync_all()
        .map_err(|e| format!("cannot sync the pinned worker executable: {e}"))?;
    let after = BinId::of(source).map_err(|e| format!("cannot verify the oxa executable: {e}"))?;
    if before != after || copied != before.len {
        let _ = fs::remove_file(&path);
        return Err("the oxa executable changed while it was being pinned; restart oxa".into());
    }
    Ok(path)
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

/// The argv prefix that runs the child in `cwd`. The environment is not set here: the supervisor
/// clears it and applies exactly [`worker_env`], so the policy lives in one place.
fn cwd_wrapper(program: &Path, cwd: &Path) -> Vec<String> {
    vec![
        program.to_string_lossy().into_owned(),
        "-C".to_string(),
        cwd.to_string_lossy().into_owned(),
    ]
}

/// The exact environment of a worker: `launch::child_env` (no trust or session hint,
/// `OXA_WORKER_DEPTH=1`, the parent's offline policy) with `PATH` set to the trusted one.
fn worker_env(parent: &[(String, String)], offline: bool, path: &std::ffi::OsStr) -> Vec<(String, String)> {
    let mut env = child_env(parent, offline);
    env.retain(|(k, _)| k != "PATH");
    env.push(("PATH".into(), path.to_string_lossy().into_owned()));
    env
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

fn child_session_id(parent: &str, name: &str, started: u64) -> String {
    let mut hasher = DefaultHasher::new();
    parent.hash(&mut hasher);
    format!("worker-{:016x}-{name}-{started}", hasher.finish())
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
        let (oxa_bin, pin_error) = match pin_executable(&cfg.oxa_bin, &cfg.agent_dir) {
            Ok(path) => (path, None),
            Err(error) => (cfg.oxa_bin, Some(error)),
        };
        WorkerCore {
            bin_id: BinId::of(&oxa_bin).ok(),
            oxa_bin,
            pin_error,
            cwd: cfg.cwd,
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

    /// Refuses to run a worker binary that is not the one this session started from.
    fn check_bin(&self) -> Result<(), String> {
        if let Some(error) = &self.pin_error {
            return Err(error.clone());
        }
        match (&self.bin_id, BinId::of(&self.oxa_bin)) {
            (Some(then), Ok(now)) if *then == now => Ok(()),
            _ => Err(format!(
                "the oxa executable {} changed since this session started (or cannot be read); restart oxa before starting workers",
                self.oxa_bin.display()
            )),
        }
    }

    /// Checks the empty worktree out inside the sandbox (`oxa work --supervise`, the same
    /// supervisor `oxa work` uses): hooks and credential helpers are off, there is no network,
    /// and the filters of the repository run in bwrap, never on the host.
    fn materialize(
        &self,
        tools: &HostTools,
        git: &Path,
        worktree: &Path,
        branch: &str,
        autonomy: Autonomy,
    ) -> Result<(), String> {
        self.check_bin()?;
        // Yolo deliberately has no OS sandbox. Non-Linux hosts have no bwrap and use the
        // repository's supported unsandboxed path too; the in-process Git guards remain.
        if autonomy == Autonomy::Yolo || !cfg!(target_os = "linux") {
            let out = host_git(tools, git, worktree, &["checkout", "HEAD", "--", "."])
                .output()
                .map_err(|e| format!("cannot run the worktree checkout: {e}"))?;
            return if out.status.success() {
                Ok(())
            } else {
                Err(format!(
                    "the worktree checkout failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ))
            };
        }
        let request = oxa_work::SupervisorRequest {
            source: tools.project.clone(),
            operation: oxa_work::SupervisorOperation::Checkout {
                branch: branch.to_string(),
                create: false,
                adopted: true,
                normalize: None,
            },
        };
        let json = serde_json::to_string(&request).map_err(|e| e.to_string())?;
        let mut command = Command::new(&self.oxa_bin);
        command.env_clear();
        for (k, v) in &self.parent_env {
            if k != "PATH" && !k.starts_with("GIT_") {
                command.env(k, v);
            }
        }
        let out = command
            .env("PATH", &tools.path)
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .current_dir(worktree)
            .args(["work", "--supervise", json.as_str()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| format!("cannot run the sandboxed checkout: {e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(format!(
                "the sandboxed worktree checkout failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ))
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
        let tools = match HostTools::resolve(&self.parent_env, cwd) {
            Ok(tools) => tools,
            Err(e) => return ToolOutput::error(format!("worker: {e}")),
        };
        let Some(git) = tools.program("git") else {
            return ToolOutput::error("worker: git was not found in a trusted PATH directory");
        };
        let branch = format!("agent/{name}");
        let refname = format!("refs/heads/{branch}");
        if host_git(
            &tools,
            &git,
            cwd,
            &["rev-parse", "--verify", "--quiet", refname.as_str()],
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
        {
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
        if let Err(e) = self.check_bin() {
            return ToolOutput::error(format!("worker: {e}"));
        }
        for step in &steps {
            // Step 0 is the program name (`git`); the trusted binary runs in its place.
            let Some((_, rest)) = step.split_first() else {
                continue;
            };
            let rest: Vec<&str> = rest.iter().map(String::as_str).collect();
            match host_git(&tools, &git, cwd, &rest).output() {
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
        // The worktree exists with no files: materialize them inside the sandbox, where a
        // repository's smudge filters and attributes cannot reach the host. A failed checkout
        // removes the empty worktree and branch so the name can be tried again.
        if let Err(e) = self.materialize(&tools, &git, &worktree, &branch, autonomy) {
            let path = worktree.to_string_lossy().into_owned();
            let _ = host_git(
                &tools,
                &git,
                cwd,
                &["worktree", "remove", "--force", path.as_str()],
            )
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
            let _ = host_git(&tools, &git, cwd, &["branch", "-D", branch.as_str()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            return ToolOutput::error(format!("worker: {e}"));
        }

        // Artifacts are per parent session: two sessions of one agent directory may start a
        // worker with the same name and must not share a goal file or a log.
        let dir = self.agent_dir.join("workers").join(artifact_slug(&parent));
        let goal_file = dir.join(format!("{name}.goal.md"));
        if let Err(e) = write_private(&dir, &goal_file, goal) {
            return ToolOutput::error(format!("worker: cannot write the goal file: {e}"));
        }

        let started = now_ms();
        // Session-store identifiers are limited to 128 bytes. The registry already records the
        // full parent relationship, so a bounded digest-shaped component is sufficient here.
        let session_id = child_session_id(&parent, name, started);
        let spec = WorkerSpec {
            name: name.to_string(),
            session_id: session_id.clone(),
            model: model.clone(),
            goal_file: goal_file.to_string_lossy().into_owned(),
            mode: wanted,
            max_tokens: self.parent_scope.max_tokens,
            exclude_tools: self.parent_scope.exclude_tools.clone(),
            session_dir: self.parent_scope.session_dir.clone(),
            narrowing: self.parent_scope.narrowing.clone(),
        };
        let (argv, notice) = match child_argv(&spec, mode, autonomy) {
            Ok(v) => v,
            Err(e) => return ToolOutput::error(format!("worker: {e}")),
        };
        let env = worker_env(&self.parent_env, oxa_proto::offline(), &tools.path);
        // `env` runs outside the sandbox: resolved from the session's project like every other
        // host tool, never from a project-controlled PATH entry.
        let Some(env_bin) = tools.program("env") else {
            return ToolOutput::error("worker: env was not found in a trusted PATH directory");
        };
        if let Err(e) = self.check_bin() {
            return ToolOutput::error(format!("worker: {e}"));
        }
        let mut command = cwd_wrapper(&env_bin, &worktree);
        command.push(self.oxa_bin.to_string_lossy().into_owned());
        command.extend(argv.iter().cloned());
        let refs: Vec<&str> = command.iter().map(String::as_str).collect();
        let log = dir.join(format!("{name}.log"));
        let pgid = match self.supervisor.spawn(&refs, &env, &log) {
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
        // Owner policy: budgets warn and record, they never block. Each worker gets the parent's
        // whole allowance, so say so where the model and the owner can see it.
        if let Some(tokens) = self.parent_scope.max_tokens {
            text.push_str(&format!(
                "\nbudget: this worker may use up to {tokens} tokens (--max-tokens, the same allowance as this session; the session and its workers together may use more)"
            ));
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
        match self.supervisor.stop(records[index].pgid) {
            Ok(Some(_)) => {}
            Ok(None) => {
                return ToolOutput::error(format!(
                    "worker: cannot stop {name}: this process does not own that worker"
                ))
            }
            Err(e) => return ToolOutput::error(format!("worker: cannot stop {name}: {e}")),
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
    /// it finished or its process is gone. Only the typed outcome is returned: child logs can
    /// contain credentials or other private diagnostics and must not become parent-model input.
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
        Ok(Some(format!("worker {name}: {outcome}")))
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
        let mut live = records.iter().filter(|r| is_live(r, &self.sessions_dir)).count();
        let mut notices = Vec::new();
        let mut launched: Vec<(u32, String)> = Vec::new();
        for rec in records.iter_mut() {
            let alive = PathBuf::from(format!("/proc/{}", rec.pid)).exists();
            if !resume::due(rec, &live_state(rec, &self.sessions_dir), alive) {
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
            let tools = match HostTools::resolve(&self.parent_env, &self.cwd) {
                Ok(tools) => tools,
                Err(e) => {
                    notices.push(format!("worker {}: not resumed: {e}", rec.name));
                    continue;
                }
            };
            let Some(env_bin) = tools.program("env") else {
                notices.push(format!(
                    "worker {}: not resumed, env was not found in a trusted PATH directory",
                    rec.name
                ));
                continue;
            };
            if let Err(e) = self.check_bin() {
                notices.push(format!("worker {}: not resumed: {e}", rec.name));
                continue;
            }
            let env = worker_env(&self.parent_env, oxa_proto::offline(), &tools.path);
            let mut command = cwd_wrapper(&env_bin, &rec.worktree);
            command.push(self.oxa_bin.to_string_lossy().into_owned());
            command.extend(argv);
            let refs: Vec<&str> = command.iter().map(String::as_str).collect();
            match self.supervisor.spawn(&refs, &env, &rec.log_file) {
                Ok(pgid) => {
                    rec.pid = pgid;
                    rec.pgid = pgid;
                    launched.push((
                        pgid,
                        format!(
                            "worker {}: relaunched with --continue={}",
                            rec.name, rec.session_id
                        ),
                    ));
                    live += 1;
                    notices.extend(notice);
                }
                Err(e) => notices.push(format!("worker {}: cannot relaunch: {e}", rec.name)),
            }
        }
        if !launched.is_empty() {
            if let Err(e) = registry::save(&self.agent_dir, &parent, &records) {
                // Not recorded, so nothing would ever stop them: stop them now, as `start` does.
                let mut retained = Vec::new();
                for (pgid, _) in &launched {
                    if let Err(error) = self.supervisor.stop(*pgid) {
                        retained.push(format!("{pgid} ({error})"));
                    }
                }
                if retained.is_empty() {
                    notices.push(format!(
                        "worker: relaunched workers were not recorded and were stopped: {e}"
                    ));
                } else {
                    notices.push(format!(
                        "worker: registry update failed ({e}); cleanup failed for process groups {}; they remain tracked by this session and require another stop attempt",
                        retained.join(", ")
                    ));
                }
            } else {
                notices.extend(launched.into_iter().map(|(_, notice)| notice));
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

fn worker_access(args: &Value, cwd: &Path) -> Result<Vec<Access>, String> {
    match str_arg(args, "action") {
        // Listing only reads the worker registry and status through this trusted built-in. It
        // neither launches a command nor asks the network permission gate, including in plan mode.
        Some("list") => Ok(Vec::new()),
        Some("start") => {
            let name = str_arg(args, "name").unwrap_or("");
            let Some(path) = worktree_path(cwd, name) else {
                return Err(format!("worker: invalid name {name:?}"));
            };
            Ok(vec![
                Access::Exec {
                    command: COMMAND.to_string(),
                    net: true,
                },
                Access::Write(path),
            ])
        }
        // Stopping owns a host process operation, but needs no network access.
        _ => Ok(vec![Access::Exec {
            command: COMMAND.to_string(),
            net: false,
        }]),
    }
}

fn approved_worktree_is_unchanged(path: &Path, approved: &Path) -> bool {
    oxa_perm::resolve_real(path).as_deref() == Some(approved)
}

impl Tool for WorkerTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    /// `start` runs `git worktree add` and a child session, so it asks for the command with the
    /// network on and for the worktree it writes. `stop` names a network-free command; `list` is
    /// an internal read-only query and requires no external access.
    fn access(&self, args: &Value, cx: &ToolCtx) -> Result<Vec<Access>, String> {
        worker_access(args, cx.cwd)
    }

    fn run(&self, args: &Value, cx: &ToolCtx, _live: &mut dyn FnMut(&str)) -> ToolOutput {
        match str_arg(args, "action") {
            Some("start") => {
                let name = str_arg(args, "name").unwrap_or("");
                let Some(path) = worktree_path(cx.cwd, name) else {
                    return ToolOutput::error(format!("worker: invalid name {name:?}"));
                };
                if let Some(approved) = cx.checked_real(&path) {
                    if !approved_worktree_is_unchanged(&path, approved) {
                        return ToolOutput::error(
                            "worker: worktree destination changed after permission approval",
                        );
                    }
                }
                self.core.start(args, cx.cwd, cx.mode, self.core.yolo.level())
            }
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
    fn poll_event(
        &self,
        kind: &str,
        args: &Value,
        _budget: std::time::Duration,
    ) -> Option<Result<Option<String>, String>> {
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
        // `work --supervise` is the sandboxed checkout: it records its argv and returns.
        fs::write(
            &fake,
            format!(
                "#!/bin/sh\nif [ \"$1\" = work ]; then echo \"$@\" >> '{}/ran-supervise'; exit 0; fi\nsleep 30\n",
                root.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        (root, repo, agent, fake)
    }

    fn hooks(fake: &Path, agent: &Path) -> WorkerHooks {
        hooks_with(fake, agent, Vec::new(), ParentScope::default())
    }

    fn hooks_with(
        fake: &Path,
        agent: &Path,
        parent_env: Vec<(String, String)>,
        parent_scope: ParentScope,
    ) -> WorkerHooks {
        hooks_with_autonomy(fake, agent, parent_env, parent_scope, Autonomy::Standard)
    }

    fn hooks_with_autonomy(
        fake: &Path,
        agent: &Path,
        parent_env: Vec<(String, String)>,
        parent_scope: ParentScope,
        autonomy: Autonomy,
    ) -> WorkerHooks {
        let kill = ["/bin/kill", "/usr/bin/kill"]
            .into_iter()
            .map(PathBuf::from)
            .find(|p| p.is_file())
            .expect("a kill executable");
        WorkerHooks::new(WorkerConfig {
            oxa_bin: fake.to_path_buf(),
            cwd: agent.parent().unwrap().join("repo"),
            agent_dir: agent.to_path_buf(),
            max: 4,
            supervisor: Supervisor::new(Duration::from_secs(1), kill).expect("supervisor"),
            sessions_dir: agent.join("sessions"),
            parent_env,
            parent_scope,
            yolo: YoloSwitch::new(autonomy),
            resume_on_start: true,
        })
    }

    fn set_table_mode(hooks: &WorkerHooks, mode: Mode) {
        *lock(&hooks.core.table_mode) = Some(mode);
    }

    /// A project that ships `env` and `git` shims in `<repo>/shim`, first on the parent's PATH.
    /// A shim that runs leaves `<root>/ran-<name>`.
    fn shim_env(root: &Path, repo: &Path) -> Vec<(String, String)> {
        let shim = repo.join("shim");
        fs::create_dir_all(&shim).unwrap();
        for name in ["env", "git"] {
            let file = shim.join(name);
            fs::write(
                &file,
                format!("#!/bin/sh\ntouch '{}/ran-{name}'\nexit 1\n", root.display()),
            )
            .unwrap();
            fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();
        }
        vec![
            ("PATH".into(), format!("{}:/usr/bin:/bin", shim.display())),
            ("HOME".into(), root.display().to_string()),
        ]
    }

    #[test]
    fn project_env_shim_is_refused() {
        let (root, repo, agent, fake) = fixture("envshim");
        let env = shim_env(&root, &repo);
        let tools = HostTools::resolve(&env, &repo).unwrap();
        let env_bin = tools.program("env").expect("a trusted env");
        assert!(!env_bin.starts_with(&repo), "{}", env_bin.display());
        // Resolved from the worktree as the project, the shim would have been accepted.
        let hooks = hooks_with(&fake, &agent, env, ParentScope::default());
        set_session(&hooks, "parent-1");
        let out = hooks.core.start(
            &json!({"action": "start", "name": "alpha", "goal": "one"}),
            &repo,
            Mode::Workspace,
            Autonomy::Standard,
        );
        assert!(!out.is_error, "{}", out.text);
        std::thread::sleep(Duration::from_millis(300));
        assert!(!root.join("ran-env").exists(), "the project env shim ran");
        let _ = hooks.core.stop(&json!({"action": "stop", "name": "alpha"}));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn project_git_shim_is_refused() {
        let (root, repo, agent, fake) = fixture("gitshim");
        let env = shim_env(&root, &repo);
        assert!(!HostTools::resolve(&env, &repo)
            .unwrap()
            .program("git")
            .unwrap()
            .starts_with(&repo));
        let hooks = hooks_with(&fake, &agent, env, ParentScope::default());
        set_session(&hooks, "parent-1");
        let out = hooks.core.start(
            &json!({"action": "start", "name": "alpha", "goal": "one"}),
            &repo,
            Mode::Workspace,
            Autonomy::Standard,
        );
        assert!(!out.is_error, "{}", out.text);
        assert!(!root.join("ran-git").exists(), "the project git shim ran");
        assert!(worktree_path(&repo, "alpha").unwrap().is_dir());
        let _ = hooks.core.stop(&json!({"action": "stop", "name": "alpha"}));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn worker_start_and_resume_get_the_same_child_env() {
        let (root, repo, agent, _) = fixture("exactenv");
        let out_file = root.join("env.out");
        let fake = root.join("env-oxa");
        fs::write(
            &fake,
            format!(
                "#!/bin/sh\n[ \"$1\" = work ] && exit 0\n/usr/bin/env > '{}'\nsleep 30\n",
                out_file.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        let env = vec![
            ("PATH".to_string(), "/usr/bin:/bin".to_string()),
            ("HOME".to_string(), root.display().to_string()),
            ("OXA_TRUST_PROJECT".to_string(), "1".to_string()),
            ("OXA_SESSION_HINT".to_string(), "abc.s1".to_string()),
            ("KEEP_ME".to_string(), "yes".to_string()),
        ];
        let hooks = hooks_with(&fake, &agent, env, ParentScope::default());
        set_session(&hooks, "parent-1");
        set_table_mode(&hooks, Mode::Workspace);
        let out = hooks.core.start(
            &json!({"action": "start", "name": "alpha", "goal": "one"}),
            &repo,
            Mode::Workspace,
            Autonomy::Standard,
        );
        assert!(!out.is_error, "{}", out.text);
        let capture_env = || {
            for _ in 0..100 {
                if let Ok(text) = fs::read_to_string(&out_file) {
                    if text.contains("KEEP_ME=yes\n") {
                        return text;
                    }
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            panic!("missing env output");
        };
        let text = capture_env();
        let lines: Vec<&str> = text.lines().collect();
        assert!(
            !lines.iter().any(|l| l.starts_with("OXA_TRUST_PROJECT=")),
            "{text}"
        );
        assert!(
            !lines.iter().any(|l| l.starts_with("OXA_SESSION_HINT=")),
            "{text}"
        );
        assert!(lines.contains(&"OXA_WORKER_DEPTH=1"), "{text}");
        assert!(lines.contains(&"KEEP_ME=yes"), "{text}");
        assert!(lines.iter().any(|l| l.starts_with("PATH=")), "{text}");
        let rec = registry::load(&agent, "parent-1").unwrap().remove(0);
        hooks.core.supervisor.stop(rec.pgid).unwrap();
        fs::remove_file(&out_file).unwrap();
        let notices = hooks.core.resume();
        assert!(
            notices.iter().any(|notice| notice.contains("relaunched")),
            "{notices:?}"
        );
        let resumed = capture_env();
        assert_eq!(
            resumed.lines().collect::<std::collections::BTreeSet<_>>(),
            text.lines().collect::<std::collections::BTreeSet<_>>()
        );
        let _ = hooks.core.stop(&json!({"action": "stop", "name": "alpha"}));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn worker_start_notice_shows_the_budget() {
        let (root, repo, agent, fake) = fixture("budget");
        let scope = ParentScope {
            max_tokens: Some(4242),
            ..ParentScope::default()
        };
        let hooks = hooks_with(&fake, &agent, Vec::new(), scope);
        set_session(&hooks, "parent-1");
        let out = hooks.core.start(
            &json!({"action": "start", "name": "alpha", "goal": "one"}),
            &repo,
            Mode::Workspace,
            Autonomy::Standard,
        );
        assert!(!out.is_error, "{}", out.text);
        assert!(out.text.contains("4242 tokens"), "{}", out.text);
        let _ = hooks.core.stop(&json!({"action": "stop", "name": "alpha"}));
        let _ = fs::remove_dir_all(&root);
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
        assert_eq!(
            hooks.poll_event("worker", &args, std::time::Duration::from_secs(5)),
            Some(Ok(None))
        );
        // Another kind is not ours.
        assert_eq!(
            hooks.poll_event("pr", &args, std::time::Duration::from_secs(5)),
            None
        );
        // The worker reports its typed outcome without forwarding its potentially secret log.
        let status = json!({"session_id": rec.session_id, "pid": rec.pid, "phase": "finished",
            "outcome": {"type": "oxa_result", "outcome": "done", "error_class": null, "exit_code": 0}});
        fs::create_dir_all(rec.status_file.parent().unwrap()).unwrap();
        fs::write(&rec.status_file, status.to_string()).unwrap();
        let log: Vec<String> = (1..=25).map(|n| format!("line {n}")).collect();
        fs::write(&rec.log_file, log.join("\n")).unwrap();
        let text = hooks
            .poll_event("worker", &args, std::time::Duration::from_secs(5))
            .unwrap()
            .unwrap()
            .expect("done");
        assert!(text.contains("outcome done"), "{text}");
        assert!(!text.contains("line 25") && !text.contains("log lines"), "{text}");
        // An unknown worker is a probe failure.
        assert!(hooks
            .poll_event(
                "worker",
                &json!({"name": "nope"}),
                std::time::Duration::from_secs(5)
            )
            .unwrap()
            .is_err());
        let _ = hooks.core.stop(&json!({"action": "stop", "name": "alpha"}));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn reopen_relaunches_unfinished_worker_with_continue() {
        let (root, repo, agent, _fake) = fixture("resume");
        // A fake oxa that records its argv next to itself, then sleeps. The worktree step
        // (`oxa work --supervise`) just succeeds.
        let fake = root.join("argv-oxa");
        fs::write(
            &fake,
            format!(
                "#!/bin/sh\nif [ \"$1\" = work ]; then exit 0; fi\nprintf '%s\\n' \"$@\" > '{}'\nexec /bin/sleep 30\n",
                root.join("argv.txt").display()
            ),
        )
        .unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        let env = vec![("PATH".to_string(), "/usr/bin:/bin".to_string())];
        let hooks = hooks_with(&fake, &agent, env, ParentScope::default());
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
    fn worker_start_never_runs_a_checkout_filter_on_the_host() {
        let (root, repo, agent, fake) = fixture("filter");
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
        fs::write(repo.join("a.txt"), "content\n").unwrap();
        fs::write(repo.join(".gitattributes"), "a.txt filter=probe\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "files"]);
        // A smudge filter that leaves a marker outside the worktree when it runs.
        let marker = root.join("filter-ran");
        git(&[
            "config",
            "filter.probe.smudge",
            &format!("sh -c 'touch {}; cat'", marker.display()),
        ]);
        let hooks = hooks(&fake, &agent);
        set_session(&hooks, "parent-1");
        let out = hooks.core.start(
            &json!({"action": "start", "name": "alpha", "goal": "one"}),
            &repo,
            Mode::Workspace,
            Autonomy::Standard,
        );
        assert!(!out.is_error, "{}", out.text);
        assert!(!marker.exists(), "a repository filter ran on the host");
        // The checkout was handed to the sandboxed supervisor as a checkout request.
        let ran = fs::read_to_string(root.join("ran-supervise")).unwrap_or_default();
        assert!(
            ran.contains("--supervise") && ran.contains("\"kind\":\"checkout\""),
            "supervisor argv: {ran}"
        );
        let _ = hooks.core.stop(&json!({"action": "stop", "name": "alpha"}));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn resume_stops_relaunched_workers_when_registry_write_fails() {
        let (root, repo, agent, fake) = fixture("rollback");
        let hooks = hooks(&fake, &agent);
        set_session(&hooks, "parent-1");
        set_table_mode(&hooks, Mode::Workspace);
        start_alpha(&hooks, &repo);
        let rec = registry::load(&agent, "parent-1").unwrap().remove(0);
        hooks.core.supervisor.stop(rec.pgid).unwrap();
        // A read-only registry directory makes the save fail (a privileged user ignores modes).
        let dir = agent.join("workers");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();
        let writable = fs::write(dir.join(".probe"), b"x").is_ok();
        let notices = hooks.core.resume();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        if !writable {
            assert!(notices.iter().any(|n| n.contains("were stopped")), "{notices:?}");
            // The relaunched process is gone and the registry still holds the old record.
            let now = registry::load(&agent, "parent-1").unwrap().remove(0);
            assert_eq!(now.pid, rec.pid);
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn worker_start_uses_the_private_copy_after_the_original_is_replaced() {
        let (root, repo, agent, fake) = fixture("pin");
        let hooks = hooks(&fake, &agent);
        set_session(&hooks, "parent-1");
        // Another file renamed over the binary after the session started.
        let other = root.join("other-oxa");
        fs::write(
            &other,
            format!("#!/bin/sh\ntouch '{}/replacement-ran'\n", root.display()),
        )
        .unwrap();
        fs::set_permissions(&other, fs::Permissions::from_mode(0o755)).unwrap();
        fs::rename(&other, &fake).unwrap();
        let out = hooks.core.start(
            &json!({"action": "start", "name": "alpha", "goal": "one"}),
            &repo,
            Mode::Workspace,
            Autonomy::Standard,
        );
        assert!(!out.is_error, "worker start failed");
        assert!(!root.join("replacement-ran").exists(), "the replacement ran");
        assert!(worktree_path(&repo, "alpha").unwrap().exists());
        let _ = hooks.core.stop(&json!({"action": "stop", "name": "alpha"}));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn yolo_worker_materializes_without_the_sandbox_supervisor() {
        let (root, repo, agent, fake) = fixture("yolo-checkout");
        fs::write(repo.join("tracked.txt"), "content\n").unwrap();
        let ok = Command::new("git")
            .args(["add", "."])
            .current_dir(&repo)
            .status()
            .unwrap()
            .success()
            && Command::new("git")
                .args([
                    "-c",
                    "user.name=test",
                    "-c",
                    "user.email=test@example.invalid",
                    "commit",
                    "-qm",
                    "tracked",
                ])
                .current_dir(&repo)
                .status()
                .unwrap()
                .success();
        assert!(ok, "fixture commit failed");
        let hooks = hooks_with_autonomy(&fake, &agent, Vec::new(), ParentScope::default(), Autonomy::Yolo);
        set_session(&hooks, "parent-1");
        let out = hooks.core.start(
            &json!({"action": "start", "name": "alpha", "goal": "one", "mode": "yolo"}),
            &repo,
            Mode::Workspace,
            Autonomy::Yolo,
        );
        assert!(!out.is_error, "yolo worker start failed");
        assert!(worktree_path(&repo, "alpha")
            .unwrap()
            .join("tracked.txt")
            .is_file());
        assert!(
            !root.join("ran-supervise").exists(),
            "yolo invoked the sandbox supervisor"
        );
        let _ = hooks.core.stop(&json!({"action": "stop", "name": "alpha"}));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn project_true_cannot_enable_workers() {
        let user_off = json!({"oxa": {"workers": {"enabled": false}}});
        let project_on = json!({"oxa": {"workers": {"enabled": true}}});
        let set = oxa_config::Settings::from_values(user_off, project_on.clone());
        assert!(!enabled(&set));
        assert!(!built(None, enabled(&set)));
        // The project is ignored in both directions: unset by the user means the default.
        assert!(enabled(&oxa_config::Settings::from_values(json!({}), project_on)));
        let project_off = json!({"oxa": {"workers": {"enabled": false}}});
        assert!(enabled(&oxa_config::Settings::from_values(
            json!({}),
            project_off
        )));
    }

    #[test]
    fn project_cannot_raise_worker_max() {
        let set = |user: Value, project: Value| oxa_config::Settings::from_values(user, project);
        let two = json!({"oxa": {"workers": {"max": 2}}});
        let ten = json!({"oxa": {"workers": {"max": 10}}});
        assert_eq!(max_workers(&set(two.clone(), ten.clone())), 2);
        assert_eq!(max_workers(&set(json!({}), ten)), 4);
        assert_eq!(max_workers(&set(two, json!({}))), 2);
    }

    #[test]
    fn worker_controls_read_the_user_file_only() {
        use oxa_config::Settings;
        // The user turned workers off and capped them: a trusted project cannot undo that.
        let user = Settings::from_values(
            json!({"oxa": {"workers": {"enabled": false, "max": 1}}}),
            json!({"oxa": {"workers": {"enabled": true, "max": 99}}}),
        );
        assert!(!enabled(&user));
        assert_eq!(max_workers(&user), 1);
        // A project alone cannot change the defaults either way.
        let project = Settings::from_values(
            json!({}),
            json!({"oxa": {"workers": {"enabled": false, "max": 99}}}),
        );
        assert!(enabled(&project));
        assert_eq!(max_workers(&project), 4);
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

    #[test]
    fn worker_list_declares_no_command_or_network_access() {
        let cwd = Path::new("/example/project");
        assert_eq!(
            worker_access(&json!({"action": "list"}), cwd).unwrap(),
            Vec::new()
        );

        let stop = worker_access(&json!({"action": "stop", "name": "alpha"}), cwd).unwrap();
        assert_eq!(
            stop,
            vec![Access::Exec {
                command: COMMAND.to_string(),
                net: false,
            }]
        );
    }

    #[test]
    fn approved_new_worktree_keeps_its_nonexistent_tail() {
        let root = scratch("pin-new");
        let path = root.join("missing-parent").join("repo").join("alpha");
        let approved = oxa_perm::resolve_real(&path).expect("permission pin");

        assert!(approved_worktree_is_unchanged(&path, &approved));

        let elsewhere = root.join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join("missing-parent")).unwrap();
        assert!(!approved_worktree_is_unchanged(&path, &approved));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn child_session_id_is_bounded_for_the_longest_parent_id() {
        let parent = "p".repeat(128);
        let id = child_session_id(&parent, "forty-character-worker-name-000000000000", u64::MAX);
        // Session identifiers are treated as private metadata by the log scanners. Keep assertion
        // failures useful without copying the generated identifier into test output.
        assert!(id.len() <= 128, "generated child session id is too long");
        assert!(id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
        let other = child_session_id(
            &format!("{parent}x"),
            "forty-character-worker-name-000000000000",
            u64::MAX,
        );
        assert!(id != other, "distinct parents must produce distinct child ids");
    }
}
