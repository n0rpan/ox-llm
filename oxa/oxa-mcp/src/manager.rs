//! F6: the MCP manager behind `dyn Hooks` (background connect, exposure, resource tools, `mcp.log`). Owner: task F6. Signatures below are frozen; bodies are yours.
// ported from pi packages/coding-agent/src/core/mcp-servers.ts and src/extensions/mcp/*
use crate::{
    effective_exposure, expand_value, project_config_path, result_text, set_server_field, split_full_name,
    tool_names, user_config_path, HttpTransport, McpResource, OAuthTokens, ServerKind, ServerState,
    StdioTransport, TokenSource, ToolStatus, Transport, AUTH_FILE, CODE_NEEDS_AUTH, LOG_FILE,
    LOG_ROTATE_BYTES, RESOURCE_TOOLS,
};
use crate::{
    AccessGate, CallToolResult, ConfigScope, LoginIo, McpClient, McpConfig, McpTool, ServerConfig,
    ServerStatus, TokenStore,
};
use oxa_proto::{codes, image_placeholder, tool_errors, Level, Source};
use oxa_proto::{
    Access, CancelToken, CommandSpec, Exposure, HookPoint, HookReply, OxaError, Tool, ToolClass, ToolCtx,
    ToolOutput, ToolSpec, Ui, Value,
};
use serde_json::{json, Map};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, TryLockError, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// `true` now that F5 (`oauth.rs`) is implemented. While `false`, HTTP servers connect without a
/// token source (a 401 is `NeedsAuth`), and `login` / `logout` return `unsupported_in_oxa`.
const OAUTH_AVAILABLE: bool = true;
/// How often a connected server's idle messages (pings, logging) are handled.
const PUMP_EVERY: Duration = Duration::from_millis(250);
/// Upper bound for `stop()` to wait for every server to stop.
const STOP_WAIT: Duration = Duration::from_secs(10);
/// How long `/mcp login` waits for the sign-in.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
/// Limits of the `mcp_servers` section (pi's values; the same as `oxa-core`'s prompt module).
const MAX_SERVER_DESCRIPTION_CHARS: usize = 250;
const MAX_SERVERS_SECTION_CHARS: usize = 4096;

/// What the manager needs from its embedder (`oxa-cli`).
#[derive(Clone)]
pub struct ManagerOpts {
    /// From `crate::load_config` (project file passed only when trusted).
    pub config: McpConfig,
    /// Session working directory.
    pub cwd: PathBuf,
    /// For `mcp.json`, `mcp-auth.json`, `mcp.log`.
    pub agent_dir: PathBuf,
    pub home: PathBuf,
    /// The project is trusted (project overrides may be written).
    pub trusted: bool,
    pub gate: AccessGate,
    /// Sent as the client version in `initialize`.
    pub client_version: String,
}

/// One server at run time.
pub(crate) struct ServerEntry {
    pub(crate) status: ServerStatus,
    pub(crate) client: Option<Arc<Mutex<McpClient>>>,
    pub(crate) raw_tools: Vec<McpTool>,
    /// Bumped on every (re)connect so stale worker threads can tell they were replaced.
    pub(crate) generation: u64,
}

pub(crate) struct Inner {
    pub(crate) servers: Vec<ServerEntry>,
    pub(crate) started: bool,
    pub(crate) stopping: bool,
    /// Startup problems not yet reported.
    pub(crate) unreported: Vec<String>,
}

/// State shared with the connection threads and the tool adapters.
pub(crate) struct ManagerShared {
    pub(crate) opts: ManagerOpts,
    pub(crate) inner: Mutex<Inner>,
    /// Notified whenever a server's state changes.
    pub(crate) changed: Condvar,
    pub(crate) tokens: Arc<TokenStore>,
    pub(crate) log: Mutex<McpLog>,
}

/// All MCP servers of a session.
pub struct McpManager {
    pub(crate) shared: Arc<ManagerShared>,
}

// ---------------------------------------------------------------------------------------------
// small helpers
// ---------------------------------------------------------------------------------------------

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn try_lock<T>(m: &Mutex<T>) -> Option<MutexGuard<'_, T>> {
    match m.try_lock() {
        Ok(g) => Some(g),
        Err(TryLockError::Poisoned(p)) => Some(p.into_inner()),
        Err(TryLockError::WouldBlock) => None,
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn not_connected(server: &str) -> OxaError {
    OxaError::new(codes::MCP, format!("MCP server {server} is not connected"))
}

fn unknown_server(server: &str) -> OxaError {
    OxaError::new(codes::NOT_FOUND, format!("unknown MCP server: {server}"))
}

/// A server is usable when it is connected and its client is in place.
fn is_live(e: &ServerEntry) -> bool {
    e.status.state == ServerState::Connected && e.client.is_some()
}

/// `true` when the configuration gives some tool of `cfg` the exposure `x`.
fn has_exposure(cfg: &ServerConfig, x: Exposure) -> bool {
    cfg.exposure == x || cfg.tool_exposure.iter().any(|(_, e)| *e == x)
}

/// The file that receives an exposure / enabled change of `cfg`.
fn saves_to(cfg: &ServerConfig, opts: &ManagerOpts) -> PathBuf {
    if cfg.project_override || cfg.defined_in == ConfigScope::Project {
        project_config_path(&opts.cwd)
    } else {
        user_config_path(&opts.agent_dir)
    }
}

fn tool_statuses(cfg: &ServerConfig, raw: &[McpTool]) -> Vec<ToolStatus> {
    let names: Vec<String> = raw.iter().map(|t| t.name.clone()).collect();
    let full = tool_names(&cfg.name, &names);
    raw.iter()
        .zip(full)
        .map(|(t, full_name)| {
            let exposure = effective_exposure(cfg, &t.name);
            ToolStatus {
                name: t.name.clone(),
                full_name,
                description: t.description.clone(),
                exposure,
                overridden: exposure != cfg.exposure,
                annotations: t.annotations,
            }
        })
        .collect()
}

/// JSON with every object's keys sorted (recursively): the canonical schema text.
fn canonical(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            let mut out = Map::new();
            for k in keys {
                out.insert(k.clone(), canonical(&m[k]));
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(canonical).collect()),
        other => other.clone(),
    }
}

fn canonical_text(v: &Value) -> String {
    serde_json::to_string(&canonical(v)).unwrap_or_else(|_| "{}".to_string())
}

/// The host of an HTTP(S) URL, lower case, without user info and port.
fn url_host(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit_once('@').map(|(_, h)| h).unwrap_or(authority);
    let host = if let Some(v6) = authority.strip_prefix('[') {
        format!("[{}]", v6.split(']').next().unwrap_or(""))
    } else {
        authority.split(':').next().unwrap_or("").to_string()
    };
    host.to_ascii_lowercase()
}

/// What starting / reaching a server asks the permission gate.
fn server_access(cfg: &ServerConfig) -> Access {
    match &cfg.kind {
        ServerKind::Stdio(s) => {
            let mut command = s.command.clone();
            for a in &s.args {
                command.push(' ');
                command.push_str(a);
            }
            Access::Exec { command, net: true }
        }
        ServerKind::Http(h) => Access::Net {
            host: url_host(&h.url),
        },
    }
}

/// Text for the `live` display of a progress notification.
fn progress_text(progress: f64, total: Option<f64>, message: Option<&str>) -> String {
    match (message, total) {
        (Some(m), _) if !m.is_empty() => m.to_string(),
        (_, Some(t)) => format!("{progress}/{t}"),
        _ => format!("{progress}"),
    }
}

/// O-144 (text-only engine): `crate::result_text` turns image blocks into the fixed image
/// placeholder line; audio blocks are handed to it as image blocks so they get the same line.
fn tool_result_text(server: &str, r: &CallToolResult) -> String {
    if !r
        .content
        .iter()
        .any(|b| b.get("type").and_then(Value::as_str) == Some("audio"))
    {
        return result_text(server, r);
    }
    let mut mapped = r.clone();
    for b in &mut mapped.content {
        if b.get("type").and_then(Value::as_str) == Some("audio") {
            *b = json!({ "type": "image" });
        }
    }
    result_text(server, &mapped)
}

fn result_json(r: &CallToolResult) -> Value {
    let mut m = Map::new();
    m.insert("content".into(), Value::Array(r.content.clone()));
    if let Some(s) = &r.structured_content {
        m.insert("structuredContent".into(), s.clone());
    }
    m.insert("isError".into(), Value::Bool(r.is_error));
    Value::Object(m)
}

/// The HTTP status in a transport error (`... answered HTTP 503: ...`).
fn http_status(e: &OxaError) -> Option<u16> {
    let at = e.message.find("answered HTTP ")? + "answered HTTP ".len();
    e.message.get(at..at + 3)?.parse().ok()
}

/// Resource lists and reads are retried once after 408 / 429 / 5xx.
fn with_retry<T>(mut f: impl FnMut() -> Result<T, OxaError>) -> Result<T, OxaError> {
    match f() {
        Err(e) if matches!(http_status(&e), Some(408) | Some(429) | Some(500..=599)) => f(),
        other => other,
    }
}

fn error_text(e: &OxaError) -> String {
    match &e.hint {
        Some(h) => format!("{} ({h})", e.message),
        None => e.message.clone(),
    }
}

// ---------------------------------------------------------------------------------------------
// connection threads
// ---------------------------------------------------------------------------------------------

impl ManagerShared {
    fn index_of(inner: &Inner, name: &str) -> Option<usize> {
        inner.servers.iter().position(|e| e.status.config.name == name)
    }

    fn is_current(&self, idx: usize, gen: u64) -> bool {
        let inner = lock(&self.inner);
        !inner.stopping && inner.servers.get(idx).is_some_and(|e| e.generation == gen)
    }

    /// Prepare a new connection attempt of `idx`: drop the old client (its worker closes it),
    /// clear what came from the server, `Pending`. Returns what the worker needs.
    fn begin(inner: &mut Inner, idx: usize) -> (u64, ServerConfig) {
        let e = &mut inner.servers[idx];
        e.generation += 1;
        e.client = None;
        e.raw_tools.clear();
        e.status.tools.clear();
        e.status.has_resources = false;
        e.status.instructions = None;
        e.status.state = ServerState::Pending;
        (e.generation, e.status.config.clone())
    }

    /// Drop the connection of `idx` without starting a new one (its worker closes it).
    fn disconnect(inner: &mut Inner, idx: usize, state: ServerState) {
        let e = &mut inner.servers[idx];
        e.generation += 1;
        e.client = None;
        e.raw_tools.clear();
        e.status.tools.clear();
        e.status.has_resources = false;
        e.status.instructions = None;
        e.status.state = state;
    }

    fn spawn_worker(self: &Arc<Self>, idx: usize, gen: u64, cfg: ServerConfig) {
        let shared = Arc::clone(self);
        let name = cfg.name.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("mcp-{name}"))
            .spawn(move || {
                // A1/A2: a panic while connecting (a malformed server reply hitting a bug)
                // fails this server only; the session and the other servers go on.
                let keep = Arc::clone(&shared);
                let server = cfg.name.clone();
                if let Err(error) = oxa_proto::panic::contain(|| worker(shared, idx, gen, cfg)) {
                    let msg = format!("internal error (panic): {error}");
                    keep.fail(
                        idx,
                        gen,
                        ServerState::Failed(msg.clone()),
                        format!("MCP server {server} failed: {msg}"),
                    );
                }
            });
        if let Err(e) = spawned {
            let msg = format!("cannot start a thread: {e}");
            self.fail(
                idx,
                gen,
                ServerState::Failed(msg.clone()),
                format!("MCP server {name} failed: {msg}"),
            );
        }
    }

    /// Set the state of a current attempt. `false` when the attempt is stale.
    fn set_state(&self, idx: usize, gen: u64, state: ServerState) -> bool {
        let mut inner = lock(&self.inner);
        if inner.stopping || inner.servers[idx].generation != gen {
            return false;
        }
        inner.servers[idx].status.state = state;
        drop(inner);
        self.changed.notify_all();
        true
    }

    /// A failed attempt: the state plus one report line.
    fn fail(&self, idx: usize, gen: u64, state: ServerState, report: String) {
        let mut inner = lock(&self.inner);
        if inner.stopping || inner.servers[idx].generation != gen {
            return;
        }
        inner.servers[idx].status.state = state;
        inner.unreported.push(report);
        drop(inner);
        self.changed.notify_all();
    }

    fn fail_with(&self, idx: usize, gen: u64, name: &str, e: &OxaError) {
        if e.code == CODE_NEEDS_AUTH {
            self.fail(
                idx,
                gen,
                ServerState::NeedsAuth,
                format!("MCP server {name} needs sign-in: run /mcp login {name}"),
            );
        } else {
            let text = error_text(e);
            self.fail(
                idx,
                gen,
                ServerState::Failed(text.clone()),
                format!("MCP server {name} failed: {text}"),
            );
        }
    }

    /// The connection of a live server broke (seen by a pump or a call).
    fn connection_lost(&self, name: &str, gen: u64, e: &OxaError) {
        let mut inner = lock(&self.inner);
        let Some(idx) = Self::index_of(&inner, name) else {
            return;
        };
        if inner.stopping || inner.servers[idx].generation != gen {
            return;
        }
        let text = error_text(e);
        Self::disconnect(&mut inner, idx, ServerState::Failed(text.clone()));
        inner
            .unreported
            .push(format!("MCP server {name} disconnected: {text}"));
        drop(inner);
        self.changed.notify_all();
    }

    /// Install a connected client. `false` when the attempt is stale.
    fn install(&self, idx: usize, gen: u64, client: &Arc<Mutex<McpClient>>, tools: Vec<McpTool>) -> bool {
        let (has_resources, instructions, stderr_tail) = {
            let c = lock(client);
            (
                c.has_resources(),
                c.instructions().map(str::to_string),
                c.stderr_tail(),
            )
        };
        let mut inner = lock(&self.inner);
        if inner.stopping || inner.servers[idx].generation != gen {
            return false;
        }
        let e = &mut inner.servers[idx];
        e.status.tools = tool_statuses(&e.status.config, &tools);
        e.raw_tools = tools;
        e.status.has_resources = has_resources;
        e.status.instructions = instructions;
        e.status.stderr_tail = stderr_tail;
        e.status.state = ServerState::Connected;
        e.client = Some(Arc::clone(client));
        drop(inner);
        self.changed.notify_all();
        true
    }

    /// Run one `!command` of an `env` / `headers` value, after asking the gate.
    fn run_value_command(&self, command: &str) -> Result<String, OxaError> {
        let access = Access::Exec {
            command: command.to_string(),
            net: true,
        };
        if !(self.opts.gate)(&access) {
            return Err(OxaError::new(
                codes::DENIED,
                format!("permission to run `{command}` was denied"),
            ));
        }
        // Item 1 (sec-prod): offline, the value command runs through bwrap without network.
        let argv = crate::offline::launch(
            &format!("the value command `{command}`"),
            oxa_proto::offline(),
            &std::env::var("PATH").unwrap_or_default(),
            &self.opts.home,
            &self.opts.cwd,
            vec!["sh".to_string(), "-c".to_string(), command.to_string()],
        )?;
        let out = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .current_dir(&self.opts.cwd)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .map_err(|e| OxaError::new(codes::MCP, format!("cannot run `{command}`: {e}")))?;
        if !out.status.success() {
            return Err(OxaError::new(
                codes::MCP,
                format!("`{command}` failed with {}", out.status),
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn expand_pairs(&self, pairs: &[(String, String)]) -> Result<Vec<(String, String)>, OxaError> {
        let lookup = |k: &str| std::env::var(k).ok();
        let run = |c: &str| self.run_value_command(c);
        pairs
            .iter()
            .map(|(k, v)| Ok((k.clone(), expand_value(v, &lookup, &run)?)))
            .collect()
    }

    /// Open the transport, `initialize`, `tools/list`.
    fn open_client(self: &Arc<Self>, cfg: &ServerConfig) -> Result<(McpClient, Vec<McpTool>), OxaError> {
        let timeout = Duration::from_secs(u64::from(cfg.timeout_s.max(1)));
        let transport: Box<dyn Transport> = match &cfg.kind {
            ServerKind::Stdio(s) => {
                let env = self.expand_pairs(&s.env)?;
                Box::new(StdioTransport::spawn(s, &env, &self.opts.cwd, &self.opts.home)?)
            }
            ServerKind::Http(h) => {
                let headers = self.expand_pairs(&h.headers)?;
                let tokens: Option<Arc<dyn TokenSource>> = if OAUTH_AVAILABLE {
                    Some(Arc::new(OAuthTokens::new(
                        &cfg.name,
                        &h.url,
                        Arc::clone(&self.tokens),
                    )))
                } else {
                    None
                };
                Box::new(HttpTransport::new(h, headers, tokens, timeout))
            }
        };
        let mut client = McpClient::connect(transport, &self.opts.client_version, timeout)?;
        let weak: Weak<ManagerShared> = Arc::downgrade(self);
        let server = cfg.name.clone();
        client.set_log_sink(Box::new(move |level, logger, message| {
            if let Some(shared) = weak.upgrade() {
                lock(&shared.log).append(now_secs(), &server, level, logger, message);
            }
        }));
        match client.list_tools() {
            Ok(tools) => Ok((client, tools)),
            Err(e) => {
                client.close();
                Err(e)
            }
        }
    }
}

/// One connection attempt and, once connected, the life of the connection. The thread stays
/// alive while the connection lives: a stdio server dies with the thread that started it
/// (`PR_SET_PDEATHSIG`). It handles idle server messages (pings, logging) and closes the client
/// when the connection is replaced, disabled or stopped.
fn worker(shared: Arc<ManagerShared>, idx: usize, gen: u64, cfg: ServerConfig) {
    if !shared.is_current(idx, gen) {
        return;
    }
    let name = cfg.name.clone();
    if !(shared.opts.gate)(&server_access(&cfg)) {
        shared.fail(
            idx,
            gen,
            ServerState::Denied,
            format!("MCP server {name} was not started: permission denied"),
        );
        return;
    }
    if !shared.set_state(idx, gen, ServerState::Connecting) {
        return;
    }
    let (client, tools) = match shared.open_client(&cfg) {
        Ok(ok) => ok,
        Err(e) => {
            shared.fail_with(idx, gen, &name, &e);
            return;
        }
    };
    let client = Arc::new(Mutex::new(client));
    if shared.install(idx, gen, &client, tools) {
        loop {
            let inner = lock(&shared.inner);
            if inner.stopping || inner.servers[idx].generation != gen {
                break;
            }
            let (inner, _) = shared
                .changed
                .wait_timeout(inner, PUMP_EVERY)
                .unwrap_or_else(|p| p.into_inner());
            if inner.stopping || inner.servers[idx].generation != gen {
                break;
            }
            drop(inner);
            // A caller holding the client is running a request, which handles messages itself.
            let pumped = try_lock(&client).map(|mut c| {
                let r = c.pump();
                (r, c.stderr_tail())
            });
            match pumped {
                Some((Err(e), _)) => {
                    shared.connection_lost(&name, gen, &e);
                    break;
                }
                Some((Ok(()), tail)) => {
                    let mut inner = lock(&shared.inner);
                    if inner.servers[idx].generation == gen {
                        inner.servers[idx].status.stderr_tail = tail;
                    }
                }
                None => {}
            }
        }
    }
    lock(&client).close();
}

/// Call `tool` of the live server `server`. Never retried.
fn call_on(
    shared: &ManagerShared,
    server: &str,
    tool: &str,
    args: &Value,
    cancel: &CancelToken,
    live: &mut dyn FnMut(&str),
) -> Result<CallToolResult, OxaError> {
    let (client, gen) = live_client(shared, server)?;
    let mut c = lock(&client);
    if let Err(e) = c.pump() {
        drop(c);
        shared.connection_lost(server, gen, &e);
        return Err(not_connected(server));
    }
    let mut on_progress = |p: f64, t: Option<f64>, m: Option<&str>| live(&progress_text(p, t, m));
    c.call_tool(tool, args, cancel, &mut on_progress)
}

fn live_client(shared: &ManagerShared, server: &str) -> Result<(Arc<Mutex<McpClient>>, u64), OxaError> {
    let inner = lock(&shared.inner);
    let idx = ManagerShared::index_of(&inner, server).ok_or_else(|| unknown_server(server))?;
    let e = &inner.servers[idx];
    match (&e.client, is_live(e)) {
        (Some(c), true) => Ok((Arc::clone(c), e.generation)),
        _ => Err(not_connected(server)),
    }
}

// ---------------------------------------------------------------------------------------------
// tools
// ---------------------------------------------------------------------------------------------

/// A server name and its client.
type NamedClient = (String, Arc<Mutex<McpClient>>);

fn exposure_rank(x: Exposure) -> u8 {
    match x {
        Exposure::Direct => 0,
        Exposure::Codemode => 1,
        Exposure::Deferred => 2,
        Exposure::Hidden => 3,
    }
}

/// The exposure of the resource tools: the widest among the live, non-hidden servers that
/// have resources; `None` = no such server, no resource tools.
fn resource_exposure(inner: &Inner) -> Option<Exposure> {
    inner
        .servers
        .iter()
        .filter(|e| is_live(e) && e.status.has_resources && e.status.config.exposure != Exposure::Hidden)
        .map(|e| e.status.config.exposure)
        .min_by_key(|x| exposure_rank(*x))
}

fn resource_spec(kind: ResourceToolKind, exposure: Exposure) -> ToolSpec {
    let server_prop = json!({ "type": "string", "description": "MCP server name" });
    let cursor_prop =
        json!({ "type": "string", "description": "nextCursor of a previous page (requires server)" });
    let (name, description, schema) = match kind {
        ResourceToolKind::List => (
            RESOURCE_TOOLS[0],
            "List the resources of connected MCP servers. Without `server`, the first page of every server that has resources. Read one with `read_mcp_resource`.",
            json!({
                "type": "object",
                "properties": { "server": server_prop, "cursor": cursor_prop },
                "additionalProperties": false,
            }),
        ),
        ResourceToolKind::ListTemplates => (
            RESOURCE_TOOLS[1],
            "List the resource templates (parameterised URIs) of connected MCP servers. Without `server`, the first page of every server that has resources.",
            json!({
                "type": "object",
                "properties": { "server": server_prop, "cursor": cursor_prop },
                "additionalProperties": false,
            }),
        ),
        ResourceToolKind::Read => (
            RESOURCE_TOOLS[2],
            "Read one resource of an MCP server by URI. Text comes back as text; binary content is saved to a file and its path is returned.",
            json!({
                "type": "object",
                "properties": {
                    "server": server_prop,
                    "uri": { "type": "string", "description": "Resource URI" },
                },
                "required": ["server", "uri"],
                "additionalProperties": false,
            }),
        ),
    };
    ToolSpec {
        name: name.to_string(),
        description: description.to_string(),
        schema_json: canonical_text(&schema),
        class: ToolClass::Read,
        exposure,
        // The resource tools span servers; no single server is their source.
        source: Source::Mcp(String::new()),
    }
}

impl ManagerShared {
    /// Every reachable tool of every live server plus the resource tools, sorted by name.
    fn all_tools(self: &Arc<Self>) -> Vec<Arc<dyn Tool>> {
        let inner = lock(&self.inner);
        let mut out: Vec<Arc<dyn Tool>> = Vec::new();
        for e in inner.servers.iter().filter(|e| is_live(e)) {
            let cfg = &e.status.config;
            let names: Vec<String> = e.raw_tools.iter().map(|t| t.name.clone()).collect();
            for (t, full) in e.raw_tools.iter().zip(tool_names(&cfg.name, &names)) {
                let spec = tool_spec(cfg, t, &full);
                if spec.exposure == Exposure::Hidden {
                    continue;
                }
                out.push(Arc::new(McpToolAdapter {
                    spec,
                    server: cfg.name.clone(),
                    tool: t.name.clone(),
                    class: t.annotations.class(cfg.trust_annotations),
                    shared: Arc::clone(self),
                }));
            }
        }
        if let Some(x) = resource_exposure(&inner) {
            for kind in [
                ResourceToolKind::List,
                ResourceToolKind::ListTemplates,
                ResourceToolKind::Read,
            ] {
                out.push(Arc::new(ResourceTool {
                    kind,
                    spec: resource_spec(kind, x),
                    shared: Arc::clone(self),
                }));
            }
        }
        drop(inner);
        out.sort_by(|a, b| a.spec().name.cmp(&b.spec().name));
        out
    }

    /// Live, non-hidden servers with resources, sorted by name; or the one named `server`.
    fn resource_clients(&self, server: Option<&str>) -> Result<Vec<NamedClient>, String> {
        let inner = lock(&self.inner);
        let usable = |e: &ServerEntry| {
            is_live(e) && e.status.has_resources && e.status.config.exposure != Exposure::Hidden
        };
        if let Some(name) = server {
            let Some(idx) = ManagerShared::index_of(&inner, name) else {
                return Err(format!("unknown MCP server: {name}"));
            };
            let e = &inner.servers[idx];
            if !is_live(e) || e.status.config.exposure == Exposure::Hidden {
                return Err(format!("MCP server {name} is not connected"));
            }
            if !usable(e) {
                return Err(format!("MCP server {name} has no resources"));
            }
            return Ok(e
                .client
                .iter()
                .map(|c| (name.to_string(), Arc::clone(c)))
                .collect());
        }
        let mut out: Vec<NamedClient> = inner
            .servers
            .iter()
            .filter(|e| usable(e))
            .filter_map(|e| {
                e.client
                    .as_ref()
                    .map(|c| (e.status.config.name.clone(), Arc::clone(c)))
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }
}

// ---------------------------------------------------------------------------------------------
// McpManager
// ---------------------------------------------------------------------------------------------

impl McpManager {
    /// Build the manager; nothing connects yet. Every configured server gets a status
    /// (`Disabled` or `Pending`).
    pub fn new(opts: ManagerOpts) -> McpManager {
        let servers = opts
            .config
            .servers
            .iter()
            .map(|cfg| ServerEntry {
                status: ServerStatus {
                    config: cfg.clone(),
                    state: if cfg.enabled {
                        ServerState::Pending
                    } else {
                        ServerState::Disabled
                    },
                    tools: Vec::new(),
                    has_resources: false,
                    instructions: None,
                    stderr_tail: String::new(),
                    saves_to: saves_to(cfg, &opts),
                },
                client: None,
                raw_tools: Vec::new(),
                generation: 0,
            })
            .collect();
        let tokens = Arc::new(TokenStore::open(&opts.agent_dir.join(AUTH_FILE)));
        let log = McpLog::open(&opts.agent_dir.join(LOG_FILE));
        let unreported = opts.config.diagnostics.clone();
        McpManager {
            shared: Arc::new(ManagerShared {
                opts,
                inner: Mutex::new(Inner {
                    servers,
                    started: false,
                    stopping: false,
                    unreported,
                }),
                changed: Condvar::new(),
                tokens,
                log: Mutex::new(log),
            }),
        }
    }

    /// Connect every enabled server in the background, one thread per server: ask the gate
    /// (`Access::Exec` for stdio, `Access::Net` for HTTP), expand `env` / `headers`, open the
    /// transport, `initialize`, `tools/list`. Returns at once. Idempotent.
    pub fn start(&self) {
        let jobs: Vec<(usize, u64, ServerConfig)> = {
            let mut inner = lock(&self.shared.inner);
            if inner.started || inner.stopping {
                return;
            }
            inner.started = true;
            let enabled: Vec<usize> = (0..inner.servers.len())
                .filter(|&i| inner.servers[i].status.config.enabled)
                .collect();
            enabled
                .into_iter()
                .map(|i| {
                    let (gen, cfg) = ManagerShared::begin(&mut inner, i);
                    (i, gen, cfg)
                })
                .collect()
        };
        self.shared.changed.notify_all();
        for (idx, gen, cfg) in jobs {
            self.shared.spawn_worker(idx, gen, cfg);
        }
    }

    /// Block until every server that has (by configuration) `Direct` tools is connected or has
    /// failed, at most `timeout`. Returns `true` when none is still connecting. The core calls
    /// this once, before the first prefix of a session is built, with `FIRST_PROMPT_WAIT_S`.
    pub fn wait_for_direct(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut inner = lock(&self.shared.inner);
        loop {
            // Not started (e.g. `--no-mcp`) or stopped: nothing is connecting.
            if !inner.started || inner.stopping {
                return true;
            }
            let busy = inner.servers.iter().any(|e| {
                e.status.config.enabled
                    && has_exposure(&e.status.config, Exposure::Direct)
                    && matches!(e.status.state, ServerState::Pending | ServerState::Connecting)
            });
            if !busy {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            inner = self
                .shared
                .changed
                .wait_timeout(inner, deadline - now)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
    }

    /// All servers; those needing attention first, then by name (the `/mcp` list order).
    pub fn servers(&self) -> Vec<ServerStatus> {
        let inner = lock(&self.shared.inner);
        let mut out: Vec<ServerStatus> = inner
            .servers
            .iter()
            .map(|e| {
                let mut s = e.status.clone();
                if let Some(c) = e.client.as_ref().and_then(|c| try_lock(c)) {
                    s.stderr_tail = c.stderr_tail();
                }
                s
            })
            .collect();
        drop(inner);
        out.sort_by(|a, b| {
            b.state
                .needs_attention()
                .cmp(&a.state.needs_attention())
                .then_with(|| a.config.name.cmp(&b.config.name))
        });
        out
    }

    pub fn server(&self, name: &str) -> Option<ServerStatus> {
        self.servers().into_iter().find(|s| s.config.name == name)
    }

    /// Problems found since the last call (configuration errors, failed connections, required
    /// sign-ins), one line each. The front end shows them once after startup.
    pub fn take_report(&self) -> Vec<String> {
        std::mem::take(&mut lock(&self.shared.inner).unreported)
    }

    /// Close and connect again (also after a sign-in). Returns at once; watch `servers()`.
    pub fn reconnect(&self, name: &str) -> Result<(), OxaError> {
        let (idx, gen, cfg) = {
            let mut inner = lock(&self.shared.inner);
            if inner.stopping {
                return Err(OxaError::new(codes::MCP, "MCP servers are stopped"));
            }
            let idx = ManagerShared::index_of(&inner, name).ok_or_else(|| unknown_server(name))?;
            if !inner.servers[idx].status.config.enabled {
                return Err(OxaError::new(
                    codes::INVALID,
                    format!("MCP server {name} is disabled"),
                ));
            }
            let (gen, cfg) = ManagerShared::begin(&mut inner, idx);
            (idx, gen, cfg)
        };
        self.shared.changed.notify_all();
        self.shared.spawn_worker(idx, gen, cfg);
        Ok(())
    }

    /// Enable or disable a server and save the change to `scope`'s file without replacing
    /// unrelated content (`ConfigScope::Project` on a user-level server writes a project
    /// override; needs a trusted project). Disabling disconnects; enabling connects. Disabled
    /// servers stay listed. **W3:** the declared tool list changes only at the next epoch.
    pub fn set_enabled(&self, name: &str, enabled: bool, scope: ConfigScope) -> Result<(), OxaError> {
        let opts = &self.shared.opts;
        let cfg = self.server(name).ok_or_else(|| unknown_server(name))?.config;
        let file = match scope {
            ConfigScope::User => {
                if cfg.defined_in == ConfigScope::Project {
                    return Err(OxaError::new(
                        codes::INVALID,
                        format!("MCP server {name} is defined in the project mcp.json; change it there"),
                    ));
                }
                user_config_path(&opts.agent_dir)
            }
            ConfigScope::Project => {
                if !opts.trusted {
                    return Err(OxaError::new(
                        codes::UNTRUSTED,
                        "the project is not trusted; its mcp.json is not written",
                    ));
                }
                project_config_path(&opts.cwd)
            }
        };
        set_server_field(&file, name, "enabled", &Value::Bool(enabled))?;
        let job = {
            let mut inner = lock(&self.shared.inner);
            let idx = ManagerShared::index_of(&inner, name).ok_or_else(|| unknown_server(name))?;
            let started = inner.started && !inner.stopping;
            let e = &mut inner.servers[idx];
            if scope == ConfigScope::Project && e.status.config.defined_in == ConfigScope::User {
                e.status.config.project_override = true;
            }
            let was = e.status.config.enabled;
            e.status.config.enabled = enabled;
            e.status.saves_to = saves_to(&e.status.config, opts);
            if !enabled {
                ManagerShared::disconnect(&mut inner, idx, ServerState::Disabled);
                None
            } else if !was || e.status.state == ServerState::Disabled {
                if started {
                    let (gen, cfg) = ManagerShared::begin(&mut inner, idx);
                    Some((idx, gen, cfg))
                } else {
                    e.status.state = ServerState::Pending;
                    None
                }
            } else {
                None
            }
        };
        self.shared.changed.notify_all();
        if let Some((idx, gen, cfg)) = job {
            self.shared.spawn_worker(idx, gen, cfg);
        }
        Ok(())
    }

    /// Change the server-level exposure and save it to the file that defines the server (or to
    /// its project override when one exists). Takes effect for codemode / tool search at once,
    /// for declared (`Direct`) tools at the next epoch.
    pub fn set_exposure(&self, name: &str, exposure: Exposure) -> Result<(), OxaError> {
        let file = self.writable_file(name)?;
        set_server_field(
            &file,
            name,
            "exposure",
            &Value::String(exposure.as_str().to_string()),
        )?;
        self.update_config(name, |cfg| cfg.exposure = exposure)
    }

    /// Set (`Some`) or clear (`None`) one `toolExposure` entry and save it.
    pub fn set_tool_exposure(
        &self,
        name: &str,
        tool_pattern: &str,
        exposure: Option<Exposure>,
    ) -> Result<(), OxaError> {
        let file = self.writable_file(name)?;
        let mut list = self
            .server(name)
            .ok_or_else(|| unknown_server(name))?
            .config
            .tool_exposure;
        match exposure {
            Some(x) => match list.iter_mut().find(|(k, _)| k == tool_pattern) {
                Some(entry) => entry.1 = x,
                None => list.push((tool_pattern.to_string(), x)),
            },
            None => list.retain(|(k, _)| k != tool_pattern),
        }
        let mut obj = Map::new();
        for (k, x) in &list {
            obj.insert(k.clone(), Value::String(x.as_str().to_string()));
        }
        set_server_field(&file, name, "toolExposure", &Value::Object(obj))?;
        self.update_config(name, move |cfg| cfg.tool_exposure = list)
    }

    /// Sign in to an OAuth server (`crate::login`), then reconnect it.
    pub fn login(
        &self,
        name: &str,
        io: &mut dyn LoginIo,
        timeout: Duration,
        cancel: &CancelToken,
    ) -> Result<(), OxaError> {
        let cfg = self.server(name).ok_or_else(|| unknown_server(name))?.config;
        let ServerKind::Http(http) = &cfg.kind else {
            return Err(OxaError::new(
                codes::INVALID,
                format!("MCP server {name} is a stdio server; sign-in is for HTTP servers"),
            ));
        };
        if !OAUTH_AVAILABLE {
            return Err(OxaError::new(
                codes::UNSUPPORTED_IN_OXA,
                "MCP sign-in is not available in this build yet (OAuth support is pending)",
            ));
        }
        crate::login(name, http, &self.shared.tokens, io, timeout, cancel)?;
        if cfg.enabled {
            self.reconnect(name)?;
        }
        Ok(())
    }

    /// Delete the stored credentials of a server and disconnect it. `Ok(false)` when there were
    /// none.
    pub fn logout(&self, name: &str) -> Result<bool, OxaError> {
        let cfg = self.server(name).ok_or_else(|| unknown_server(name))?.config;
        let ServerKind::Http(http) = &cfg.kind else {
            return Err(OxaError::new(
                codes::INVALID,
                format!("MCP server {name} is a stdio server; it has no stored credentials"),
            ));
        };
        if !OAUTH_AVAILABLE {
            return Err(OxaError::new(
                codes::UNSUPPORTED_IN_OXA,
                "MCP sign-out is not available in this build yet (OAuth support is pending)",
            ));
        }
        let had = self.shared.tokens.delete(name, &http.url)?;
        {
            let mut inner = lock(&self.shared.inner);
            if let Some(idx) = ManagerShared::index_of(&inner, name) {
                if inner.servers[idx].status.config.enabled {
                    ManagerShared::disconnect(&mut inner, idx, ServerState::NeedsAuth);
                }
            }
        }
        self.shared.changed.notify_all();
        Ok(had)
    }

    /// Call a tool by its full name (`mcp__<server>__<tool>`), whatever its exposure except
    /// `Hidden` (`Err(not_found)`). `live` receives progress messages. Never retried.
    pub fn call_tool(
        &self,
        full_name: &str,
        args: &Value,
        cancel: &CancelToken,
        live: &mut dyn FnMut(&str),
    ) -> Result<CallToolResult, OxaError> {
        let not_found = || OxaError::new(codes::NOT_FOUND, format!("MCP tool {full_name} not found"));
        let (server, tool) = {
            let inner = lock(&self.shared.inner);
            let mut found: Option<(String, String, Exposure)> = None;
            for e in inner.servers.iter().filter(|e| is_live(e)) {
                let cfg = &e.status.config;
                let names: Vec<String> = e.raw_tools.iter().map(|t| t.name.clone()).collect();
                if let Some(t) = tool_names(&cfg.name, &names)
                    .iter()
                    .position(|n| n == full_name)
                    .map(|i| &e.raw_tools[i])
                {
                    found = Some((cfg.name.clone(), t.name.clone(), effective_exposure(cfg, &t.name)));
                    break;
                }
            }
            match found {
                Some((_, _, Exposure::Hidden)) => return Err(not_found()),
                Some((s, t, _)) => (s, t),
                None => {
                    // A configured server that is not connected: say so rather than "not found".
                    let wanted = split_full_name(full_name).map(|(s, _)| s);
                    // `mangle(name, "x")` = `mcp__<sanitized name>__x`: compare sanitized names.
                    let offline = inner.servers.iter().find(|e| {
                        e.status.config.enabled
                            && wanted.is_some_and(|w| {
                                crate::mangle(&e.status.config.name, "x") == format!("mcp__{w}__x")
                            })
                    });
                    return Err(match offline {
                        Some(e) if !is_live(e) => not_connected(&e.status.config.name),
                        _ => not_found(),
                    });
                }
            }
        };
        let args = if args.is_null() { json!({}) } else { args.clone() };
        call_on(&self.shared, &server, &tool, &args, cancel, live)
    }

    /// Specs of every reachable (non-hidden) tool of every connected server plus the resource
    /// tools, sorted by name: the input of `oxa-codemode`'s catalog.
    pub fn catalog(&self) -> Vec<ToolSpec> {
        self.shared.all_tools().iter().map(|t| t.spec().clone()).collect()
    }

    /// `(server name, configured description, server instructions)` of every enabled server,
    /// sorted by name: the input of codemode's `describeNamespace`.
    pub fn namespaces(&self) -> Vec<(String, Option<String>, Option<String>)> {
        let inner = lock(&self.shared.inner);
        let mut out: Vec<(String, Option<String>, Option<String>)> = inner
            .servers
            .iter()
            .filter(|e| e.status.config.enabled)
            .map(|e| {
                (
                    e.status.config.name.clone(),
                    e.status.config.description.clone(),
                    e.status.instructions.clone(),
                )
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// `true` when an enabled server has `Codemode` tools by configuration and
    /// `autoEnableCodemode` is not `false` (the embedder then enables the `codemode` tool).
    pub fn wants_codemode(&self) -> bool {
        if self.shared.opts.config.auto_enable_codemode == Some(false) {
            return false;
        }
        let inner = lock(&self.shared.inner);
        inner
            .servers
            .iter()
            .any(|e| e.status.config.enabled && has_exposure(&e.status.config, Exposure::Codemode))
    }

    /// `true` when an enabled server has `Deferred` tools by configuration.
    pub fn wants_tool_search(&self) -> bool {
        let inner = lock(&self.shared.inner);
        inner
            .servers
            .iter()
            .any(|e| e.status.config.enabled && has_exposure(&e.status.config, Exposure::Deferred))
    }

    /// Disconnect everything (stdin close, SIGTERM, SIGKILL of each group). Idempotent.
    pub fn stop(&self) {
        let clients: Vec<Arc<Mutex<McpClient>>> = {
            let mut inner = lock(&self.shared.inner);
            if inner.stopping {
                return;
            }
            inner.stopping = true;
            let mut out = Vec::new();
            for e in inner.servers.iter_mut() {
                e.generation += 1;
                if let Some(c) = e.client.take() {
                    out.push(c);
                }
                e.raw_tools.clear();
                e.status.tools.clear();
                e.status.has_resources = false;
                e.status.instructions = None;
                if e.status.config.enabled && !e.status.state.needs_attention() {
                    e.status.state = ServerState::Pending;
                }
            }
            out
        };
        self.shared.changed.notify_all();
        // Close in parallel (each stdio close can take up to 4 s); bounded overall.
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let mut pending = 0usize;
        for c in clients {
            let tx = tx.clone();
            let spawned = std::thread::Builder::new()
                .name("mcp-stop".into())
                .spawn(move || {
                    lock(&c).close();
                    let _ = tx.send(());
                });
            if spawned.is_ok() {
                pending += 1;
            }
        }
        drop(tx);
        let deadline = Instant::now() + STOP_WAIT;
        while pending > 0 {
            let now = Instant::now();
            if now >= deadline || rx.recv_timeout(deadline - now).is_err() {
                break;
            }
            pending -= 1;
        }
    }

    /// The file a server's exposure change goes to; refused for an untrusted project file.
    fn writable_file(&self, name: &str) -> Result<PathBuf, OxaError> {
        let status = self.server(name).ok_or_else(|| unknown_server(name))?;
        if status.saves_to == project_config_path(&self.shared.opts.cwd) && !self.shared.opts.trusted {
            return Err(OxaError::new(
                codes::UNTRUSTED,
                "the project is not trusted; its mcp.json is not written",
            ));
        }
        Ok(status.saves_to)
    }

    /// Change the live configuration of `name` and recompute its tool exposures.
    fn update_config(&self, name: &str, f: impl FnOnce(&mut ServerConfig)) -> Result<(), OxaError> {
        let mut inner = lock(&self.shared.inner);
        let idx = ManagerShared::index_of(&inner, name).ok_or_else(|| unknown_server(name))?;
        let e = &mut inner.servers[idx];
        f(&mut e.status.config);
        e.status.tools = tool_statuses(&e.status.config, &e.raw_tools);
        drop(inner);
        self.shared.changed.notify_all();
        Ok(())
    }

    /// The live configuration (with changes made through this manager).
    fn live_config(&self) -> McpConfig {
        let inner = lock(&self.shared.inner);
        McpConfig {
            servers: inner.servers.iter().map(|e| e.status.config.clone()).collect(),
            auto_enable_codemode: self.shared.opts.config.auto_enable_codemode,
            diagnostics: Vec::new(),
        }
    }
}

impl Drop for McpManager {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The `mcp_servers` prefix section, from configuration only (W3; see the crate docs): one
/// line per enabled server that has `Codemode` or `Deferred` tools, sorted by name, saying how
/// its tools are reached and its configured `description`. Empty string = omit the section.
/// Pure.
pub fn servers_section(config: &McpConfig) -> String {
    // (name, reach, description); a server with both codemode and deferred tools is listed
    // under codemode (its tools are all reachable from scripts).
    let mut listed: Vec<(&str, &str, &str)> = config
        .servers
        .iter()
        .filter(|s| s.enabled)
        .filter_map(|s| {
            let reach = if has_exposure(s, Exposure::Codemode) {
                "codemode"
            } else if has_exposure(s, Exposure::Deferred) {
                "tool_search"
            } else {
                return None;
            };
            Some((s.name.as_str(), reach, s.description.as_deref().unwrap_or("")))
        })
        .collect();
    if listed.is_empty() {
        return String::new();
    }
    listed.sort_by(|a, b| a.0.cmp(b.0));
    let mut intro = String::from("MCP servers whose tools are not declared to you.");
    if listed.iter().any(|s| s.1 == "codemode") {
        intro.push_str(" Call the tools of `codemode` servers from codemode scripts.");
    }
    if listed.iter().any(|s| s.1 == "tool_search") {
        intro.push_str(
            " Find the tools of `tool_search` servers with `tool_search` and call them with `call_tool`.",
        );
    }
    let heads: Vec<String> = listed.iter().map(|s| format!("- {} ({})", s.0, s.1)).collect();
    let omitted = |count: usize| -> Option<String> {
        (count > 0).then(|| {
            format!(
                "- … {count} more server{}; find their tools with searchTools()",
                if count == 1 { "" } else { "s" }
            )
        })
    };
    let size = |kept: usize| -> usize {
        let mut n = intro.chars().count();
        for h in &heads[..kept] {
            n += 1 + h.chars().count();
        }
        if let Some(o) = omitted(heads.len() - kept) {
            n += 1 + o.chars().count();
        }
        n
    };
    let mut kept = heads.len();
    while kept > 0 && size(kept) > MAX_SERVERS_SECTION_CHARS {
        kept -= 1;
    }
    let per_server = MAX_SERVERS_SECTION_CHARS
        .saturating_sub(size(kept))
        .checked_div(kept)
        .unwrap_or(0)
        .saturating_sub(2)
        .min(MAX_SERVER_DESCRIPTION_CHARS);
    let mut lines = vec![intro.clone()];
    for (i, s) in listed.iter().take(kept).enumerate() {
        let first_line = s.2.lines().next().unwrap_or("").trim();
        let summary = truncate_chars(first_line, per_server);
        lines.push(if summary.is_empty() {
            heads[i].clone()
        } else {
            format!("{}: {summary}", heads[i])
        });
    }
    if let Some(o) = omitted(heads.len() - kept) {
        lines.push(o);
    }
    lines.join("\n")
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    if max <= 1 {
        return String::new();
    }
    let head: String = text.chars().take(max - 1).collect();
    format!("{}…", head.trim_end())
}

/// The `ToolSpec` of one MCP tool: name from `crate::tool_names`, description and schema from
/// the server (schema re-serialised canonically), class from the annotations, the effective
/// exposure, `Source::Mcp(server)`.
pub fn tool_spec(server: &ServerConfig, tool: &McpTool, full_name: &str) -> ToolSpec {
    ToolSpec {
        name: full_name.to_string(),
        description: tool.description.clone(),
        schema_json: canonical_text(&tool.input_schema),
        class: tool.annotations.class(server.trust_annotations),
        exposure: effective_exposure(server, &tool.name),
        source: Source::Mcp(server.name.clone()),
    }
}

/// One MCP tool as a `dyn Tool`.
pub struct McpToolAdapter {
    pub(crate) spec: ToolSpec,
    pub(crate) server: String,
    /// The server's own name for the tool.
    pub(crate) tool: String,
    pub(crate) class: ToolClass,
    pub(crate) shared: Arc<ManagerShared>,
}

impl Tool for McpToolAdapter {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    /// `[Access::Mcp { server, tool, class }]`. `Err` when `args` is not an object.
    fn access(&self, args: &Value, _cx: &ToolCtx) -> Result<Vec<Access>, String> {
        if !args.is_object() {
            return Err(tool_errors::BAD_JSON.to_string());
        }
        Ok(vec![Access::Mcp {
            server: self.server.clone(),
            tool: self.tool.clone(),
            class: self.class,
        }])
    }
    /// `tools/call`; the text is `crate::result_text`; `is_error` when the result has
    /// `isError`, the server is not connected (fixed text `MCP server <name> is not
    /// connected`), or the call failed. `ui` holds the full `CallToolResult` JSON.
    fn run(&self, args: &Value, cx: &ToolCtx, live: &mut dyn FnMut(&str)) -> ToolOutput {
        if !args.is_object() {
            return ToolOutput::error(tool_errors::BAD_JSON);
        }
        match call_on(&self.shared, &self.server, &self.tool, args, cx.cancel, live) {
            Ok(r) => ToolOutput {
                text: tool_result_text(&self.server, &r),
                is_error: r.is_error,
                ui: result_json(&r),
            },
            Err(e) if e.code == codes::ABORTED => ToolOutput::error(tool_errors::ABORTED),
            Err(e) => ToolOutput::error(e.message),
        }
    }
}

/// Which resource tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceToolKind {
    /// `list_mcp_resources {server?, cursor?}` -> JSON `{server?, resources: [..], nextCursor?}`.
    List,
    /// `list_mcp_resource_templates {server?, cursor?}`.
    ListTemplates,
    /// `read_mcp_resource {server, uri}`: text as text; other binary content is saved under
    /// the spill directory and the path is returned.
    Read,
}

/// The resource tools. Offered when a connected, non-hidden server has resources; their
/// exposure is the widest among those servers (`Direct`, then `Codemode` / `Deferred`). Class
/// `Read`; `access` is `[Access::Mcp { server, tool: <tool name>, class: Read }]`. Lists and
/// reads are retried once after 408 / 429 / 5xx.
pub struct ResourceTool {
    pub(crate) kind: ResourceToolKind,
    pub(crate) spec: ToolSpec,
    pub(crate) shared: Arc<ManagerShared>,
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Result<Option<&'a str>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(_) => Err(format!("Invalid tool arguments: `{key}` must be a string")),
    }
}

fn resource_entry(server: Option<&str>, r: &McpResource, uri_key: &str) -> Value {
    let mut m = Map::new();
    if let Some(s) = server {
        m.insert("server".into(), Value::String(s.to_string()));
    }
    m.insert(uri_key.into(), Value::String(r.uri.clone()));
    m.insert("name".into(), Value::String(r.name.clone()));
    if let Value::Object(extra) = &r.extra {
        for (k, v) in extra {
            if !m.contains_key(k) {
                m.insert(k.clone(), v.clone());
            }
        }
    }
    Value::Object(m)
}

/// File extension for a saved binary resource.
fn extension_for(mime: &str) -> &'static str {
    match mime {
        "application/pdf" => "pdf",
        "application/json" => "json",
        "application/zip" => "zip",
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "audio/mpeg" => "mp3",
        "audio/wav" | "audio/x-wav" => "wav",
        _ => "bin",
    }
}

impl ResourceTool {
    fn list(&self, args: &Value) -> ToolOutput {
        let (server, cursor) = match (str_arg(args, "server"), str_arg(args, "cursor")) {
            (Ok(s), Ok(c)) => (s, c),
            (Err(e), _) | (_, Err(e)) => return ToolOutput::error(e),
        };
        if cursor.is_some() && server.is_none() {
            return ToolOutput::error("Invalid tool arguments: `cursor` requires `server`");
        }
        let clients = match self.shared.resource_clients(server) {
            Ok(c) => c,
            Err(e) => return ToolOutput::error(e),
        };
        let (key, uri_key) = match self.kind {
            ResourceToolKind::ListTemplates => ("resourceTemplates", "uriTemplate"),
            _ => ("resources", "uri"),
        };
        let mut items: Vec<Value> = Vec::new();
        let mut cursors = Map::new();
        for (name, client) in &clients {
            let page = with_retry(|| {
                let mut c = lock(client);
                match self.kind {
                    ResourceToolKind::ListTemplates => c.list_resource_templates(cursor),
                    _ => c.list_resources(cursor),
                }
            });
            let (list, next) = match page {
                Ok(p) => p,
                Err(e) => return ToolOutput::error(format!("MCP server {name}: {}", e.message)),
            };
            let tag = if server.is_some() {
                None
            } else {
                Some(name.as_str())
            };
            items.extend(list.iter().map(|r| resource_entry(tag, r, uri_key)));
            if let Some(n) = next {
                cursors.insert(name.clone(), Value::String(n));
            }
        }
        let mut out = Map::new();
        if let Some(s) = server {
            out.insert("server".into(), Value::String(s.to_string()));
        }
        out.insert(key.into(), Value::Array(items));
        if let Some(s) = server {
            if let Some(n) = cursors.remove(s) {
                out.insert("nextCursor".into(), n);
            }
        } else if !cursors.is_empty() {
            // More pages exist on these servers: ask again with `server` and the cursor.
            out.insert("nextCursors".into(), Value::Object(cursors));
        }
        let out = Value::Object(out);
        ToolOutput {
            text: serde_json::to_string_pretty(&out).unwrap_or_default(),
            is_error: false,
            ui: out,
        }
    }

    fn read(&self, args: &Value, cx: &ToolCtx) -> ToolOutput {
        let (server, uri) = match (str_arg(args, "server"), str_arg(args, "uri")) {
            (Ok(Some(s)), Ok(Some(u))) => (s, u),
            (Err(e), _) | (_, Err(e)) => return ToolOutput::error(e),
            _ => return ToolOutput::error("Invalid tool arguments: `server` and `uri` are required"),
        };
        let clients = match self.shared.resource_clients(Some(server)) {
            Ok(c) => c,
            Err(e) => return ToolOutput::error(e),
        };
        let Some((_, client)) = clients.first() else {
            return ToolOutput::error(format!("MCP server {server} is not connected"));
        };
        let contents = match with_retry(|| lock(client).read_resource(uri)) {
            Ok(c) => c,
            Err(e) => return ToolOutput::error(format!("MCP server {server}: {}", e.message)),
        };
        let mut parts: Vec<String> = Vec::new();
        for (i, item) in contents.iter().enumerate() {
            let item_uri = item.get("uri").and_then(Value::as_str).unwrap_or(uri);
            let mime = item.get("mimeType").and_then(Value::as_str).unwrap_or("");
            if let Some(t) = item.get("text").and_then(Value::as_str) {
                parts.push(t.to_string());
                continue;
            }
            let Some(blob) = item.get("blob").and_then(Value::as_str) else {
                continue;
            };
            if mime.starts_with("image/") {
                // O-144: the engine is text-only.
                parts.push(image_placeholder("image", 0, 0));
                continue;
            }
            use base64::Engine as _;
            let bytes = match base64::engine::general_purpose::STANDARD.decode(blob.trim()) {
                Ok(b) => b,
                Err(_) => {
                    parts.push(format!("[binary resource {item_uri} omitted: invalid base64]"));
                    continue;
                }
            };
            let mut h = Sha256::new();
            h.update(server.as_bytes());
            h.update([0u8]);
            h.update(item_uri.as_bytes());
            h.update([0u8]);
            h.update(i.to_string().as_bytes());
            let digest = h.finalize();
            let hex: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
            let path = cx
                .spill_dir
                .join(format!("mcp-resource-{hex}.{}", extension_for(mime)));
            let saved = std::fs::create_dir_all(cx.spill_dir).and_then(|_| std::fs::write(&path, &bytes));
            match saved {
                Ok(()) => parts.push(format!(
                    "[binary resource {item_uri} ({}, {} bytes) saved to {}]",
                    if mime.is_empty() {
                        "application/octet-stream"
                    } else {
                        mime
                    },
                    bytes.len(),
                    path.display()
                )),
                Err(e) => return ToolOutput::error(format!("cannot save resource {item_uri}: {e}")),
            }
        }
        ToolOutput {
            text: parts.join("\n"),
            is_error: false,
            ui: json!({ "server": server, "uri": uri, "contents": contents.len() }),
        }
    }
}

impl Tool for ResourceTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn access(&self, args: &Value, _cx: &ToolCtx) -> Result<Vec<Access>, String> {
        if !args.is_object() {
            return Err(tool_errors::BAD_JSON.to_string());
        }
        let server = str_arg(args, "server")?;
        if self.kind == ResourceToolKind::Read {
            if server.is_none() || str_arg(args, "uri")?.is_none() {
                return Err("Invalid tool arguments: `server` and `uri` are required".to_string());
            }
        } else {
            str_arg(args, "cursor")?;
        }
        let servers: Vec<String> = match server {
            Some(s) => vec![s.to_string()],
            None => self
                .shared
                .resource_clients(None)
                .unwrap_or_default()
                .into_iter()
                .map(|(n, _)| n)
                .collect(),
        };
        Ok(servers
            .into_iter()
            .map(|server| Access::Mcp {
                server,
                tool: self.spec.name.clone(),
                class: ToolClass::Read,
            })
            .collect())
    }
    fn run(&self, args: &Value, cx: &ToolCtx, _live: &mut dyn FnMut(&str)) -> ToolOutput {
        if !args.is_object() {
            return ToolOutput::error(tool_errors::BAD_JSON);
        }
        match self.kind {
            ResourceToolKind::List | ResourceToolKind::ListTemplates => self.list(args),
            ResourceToolKind::Read => self.read(args, cx),
        }
    }
}

/// `mcp.log`: server logging notifications, one line each:
/// `<time> [<server>] <level> <logger>: <message>` (`time` = RFC 3339 UTC, seconds). The file
/// moves to `mcp.log.1` (replacing it) once it has grown past `LOG_ROTATE_BYTES`.
pub struct McpLog {
    pub(crate) path: PathBuf,
}

/// `(year, month, day)` of a day count since 1970-01-01 (proleptic Gregorian).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

impl McpLog {
    pub fn open(path: &Path) -> McpLog {
        McpLog {
            path: path.to_path_buf(),
        }
    }
    /// Format one line (pure; `unix_secs` is the time).
    pub fn format_line(unix_secs: u64, server: &str, level: &str, logger: &str, message: &str) -> String {
        let days = (unix_secs / 86_400) as i64;
        let rem = unix_secs % 86_400;
        let (y, mo, d) = civil_from_days(days);
        let time = format!(
            "{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}Z",
            rem / 3600,
            (rem / 60) % 60,
            rem % 60
        );
        // One line per message: line breaks inside the message are escaped.
        let message = message.replace('\r', "\\r").replace('\n', "\\n");
        if logger.is_empty() {
            format!("{time} [{server}] {level}: {message}")
        } else {
            format!("{time} [{server}] {level} {logger}: {message}")
        }
    }
    /// Append one line, rotating first when needed. Failures are ignored (logging never breaks
    /// a session).
    pub fn append(&mut self, unix_secs: u64, server: &str, level: &str, logger: &str, message: &str) {
        use std::io::Write;
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::metadata(&self.path).is_ok_and(|m| m.len() > LOG_ROTATE_BYTES) {
            let mut rotated = self.path.clone().into_os_string();
            rotated.push(".1");
            let _ = std::fs::rename(&self.path, PathBuf::from(rotated));
        }
        let line = Self::format_line(unix_secs, server, level, logger, message);
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = f.write_all(format!("{line}\n").as_bytes());
        }
    }
}

/// `/mcp login` talks to the user through the session's `Ui`.
struct UiLoginIo<'a> {
    ui: &'a mut dyn Ui,
}

impl LoginIo for UiLoginIo<'_> {
    fn open_url(&mut self, url: &str) {
        self.ui
            .notify(&format!("Open this URL to sign in: {url}"), Level::Info);
    }
    fn pasted_redirect(&mut self) -> Option<String> {
        None
    }
    fn status(&mut self, text: &str) {
        self.ui.notify(text, Level::Info);
    }
}

impl oxa_proto::Hooks for McpManager {
    fn fire(&self, _p: HookPoint, _payload: &Value, _ui: &mut dyn Ui) -> HookReply {
        HookReply::Continue
    }
    /// Every reachable tool of every connected server as an [`McpToolAdapter`], with its
    /// effective exposure in the spec, plus the resource tools; sorted by name. The core
    /// declares only the `Direct` ones and reads this once per epoch (late server = next
    /// epoch).
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.shared.all_tools()
    }
    /// `/mcp` (the interactive mode opens its selector for the bare command; with arguments
    /// `login <server>`, `logout <server>`, `reconnect <server>` it is run here).
    fn commands(&self) -> Vec<CommandSpec> {
        vec![CommandSpec {
            name: "mcp".to_string(),
            description: "Manage MCP servers".to_string(),
            argument_hint: Some("[login|logout|reconnect <server>]".to_string()),
            source: Source::Builtin,
        }]
    }
    fn run_command(&self, name: &str, args: &str, ui: &mut dyn Ui) -> Result<(), OxaError> {
        if name != "mcp" {
            return Err(OxaError::new(
                codes::NOT_FOUND,
                format!("unknown command: /{name}"),
            ));
        }
        let mut words = args.split_whitespace();
        let sub = words.next();
        let target = words.next();
        let usage = || OxaError::new(codes::USAGE, "usage: /mcp [login|logout|reconnect <server>]");
        if words.next().is_some() {
            return Err(usage());
        }
        match (sub, target) {
            (None, _) => {
                let servers = self.servers();
                if servers.is_empty() {
                    ui.notify("No MCP servers configured", Level::Info);
                }
                for s in servers {
                    let line = match &s.state {
                        ServerState::Failed(e) => format!("{}: failed: {e}", s.config.name),
                        other => format!("{}: {} ({} tools)", s.config.name, other.label(), s.tools.len()),
                    };
                    ui.notify(&line, Level::Info);
                }
                Ok(())
            }
            (Some("reconnect"), Some(server)) => {
                self.reconnect(server)?;
                ui.notify(&format!("Reconnecting MCP server {server}"), Level::Info);
                Ok(())
            }
            (Some("login"), Some(server)) => {
                let cancel = CancelToken::new();
                let mut io = UiLoginIo { ui: &mut *ui };
                self.login(server, &mut io, LOGIN_TIMEOUT, &cancel)?;
                ui.notify(&format!("Signed in to MCP server {server}"), Level::Info);
                Ok(())
            }
            (Some("logout"), Some(server)) => {
                let had = self.logout(server)?;
                let text = if had {
                    format!("Signed out of MCP server {server}")
                } else {
                    format!("MCP server {server} had no stored credentials")
                };
                ui.notify(&text, Level::Info);
                Ok(())
            }
            _ => Err(usage()),
        }
    }
    /// `[("mcp_servers", servers_section(config))]`, or empty when the text is empty.
    fn prompt_sections(&self) -> Vec<(String, String)> {
        let text = servers_section(&self.live_config());
        if text.is_empty() {
            Vec::new()
        } else {
            vec![("mcp_servers".to_string(), text)]
        }
    }
    /// No hook point is used (`fire` always continues).
    fn wants(&self, _p: HookPoint) -> bool {
        false
    }
    fn name(&self) -> &str {
        "mcp"
    }
    fn shutdown(&self) {
        self.stop();
    }
}
