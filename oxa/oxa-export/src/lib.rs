//! W0-FROZEN (architect-owned).
//!
//! # oxa-export: HTML export of sessions
//!
//! `/export`, `--export` and RPC `export_html` all end in [`export_html`]: a pure function from
//! a session (header + every entry of the file) to one self-contained HTML document built from
//! pi's export template (copied under MIT, see `assets/README.md`).
//!
//! Contracts:
//! * **Pure and deterministic.** Same header, entries and options = the same bytes. No clock,
//!   no environment, no file or network access inside this crate except [`write_export`].
//! * **Whole tree.** The export contains every entry of the file (all branches), like pi; the
//!   template draws the tree sidebar from `id` / `parentId`.
//! * **pi JSON shape.** The embedded session data is pi's v3 JSON (the bytes
//!   `oxa_session::encode_visible_entry` writes), base64-encoded into `{{SESSION_DATA}}`.
//! * **No wire involvement.** Export never touches the transcript or the engine (W1 is not at
//!   stake); `ToolOutput::ui` data may be shown.
//! * No third-party upload (`/share` is omitted in oxa).
#![forbid(unsafe_code)]

mod ansi;
mod html;
mod names;
mod template;

pub use ansi::*;
pub use html::*;
pub use names::*;
pub use template::*;
