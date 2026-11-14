//! Architect-owned (W0): the data model shared by config, transports, OAuth and the manager.
use oxa_proto::{Access, Exposure, ToolClass, Value};
use std::path::PathBuf;
use std::sync::Arc;

/// Asks the session's permission policy (and, through it, the user). `true` = allowed.
/// Given by the embedder; the manager never decides permissions itself.
pub type AccessGate = Arc<dyn Fn(&Access) -> bool + Send + Sync>;

/// Which `mcp.json` defines something.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConfigScope {
    /// `<agent dir>/mcp.json`.
    User,
    /// `<cwd>/.oxa/mcp.json` (trusted projects only).
    Project,
}

/// A stdio server: `command` is one executable, never a shell string.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StdioConfig {
    pub command: String,
    pub args: Vec<String>,
    /// Raw values (`${VAR}` and `!command` unexpanded), in file order.
    pub env: Vec<(String, String)>,
    /// Relative values resolve against the session directory; `~/` is the home directory.
    pub cwd: Option<String>,
}

/// How the client identifies itself to the authorization server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ClientRegistration {
    /// Registered client if `clientId` is set, else dynamic registration.
    #[default]
    Auto,
    /// Always register dynamically.
    Dcr,
    /// Client ID Metadata Document: not available in oxa (no hosted document);
    /// `unsupported_in_oxa`.
    Cimd,
}

/// The `oauth` object of an HTTP server.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OAuthSettings {
    pub client_id: Option<String>,
    /// Raw value (may be `${VAR}`).
    pub client_secret: Option<String>,
    pub callback_port: Option<u16>,
    pub callback_url: Option<String>,
    pub scope: Option<String>,
    pub client_name: Option<String>,
    pub client_registration: ClientRegistration,
    pub auth_server_metadata_url: Option<String>,
}

/// A streamable HTTP server.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HttpConfig {
    pub url: String,
    /// Raw values (`${VAR}` and `!command` unexpanded), in file order.
    pub headers: Vec<(String, String)>,
    pub oauth: Option<OAuthSettings>,
    /// H (SET-050): the `httpProxy` setting (agent-directory settings only), set by the
    /// embedder, never read from `mcp.json`. `None` = the HTTP client's default.
    pub proxy: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerKind {
    Stdio(StdioConfig),
    Http(HttpConfig),
}

/// One effective server entry after the user/project merge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    /// Letters, digits, `_`, `-`.
    pub name: String,
    pub kind: ServerKind,
    pub enabled: bool,
    /// Server-level exposure; default `Codemode`.
    pub exposure: Exposure,
    /// `toolExposure` in file order: `(exact name or `*` pattern, exposure)`.
    pub tool_exposure: Vec<(String, Exposure)>,
    pub description: Option<String>,
    pub timeout_s: u32,
    /// The file that defines the server (`command` / `url`).
    pub defined_in: ConfigScope,
    /// A project entry without `command` / `url` / `type` overrides `enabled`, `exposure`,
    /// `toolExposure` of this user-level server.
    pub project_override: bool,
    /// Item 4 (sec-prod): the server's `readOnlyHint` annotations are believed (a tool the server
    /// labels read-only runs without a prompt in plan and workspace modes). Default `true` for a
    /// stdio server, `false` for a remote (HTTP) one; `"trustAnnotations"` in the entry overrides.
    pub trust_annotations: bool,
}

/// The merged configuration.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct McpConfig {
    /// Sorted by name.
    pub servers: Vec<ServerConfig>,
    /// `autoEnableCodemode` beside `mcpServers` (project wins); `None` = default (true).
    pub auto_enable_codemode: Option<bool>,
    /// Invalid entries and other problems, one line each; the entries are skipped.
    pub diagnostics: Vec<String>,
}

/// Annotations a server declares for a tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ToolAnnotations {
    pub read_only_hint: Option<bool>,
    pub destructive_hint: Option<bool>,
    pub idempotent_hint: Option<bool>,
    pub open_world_hint: Option<bool>,
}

impl ToolAnnotations {
    /// The permission class used for `Access::Mcp`: `Read` only when the server says
    /// `readOnlyHint: true` and the annotations are trusted (`ServerConfig::trust_annotations`);
    /// everything else is `Write`. The hint is the server's claim, not checked by oxa.
    pub fn class(&self, trusted: bool) -> ToolClass {
        if trusted && self.read_only_hint == Some(true) {
            ToolClass::Read
        } else {
            ToolClass::Write
        }
    }
}

/// A tool as listed by a server.
#[derive(Debug, Clone, PartialEq)]
pub struct McpTool {
    /// The server's own tool name.
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub annotations: ToolAnnotations,
}

/// The result of `tools/call`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CallToolResult {
    /// Content blocks as received (`{type: "text", text}`, `{type: "image", ..}`,
    /// `{type: "resource_link", ..}`, ..).
    pub content: Vec<Value>,
    pub structured_content: Option<Value>,
    pub is_error: bool,
}

/// A resource or resource template as listed by a server (remaining fields in `extra`).
#[derive(Debug, Clone, PartialEq)]
pub struct McpResource {
    pub uri: String,
    pub name: String,
    pub extra: Value,
}

/// Connection state of a server, as shown by `/mcp` and `oxa mcp list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerState {
    Disabled,
    /// Waiting for the permission gate or not started yet.
    Pending,
    Connecting,
    Connected,
    /// The server rejected an unauthenticated connection; sign in with `/mcp login <name>`.
    NeedsAuth,
    /// The permission gate refused to start / reach it.
    Denied,
    /// Connection or protocol failure; the full error text.
    Failed(String),
}

impl ServerState {
    /// Servers that need attention sort first in `/mcp`.
    pub fn needs_attention(&self) -> bool {
        matches!(
            self,
            ServerState::NeedsAuth | ServerState::Denied | ServerState::Failed(_)
        )
    }
    pub fn label(&self) -> &'static str {
        match self {
            ServerState::Disabled => "disabled",
            ServerState::Pending => "pending",
            ServerState::Connecting => "connecting",
            ServerState::Connected => "connected",
            ServerState::NeedsAuth => "needs sign-in",
            ServerState::Denied => "denied",
            ServerState::Failed(_) => "failed",
        }
    }
}

/// One tool of a server with its effective exposure.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolStatus {
    /// The server's own name for it.
    pub name: String,
    /// `mcp__<server>__<tool>` (mangled; see `names.rs`).
    pub full_name: String,
    pub description: String,
    pub exposure: Exposure,
    /// `true` when `exposure` differs from the server's.
    pub overridden: bool,
    pub annotations: ToolAnnotations,
}

/// Everything `/mcp` and `oxa mcp list` show about one server.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerStatus {
    pub config: ServerConfig,
    pub state: ServerState,
    pub tools: Vec<ToolStatus>,
    pub has_resources: bool,
    /// The server's `instructions` from `initialize`, when connected.
    pub instructions: Option<String>,
    /// Tail of stderr of a stdio server (last 20 lines).
    pub stderr_tail: String,
    /// The file that would receive an exposure / enabled change.
    pub saves_to: PathBuf,
}
