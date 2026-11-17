//! W0-FROZEN (architect-owned).
//!
//! # oxa-rpc: the headless modes
//! `-p` print, `--mode json` (JSONL event stream), `--mode rpc` (commands on stdin, responses
//! and events on stdout) and a Rust RPC client.
//!
//! ## Contracts
//! * Record shapes and event names are pi's (docs: json.md, rpc.md, rpc-commands.md,
//!   rpc-extension-ui.md), so pi JSON/RPC clients work unchanged. oxa's own events
//!   (`prefill`, `prefix_epoch`, `cache_miss`, `mode_changed`, `notice`) are additive.
//!   Additions to RPC: command `set_mode`; a permission Ask is relayed as an extension-UI
//!   `confirm` request. Print / json mode: Ask -> Deny (`HeadlessUi`).
//! * Framing: one JSON object per line, LF only, no pretty printing; stdout carries records
//!   only (diagnostics go to stderr).
//! * Print mode writes only the final assistant text to stdout.
//! * RPC threading: a reader thread parses commands; the session runs on its own thread;
//!   `steer`, `follow_up`, `abort`, `abort_retry`, `clear_queue`, `get_state` are answered
//!   while a prompt runs (through `Handle`), other commands answer `busy`-style errors as pi
//!   does. stdin close = shutdown.
#![forbid(unsafe_code)]

#[cfg(all(target_os = "linux", feature = "a2a-control"))]
mod a2a_control;

mod client;
mod cmd_run;
mod cmd_session;
mod events;
mod ext_ui;
mod print;
mod server;

pub use client::RpcClient;
pub use events::{
    entry_to_json, event_to_json, message_to_json, session_header_record, state_to_json, write_record,
    JsonSink,
};
pub use ext_ui::{RpcUi, UiRouter};
pub use print::{run_json, run_json_outcome, run_print, run_print_outcome};
pub use server::{parse_command, response_err, response_ok, serve, serve_outcome, RpcCommand};

use oxa_core::{Handle, Session};
use oxa_proto::{OxaError, Value};
use std::io::Write;
use std::sync::{Arc, Mutex};

/// A line-atomic, shared record writer (stdout in production).
pub type Out = Arc<Mutex<Box<dyn Write + Send>>>;

/// What command handlers work on. Owned by the session thread.
pub struct RpcCx {
    pub session: Session,
    pub handle: Handle,
    pub out: Out,
    /// Routes extension-UI responses from the reader thread to waiting dialogs.
    pub ui: Arc<UiRouter>,
    pub dirs: oxa_config::Dirs,
}

/// A command handler's verdict: `None` = not my command; `Some(Ok(data))` = success response
/// with `data` (`Value::Null` = no data field); `Some(Err(e))` = error response.
pub type Handled = Option<Result<Value, OxaError>>;

/// Run-control commands (C10).
pub fn handle_run(cx: &mut RpcCx, cmd: &RpcCommand) -> Handled {
    cmd_run::handle(cx, cmd)
}

/// Session commands (C11).
pub fn handle_session(cx: &mut RpcCx, cmd: &RpcCommand) -> Handled {
    cmd_session::handle(cx, cmd)
}
