//! K5a: the `worker` tool. `start` launches a background worker (a child `oxa` process in its own
//! git worktree, see `launch.rs`), `list` reads the registry, `stop` ends one. The workers of a
//! session are recorded per parent session id (`registry.rs`); children are spawned through the
//! process supervisor (`proc.rs`).
//!
//! * Not built in a worker child: `OXA_WORKER_DEPTH` set means nested (see [`built`]).
//! * `start` checks, in order: the name, the goal, the parent session id, a worker of that name in
//!   this session, `oxa.workers.max` live workers, an existing branch `agent/<name>`. Then it makes
//!   the worktree (`git worktree add`), writes the goal file (mode 0600), and spawns the child.
//! * The child runs under `env -C <worktree>`: its cwd is the worktree. Its environment is exactly
//!   `launch::child_env` with the trusted `PATH`, passed to the supervisor (which clears the
//!   environment first), so no value reaches argv and no secret is listed in `ps`.
//! * `Hooks::shutdown` stops every worker through the supervisor.
use super::launch::{child_argv, child_env, valid_name, worktree_argv, WorkerMode, WorkerSpec};
use super::proc::Supervisor;
use super::registry::{self, WorkerRecord, WorkerState};
use oxa_proto::{
    Access, Autonomy, CommandSpec, Exposure, HookPoint, HookReply, Hooks, Host, HostAction, Mode, OxaError,
    Source, Tool, ToolClass, ToolCtx, ToolOutput, ToolSpec, Ui, Value, YoloSwitch,
};
use std::fs::{self, OpenOptions};
use std::hash::{DefaultHasher, Hash, Hasher};
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

const DESCRIPTION: &str = "Background workers: each is a separate oxa process that runs one goal in its own git worktree on branch agent/<name>. Actions: start {name, goal, model?, mode?} starts one (name: lowercase letters, digits and dashes, at most 40 characters); list shows this session's workers and their state; stop {name} stops one. mode is plan, ask, workspace, auto or yolo, never wider than this session.";

const SCHEMA: &str = r#"{"type":"object","properties":{"action":{"type":"string","enum":["start","list","stop"]},"name":{"type":"string"},"goal":{"type":"string"},"model":{"type":"string"},"mode":{"type":"string","enum":["plan","ask","workspace","auto","yolo"]}},"required":["action"]}"#;

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
    oxa_bin: PathBuf,
    agent_dir: PathBuf,
    max: usize,
    sessions_dir: PathBuf,
    parent_env: Vec<(String, String)>,
    parent_scope: ParentScope,
    yolo: YoloSwitch,
    supervisor: Supervisor,
    /// Serializes start and stop: the registry is read, changed and written back.
    gate: Mutex<()>,
    /// (session id, model) of the parent session, as the host last reported them.
    session: Mutex<(String, String)>,
    host: Mutex<Option<Arc<dyn Host>>>,
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

fn parse_mode(text: &str) -> Option<WorkerMode> {
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
        WorkerCore {
            oxa_bin: cfg.oxa_bin,
            agent_dir: cfg.agent_dir,
            max: cfg.max,
            sessions_dir: cfg.sessions_dir,
            parent_env: cfg.parent_env,
            parent_scope: cfg.parent_scope,
            yolo: cfg.yolo,
            supervisor: cfg.supervisor,
            gate: Mutex::new(()),
            session: Mutex::new((String::new(), String::new())),
            host: Mutex::new(None),
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
            _ => ToolOutput::error("worker: action must be start, list or stop"),
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
    fn fire(&self, p: HookPoint, _payload: &Value, _ui: &mut dyn Ui) -> HookReply {
        if matches!(p, HookPoint::SessionStart | HookPoint::ModelSelect) {
            self.core.learn();
        }
        HookReply::Continue
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
        hooks_with(fake, agent, Vec::new(), ParentScope::default())
    }

    fn hooks_with(
        fake: &Path,
        agent: &Path,
        parent_env: Vec<(String, String)>,
        parent_scope: ParentScope,
    ) -> WorkerHooks {
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
            parent_env,
            parent_scope,
            yolo: YoloSwitch::new(Autonomy::Standard),
        })
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
    fn worker_spawn_gets_exact_child_env() {
        let (root, repo, agent, _) = fixture("exactenv");
        let out_file = root.join("env.out");
        let fake = root.join("env-oxa");
        fs::write(
            &fake,
            format!("#!/bin/sh\n/usr/bin/env > '{}'\n", out_file.display()),
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
        let out = hooks.core.start(
            &json!({"action": "start", "name": "alpha", "goal": "one"}),
            &repo,
            Mode::Workspace,
            Autonomy::Standard,
        );
        assert!(!out.is_error, "{}", out.text);
        for _ in 0..100 {
            if out_file.exists() && fs::metadata(&out_file).map(|m| m.len() > 0).unwrap_or(false) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let text = fs::read_to_string(&out_file).expect("env output");
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
