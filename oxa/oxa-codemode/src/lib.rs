//! W0-FROZEN
//! # oxa-codemode: scripts that call tools, and `tool_search` / `call_tool`
//!
//! `codemode` lets the model write a JavaScript script that calls oxa's other tools; only the
//! script's output reaches the model. Same QuickJS runtime as extensions (`oxa-script`).
//!
//! ## Contracts
//!
//! * **Sandbox.** A script has the globals of [`api::install_globals`] and nothing else. Tools
//!   are reached only through `Host::call(HostAction::CallTool)`, i.e. through the full loop
//!   path (hooks, `Policy`, `shape`).
//! * **W3: the prefix never changes because of codemode or search.** The `codemode`,
//!   `tool_search` and `call_tool` specs are constant texts: they never list tools. Tools with
//!   `Exposure::Codemode` / `Deferred` are found at run time (`searchTools`, `ALL_TOOLS`,
//!   `tool_search`) from the [`Catalog`], which may grow while MCP servers connect.
//! * **`tool_search` is adapted (verdict A).** pi declares matched tools for the next call;
//!   that would break W3. Here matches are returned as text and called through the one frozen
//!   dispatcher `call_tool {name, args}`; a deferred tool is callable only after a search of
//!   this session matched it.
//! * **W4.** The result text is deterministic for a deterministic script: no wall time (pi
//!   prints it; oxa omits it), fixed headers, output capped by `max_output_tokens` with head +
//!   tail and a spill file. The loop still passes it through `shape`.
//! * **Tool input (verdict A).** pi takes raw JavaScript as a free-form tool input; over
//!   `/v1/chat/completions` the arguments are JSON: `{"code": "<script>"}`.
//! * **Omitted (verdict O):** `image()`, `models.*` -> throw `unsupported_in_oxa`.
//! * **Limits** (pi's): 256 MiB per script VM; at most [`DEFAULT_MAX_WORKERS`] workers alive
//!   per process; a script that gets no worker within 60 s fails; scripts cannot start
//!   `codemode`; a promise that can never settle fails at once.

pub mod api;
mod hooks;
mod pool;
mod search;
mod tool;

pub use hooks::*;
pub use pool::*;
pub use search::*;
pub use tool::*;

/// pi's worker cap (`OXA_CODEMODE_MAX_WORKERS` overrides it; read by `oxa-config`).
pub const DEFAULT_MAX_WORKERS: usize = 2;
/// Seconds a script waits for a free worker before it fails.
pub const WORKER_WAIT_S: u64 = 60;
/// Tool names owned by this crate.
pub const CODEMODE_TOOL: &str = "codemode";
pub const TOOL_SEARCH_TOOL: &str = "tool_search";
pub const CALL_TOOL_TOOL: &str = "call_tool";
/// `custom_type` of the session entry that persists `store()` values.
pub const STORE_ENTRY_TYPE: &str = "codemode-store";
