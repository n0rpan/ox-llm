//! E11: `builtin:subagent`.
// ported from pi packages/coding-agent/examples/extensions/subagent/ (index.ts, agents.ts)
//!
//! The subagent policy of OXA-ENGINE-SUPPORT sec. 2 (one inference slot, so "parallel" children
//! are a queue):
//!
//! * **Concurrency 1 on the local engine.** A lock held for the whole call serialises calls that
//!   arrive concurrently (e.g. from codemode). A `tasks` call whose children all run locally
//!   runs them in call order and shows the rest as `subagent k/n, queued`; when some child runs
//!   on a remote (cloud) model, up to `oxa.subagent.remoteConcurrency` (default 4) children run
//!   at once, local ones still one at a time, and results come back in call order (#458);
//!   more than [`MAX_PARALLEL_TASKS`] tasks are refused with pi's text. Depth 1: a child gets
//!   `OXA_SUBAGENT_DEPTH=1` and the binary then does not build this hook set.
//! * **Eight children per parent session, over the whole tree (O-146).** The cap is not per
//!   `tasks` array: a `chain` longer than eight steps is refused too, and children that run
//!   or wait are counted per parent session over every call in flight (a call queued behind
//!   the running one has reserved its tasks), so several calls cannot add up to more than
//!   eight. Children cannot spawn (depth 1), so these are all descendants. A refusal is a
//!   tool error to the model and starts nothing.
//! * **Own session per child.** Child `n` of parent session `P` gets the hint `P.s<n>` (env
//!   `OXA_SESSION_HINT`); the child binary sends it as `X-Session-Id` / `X-Oxe-Session` with
//!   `X-Oxe-Priority: background` and `X-Oxe-Turn-Kind: subagent` (`oxa-cli/src/child.rs`).
//!   `n` counts up per parent session id inside this process.
//! * **Park first.** Before the first child of a call: `HostAction::ParkSession` (one H6 + H4
//!   request over the parent's wire; `{parked: false}` without H6). Nothing else is sent on the
//!   parent session while children run (the parent is blocked inside this tool call).
//! * **Cleanup (EG5).** After each child (success, failure or abort):
//!   `HostAction::ClearEngineHint(hint)`; a failure is a warning notice, never a tool failure.
//! * **Child limits.** `oxa.subagent.softTokens` (default 24000), never above the parent's
//!   `compactAt` and below the context window, handed to the child as `OXA_CHILD_SOFT_TOKENS`.
//! * **Child model.** The child gets `--model <the session's model>`, or the agent's own
//!   `model:` when that model is remote or the session's is (#458; `HostAction::ModelInfo`).
//!   A local `model:` under a local session is ignored with a warning notice (one model is
//!   loaded).
//! * **Result.** The final assistant text of each child only (W4: deterministic, no timings,
//!   no usage), capped at `oxa.subagent.resultCap` bytes (default 6144) on a UTF-8 boundary with
//!   a fixed note naming the spill file that holds the full text.
//! * **Abort.** The tool's cancel token and [`Hooks::shutdown`] kill the child's process group
//!   (SIGTERM, SIGKILL after [`KILL_GRACE`]); queued tasks never start.
//! * **Small models.** Not declared in an epoch whose model's context window is below
//!   [`MIN_CONTEXT`] unless `oxa.subagent.smallContext` is true. `tools()` runs on the session
//!   thread while it builds an epoch, where a `Host::call` cannot be answered; the window is
//!   therefore learned through the host in `attach` and again when `SessionStart` or
//!   `ModelSelect` fire (both run on a helper thread while the session serves the host).
//!
//! Model-facing texts (description, schema, errors, truncation note, the child's task line)
//! live in `oxa_tools::descriptions::subagent` (README rule 4).
use oxa_config::{Dirs, Settings};
use oxa_proto::{
    Access, CancelToken, CommandSpec, Exposure, HookPoint, HookReply, Hooks, Host, HostAction, Level,
    OxaError, Source, Tool, ToolClass, ToolCtx, ToolOutput, ToolSpec, Ui, Value, YoloSwitch,
};
use oxa_tools::descriptions::subagent as T;
use serde_json::json;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ---- names shared with the child side (`oxa-cli/src/child.rs`) ----

/// Env: the child's session hint `<parent session id>.s<n>`.
pub const ENV_SESSION_HINT: &str = "OXA_SESSION_HINT";
/// Env: `1` in a child; the binary then does not build `builtin:subagent` (depth 1).
pub const ENV_SUBAGENT_DEPTH: &str = "OXA_SUBAGENT_DEPTH";
/// Env: the child's soft compaction limit (overrides `oxa.compaction.softTokens`).
pub const ENV_CHILD_SOFT_TOKENS: &str = "OXA_CHILD_SOFT_TOKENS";

/// L4-SUBVERIFY: whether a value of [`ENV_SUBAGENT_DEPTH`] marks a subagent child. The same rule
/// as `oxa-cli/src/child.rs`: a depth above 0 is a child, and a value that is not a number fails
/// closed (a child). Unset, or `0`, is not.
pub(crate) fn depth_marks_child(value: Option<&str>) -> bool {
    match value {
        None => false,
        Some(v) => v.trim().parse::<u32>().map_or(true, |d| d > 0),
    }
}

/// L4-SUBVERIFY: this process is a subagent child (read from the process environment).
pub(crate) fn is_child_process() -> bool {
    depth_marks_child(std::env::var(ENV_SUBAGENT_DEPTH).ok().as_deref())
}

#[cfg(test)]
mod depth_tests {
    use super::depth_marks_child;

    #[test]
    fn only_a_positive_or_garbled_depth_is_a_child() {
        assert!(!depth_marks_child(None));
        assert!(!depth_marks_child(Some("0")));
        assert!(depth_marks_child(Some("1")));
        assert!(depth_marks_child(Some(" 2 ")));
        assert!(depth_marks_child(Some("deep")));
    }
}

// ---- settings ----

pub const SETTING_ENABLED: &str = "oxa.subagent.enabled";
pub const SETTING_SOFT_TOKENS: &str = "oxa.subagent.softTokens";
pub const SETTING_RESULT_CAP: &str = "oxa.subagent.resultCap";
pub const SETTING_SMALL_CONTEXT: &str = "oxa.subagent.smallContext";
pub const DEFAULT_SOFT_TOKENS: u32 = 24_000;
pub const DEFAULT_RESULT_CAP: usize = 6144;
pub const SETTING_REMOTE_CONCURRENCY: &str = "oxa.subagent.remoteConcurrency";
pub const DEFAULT_REMOTE_CONCURRENCY: usize = 4;

/// pi's limit on `tasks`.
pub const MAX_PARALLEL_TASKS: usize = 8;
/// Below this context window (the served model's `model_limits` cap; Flash: 16K) the tool is
/// not declared by default (O-146).
pub const MIN_CONTEXT: u32 = 32_000;
/// SIGTERM to the child's group, then SIGKILL after this long.
pub const KILL_GRACE: Duration = Duration::from_secs(2);
/// After the child exited, how long its stdout may stay open (a background grandchild).
const DRAIN: Duration = Duration::from_millis(200);
const POLL: Duration = Duration::from_millis(20);
/// The child's stderr kept for a failure report (the tail).
const STDERR_KEEP: usize = 4096;

// ---- model-facing texts ----

pub const TOOL_NAME: &str = T::NAME;
/// The tool's `Source::Extension` name.
pub const SOURCE: &str = "oxa:subagent";
pub const DESCRIPTION: &str = T::DESCRIPTION;
pub const SCHEMA: &str = T::SCHEMA;
const TOO_MANY: &str = T::TOO_MANY;
const TOO_MANY_CHAIN: &str = T::TOO_MANY_CHAIN;
const TOO_MANY_SESSION: &str = T::TOO_MANY_SESSION;
const NOT_ATTACHED: &str = T::NOT_ATTACHED;
const NOT_STARTED: &str = T::NOT_STARTED;
const ABORTED: &str = T::ABORTED;

// ---- notice texts (UI only) ----

/// `{agent}`, `{model}`, `{session}`.
pub const NOTICE_MODEL_IGNORED: &str =
    "subagent: agent {agent} asks for model {model}; ignored, the session's model {session} is used (one model is loaded)";
/// `{error}`.
pub const NOTICE_PARK_FAILED: &str =
    "subagent: parking the session before the child failed ({error}); its cache may be prefilled again";
/// `{hint}`, `{error}`.
pub const NOTICE_CLEAR_FAILED: &str = "subagent: clearing the engine state of {hint} failed ({error})";

/// One agent definition (`agents/*.md` with frontmatter `name`, `description`, `tools`,
/// `model`; the body is its system prompt).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDef {
    pub name: String,
    pub description: String,
    pub tools: Option<Vec<String>>,
    pub model: Option<String>,
    pub system_prompt: String,
    pub path: PathBuf,
    /// `true` = from the project (`.oxa/agents`), trust-gated.
    pub project: bool,
}

/// Discover agents: `<agent_dir>/agents/*.md`, then (when `project_trusted`)
/// `<cwd>/.oxa/agents/*.md`; a project agent overrides a user agent of the same name.
/// Sorted by name. A file without a string `name` and `description` is skipped (pi).
pub fn discover_agents(dirs: &Dirs, cwd: &Path, project_trusted: bool) -> Vec<AgentDef> {
    let mut by_name: std::collections::BTreeMap<String, AgentDef> = std::collections::BTreeMap::new();
    for a in load_dir(&dirs.agent_dir.join("agents"), false) {
        by_name.insert(a.name.clone(), a);
    }
    if project_trusted {
        let dir = cwd.join(oxa_proto::names::PROJECT_DIR).join("agents");
        for a in load_dir(&dir, true) {
            by_name.insert(a.name.clone(), a);
        }
    }
    by_name.into_values().collect()
}

fn load_dir(dir: &Path, project: bool) -> Vec<AgentDef> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "md") && p.is_file())
        .collect();
    paths.sort();
    let mut out = Vec::new();
    for path in paths {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let (pairs, body) = oxa_resources::parse_frontmatter(&text);
        let get = |k: &str| oxa_resources::frontmatter_get(&pairs, k).map(str::to_string);
        let (Some(name), Some(description)) = (get("name"), get("description")) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        out.push(AgentDef {
            name,
            description,
            tools: get("tools").and_then(|t| tool_list(&t)),
            model: get("model").filter(|m| !m.is_empty()),
            system_prompt: body.to_string(),
            path,
            project,
        });
    }
    out
}

/// `read, bash` or `[read, bash]` -> names; empty -> `None` (pi's `parseToolList`).
fn tool_list(raw: &str) -> Option<Vec<String>> {
    let raw = raw.trim();
    let raw = raw
        .strip_prefix('[')
        .and_then(|r| r.strip_suffix(']'))
        .unwrap_or(raw);
    let tools: Vec<String> = raw
        .split(',')
        .map(|t| t.trim().trim_matches(['"', '\'']).trim().to_string())
        .filter(|t| !t.is_empty())
        .collect();
    (!tools.is_empty()).then_some(tools)
}

/// The tool description with the agent list (sorted by name; configuration only, W3).
fn description_for(agents: &[AgentDef]) -> String {
    let list = if agents.is_empty() {
        "none".to_string()
    } else {
        agents
            .iter()
            .map(|a| {
                let source = if a.project { "project" } else { "user" };
                format!("{} ({source}): {}", a.name, one_line(&a.description))
            })
            .collect::<Vec<_>>()
            .join("; ")
    };
    format!("{DESCRIPTION}{}", T::AVAILABLE_AGENTS.replace("{list}", &list))
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The configuration read from the settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Config {
    soft_tokens: u32,
    result_cap: usize,
    /// `oxa.subagent.enabled` when the user set it (`None` = the O-146 default).
    enabled: Option<bool>,
    /// `oxa.subagent.smallContext: true` (older setting): declare on a small model too.
    small_context: bool,
    /// `oxa.subagent.remoteConcurrency`: children of one `tasks` call on a remote model that
    /// run at once (#458; local children always run one at a time).
    remote_concurrency: usize,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            soft_tokens: DEFAULT_SOFT_TOKENS,
            result_cap: DEFAULT_RESULT_CAP,
            enabled: None,
            small_context: false,
            remote_concurrency: DEFAULT_REMOTE_CONCURRENCY,
        }
    }
}

/// State shared by the hook set and its tool.
struct Inner {
    oxa_bin: PathBuf,
    agents: Vec<AgentDef>,
    config: Mutex<Config>,
    host: Mutex<Option<Arc<dyn Host>>>,
    /// The context window of the session's model, as last learned through the host.
    context_window: Mutex<Option<u32>>,
    /// The session's model runs on the Rust Flash engine (`park: "disk"`), as last learned.
    flash: Mutex<bool>,
    /// Next child number per parent session id.
    counters: Mutex<HashMap<String, u32>>,
    /// Children of each parent session that run or wait, over every call in flight (O-146:
    /// at most [`MAX_PARALLEL_TASKS`] per parent session; children cannot spawn, so this is
    /// the whole tree).
    pending: Mutex<HashMap<String, usize>>,
    /// Held for the whole call: one child process at a time.
    running: Mutex<()>,
    /// The process groups of the live children.
    child_pgid: Mutex<Vec<u32>>,
    /// Set by `shutdown`: the call in progress stops.
    abort: AtomicBool,
    /// The parent's autonomy switch (O-182, #202), read when a child's argv is built: the child
    /// gets `--standard`, `--auto` or `--yolo` for its level then. Not a global: the process
    /// hands its own switch in (`with_yolo`); `None` = no level flag.
    yolo: Mutex<Option<YoloSwitch>>,
}

/// The built-in subagent hook set. O-146: on by default for every model but the Rust Flash
/// engine and models below [`MIN_CONTEXT`]; `oxa.subagent.enabled` decides when set.
///
/// Provides one `Direct` tool `subagent` with pi's three shapes: single `{agent, task}`,
/// `{tasks: [..]}` (parallel = queued on the one engine) and `{chain: [..]}` (`{previous}`
/// substitution). Each child is the process `<oxa_bin> --mode json -p --no-session --model
/// <session model> --permission-mode <mode> [--standard|--auto|--yolo] [--tools ..] [--append-system-prompt <agent
/// prompt>] "Task: <task>"` (the parent's autonomy level, when it handed its switch in) in its own
/// process group, with the environment of the module docs.
/// Before the first child starts, the parent parks its state (`HostAction::ParkSession`).
/// Child JSON records stream to `live` as progress lines; abort kills the child's process
/// group. The tool result is the final assistant text of each child (W4: no timings).
pub struct SubagentHooks {
    inner: Arc<Inner>,
    tool: Arc<dyn Tool>,
}

impl SubagentHooks {
    pub fn new(oxa_bin: PathBuf, agents: Vec<AgentDef>) -> SubagentHooks {
        let description = description_for(&agents);
        let inner = Arc::new(Inner {
            oxa_bin,
            agents,
            config: Mutex::new(Config::default()),
            host: Mutex::new(None),
            context_window: Mutex::new(None),
            flash: Mutex::new(false),
            counters: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            running: Mutex::new(()),
            child_pgid: Mutex::new(Vec::new()),
            abort: AtomicBool::new(false),
            yolo: Mutex::new(None),
        });
        let tool: Arc<dyn Tool> = Arc::new(SubagentTool {
            spec: ToolSpec {
                name: TOOL_NAME.to_string(),
                description,
                schema_json: SCHEMA.to_string(),
                class: ToolClass::Exec,
                exposure: Exposure::Direct,
                // Not `builtin:*`: `resolve_toolset` makes those opt-in through `defaultTools`,
                // and not `Source::Builtin`: an epoch keeps built-ins from the previous one, so a
                // model switch to Flash could not drop the tool. It is selected like a built-in
                // through `Tool::default_selected` (a member of the default selection).
                source: Source::Extension(SOURCE.to_string()),
            },
            inner: inner.clone(),
        });
        SubagentHooks { inner, tool }
    }

    // The names the child side and the binary need (`lib.rs` re-exports the type only).
    pub const ENV_SESSION_HINT: &'static str = ENV_SESSION_HINT;
    pub const ENV_SUBAGENT_DEPTH: &'static str = ENV_SUBAGENT_DEPTH;
    pub const ENV_CHILD_SOFT_TOKENS: &'static str = ENV_CHILD_SOFT_TOKENS;
    pub const SETTING_ENABLED: &'static str = SETTING_ENABLED;
    pub const TOOL_NAME: &'static str = TOOL_NAME;
    pub const MAX_PARALLEL_TASKS: usize = MAX_PARALLEL_TASKS;
    pub const MIN_CONTEXT: u32 = MIN_CONTEXT;

    /// O-182, #202: the parent's autonomy switch. Each child inherits the level the parent has
    /// when the child starts (`--standard`, `--auto` or `--yolo`).
    pub fn with_yolo(self, switch: YoloSwitch) -> SubagentHooks {
        *lock(&self.inner.yolo) = Some(switch);
        self
    }

    /// Read `oxa.subagent.enabled`, `oxa.subagent.softTokens`, `oxa.subagent.resultCap` and
    /// `oxa.subagent.smallContext`.
    pub fn with_settings(self, settings: &Settings) -> SubagentHooks {
        *lock(&self.inner.config) = Config {
            soft_tokens: settings.u32(SETTING_SOFT_TOKENS, DEFAULT_SOFT_TOKENS).max(1),
            result_cap: settings.u32(SETTING_RESULT_CAP, DEFAULT_RESULT_CAP as u32) as usize,
            enabled: enabled_setting(settings),
            small_context: settings.bool(SETTING_SMALL_CONTEXT, false),
            remote_concurrency: (settings.u32(SETTING_REMOTE_CONCURRENCY, DEFAULT_REMOTE_CONCURRENCY as u32)
                as usize)
                .clamp(1, MAX_PARALLEL_TASKS),
        };
        self
    }

    /// O-146: whether the tool is declared for a model with this context window (`None` =
    /// unknown) on this engine. Explicit `oxa.subagent.enabled` wins (`true` declares it on any
    /// model, `false` never); otherwise it is on, except on the Rust Flash engine and below
    /// [`MIN_CONTEXT`] (the served model's `model_limits` cap), where `smallContext: true` still
    /// declares it. An unknown window counts as large.
    pub fn declared_for(&self, window: Option<u32>, flash: bool) -> bool {
        let c = *lock(&self.inner.config);
        match c.enabled {
            Some(on) => on,
            None if c.small_context => true,
            None => !flash && !window.is_some_and(|w| w < MIN_CONTEXT),
        }
    }

    /// The agents this hook set offers (sorted by name).
    pub fn agents(&self) -> &[AgentDef] {
        &self.inner.agents
    }

    /// Learn the context window of the session's model through the host. Only from a context
    /// where the session serves host calls (attach, fired hook points).
    fn learn_limits(&self) {
        let host = lock(&self.inner.host).clone();
        if let Some(host) = host {
            if let Ok(v) = host.call(HostAction::EngineLimits) {
                let window = v
                    .get("contextWindow")
                    .and_then(Value::as_u64)
                    .map(|n| u32::try_from(n).unwrap_or(u32::MAX));
                *lock(&self.inner.context_window) = window;
                *lock(&self.inner.flash) = v.get("parkDiskOnly").and_then(Value::as_bool).unwrap_or(false);
            }
        }
    }
}

/// `oxa.subagent.enabled` as the user set it; `None` when unset or `null`.
pub fn enabled_setting(settings: &Settings) -> Option<bool> {
    settings.get(SETTING_ENABLED).and_then(Value::as_bool)
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl Hooks for SubagentHooks {
    fn fire(&self, p: HookPoint, _payload: &Value, _ui: &mut dyn Ui) -> HookReply {
        // A model switch opens a new epoch: the declaration is evaluated again for its model.
        if matches!(p, HookPoint::SessionStart | HookPoint::ModelSelect) {
            self.learn_limits();
        }
        HookReply::Continue
    }
    /// The tool list is part of the frozen prefix (W3): it changes only with what was learned at
    /// `SessionStart` / `ModelSelect` (a new epoch).
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        let window = *lock(&self.inner.context_window);
        let flash = *lock(&self.inner.flash);
        if self.declared_for(window, flash) {
            vec![self.tool.clone()]
        } else {
            Vec::new()
        }
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
        Vec::new()
    }
    /// Only the points `fire` acts on.
    fn wants(&self, p: HookPoint) -> bool {
        matches!(p, HookPoint::SessionStart | HookPoint::ModelSelect)
    }
    fn name(&self) -> &str {
        "builtin:subagent"
    }
    fn attach(&self, host: Arc<dyn Host>) {
        *lock(&self.inner.host) = Some(host);
        // `attach` runs on a helper thread while the session serves the host (E-INT 2).
        self.learn_limits();
    }
    /// Kill the live child's process group (SIGTERM, SIGKILL after the grace); the call in
    /// progress returns aborted and its queued tasks do not start. Idempotent.
    fn shutdown(&self) {
        self.inner.abort.store(true, Ordering::SeqCst);
        let pgids = lock(&self.inner.child_pgid).clone();
        for pgid in pgids {
            kill_group(pgid);
        }
    }
}

/// SIGTERM to the group, SIGKILL when it is still alive after [`KILL_GRACE`].
fn kill_group(pgid: u32) {
    signal_group(pgid, "TERM");
    let end = Instant::now() + KILL_GRACE;
    while Instant::now() < end {
        if !group_alive(pgid) {
            return;
        }
        std::thread::sleep(POLL);
    }
    signal_group(pgid, "KILL");
}

/// `kill -<sig> -- -<pgid>` (no `unsafe` in this crate).
fn signal_group(pgid: u32, sig: &str) -> bool {
    Command::new("kill")
        .arg(format!("-{sig}"))
        .arg("--")
        .arg(format!("-{pgid}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn group_alive(pgid: u32) -> bool {
    signal_group(pgid, "0")
}

// ---- the tool ----

struct SubagentTool {
    spec: ToolSpec,
    inner: Arc<Inner>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Item {
    agent: String,
    task: String,
    cwd: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    Single,
    Parallel,
    Chain,
}

impl Shape {
    fn as_str(self) -> &'static str {
        match self {
            Shape::Single => "single",
            Shape::Parallel => "parallel",
            Shape::Chain => "chain",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Completed,
    Failed,
    Aborted,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Completed => "completed",
            Status::Failed => "failed",
            Status::Aborted => "aborted",
        }
    }
}

/// What one child left behind.
#[derive(Debug, Clone)]
struct ChildResult {
    agent: String,
    status: Status,
    /// The text of each assistant message (`None` = no text block), in order.
    texts: Vec<Option<String>>,
    stop_reason: Option<String>,
    error_message: Option<String>,
    signal: Option<i32>,
    stderr: String,
    exit: Option<i32>,
    hint: Option<String>,
}

impl ChildResult {
    fn unstarted(agent: &str, status: Status, error: String) -> ChildResult {
        ChildResult {
            agent: agent.to_string(),
            status,
            texts: Vec::new(),
            stop_reason: None,
            error_message: Some(error),
            signal: None,
            stderr: String::new(),
            exit: None,
            hint: None,
        }
    }
    fn failed(&self) -> bool {
        self.status != Status::Completed
    }
    /// pi's `getFinalOutput`: the first text of the last assistant message that has one.
    fn final_output(&self) -> String {
        self.texts
            .iter()
            .rev()
            .flatten()
            .next()
            .cloned()
            .unwrap_or_default()
    }
    /// pi's `getFailureEvidence`.
    fn failure_evidence(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        let mut add = |label: &str, value: Option<String>| {
            if let Some(v) = value {
                let v = v.trim().to_string();
                if !v.is_empty() && !parts.iter().any(|p| p.ends_with(&v)) {
                    parts.push(format!("{label}: {v}"));
                }
            }
        };
        add(T::EVIDENCE_ERROR, self.error_message.clone());
        add(T::EVIDENCE_SIGNAL, self.signal.map(|s| s.to_string()));
        add(T::EVIDENCE_STDERR, Some(self.stderr.clone()));
        add(T::EVIDENCE_OUTPUT, Some(self.final_output()));
        if parts.is_empty() {
            T::NO_OUTPUT.to_string()
        } else {
            parts.join("\n\n")
        }
    }
    fn output(&self) -> String {
        if self.failed() {
            self.failure_evidence()
        } else {
            let out = self.final_output();
            if out.is_empty() {
                T::NO_OUTPUT.to_string()
            } else {
                out
            }
        }
    }
}

/// The limits the session reported (`HostAction::EngineLimits`).
#[derive(Debug, Clone)]
struct Limits {
    session_id: String,
    model: String,
    /// The session's model runs at a cloud provider through oxc.
    remote: bool,
    context_window: u32,
    compact_at: Option<u32>,
}

fn parse_limits(v: &Value) -> Limits {
    let num = |k: &str| {
        v.get(k)
            .and_then(Value::as_u64)
            .map(|n| u32::try_from(n).unwrap_or(u32::MAX))
    };
    Limits {
        session_id: v
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        model: v.get("model").and_then(Value::as_str).unwrap_or("").to_string(),
        remote: v.get("remote").and_then(Value::as_bool).unwrap_or(false),
        context_window: num("contextWindow").unwrap_or(u32::MAX),
        compact_at: num("compactAt"),
    }
}

/// The child's soft compaction limit: the setting, never above the parent's `compactAt` and
/// always below the context window.
fn child_soft_tokens(setting: u32, l: &Limits) -> u32 {
    setting
        .min(l.compact_at.unwrap_or(u32::MAX))
        .min(l.context_window.saturating_sub(1))
        .max(1)
}

fn parse_items(v: Option<&Value>) -> Vec<Item> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|x| Item {
                    agent: x.get("agent").and_then(Value::as_str).unwrap_or("").to_string(),
                    task: x.get("task").and_then(Value::as_str).unwrap_or("").to_string(),
                    cwd: x.get("cwd").and_then(Value::as_str).map(str::to_string),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn available_list(agents: &[AgentDef]) -> String {
    let list: Vec<String> = agents
        .iter()
        .map(|a| format!("{} ({})", a.name, if a.project { "project" } else { "user" }))
        .collect();
    if list.is_empty() {
        "none".to_string()
    } else {
        list.join(", ")
    }
}

impl Tool for SubagentTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    /// One child process per task, sandboxed like the session itself: `Exec`, so plan mode
    /// refuses it and the loop runs it serially (never in a parallel read batch).
    fn access(&self, _args: &Value, _cx: &ToolCtx) -> Result<Vec<Access>, String> {
        Ok(vec![Access::Exec {
            command: TOOL_NAME.to_string(),
            net: false,
        }])
    }
    fn run(&self, args: &Value, cx: &ToolCtx, live: &mut dyn FnMut(&str)) -> ToolOutput {
        let mut ui = oxa_proto::HeadlessUi::default();
        self.run_with_ui(args, cx, live, &mut ui)
    }
    /// Serial with the session's UI (warning notices); never in a parallel batch.
    /// O-146: a member of oxa's default tool selection where the hook set offers it.
    fn default_selected(&self) -> bool {
        true
    }
    fn needs_ui(&self) -> bool {
        true
    }
    fn run_with_ui(
        &self,
        args: &Value,
        cx: &ToolCtx,
        live: &mut dyn FnMut(&str),
        ui: &mut dyn Ui,
    ) -> ToolOutput {
        let inner = &self.inner;
        let chain = parse_items(args.get("chain"));
        let tasks = parse_items(args.get("tasks"));
        let agent = args.get("agent").and_then(Value::as_str).unwrap_or("");
        let task = args.get("task").and_then(Value::as_str).unwrap_or("");
        let has_single = !agent.is_empty() && !task.is_empty();
        let modes = usize::from(!chain.is_empty()) + usize::from(!tasks.is_empty()) + usize::from(has_single);
        if modes != 1 {
            return ToolOutput::error(T::INVALID_MODE.replace("{available}", &available_list(&inner.agents)));
        }
        let (shape, items) = if !chain.is_empty() {
            if chain.len() > MAX_PARALLEL_TASKS {
                return ToolOutput::error(TOO_MANY_CHAIN.replace("{n}", &chain.len().to_string()));
            }
            (Shape::Chain, chain)
        } else if !tasks.is_empty() {
            if tasks.len() > MAX_PARALLEL_TASKS {
                return ToolOutput::error(TOO_MANY.replace("{n}", &tasks.len().to_string()));
            }
            (Shape::Parallel, tasks)
        } else {
            (
                Shape::Single,
                vec![Item {
                    agent: agent.to_string(),
                    task: task.to_string(),
                    cwd: args.get("cwd").and_then(Value::as_str).map(str::to_string),
                }],
            )
        };

        let Some(host) = lock(&inner.host).clone() else {
            return ToolOutput::error(NOT_ATTACHED);
        };
        let limits = match host.call(HostAction::EngineLimits) {
            Ok(v) => parse_limits(&v),
            Err(e) => return ToolOutput::error(T::FAILED.replace("{error}", &e.message)),
        };
        // O-146: at most eight children per parent session, running or waiting, over every
        // call in flight. Reserved before this call waits for its turn, so a call queued
        // behind the running one counts too; refused calls start nothing.
        let budget = match Budget::reserve(inner, &limits.session_id, items.len()) {
            Ok(b) => b,
            Err(pending) => {
                return ToolOutput::error(
                    TOO_MANY_SESSION
                        .replace("{pending}", &pending.to_string())
                        .replace("{n}", &items.len().to_string()),
                )
            }
        };
        // Concurrency 1: one call (and so one child) at a time.
        let _running = lock(&inner.running);
        inner.abort.store(false, Ordering::SeqCst);
        let config = *lock(&inner.config);
        let mut run = Run {
            inner,
            host: host.as_ref(),
            limits,
            config,
            cx,
            ui,
            live,
            parked: false,
            warned_models: Vec::new(),
            remote_models: HashMap::new(),
            n: items.len(),
            agents: items.iter().map(|i| i.agent.clone()).collect(),
            done: vec![None; items.len()],
            activity: vec![None; items.len()],
            budget,
        };

        let mut results: Vec<ChildResult> = Vec::new();
        match shape {
            Shape::Chain => {
                let mut previous = String::new();
                for (i, item) in items.iter().enumerate() {
                    let task = item.task.replace("{previous}", &previous);
                    let r = run.child(i, item, &task);
                    let failed = r.failed();
                    previous = r.final_output();
                    results.push(r);
                    if failed {
                        break;
                    }
                }
            }
            Shape::Parallel if run.concurrency(&items) > 1 => {
                let limit = run.concurrency(&items);
                results = run.concurrent(&items, limit);
            }
            Shape::Parallel | Shape::Single => {
                for (i, item) in items.iter().enumerate() {
                    let r = run.child(i, item, &item.task);
                    results.push(r);
                }
            }
        }
        let text = match shape {
            Shape::Single => {
                let r = &results[0];
                if r.failed() {
                    let why = r.stop_reason.clone().filter(|s| !s.is_empty() && s != "stop");
                    T::SINGLE_FAILED
                        .replace("{why}", why.as_deref().unwrap_or(r.status.as_str()))
                        .replace("{output}", &r.output())
                } else {
                    r.output()
                }
            }
            Shape::Chain => {
                let last = results.last().expect("a chain has at least one step");
                if last.failed() {
                    T::CHAIN_STOPPED
                        .replace("{step}", &results.len().to_string())
                        .replace("{agent}", &last.agent)
                        .replace("{output}", &last.output())
                } else {
                    last.output()
                }
            }
            Shape::Parallel => {
                let ok = results.iter().filter(|r| !r.failed()).count();
                let summaries: Vec<String> = results
                    .iter()
                    .map(|r| {
                        T::PARALLEL_ITEM
                            .replace("{agent}", &r.agent)
                            .replace("{status}", r.status.as_str())
                            .replace("{output}", &r.output())
                    })
                    .collect();
                let head = T::PARALLEL_HEADER
                    .replace("{ok}", &ok.to_string())
                    .replace("{total}", &results.len().to_string());
                format!("{head}{}", summaries.join(T::PARALLEL_SEPARATOR))
            }
        };
        let is_error = results.iter().any(ChildResult::failed);
        let details = json!({
            "mode": shape.as_str(),
            "results": results.iter().map(|r| json!({
                "agent": r.agent,
                "status": r.status.as_str(),
                "exitCode": r.exit,
                "sessionHint": r.hint,
            })).collect::<Vec<_>>(),
            "isError": is_error,
        });
        ToolOutput {
            text: cap_result(&text, config.result_cap, cx.spill_dir),
            is_error,
            ui: details,
        }
    }
}

/// Cut `text` at `cap` bytes on a UTF-8 boundary and append the fixed truncation note naming the
/// spill file (`<spill_dir>/subagent-<sha256>.txt`, the full text). Deterministic.
fn cap_result(text: &str, cap: usize, spill_dir: &Path) -> String {
    if text.len() <= cap {
        return text.to_string();
    }
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    let mut name = String::from("subagent-");
    for b in Sha256::digest(text.as_bytes()) {
        let _ = write!(name, "{b:02x}");
    }
    name.push_str(".txt");
    let path = spill_dir.join(name);
    let saved = spill(&path, text.as_bytes());
    let bytes = text.len().to_string();
    let note = if saved {
        T::TRUNCATED_SAVED
            .replace("{bytes}", &bytes)
            .replace("{path}", &path.display().to_string())
    } else {
        T::TRUNCATED_NOT_SAVED.replace("{bytes}", &bytes)
    };
    let budget = cap.saturating_sub(note.len() + 2);
    let mut cut = budget.min(text.len());
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n\n{note}", &text[..cut])
}

fn spill(path: &Path, bytes: &[u8]) -> bool {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let Some(dir) = path.parent() else {
        return false;
    };
    if std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .is_err()
    {
        return false;
    }
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(mut f) => {
            use std::io::Write as _;
            f.write_all(bytes).is_ok()
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => path.is_file(),
        Err(_) => false,
    }
}

/// One call in progress.
struct Run<'a, 'b> {
    inner: &'a Inner,
    host: &'a dyn Host,
    limits: Limits,
    config: Config,
    cx: &'a ToolCtx<'b>,
    ui: &'a mut dyn Ui,
    live: &'a mut dyn FnMut(&str),
    parked: bool,
    warned_models: Vec<String>,
    /// #458: whether a model runs remotely, as the host said during this call.
    remote_models: HashMap<String, bool>,
    /// Number of tasks and their agents (for the progress lines).
    n: usize,
    agents: Vec<String>,
    /// Status of each finished task.
    done: Vec<Option<Status>>,
    /// The activity of each running task.
    activity: Vec<Option<String>>,
    /// This call's share of the parent session's eight children.
    budget: Budget<'a>,
}

/// A call's reservation in [`Inner::pending`]: one unit per task that runs or waits. A
/// finished task gives its unit back; dropping the reservation gives back the rest (a chain
/// that stopped early, an abort, a panic).
struct Budget<'a> {
    inner: &'a Inner,
    session: String,
    left: usize,
}

impl<'a> Budget<'a> {
    /// Reserve `n` children for `session`, or `Err(children already running or waiting)`
    /// when that would pass [`MAX_PARALLEL_TASKS`].
    fn reserve(inner: &'a Inner, session: &str, n: usize) -> Result<Budget<'a>, usize> {
        let mut pending = lock(&inner.pending);
        let now = pending.get(session).copied().unwrap_or(0);
        if now.saturating_add(n) > MAX_PARALLEL_TASKS {
            return Err(now);
        }
        pending.insert(session.to_string(), now + n);
        Ok(Budget {
            inner,
            session: session.to_string(),
            left: n,
        })
    }

    fn release(&mut self, n: usize) {
        let n = n.min(self.left);
        if n == 0 {
            return;
        }
        self.left -= n;
        let mut pending = lock(&self.inner.pending);
        if let Some(now) = pending.get_mut(&self.session) {
            *now = now.saturating_sub(n);
            if *now == 0 {
                pending.remove(&self.session);
            }
        }
    }
}

impl Drop for Budget<'_> {
    fn drop(&mut self) {
        self.release(self.left);
    }
}

/// A child ready to start: its argv, environment and working directory.
struct Prepared {
    agent: String,
    argv: Vec<String>,
    env: Vec<(String, String)>,
    cwd: PathBuf,
    hint: String,
    /// Its model runs at a cloud provider (it does not take the local engine's slot).
    remote: bool,
}

/// What a concurrently running child reports to the call's thread (which owns the UI).
enum Event {
    Activity(usize, String),
    Skipped(usize, ChildResult),
    Done(usize, ChildResult, Prepared),
}

impl Run<'_, '_> {
    fn stopped(&self) -> bool {
        self.cx.cancel.is_cancelled() || self.inner.abort.load(Ordering::SeqCst)
    }

    /// Task `k` is running and doing `activity`; redraw the progress block.
    fn progress(&mut self, k: usize, activity: &str) {
        if let Some(a) = self.activity.get_mut(k) {
            *a = Some(activity.to_string());
        }
        self.render();
    }

    /// The progress block: one line per task (`done` / the running task's activity / `queued`).
    fn render(&mut self) {
        let mut lines = Vec::with_capacity(self.n);
        for j in 0..self.n {
            let line = if let Some(status) = self.done[j] {
                format!(
                    "subagent {}/{} {}: {}",
                    j + 1,
                    self.n,
                    self.agents[j],
                    match status {
                        Status::Completed => "done",
                        Status::Failed => "failed",
                        Status::Aborted => "aborted",
                    }
                )
            } else if let Some(activity) = &self.activity[j] {
                format!("subagent {}/{} {}: {activity}", j + 1, self.n, self.agents[j])
            } else {
                format!("subagent {}/{}, queued", j + 1, self.n)
            };
            lines.push(line);
        }
        (self.live)(&lines.join("\n"));
    }

    fn notify(&mut self, text: String) {
        self.ui.notify(&text, Level::Warning);
    }

    fn finish(&mut self, k: usize, r: ChildResult) -> ChildResult {
        self.done[k] = Some(r.status);
        self.activity[k] = None;
        self.budget.release(1);
        r
    }

    /// Whether `model` runs at a cloud provider, asked once per model and call (#458). A host
    /// that cannot tell means local.
    fn model_remote(&mut self, model: &str) -> bool {
        if model == self.limits.model {
            return self.limits.remote;
        }
        if let Some(r) = self.remote_models.get(model) {
            return *r;
        }
        let r = self
            .host
            .call(HostAction::ModelInfo(model.to_string()))
            .ok()
            .and_then(|v| v.get("remote").and_then(Value::as_bool))
            .unwrap_or(false);
        self.remote_models.insert(model.to_string(), r);
        r
    }

    /// The model an agent's child runs (#458): the agent's own `model:` when that model is
    /// remote or the session's is (neither shares the one local slot with the parent),
    /// otherwise the session's model. The flag says whether it is remote.
    fn agent_model(&mut self, agent: &AgentDef) -> (String, bool) {
        if let Some(m) = agent.model.as_deref().filter(|m| *m != self.limits.model) {
            let remote = self.model_remote(m);
            if remote || self.limits.remote {
                return (m.to_string(), remote);
            }
        }
        (self.limits.model.clone(), self.limits.remote)
    }

    /// How many children of this `tasks` call may run at once: more than one only when some
    /// child runs remotely (local children still take the one slot in turn).
    fn concurrency(&mut self, items: &[Item]) -> usize {
        if self.config.remote_concurrency <= 1 {
            return 1;
        }
        let agents: Vec<AgentDef> = items
            .iter()
            .filter_map(|item| self.inner.agents.iter().find(|a| a.name == item.agent).cloned())
            .collect();
        if agents.iter().any(|a| self.agent_model(a).1) {
            self.config.remote_concurrency.min(items.len())
        } else {
            1
        }
    }

    /// Prepare task `k`: its agent, model, hint and argv; park the parent before the first
    /// child. `Err` is the task's finished result (unknown agent, aborted before start).
    fn prepare(&mut self, k: usize, item: &Item, task: &str) -> Result<Prepared, Box<ChildResult>> {
        if self.stopped() {
            let r = ChildResult::unstarted(&item.agent, Status::Aborted, NOT_STARTED.to_string());
            return Err(Box::new(self.finish(k, r)));
        }
        let Some(agent) = self.inner.agents.iter().find(|a| a.name == item.agent).cloned() else {
            let available: Vec<String> = self
                .inner
                .agents
                .iter()
                .map(|a| format!("\"{}\"", a.name))
                .collect();
            let available = if available.is_empty() {
                "none".to_string()
            } else {
                available.join(", ")
            };
            let r = ChildResult::unstarted(
                &item.agent,
                Status::Failed,
                // The model's text (the agent name) is filled in last.
                T::UNKNOWN_AGENT
                    .replace("{available}", &available)
                    .replace("{agent}", &item.agent),
            );
            return Err(Box::new(self.finish(k, r)));
        };
        let (model, remote) = self.agent_model(&agent);
        if let Some(m) = agent.model.as_deref() {
            if m != model && !self.warned_models.iter().any(|w| w == &agent.name) {
                self.warned_models.push(agent.name.clone());
                let text = NOTICE_MODEL_IGNORED
                    .replace("{agent}", &agent.name)
                    .replace("{model}", m)
                    .replace("{session}", &self.limits.model);
                self.notify(text);
            }
        }

        // Park the parent once per call, right before its first child.
        if !self.parked {
            self.parked = true;
            self.progress(k, "parking the session");
            if let Err(e) = self.host.call(HostAction::ParkSession) {
                let text = NOTICE_PARK_FAILED.replace("{error}", &e.message);
                self.notify(text);
            }
            if self.stopped() {
                let r = ChildResult::unstarted(&item.agent, Status::Aborted, NOT_STARTED.to_string());
                return Err(Box::new(self.finish(k, r)));
            }
        }

        let hint = {
            let mut counters = lock(&self.inner.counters);
            let n = counters.entry(self.limits.session_id.clone()).or_insert(0);
            *n += 1;
            format!("{}.s{}", self.limits.session_id, n)
        };
        let soft = child_soft_tokens(self.config.soft_tokens, &self.limits);
        let mut argv: Vec<String> = vec!["--mode".into(), "json".into(), "-p".into(), "--no-session".into()];
        if !model.is_empty() {
            argv.push("--model".into());
            argv.push(model);
        }
        argv.push("--permission-mode".into());
        argv.push(self.cx.mode.as_str().into());
        // O-182, #202: the parent's autonomy level at this moment (a later child sees a later
        // state), always explicit so a child never takes a higher `permissions.defaultMode`.
        if let Some(level) = lock(&self.inner.yolo).as_ref().map(YoloSwitch::level) {
            argv.push(format!("--{}", level.as_str()));
        }
        if let Some(tools) = agent.tools.as_ref().filter(|t| !t.is_empty()) {
            argv.push("--tools".into());
            argv.push(tools.join(","));
        }
        if !agent.system_prompt.trim().is_empty() {
            argv.push("--append-system-prompt".into());
            argv.push(agent.system_prompt.clone());
        }
        argv.push(T::TASK.replace("{task}", task));
        let env = vec![
            (ENV_SESSION_HINT.to_string(), hint.clone()),
            (ENV_SUBAGENT_DEPTH.to_string(), "1".to_string()),
            (ENV_CHILD_SOFT_TOKENS.to_string(), soft.to_string()),
        ];
        let cwd = match &item.cwd {
            Some(c) => self.cx.cwd.join(c),
            None => self.cx.cwd.to_path_buf(),
        };
        Ok(Prepared {
            agent: agent.name,
            argv,
            env,
            cwd,
            hint,
            remote,
        })
    }

    /// After a child: clear its engine state (EG5, whatever the outcome) and record it.
    fn cleanup(&mut self, k: usize, p: &Prepared, mut r: ChildResult) -> ChildResult {
        r.hint = Some(p.hint.clone());
        // A remote child holds no engine state; a local child under a remote parent does, and
        // the host clears it whatever the parent's own capabilities say.
        let cleared = if p.remote {
            Ok(Value::Null)
        } else {
            self.host.call(HostAction::ClearEngineHint(p.hint.clone()))
        };
        if let Err(e) = cleared {
            let text = NOTICE_CLEAR_FAILED
                .replace("{hint}", &p.hint)
                .replace("{error}", &e.message);
            self.notify(text);
        }
        let r = self.finish(k, r);
        // Its line now reads done / failed / aborted; the next task is still queued.
        self.render();
        r
    }

    /// Run task `k` (one child), its cleanup included.
    fn child(&mut self, k: usize, item: &Item, task: &str) -> ChildResult {
        let p = match self.prepare(k, item, task) {
            Ok(p) => p,
            Err(r) => return *r,
        };
        self.progress(k, "running");
        let (inner, cancel) = (self.inner, self.cx.cancel);
        let r = wait_child(inner, cancel, self.host, &p, &mut |a| self.progress(k, &a));
        self.cleanup(k, &p, r)
    }

    /// #458: a `tasks` call with remote children runs up to `limit` children at once; local
    /// children still take the one local slot in turn. Results are in call order.
    fn concurrent(&mut self, items: &[Item], limit: usize) -> Vec<ChildResult> {
        let mut results: Vec<Option<ChildResult>> = vec![None; items.len()];
        let mut ready = std::collections::VecDeque::new();
        for (k, item) in items.iter().enumerate() {
            match self.prepare(k, item, &item.task) {
                Ok(p) => ready.push_back((k, p)),
                Err(r) => results[k] = Some(*r),
            }
        }
        let workers = limit.min(ready.len());
        let queue = Mutex::new(ready);
        let local = Mutex::new(());
        let (inner, cancel) = (self.inner, self.cx.cancel);
        let host = self.host;
        let (tx, rx) = mpsc::channel::<Event>();
        std::thread::scope(|scope| {
            for _ in 0..workers {
                let (tx, queue, local) = (tx.clone(), &queue, &local);
                scope.spawn(move || loop {
                    let Some((k, p)) = lock(queue).pop_front() else {
                        break;
                    };
                    let _slot = (!p.remote).then(|| lock(local));
                    if cancel.is_cancelled() || inner.abort.load(Ordering::SeqCst) {
                        let r = ChildResult::unstarted(&p.agent, Status::Aborted, NOT_STARTED.to_string());
                        let _ = tx.send(Event::Skipped(k, r));
                        continue;
                    }
                    let _ = tx.send(Event::Activity(k, "running".to_string()));
                    let r = wait_child(inner, cancel, host, &p, &mut |a| {
                        let _ = tx.send(Event::Activity(k, a));
                    });
                    let _ = tx.send(Event::Done(k, r, p));
                });
            }
            drop(tx);
            for ev in rx {
                match ev {
                    Event::Activity(k, a) => self.progress(k, &a),
                    Event::Skipped(k, r) => results[k] = Some(self.finish(k, r)),
                    Event::Done(k, r, p) => results[k] = Some(self.cleanup(k, &p, r)),
                }
            }
        });
        results
            .into_iter()
            .map(|r| r.expect("every task reports a result"))
            .collect()
    }
}

/// Spawn one child and wait for it; `on` gets its activity. An abort (the cancel token,
/// [`Hooks::shutdown`]) kills its process group.
fn wait_child(
    inner: &Inner,
    cancel: &CancelToken,
    host: &dyn Host,
    p: &Prepared,
    on: &mut dyn FnMut(String),
) -> ChildResult {
    let stopped = || cancel.is_cancelled() || inner.abort.load(Ordering::SeqCst);
    // Read at spawn, not when tasks are queued: earlier children and the park may
    // have consumed tokens. Parallel children, like active tools, may finish in flight.
    let mut argv = p.argv.clone();
    let remaining = match host.call(HostAction::EngineLimits) {
        Ok(v) => v.get("remainingTokens").and_then(Value::as_u64),
        Err(e) => return ChildResult::unstarted(&p.agent, Status::Failed, e.message),
    };
    if let Some(left) = remaining {
        if left == 0 {
            return ChildResult::unstarted(&p.agent, Status::Aborted, NOT_STARTED.to_string());
        }
        // Insert before the task, leaving the worker's prompt prefix unchanged.
        let at = argv.len() - 1;
        argv.splice(at..at, ["--max-tokens".to_string(), left.to_string()]);
    }
    let mut rec = |line: &[u8], result: &mut ChildResult| {
        if let Some(a) = record(line, result) {
            on(a);
        }
        if remaining.is_some() {
            if let Ok(v) = serde_json::from_slice::<Value>(line) {
                if v["type"] == "message_end" && v["message"]["role"] == "assistant" {
                    let u = &v["message"]["usage"];
                    // JSON-mode input already excludes cacheRead (oxa-rpc/events.rs).
                    let tokens = u["input"]
                        .as_u64()
                        .unwrap_or(0)
                        .saturating_add(u["output"].as_u64().unwrap_or(0));
                    let _ = host.call(HostAction::AccountRunTokens(tokens));
                }
            }
        }
    };
    let mut result = ChildResult::unstarted(&p.agent, Status::Failed, String::new());
    result.error_message = None;
    let mut child = match spawn(&inner.oxa_bin, &argv, &p.env, &p.cwd) {
        Ok(c) => c,
        Err(e) => {
            result.error_message = Some(T::SPAWN_FAILED.replace("{error}", &e.to_string()));
            return result;
        }
    };
    let pgid = child.id();
    lock(&inner.child_pgid).push(pgid);

    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let stdout = child.stdout.take();
    std::thread::spawn(move || {
        let Some(out) = stdout else { return };
        let mut reader = BufReader::new(out);
        loop {
            let mut line = Vec::new();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
            }
        }
    });
    let stderr_buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let stderr_thread = child.stderr.take().map(|mut err| {
        let buf = stderr_buf.clone();
        std::thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            loop {
                match err.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut b = lock(&buf);
                        b.extend_from_slice(&chunk[..n]);
                        if b.len() > STDERR_KEEP * 2 {
                            let cut = b.len() - STDERR_KEEP;
                            b.drain(..cut);
                        }
                    }
                }
            }
        })
    });

    let mut exited: Option<(std::process::ExitStatus, Instant)> = None;
    let mut eof = false;
    let mut term_at: Option<Instant> = None;
    let mut killed = false;
    loop {
        match rx.recv_timeout(POLL) {
            Ok(line) => rec(&line, &mut result),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => eof = true,
        }
        if exited.is_none() {
            if let Ok(Some(status)) = child.try_wait() {
                exited = Some((status, Instant::now()));
            }
        }
        if term_at.is_none() && exited.is_none() && stopped() {
            signal_group(pgid, "TERM");
            term_at = Some(Instant::now());
        }
        if let Some(t) = term_at {
            if !killed && t.elapsed() >= KILL_GRACE {
                signal_group(pgid, "KILL");
                killed = true;
            }
        }
        if let Some((_, at)) = exited {
            if eof || at.elapsed() >= DRAIN {
                break;
            }
        }
    }
    // Whatever the child left in its group (a background grandchild) goes with it.
    if group_alive(pgid) {
        kill_group(pgid);
    }
    lock(&inner.child_pgid).retain(|g| *g != pgid);
    while let Ok(line) = rx.try_recv() {
        rec(&line, &mut result);
    }
    if let Some(t) = stderr_thread {
        // The pipe closed with the group; never wait long for it.
        let end = Instant::now() + DRAIN;
        while !t.is_finished() && Instant::now() < end {
            std::thread::sleep(POLL);
        }
    }
    let stderr = String::from_utf8_lossy(&lock(&stderr_buf)).into_owned();
    result.stderr = tail(&stderr, STDERR_KEEP).to_string();

    let status = exited.map(|(s, _)| s);
    result.exit = status.and_then(|s| s.code());
    let signal = status.and_then(|s| {
        use std::os::unix::process::ExitStatusExt as _;
        s.signal()
    });
    if term_at.is_some() {
        result.status = Status::Aborted;
        result.error_message.get_or_insert_with(|| ABORTED.to_string());
    } else if let Some(sig) = signal {
        result.status = Status::Failed;
        result.signal = Some(sig);
        result
            .error_message
            .get_or_insert_with(|| T::SIGNALLED.replace("{signal}", &sig.to_string()));
    } else if result.exit != Some(0) {
        result.status = Status::Failed;
        let code = result.exit.map_or("unknown".to_string(), |c| c.to_string());
        result
            .error_message
            .get_or_insert_with(|| T::EXITED.replace("{code}", &code));
    } else {
        result.status = match result.stop_reason.as_deref() {
            Some("aborted") => Status::Aborted,
            Some("error") => Status::Failed,
            _ => Status::Completed,
        };
    }
    result
}

/// One JSON-mode record of the child: collect assistant texts; the activity to show, if any.
fn record(line: &[u8], result: &mut ChildResult) -> Option<String> {
    let text = String::from_utf8_lossy(line);
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let v = serde_json::from_str::<Value>(text).ok()?;
    match v.get("type").and_then(Value::as_str).unwrap_or("") {
        "message_end" => {
            let m = v.get("message")?;
            if m.get("role").and_then(Value::as_str) != Some("assistant") {
                return None;
            }
            let first_text = m
                .get("content")
                .and_then(Value::as_array)
                .and_then(|blocks| {
                    blocks
                        .iter()
                        .find(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                        .and_then(|b| b.get("text").and_then(Value::as_str))
                })
                .map(str::to_string);
            result.stop_reason = m.get("stopReason").and_then(Value::as_str).map(str::to_string);
            result.error_message = m.get("errorMessage").and_then(Value::as_str).map(str::to_string);
            let activity = first_text.as_deref().map(text_tail);
            result.texts.push(first_text);
            activity
        }
        "tool_execution_start" => {
            let name = v.get("toolName").and_then(Value::as_str).unwrap_or("tool");
            let args = v.get("args").cloned().unwrap_or(Value::Null);
            Some(tool_activity(name, &args))
        }
        "engine_queue" => Some(match v.get("pos").and_then(Value::as_u64) {
            Some(p) => format!("queued #{p}"),
            None => "queued".to_string(),
        }),
        "compaction_start" => Some("compacting".to_string()),
        _ => None,
    }
}

/// The last line of `t`, at most 60 characters (its tail).
fn text_tail(t: &str) -> String {
    let line = t.trim().lines().last().unwrap_or("").trim();
    let chars: Vec<char> = line.chars().collect();
    if chars.len() <= 60 {
        line.to_string()
    } else {
        let tail: String = chars[chars.len() - 60..].iter().collect();
        format!("...{tail}")
    }
}

/// pi's `formatToolCall` without colours.
fn tool_activity(name: &str, args: &Value) -> String {
    let s = |k: &str| args.get(k).and_then(Value::as_str);
    let short = |t: &str, n: usize| {
        let chars: Vec<char> = t.chars().collect();
        if chars.len() > n {
            format!("{}...", chars[..n].iter().collect::<String>())
        } else {
            t.to_string()
        }
    };
    match name {
        "bash" => format!("$ {}", short(s("command").unwrap_or("..."), 60)),
        "read" | "write" | "edit" => format!("{name} {}", s("path").or(s("file_path")).unwrap_or("...")),
        "ls" => format!("ls {}", s("path").unwrap_or(".")),
        "find" => format!(
            "find {} in {}",
            s("pattern").unwrap_or("*"),
            s("path").unwrap_or(".")
        ),
        "grep" => format!(
            "grep /{}/ in {}",
            s("pattern").unwrap_or(""),
            s("path").unwrap_or(".")
        ),
        _ => format!("{name} {}", short(&args.to_string(), 50)),
    }
}

fn tail(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut start = s.len() - max;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

/// Spawn the child in its own process group, dying with this process (ENG-019). A just-written executable can be briefly busy
/// (`ETXTBSY`) while another thread forks; that is retried.
fn spawn(bin: &Path, argv: &[String], env: &[(String, String)], cwd: &Path) -> std::io::Result<Child> {
    let mut attempt = 0;
    loop {
        let mut cmd = Command::new(bin);
        cmd.args(argv)
            .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .envs(oxa_proto::offline().then_some(("OXA_OFFLINE", "1"))) // P4: the child stays offline
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        // ENG-019: the child dies with this process (PR_SET_PDEATHSIG). The forking thread is the
        // tool call's thread, which blocks until the child is waited for.
        oxa_sandbox::die_with_parent(&mut cmd);
        let r = cmd.spawn();
        match r {
            Err(e) if e.raw_os_error() == Some(26) && attempt < 10 => {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(20));
            }
            other => return other,
        }
    }
}
