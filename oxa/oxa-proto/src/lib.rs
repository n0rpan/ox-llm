//! # oxa-proto v2 (FROZEN)
//!
//! The shared vocabulary of every `oxa` crate: wire messages, session entries, agent events,
//! the tool / hook / UI / engine / sandbox / policy traits, and the error type.
//!
//! **Freeze rule.** This crate is written by the architect in W0 and is frozen from then on.
//! A change goes through the architect only and bumps [`PROTO_VERSION`]. Wave tasks consume
//! these types; they never edit this crate. `cargo public-api` on this crate must be empty
//! versus the W0 tag at every wave checkpoint.
//!
//! ## The invariants every consumer must keep (plan sec. 2)
//!
//! * **W1 append-only wire.** Request N+1's `messages` are request N's messages, plus the reply
//!   exactly as received, plus new messages, byte for byte. The wire only shrinks or changes
//!   through a *rebase*, and only the five causes in [`rebase_why`] may rebase.
//! * **W2 exact echo.** An assistant message goes back as streamed: `content` untrimmed,
//!   `reasoning` verbatim, tool calls with the server's id and [`ToolCall::args_raw`] as
//!   received. An aborted, failed or cut reply is shown ([`EntryBody::Shown`]) but never sent.
//! * **W3 frozen prefix, by epoch.** System prompt + tool list are built once per epoch into a
//!   [`FrozenPrefix`], stored as a session entry and replayed on resume. No date, no git status,
//!   no mode text; cwd is the last section.
//! * **W4 deterministic capped tool output.** Same input + file state = same bytes; no timings,
//!   no ANSI; everything a model sees from a tool went through `oxa_tools::shape`.
//!   [`ToolOutput::ui`] never reaches the wire.
//! * **W5 compaction after the reply, by pure appends.** The summary request is the whole wire
//!   plus one user instruction. A prompt is never folded.
//! * **W6 explicit sampling.** `temperature: 0`, `cache_prompt: true`,
//!   `chat_template_kwargs {preserve_thinking: true, enable_thinking: <level != off>}`
//!   (`preserve_thinking` is left out for a model whose capabilities say it does not keep
//!   reasoning: F8, never forced on Flash).
//! * **W-guard.** See [`HookPoint::guard`].
//!
//! No async runtime: threads + `std::sync::mpsc`. Everything here is plain data or an
//! object-safe trait.
#![forbid(unsafe_code)]

mod engine;
mod entry;
mod error;
mod event;
mod exec;
mod hook;
mod key;
mod local;
mod tool;
mod ui;
mod wire;

pub mod guard;
pub mod panic;

pub use engine::*;
pub use entry::*;
pub use error::*;
pub use event::*;
pub use exec::*;
pub use hook::*;
pub use key::*;
pub use local::*;
pub use tool::*;
pub use ui::*;
pub use wire::*;

/// JSON value type used across the workspace (`serde_json` with `preserve_order`).
pub use serde_json::Value;

/// Version of this frozen interface. Bumped only by the architect.
pub const PROTO_VERSION: u32 = 2;

/// Product names (plan sec. 0). Never spell these literally elsewhere.
pub mod names {
    /// Binary and product name.
    pub const APP: &str = "oxa";
    /// Per-user agent directory, relative to `$HOME`; overridden by [`ENV_AGENT_DIR`].
    pub const AGENT_DIR: &str = ".oxa/agent";
    /// Per-project directory, relative to the project root.
    pub const PROJECT_DIR: &str = ".oxa";
    /// Environment variable that overrides the agent directory.
    pub const ENV_AGENT_DIR: &str = "OXA_AGENT_DIR";
    /// Set to `1` by `oxa_fake::TestEnv::envs` for every test process: test-only hooks are
    /// honoured only with it, and nothing falls back to a host directory under it.
    pub const ENV_TEST_ISOLATION: &str = "OXA_TEST_ISOLATION";
    /// Value of the `AI_AGENT` environment variable set for child processes.
    pub const AI_AGENT: &str = "oxa";
    /// Environment variable (in `ToolCtx::env`) that carries the id of the running tool call;
    /// extension tools pass it to `execute` as `toolCallId`.
    pub const ENV_TOOL_CALL_ID: &str = "OXA_TOOL_CALL_ID";
    /// Session file format version (pi v3 envelope).
    pub const SESSION_VERSION: u32 = 3;
}
