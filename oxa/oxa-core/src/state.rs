//! Architect-owned (W0-FROZEN): the shared state every core module works on.
use crate::{Shared, Stats, ToolSelection, Toolset};
use oxa_compact::CompactPolicy;
use oxa_config::{Dirs, Settings};
use oxa_proto::{
    Engine, EngineInfo, FrozenPrefix, Hooks, Mode, Policy, Sandbox, ThinkingLevel, Tool, Usage, WireMsg,
};
use oxa_resources::Resources;
use oxa_session::{SessionRef, SessionStore, Transcript, Tree};
use std::path::PathBuf;
use std::sync::Arc;

/// Everything a running session owns. `Session` (C1) wraps exactly one of these; the core
/// modules are free functions over `&mut SessionState` so that tasks can be built and tested
/// independently with a scripted engine and a `MemStore`.
///
/// Field rules:
/// * `tree` + `transcript` + the session file change only through `append_entry` and the
///   rebase doors (see the crate docs).
/// * `prefix`, `all_tools`, `active`, `resources`, `settings` change only in `epoch.rs`.
/// * `model`, `thinking`, `mode` change only through `Session::set_*`, which also append the
///   matching entry.
pub struct SessionState {
    pub engine: Arc<dyn Engine>,
    pub store: Arc<dyn SessionStore>,
    pub hooks: Vec<Arc<dyn Hooks>>,
    pub policy: Box<dyn Policy>,
    pub sandbox: Arc<dyn Sandbox>,
    pub settings: Settings,
    pub dirs: Dirs,
    pub cwd: PathBuf,
    /// Built-ins + hook tools as read at the last epoch.
    pub all_tools: Vec<Arc<dyn Tool>>,
    /// The CLI-level tool choices (`--tools`, `-xt`, `-nbt`, `-nt`). Kept so that a later
    /// epoch (`/reload`, model switch) resolves the same selection; `default_tools` is taken
    /// from `settings` again at each epoch.
    pub selection: ToolSelection,
    /// The selection in force for this epoch.
    pub active: Toolset,
    pub resources: Resources,
    pub sref: SessionRef,
    pub tree: Tree,
    pub transcript: Transcript,
    pub prefix: FrozenPrefix,
    pub info: EngineInfo,
    pub model: String,
    pub thinking: ThinkingLevel,
    pub mode: Mode,
    pub compact: CompactPolicy,
    /// Queues, cancel tokens and the idle flag shared with every `Handle`.
    pub shared: Arc<Shared>,
    /// Messages held for the next prompt (`Delivery::NextTurn`).
    pub next_turn: Vec<WireMsg>,
    /// Usage of the last completed chat request.
    pub last_usage: Option<Usage>,
    pub stats: Stats,
    /// Where `shape` spills (`<tmp_dir>/<session id>`).
    pub spill_dir: PathBuf,
    /// Environment for tool child processes (`OXA_SESSION_*`, `AI_AGENT=oxa`).
    pub tool_env: Vec<(String, String)>,
    /// How the built-ins shape results in the current epoch (`ToolCtx::tune`); default = pi.
    pub tune: oxa_proto::ToolTune,
    /// `false` after `set_auto_compaction(false)` (RPC).
    pub auto_compaction: bool,
    /// `false` after `set_auto_retry(false)` (RPC).
    pub auto_retry: bool,
    /// The receiving end of the session's `CoreHost` (E-INT 1): requests made by hook sets
    /// through `Host::call`. `None` until a session with hook sets is entered (and in tests
    /// that build the state by hand); hooks and tools then run on the session thread itself.
    pub host_rx: Option<std::sync::mpsc::Receiver<crate::HostRequest>>,
    /// Host requests accepted but held back until the run settles (session-shape actions,
    /// `WaitForIdle`). Dropping them answers their callers with `internal`.
    pub host_deferred: Vec<crate::HostRequest>,
    /// In-memory compaction memory (the pending idle summary, the cancel count of the R4
    /// fallback). Never persisted; `Default` = empty.
    pub compact_memo: crate::compaction::CompactMemo,
    /// The background-job registry the `bash` tool shares with this session (issue #127 (b)).
    /// Killed on `Session::close` / `shutdown`.
    pub jobs: Arc<oxa_tools::JobRegistry>,
}
