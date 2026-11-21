//! W0-FROZEN (architect-owned).
//!
//! # oxa-fake (test-only)
//! A fake engine that implements the engine contract of plan sec. 3, an in-process scripted
//! `Engine`, the W1-W6 checker, the golden request-hash runner and test isolation.
//!
//! ## What is here
//! * [`ScriptedEngine`]: an in-process `oxa_proto::Engine` driven by [`ScriptTurn`]s. Used by
//!   unit tests of `oxa-core` and above. Implemented in W0.
//! * [`FakeServer`] (A14): the same behaviour over real HTTP + SSE, with wire faults
//!   (`--done-gap-ms`, `--hold-headers`, `--comments-only`, `--drop-at`). Used by `oxa-engine`
//!   tests, the invariant suite and PARITY.
//! * [`check_all`] (B15): the invariant rules over recorded requests, each with a negative
//!   control. [`golden`]: pinned request hashes.
//! * [`TestEnv`]: an isolated directory tree for one test (P56). Implemented in W0.
//! * [`fixtures`]: named `AgentEvent` sequences (chat, a tool run with a diff, streaming bash,
//!   compaction, retry, error, worker cards, a secret ask). The terminal UI and the desktop app
//!   render the same sequences in their tests, so the two front ends cannot drift apart.
//!
//! ## The fake's cache model (shared by both engines)
//! One token = one byte of [`cache_bytes`]. The engine remembers, per session id, the bytes of
//! the last prompt plus the reply it generated minus the reply's last token (F1: the real engine
//! never feeds the last generated token). `cached` = length of the common prefix of the new
//! prompt bytes with that memory; `prompt` = length of the new prompt bytes; `completion` =
//! length of the reply's canonical bytes + 1 (the `\n` that follows it in the next prompt; that
//! is the token not held) unless the script gives a number. So `cached == previous prompt +
//! previous completion - 1` exactly when W1 and W2 hold, which is what
//! `Transcript::expected_cache` predicts. Prefix cache: the anchor (the bytes up to and including
//! the second `\0`: system text + tools) is shared across sessions, so a request reports at least
//! the anchor when any session holds it (a fork, a new session); `Fault::Cold` serves nothing. A `max_tokens: 0` request (H6) consumes no script
//! turn, generates nothing, holds the prompt and records no reply; its `cached` rule is
//! `prompt` (completion 0).
//!
//! ## Engine parity (F6, F7, H4, F13)
//! A prompt of `n_ctx` tokens or more is a 400 `exceed_context_size_error`; a shorter one is
//! answered, the reply bounded by `max_tokens` and by the room left (`n_ctx - prompt`): a reply
//! that does not fit is cut there with `finish_reason: "length"`. H6 ends with `length`.
//! `timings.snap_pos` is present only when `X-Oxe-Snapshot: keep` was sent. The HTTP fake
//! refuses a re-sent tool call whose `arguments` is not JSON (`FakeOpts::strict_tool_args`).
//!
//! ## The trailing-assistant rule (F9, both engines)
//! As in the real oxe service (`chat::chat_inputs`, F9 / OpenAI v1), a generating request whose
//! last message is an assistant message is history plus a NEW assistant turn: it is answered
//! (consuming a script turn) like any other request. Only an explicit in-place continuation, a
//! generating request (`max_tokens > 0`) ending in an assistant message whose body says
//! `"add_generation_prompt": false`, is refused with HTTP 400 [`CONTINUATION_400`] (HTTP fake
//! only: a `ChatRequest` cannot express it). An H6 prefill (`max_tokens: 0`) is unchanged.
//!
//! Script format: `docs/fake-script.md`.

mod check;
pub mod fixtures;
pub mod golden;
mod record;
mod script;
mod scripted;
mod server;
mod testenv;
mod turn;

pub use check::{
    check_all, check_discipline, check_hints, check_w1, check_w2, check_w3, check_w5, check_w6, Rebases,
    Violation, SIDE_MAX_TOKENS,
};
pub use record::{parse_recorded, write_log};
pub use script::{parse_script, script_to_text};
pub use scripted::ScriptedEngine;
pub use server::{
    main_from_args, CancelMode, CancelRecord, ChatTiming, FakeCaps, FakeModel, FakeOpts, FakeServer,
};
pub use testenv::TestEnv;
/// The real engine's 400 message for an explicit in-place continuation (`chat::chat_inputs`, F9).
pub const CONTINUATION_400: &str = "continuing a trailing assistant message in place (add_generation_prompt: false) is not supported by this server; send it with the generation prompt (the default) to get a new assistant turn";

/// `true` when the real engine refuses this request with [`CONTINUATION_400`]: it generates
/// (`max_tokens > 0`), says `add_generation_prompt: false` and ends in an assistant message.
pub fn rejects_continuation(
    messages: &[oxa_proto::WireMsg],
    max_tokens: u32,
    add_generation_prompt: Option<bool>,
) -> bool {
    max_tokens > 0
        && add_generation_prompt == Some(false)
        && messages.last().map(|m| m.role) == Some(oxa_proto::Role::Assistant)
}

pub use turn::{cache_bytes, kind_from_str, render_history, Fault, Recorded, ScriptTurn};
