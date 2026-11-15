//! Architect-owned: the view context and the block dispatcher (complete). The views are tasks
//! D4 (`messages.rs`) and D5 (`tools.rs`).
use crate::{Block, ViewSettings};
use oxa_config::Keymap;
use oxa_proto::Hooks;
use oxa_tui::{TermCaps, Theme};
use std::sync::Arc;
use std::sync::OnceLock;

mod messages;
mod tools;

pub use messages::*;
pub use tools::*;

/// Everything a view may read. Views are pure: same block + same context = same lines.
pub struct ViewCx<'a> {
    pub width: u16,
    pub theme: &'a Theme,
    pub settings: &'a ViewSettings,
    pub caps: TermCaps,
    /// Hook sets for extension renderers (`Hooks::render`); empty in most tests.
    pub hooks: &'a [Arc<dyn Hooks>],
    /// Session cwd, for shortening paths in tool headers.
    pub cwd: &'a str,
    /// Home directory, for showing paths as `~`; empty = no shortening. Views never read the
    /// process environment (P56).
    pub home: &'a str,
    /// The live keymap, for the key labels in hints (D-INT). `None` = the default bindings.
    pub keymap: Option<&'a Keymap>,
}

impl ViewCx<'_> {
    /// The first key bound to `action` as shown in hints; the action id when it is unbound.
    pub fn key_label(&self, action: &str) -> String {
        static DEFAULTS: OnceLock<Keymap> = OnceLock::new();
        let keymap = self
            .keymap
            .unwrap_or_else(|| DEFAULTS.get_or_init(Keymap::defaults));
        keymap
            .keys_for(action)
            .first()
            .map(|k| k.to_string())
            .unwrap_or_else(|| action.to_string())
    }
}

/// Lines of one block at `cx.width` (no blank line before or after; the caller separates
/// blocks). Dispatches to D4 / D5.
pub fn render_block(block: &Block, cx: &ViewCx) -> Vec<String> {
    if cx.settings.skin.is_oxa() {
        return crate::skin::render_block_oxa(block, cx);
    }
    match block {
        Block::User { text, images } => render_user(text, images, cx),
        Block::Assistant {
            text,
            thinking,
            streaming,
            stop,
            error,
            ..
        } => render_assistant(text, thinking, *streaming, *stop, error.as_deref(), cx),
        Block::Tool(t) => render_tool(t, cx),
        Block::Compaction {
            summary,
            tokens_before,
            tokens_after,
            expanded,
        } => render_compaction(summary, *tokens_before, *tokens_after, *expanded, cx),
        Block::BranchSummary { summary, expanded } => render_branch_summary(summary, *expanded, cx),
        Block::Custom {
            custom_type,
            content,
            details,
            expanded,
        } => render_custom(custom_type, content, details, *expanded, cx),
        Block::Bash {
            command,
            output,
            exit,
            excluded,
            running,
            expanded,
        } => render_bash(command, output, *exit, *excluded, *running, *expanded, cx),
        Block::Shown { text, why } => render_shown(text, why, cx),
        Block::Notice { level, text } => render_notice(*level, text, cx),
    }
}
