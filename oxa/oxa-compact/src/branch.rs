//! B2: branch summaries for tree navigation.
// adapted from pi packages/coding-agent/src/core/compaction/branch-summarization.ts
use oxa_proto::{Entry, EntryBody, WireMsg};
use oxa_session::{bash_text, Tree};

use crate::prompt::{self, SummaryKind};

/// The entries that are abandoned when the leaf moves from `old_leaf` to `target`: from
/// `old_leaf` back to (excluding) the common ancestor of the two, oldest first. Empty when
/// `target` is a descendant of `old_leaf`.
///
/// An unknown `old_leaf` gives an empty result. An unknown `target` has no common ancestor with
/// `old_leaf`, so the whole old branch is returned (every entry of it is abandoned).
pub fn branch_entries<'a>(tree: &'a Tree, old_leaf: &str, target: &str) -> Vec<&'a Entry> {
    let path = tree.branch(old_leaf);
    if path.is_empty() {
        return Vec::new();
    }
    // `path` is root first; everything after the common ancestor is abandoned.
    let skip = match tree.common_ancestor(old_leaf, target) {
        Some(ca) => path.iter().position(|e| e.id == ca).map_or(0, |i| i + 1),
        None => 0,
    };
    path[skip..].to_vec()
}

/// Keep the newest entries whose token total fits `budget` (walk newest-first; an entry that
/// does not fit ends the walk), returned oldest first. Nested `BranchSummary` / `Compaction`
/// entries count with their summary text and are kept whole.
pub fn budget_newest_first<'a>(
    entries: &[&'a Entry],
    budget: u32,
    count: &dyn Fn(&str) -> u32,
) -> Vec<&'a Entry> {
    let mut total: u32 = 0;
    let mut kept: Vec<&'a Entry> = Vec::new();
    for e in entries.iter().rev() {
        let cost = count(&entry_text(e));
        let next = total.saturating_add(cost);
        if next > budget {
            break;
        }
        total = next;
        kept.push(*e);
    }
    kept.reverse();
    kept
}

/// The branch-summary request: `old_wire` (the abandoned branch's wire, unchanged) + one user
/// message `prompt::summary_instruction(SummaryKind::Branch, instructions)`.
/// A pure append on the old branch; sent `background` with `TurnKind::BranchSummary`.
pub fn branch_summary_request(old_wire: &[WireMsg], instructions: Option<&str>) -> Vec<WireMsg> {
    let mut out = Vec::with_capacity(old_wire.len() + 1);
    out.extend_from_slice(old_wire);
    out.push(WireMsg::user(prompt::summary_instruction(
        SummaryKind::Branch,
        instructions,
    )));
    out
}

/// The text an entry contributes to a budget: what it puts on the wire, or for the summary
/// entries their stored summary (file lists included). Bodies without text cost nothing.
fn entry_text(e: &Entry) -> String {
    match &e.body {
        EntryBody::Msg { wire, .. } => {
            let mut s = wire.content.clone().unwrap_or_default();
            for t in &wire.tool_calls {
                s.push('\n');
                s.push_str(&t.name);
                s.push(' ');
                s.push_str(&t.args_raw);
            }
            s
        }
        EntryBody::Shown { text, .. } => text.clone(),
        EntryBody::BashExecution {
            command,
            output,
            exit,
            ..
        } => bash_text(command, output, *exit),
        EntryBody::CustomMessage { content, .. } => content.clone(),
        EntryBody::Compaction { summary, .. } | EntryBody::BranchSummary { summary, .. } => summary.clone(),
        _ => String::new(),
    }
}
