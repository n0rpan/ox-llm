//! W0-FROZEN
//! # oxa-mcp: Model Context Protocol client
//!
//! stdio and streamable HTTP transports, OAuth (PKCE, dynamic registration), `mcp.json`
//! (user + project), the four exposure modes, the resource tools, `mcp.log`, and the manager
//! that offers all of it to the core as `dyn oxa_proto::Hooks`. Behaviour follows pi's mcp.md.
//!
//! ## Contracts
//!
//! * **W3: exposure and the prefix.** Only `Exposure::Direct` tools are declared to the model,
//!   and the core reads them once per epoch: a server that connects later is offered at the
//!   next epoch (`/reload`); nothing here ever triggers an epoch. `Codemode` (the default) and
//!   `Deferred` tools never touch the prefix: they are reached through `oxa-codemode`'s
//!   catalog. The `mcp_servers` prompt section is built from *configuration only*
//!   (name, how its tools are reached, the configured `description`), never from connection
//!   state or server instructions, so it is identical on every start (adaptation of pi, which
//!   updates the section when servers connect).
//! * **W4.** A tool result's text is the server's text content in order, deterministic for a
//!   deterministic server; it then passes `shape` in the loop like every tool result (pi's own
//!   20 KB middle-cut is replaced by `shape`).
//! * **Permissions.** Starting a stdio server asks the [`AccessGate`] with
//!   `Access::Exec { command: "<command> <args..>", net: true }`; connecting an HTTP server
//!   asks with `Access::Net { host }`. Each tool call is checked by the core's `Policy` from
//!   `Tool::access` = `[Access::Mcp { server, tool, class }]`, where `class` comes from the
//!   server's annotations (`readOnlyHint` -> `Read`, else `Write`; `openWorldHint` alone does
//!   not change it). The hint is believed for a stdio server, and for a remote (HTTP) one only
//!   with `"trustAnnotations": true` in its `mcp.json` entry (sec-prod item 4). A stdio server runs with the user's rights once allowed (it needs its own
//!   caches and network); it is started in its own process group and stopped by: stdin close,
//!   then SIGTERM, then SIGKILL of the group.
//! * **Trust.** The project `mcp.json` is read only when the project is trusted.
//! * **Never blocks startup.** Servers connect in the background; the first prompt waits at
//!   most [`FIRST_PROMPT_WAIT_S`] seconds, and only for servers with `Direct` tools.
//! * **Failures are data.** An invalid entry, a failed connection or a needed sign-in is a
//!   [`ServerState`], reported once; other servers are unaffected. Tool calls are never
//!   retried; resource reads and lists are retried once after 408 / 429 / 5xx.
//! * **Omitted / rejected:** the legacy `sse` transport type (pi rejects it too), MCP Apps
//!   resources (`ui://`), image content (replaced by the fixed image placeholder line).
//! * No async runtime: one reader thread per connection, `mpsc` to the callers.

mod config;
mod http;
mod manager;
mod names;
mod oauth;
mod offline;
mod private_file;
mod proto;
mod stdio;
mod transport;
mod types;

pub use config::*;
pub use http::*;
pub use manager::*;
pub use names::*;
pub use oauth::*;
pub use proto::*;
pub use stdio::*;
pub use transport::*;
pub use types::*;

/// Seconds the first prompt waits for servers with `Direct` tools.
pub const FIRST_PROMPT_WAIT_S: u64 = 10;
/// Default per-request timeout in seconds (`timeout` in `mcp.json`); progress resets it.
pub const DEFAULT_TIMEOUT_S: u32 = 60;
/// MCP protocol revision this client speaks.
pub const PROTOCOL_VERSION: &str = "2025-11-25";
/// `mcp.log` moves to `mcp.log.1` after it grows past this size.
pub const LOG_ROTATE_BYTES: u64 = 5 * 1024 * 1024;
/// Error code of a connection that needs an OAuth sign-in.
pub const CODE_NEEDS_AUTH: &str = "mcp_needs_auth";
/// File names inside the agent directory / project directory.
pub const CONFIG_FILE: &str = "mcp.json";
pub const AUTH_FILE: &str = "mcp-auth.json";
pub const LOG_FILE: &str = "mcp.log";
