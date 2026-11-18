//! W0-FROZEN (architect-owned).
//!
//! # oxa-compact
//! Compaction and branch-summary policy as pure functions. No I/O, no engine: `oxa-core`
//! (C3, C4) orchestrates.
//!
//! ## Contracts (W5)
//! * Compaction happens after the reply: [`decide`] returns `AfterReply` when the soft limit is
//!   crossed at run end, `MidRun` only above the hard limit (`n_ctx - max_tokens -
//!   reserveTokens`).
//! * The summary request is the WHOLE current wire plus one user instruction
//!   ([`summary_request`]): a pure append, so the engine reuses the full prefix. Same tools,
//!   thinking off, `background`.
//! * After the summary the wire is rebased to [`head_after`]: `[user:<summary>, tail]` (the
//!   system message comes from the prefix). In the `MidRun` case the user's current prompt is
//!   its own, unfolded message at the end of the tail: a prompt is never folded into a summary.
//! * All instruction and wrapper text is in `prompt.rs` (architect-owned).
#![forbid(unsafe_code)]

mod branch;
mod files;
mod mechanical;
mod policy;
pub mod prompt;
mod skills;
mod summary;

pub use branch::{branch_entries, branch_summary_request, budget_newest_first};
pub use files::file_lists;
pub use mechanical::mechanical_summary;
pub use policy::{decide, policy_from, CompactPolicy, When};
pub use skills::{active_skills, active_skills_block};
pub use summary::{
    first_kept_index, head_after, summary_request, summary_request_with_target, tail_elisions,
    ELIDE_MIN_TOKENS,
};
