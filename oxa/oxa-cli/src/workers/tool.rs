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
use super::launch::{child_argv, child_env, valid_name, worktree_argv, WorkerMode, WorkerSpec};
use super::proc::Supervisor;
use super::registry::{self, WorkerRecord, WorkerState};
use oxa_proto::{
    Access, Autonomy, CommandSpec, Exposure, HookPoint, HookReply, Hooks, Host, HostAction, Mode, OxaError,
    Source, Tool, ToolClass, ToolCtx, ToolOutput, ToolSpec, Ui, Value, YoloSwitch,
};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
/// Setting: seconds a stopped worker gets after SIGINT.
pub const SETTING_GRACE: &str = "oxa.workers.stopGraceSeconds";
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
    /// Time a stopped worker gets after SIGINT.
    pub grace: Duration,
    /// The process environment of this session; the child environment is made from it.
    pub parent_env: Vec<(String, String)>,
    /// The process's autonomy switch (the parent's level sets the worker's ceiling).
    pub yolo: YoloSwitch,
}

/// State shared by the tool and the hook set.
pub struct WorkerCore {
    oxa_bin: PathBuf,
    agent_dir: PathBuf,
    max: usize,
    parent_env: Vec<(String, String)>,
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

/// Writes `text` to `path` (created with mode 0600, truncated), creating `dir` first.
fn write_private(dir: &Path, path: &Path, text: &str) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(text.as_bytes())
}

/// The argv prefix that runs the child in `cwd` with environment `child`, given the process
/// environment `parent`: `-u NAME` for each name the child lacks, `NAME=value` for each new or
/// changed one. Everything else is inherited.
fn env_wrapper(cwd: &Path, parent: &[(String, String)], child: &[(String, String)]) -> Vec<String> {
    let mut argv = vec![
        "env".to_string(),
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

/// The state of a worker from its status file and its process group.
fn live_state(rec: &WorkerRecord) -> WorkerState {
    let status: Option<Value> = fs::read(&rec.status_file)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    let alive = PathBuf::from(format!("/proc/{}", rec.pid)).exists();
    registry::state(rec, status.as_ref(), alive)
}

fn is_live(rec: &WorkerRecord) -> bool {
    matches!(
        live_state(rec),
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

fn describe(rec: &WorkerRecord) -> String {
    format!(
        "{} session {} branch {} {}",
        rec.name,
        rec.session_id,
        rec.branch,
        label(&live_state(rec))
    )
}

impl WorkerCore {
    fn new(cfg: WorkerConfig) -> io::Result<WorkerCore> {
        Ok(WorkerCore {
            oxa_bin: cfg.oxa_bin,
            agent_dir: cfg.agent_dir,
            max: cfg.max,
            parent_env: cfg.parent_env,
            yolo: cfg.yolo,
            supervisor: Supervisor::new(cfg.grace)?,
            gate: Mutex::new(()),
            session: Mutex::new((String::new(), String::new())),
            host: Mutex::new(None),
        })
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
    pub fn start(&self, args: &Value, cwd: &Path, mode: Mode, parent_yolo: bool) -> ToolOutput {
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
        let model = match str_arg(args, "model").map(str::trim).filter(|m| !m.is_empty()) {
            Some(m) => m.to_string(),
            None => session_model,
        };
        if model.is_empty() {
            return ToolOutput::error("worker: no model for the worker; pass model");
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
        let live = records.iter().filter(|r| is_live(r)).count();
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

        let dir = self.agent_dir.join("workers");
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
        };
        let (argv, notice) = match child_argv(&spec, mode, parent_yolo) {
            Ok(v) => v,
            Err(e) => return ToolOutput::error(format!("worker: {e}")),
        };
        let env = child_env(&self.parent_env);
        let mut command = env_wrapper(&worktree, &self.parent_env, &env);
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
            status_file: dir.join(format!("{name}.status.json")),
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
        let lines: Vec<String> = records.iter().map(|r| describe(r)).collect();
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

    /// The `workers` prompt section: one line per live worker of this session.
    fn prompt_lines(&self) -> Vec<(String, String)> {
        let (parent, _) = self.session();
        if parent.is_empty() {
            return Vec::new();
        }
        let records = registry::load(&self.agent_dir, &parent).unwrap_or_default();
        let lines: Vec<String> = records
            .iter()
            .filter(|r| is_live(r))
            .map(|r| describe(r))
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
            Some("start") => {
                let parent_yolo = matches!(self.core.yolo.level(), Autonomy::Yolo);
                self.core.start(args, cx.cwd, cx.mode, parent_yolo)
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
    /// The hook set, or the error that stops it (the supervisor thread could not start).
    pub fn new(cfg: WorkerConfig) -> io::Result<WorkerHooks> {
        let core = Arc::new(WorkerCore::new(cfg)?);
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
        Ok(WorkerHooks { core, tool })
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
        WorkerHooks::new(WorkerConfig {
            oxa_bin: fake.to_path_buf(),
            agent_dir: agent.to_path_buf(),
            max: 4,
            grace: Duration::from_secs(1),
            parent_env: Vec::new(),
            yolo: YoloSwitch::new(Autonomy::Standard),
        })
        .unwrap()
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
            false,
        );
        assert!(!out.is_error, "{}", out.text);
        let records = registry::load(&agent, "parent-1").unwrap();
        assert_eq!(records.len(), 1);
        let rec = &records[0];
        assert_eq!(rec.name, "alpha");
        assert_eq!(rec.branch, "agent/alpha");
        assert_eq!(rec.worktree, worktree_path(&repo, "alpha").unwrap());
        assert!(rec.worktree.is_dir());
        assert_eq!(rec.goal_file, agent.join("workers").join("alpha.goal.md"));
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
        let first = hooks.core.start(&args, &repo, Mode::Workspace, false);
        assert!(!first.is_error, "{}", first.text);
        let second = hooks.core.start(&args, &repo, Mode::Workspace, false);
        assert!(second.is_error);
        assert!(second.text.contains("already exists"), "{}", second.text);
        assert_eq!(registry::load(&agent, "parent-1").unwrap().len(), 1);
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
