//! W0-FROZEN (architect-owned).
//!
//! # oxa-core: the agent loop, the session runtime and the SDK surface
//!
//! The Rust API of this crate is the SDK (plan sec. 1): [`Session`], [`Handle`], [`Deps`],
//! [`Open`], [`State`], [`Stats`], [`build_prefix`]. Other languages use RPC (`oxa-rpc`).
//!
//! ## The loop (ported from pi `packages/agent/src/agent-loop.ts`)
//! push user -> `chat` -> push reply (W2) -> per tool call in order: hooks `ToolCall` ->
//! `Tool::access` -> `Policy` (-> `Ui::ask_access`) -> `Tool::run` -> hooks `ToolResult` ->
//! `shape` -> push `tool` message -> drain steering -> repeat. No tool calls: follow-ups, then
//! `compact::decide`, settle. Read-class calls of a batch may run in parallel; results are
//! pushed in call order. Unknown tool / bad JSON = a fixed error result
//! (`oxa_proto::tool_errors`). `Length` without tool calls: one automatic continue. After each
//! reply `cached < expected_cache()` emits `CacheMiss` with its cause.
//!
//! ## Contracts
//! * **One door for appends:** [`append_entry`] persists an entry, adds it to the tree and
//!   folds it into the transcript. Nothing else writes the session or pushes to the wire.
//! * **One door for requests:** [`request`] builds the `ChatRequest` from [`SessionState`];
//!   retries wrap it in `retry.rs`. Nothing else calls `Engine::chat`.
//! * **W1/W3:** rebases happen only in `compaction.rs`, `navigate.rs`, `epoch.rs` and through
//!   a declared extension rebase in `exec.rs`'s guard path, each via `Transcript::rebase` with
//!   its cause. An epoch is opened only by `epoch.rs`, only between runs.
//! * **W2:** only a complete `AssistantTurn` is pushed. An aborted / failed / cut reply becomes
//!   an `EntryBody::Shown` and the user message stays on the wire for the re-ask.
//! * All prompt text lives in `prompt.rs` (architect-owned). No other file contains text that
//!   reaches the model, except tool results.
//! * Threading: a `Session` lives on one thread; `prompt` blocks until settled. [`Handle`] is
//!   `Send + Clone` and is the only cross-thread surface (steer, follow-up, abort).
#![forbid(unsafe_code)]

mod agent_loop;
mod commands;
mod compaction;
mod done;
pub mod envnote;
mod epoch;
mod exec;
mod fire;
mod host;
mod navigate;
mod prefix;
mod profile;
pub mod prompt;
mod queue;
pub mod reload;
pub mod repair;
pub mod repeat;
mod resume;
mod retry;
pub mod sampling;
mod session;
mod state;
mod subagent;
mod think;
mod toolset;
mod tune;
mod user_bash;

pub use agent_loop::{append_entry, run_loop, RunEnd};
pub use commands::{
    all_commands, builtin_commands, cache_text, engine_status_lines, expand_input, run_builtin,
    write_bug_report, Expanded,
};
pub use compaction::{boot_preload, compact_now, idle_prefill, maybe_compact, SETTING_BOOT_PRELOAD};
pub use epoch::{epoch_inputs, open_epoch, reload, replay_prefix};
pub use exec::{execute_calls, request, RequestOpts, EXTENSION_REBASE_ENTRY, TOOL_CALL_ID_ENV};
pub use host::{serve_host, serve_pending, CoreHost, HostRequest};
pub use navigate::{clone_current, fork_at, fork_from, navigate, ForkPosition, Forked};
pub use prefix::{build_prefix, build_prefix_with, tools_json, PrefixOpts};
pub use profile::{declared_specs, profile_of_branch, PROFILE_ENTRY};
pub use queue::{Gate, Handle, Queue, Shared};
pub use retry::{backoff_ms, chat_with_retry, RetryPolicy};
pub use session::{prefill_estimate_secs, Deps, Open, OverCap, Session, State, Stats, SwitchPlan};
pub use state::SessionState;
pub use subagent::{discover_agents, AgentDef, SubagentHooks};
pub use think::{
    classify, classify_tree, task_prompt, thinking_for, tool_followups, Escalation, Reason, ReplyKind,
    ToolFollowups, STATUS_KEY as THINKING_STATUS_KEY, STATUS_ON as THINKING_STATUS_ON,
};
pub use toolset::{resolve_toolset, MutationQueue, ToolSelection, Toolset};
pub use user_bash::user_bash;

/// A fresh background-job registry for a session (issue #127 (b)). Re-exported so tests
/// and front ends can fill `Deps::jobs` without importing `oxa_tools` directly.
pub fn fresh_jobs() -> std::sync::Arc<oxa_tools::JobRegistry> {
    std::sync::Arc::new(oxa_tools::JobRegistry::new())
}
