//! W0-FROZEN
//! # oxa-resources: context files, skills, prompt templates, packages
//!
//! Everything that is *discovered on disk* and feeds the frozen prefix or the command list.
//!
//! ## Contracts
//!
//! * **Read once per epoch (W3).** [`discover`] is called when a session opens and on `/reload`
//!   only. Its result is plain data; `oxa_core::build_prefix` turns it into prefix sections.
//!   Nothing here watches files or re-reads behind the caller's back.
//! * **Deterministic.** Same disk state + same inputs = the same [`Resources`], field for field
//!   and in the same order (documented per function). No timestamps, no absolute-time data.
//! * **Warn, never fail.** A malformed skill, template, package or context file becomes a
//!   [`Diagnostic`] and is skipped. `discover` cannot fail.
//! * **Trust gates what loads.** With `DiscoverOpts::trusted == false` nothing under the project
//!   (`.oxa/`, project `.agents/skills`, project packages, project `SYSTEM.md`) is loaded,
//!   except the context files (`AGENTS.md`, `CLAUDE.md`), which pi also reads untrusted.
//! * **Names.** pi's `~/.pi/agent` is `Dirs`' agent dir (`~/.oxa/agent`), `.pi/` is `.oxa/`.
//!   `AGENTS.md`, `CLAUDE.md` and `.agents/skills` are read exactly as pi reads them. pi
//!   directories are never read implicitly.
//! * Only [`oxa_config::Dirs`] knows where the home and agent directories are; nothing here
//!   reads `HOME`, `XDG_*` or `OXA_*` from the environment (test isolation, P56).

mod builtin;
mod context;
mod frontmatter;
mod packages;
mod resources;
mod skills;
mod sources;
mod templates;

pub use builtin::*;
pub use context::*;
pub use frontmatter::*;
pub use packages::*;
pub use resources::*;
pub use skills::*;
pub use sources::*;
pub use templates::*;
