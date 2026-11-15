//! C4: tree navigation, fork and clone. Owner: task C4. Signatures below are frozen; bodies are yours.
// ported from pi packages/coding-agent/src/core/agent-session.ts (navigateTree, fork)
use crate::agent_loop::persist;
use crate::compaction::{idle_prefill, Counted};
use crate::exec::{request, RequestOpts};
use crate::SessionState;
use oxa_compact::prompt::{branch_summary_user_message, BRANCH_SUMMARY_PREAMBLE};
use oxa_compact::{branch_entries, branch_summary_request, file_lists};
use oxa_proto::{
    codes, rebase_why, AgentEvent, Entry, EntryBody, EventSink, HookPoint, HookReply, Level, OxaError, Part,
    Role, TurnKind, Ui, Value, WireMsg,
};
use oxa_session::{fork_session, SessionRef, Transcript, Tree};
use serde_json::json;

/// Move the leaf to `target`. Sequence: hook `SessionBeforeTree` (`Cancel`; `Summary` = use
/// it) -> when `summarize`: branch-summary request on the OLD branch's wire
/// (`oxa_compact::branch_summary_request`, foreground, `TurnKind::BranchSummary`) ->
/// `Tree::set_leaf` (a user-message target moves to its parent and returns its text for the
/// editor) -> append the `BranchSummary` entry at the new leaf when there is one -> exactly
/// one `Transcript::rebase(project(new branch).wire, leaf_move)` -> append
/// `ModelChange` / `ThinkingLevelChange` entries when the new branch's recorded values differ
/// from the current ones (the current ones stay in force) -> hook `SessionTree` -> the cost
/// notice (tokens + estimated seconds) -> park prefill (`idle_prefill`, background, cancelled by
/// user input; the front end then calls `Session::run_queued`).
///
/// Returns the editor text for a user-message (or custom-message) target, `None` otherwise.
/// Navigating to the current leaf is a no-op (pi's rule). A hook `Cancel` returns
/// `OxaError` with code `aborted` and changes nothing.
pub fn navigate(
    st: &mut SessionState,
    target: &str,
    summarize: bool,
    instructions: Option<&str>,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) -> Result<Option<String>, OxaError> {
    let old_leaf = st.tree.leaf().to_string();
    let old_wire = st.transcript.wire().to_vec();
    if target == old_leaf {
        return Ok(None);
    }
    let target_entry = st.tree.get(target).cloned().ok_or_else(|| not_found(target))?;
    let (new_leaf, editor) = match &target_entry.body {
        EntryBody::Msg { wire, parts, .. } if wire.role == Role::User => {
            (target_entry.parent.clone(), Some(user_text(wire, parts)))
        }
        EntryBody::CustomMessage { content, .. } => (target_entry.parent.clone(), Some(content.clone())),
        _ => (Some(target.to_string()), None),
    };
    let new_leaf = new_leaf
        .ok_or_else(|| OxaError::new(codes::INVALID, "cannot move the leaf above the first entry"))?;

    // The entries this navigation abandons (old leaf back to the common ancestor).
    let abandoned: Vec<Entry> = branch_entries(&st.tree, &old_leaf, target)
        .into_iter()
        .cloned()
        .collect();

    // Hook `SessionBeforeTree`: the first reply other than `Continue` ends the chain.
    let mut hook_summary: Option<(String, Value)> = None;
    for h in st.hooks.clone() {
        // pi's `SessionBeforeTreeEvent.preparation` (extensions/types.ts `TreePreparation`);
        // the flat keys stay for existing handlers.
        let payload = json!({
            "targetId": target,
            "oldLeafId": old_leaf,
            "userWantsSummary": summarize,
            "customInstructions": instructions,
            "entriesToSummarize": abandoned.len(),
            "preparation": {
                "targetId": target,
                "oldLeafId": old_leaf,
                "commonAncestorId": common_ancestor(&st.tree, &old_leaf, target),
                "entriesToSummarize": abandoned.iter().map(entry_json).collect::<Vec<_>>(),
                "userWantsSummary": summarize,
                "customInstructions": instructions,
            },
        });
        match crate::host::fire_hook(st, &h, HookPoint::SessionBeforeTree, &payload, sink, ui) {
            HookReply::Continue => {}
            HookReply::Cancel(reason) => return Err(OxaError::new(codes::ABORTED, reason)),
            HookReply::Summary { text, details } => {
                if summarize {
                    hook_summary = Some((text, details));
                }
                break;
            }
            _ => break,
        }
    }

    // The branch summary to store at the new leaf: `(wire text, details, from_hook)`.
    let mut stored: Option<(String, Value, bool)> = None;
    if let Some((text, details)) = hook_summary {
        if !text.trim().is_empty() {
            stored = Some((branch_summary_user_message(&text), details, true));
        }
    } else if summarize && !abandoned.is_empty() {
        let text = generate_summary(st, instructions, sink, ui)?;
        if !text.trim().is_empty() {
            let refs: Vec<&Entry> = abandoned.iter().collect();
            let (read, modified) = file_lists(&refs);
            let details = json!({"readFiles": read, "modifiedFiles": modified});
            let wire_text = branch_summary_user_message(&format!("{BRANCH_SUMMARY_PREAMBLE}{text}"));
            stored = Some((wire_text, details, false));
        }
    }

    // Switch the leaf. A failed append puts the old leaf back; nothing else has changed yet.
    if !st.tree.set_leaf(&new_leaf) {
        return Err(not_found(&new_leaf));
    }
    let mut summary_id: Option<String> = None;
    let mut summary_entry: Option<Value> = None;
    let mut summary_from_hook = false;
    if let Some((summary, details, from_hook)) = stored {
        summary_from_hook = from_hook;
        let body = EntryBody::BranchSummary {
            from: old_leaf.clone(),
            summary,
            details,
            from_hook,
        };
        match persist(st, body, sink) {
            Ok(e) => {
                summary_entry = Some(entry_json(&e));
                summary_id = Some(e.id);
            }
            Err(e) => {
                st.tree.set_leaf(&old_leaf);
                return Err(e);
            }
        }
    }
    // The one rebase of this navigation (after the summary entry is in the tree).
    rebase_to_leaf(st);

    // Model and thinking level recorded on the new branch: when they differ from the current
    // values, record the current ones there. The current values stay in force.
    let branch: Vec<Entry> = branch_of_leaf(st);
    let recorded_model = branch.iter().rev().find_map(|e| match &e.body {
        EntryBody::ModelChange(m) => Some(m.clone()),
        _ => None,
    });
    let recorded_thinking = branch.iter().rev().find_map(|e| match &e.body {
        EntryBody::ThinkingLevelChange(t) => Some(*t),
        _ => None,
    });
    if recorded_model.is_some_and(|m| m != st.model) {
        persist(st, EntryBody::ModelChange(st.model.clone()), sink)?;
    }
    if recorded_thinking.is_some_and(|t| t != st.thinking) {
        persist(st, EntryBody::ThinkingLevelChange(st.thinking), sink)?;
    }

    // pi's `SessionTreeEvent`: `summaryEntry` (the stored entry) and `fromExtension`.
    let payload = json!({
        "newLeafId": st.tree.leaf(),
        "oldLeafId": old_leaf,
        "summaryEntryId": summary_id,
        "summaryEntry": summary_entry,
        "fromExtension": summary_from_hook,
    });
    crate::host::observe_hooks(st, HookPoint::SessionTree, &payload, sink, ui);

    leaf_move_notice(st, &old_wire, sink);
    idle_prefill(st, sink, ui);
    Ok(editor)
}

/// S3 / F11: before the idle prefill starts, tell the user what the move costs: the tokens the
/// engine must prefill for the new branch and their estimated seconds. The engine restores
/// from the deepest checkpoint at or below the fork point: the last checkpoint it reported
/// kept (`Stats::last_snap_pos`) when that lies inside the part both branches share, else at
/// least the frozen prefix (its anchor).
fn leaf_move_notice(st: &SessionState, old_wire: &[WireMsg], sink: &mut dyn EventSink) {
    let new_wire = st.transcript.wire();
    let common = old_wire.iter().zip(new_wire).take_while(|(a, b)| a == b).count();
    let prefix = crate::compaction::prefix_tokens(st);
    // OXA-EXACT-COUNTS: the engine's own counts of the shared part and of the whole new wire
    // when it can count (two requests, only on a leaf move), else the estimate.
    let (shared, total) = match (
        crate::compaction::prompt_tokens(st, &new_wire[..common]),
        crate::compaction::prompt_tokens(st, new_wire),
    ) {
        ((s, Counted::Exact), (t, Counted::Exact)) => (s, t.max(s)),
        _ => {
            let shared = prefix.saturating_add(crate::compaction::messages_tokens(st, &new_wire[..common]));
            (
                shared,
                shared.saturating_add(crate::compaction::messages_tokens(st, &new_wire[common..])),
            )
        }
    };
    let restored = match st.stats.last_snap_pos {
        Some(pos) if pos <= shared => pos.max(prefix),
        _ => prefix,
    };
    let tokens = total.saturating_sub(restored);
    let when = if st.info.prefill_only {
        "now, in idle time"
    } else {
        "with the next request"
    };
    sink.emit(&AgentEvent::Notice {
        level: Level::Info,
        text: format!(
            "leaf moved: the engine prefills about {tokens} tokens (~{} s) {when}",
            crate::prefill_estimate_secs(tokens)
        ),
    });
}

/// Where a fork puts the new leaf (pi `AgentSessionRuntime.fork` `position`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkPosition {
    /// pi's default: `entry` must be a user message; the new session ends at its parent and the
    /// message's text is handed back (the TUI editor text, the RPC `text`).
    Before,
    /// The new session ends at `entry` itself (any entry; what `/clone` does at the leaf).
    At,
}

/// What a fork produced: the new session file and, for [`ForkPosition::Before`], the text of
/// the chosen user message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Forked {
    pub sref: SessionRef,
    pub text: Option<String>,
}

/// `/fork` with pi's default position (`before`): see [`fork_from`]. Returns the new file.
pub fn fork_at(st: &mut SessionState, entry: &str, ui: &mut dyn Ui) -> Result<SessionRef, OxaError> {
    fork_from(st, entry, ForkPosition::Before, ui).map(|f| f.sref)
}

/// `/fork` (pi `AgentSessionRuntime.fork`): hook `SessionBeforeFork` (`{entryId, position}`),
/// `oxa_session::fork_session` of the branch ending at the new leaf, switch the state to the new
/// file.
///
/// * `Before` (pi's default): `entry` must be a user message (else `invalid`, "Invalid entry ID
///   for forking", as pi throws); the new leaf is its parent and its text comes back in
///   [`Forked::text`]. oxa sessions start with the prefix-epoch entry, so a user message always
///   has a parent; one without (a hand-written file) is `invalid` (use `/new`).
/// * `At`: the new leaf is `entry` itself.
///
/// A hook `Cancel` returns `aborted`. The wire of the new file is the old wire cut at the new
/// leaf: when it differs from the current one the switch is one `leaf_move` rebase (W1; the
/// engine restores from its nearest checkpoint), a fork at the current leaf keeps the
/// transcript as it is.
pub fn fork_from(
    st: &mut SessionState,
    entry: &str,
    position: ForkPosition,
    ui: &mut dyn Ui,
) -> Result<Forked, OxaError> {
    let chosen = st.tree.get(entry).cloned().ok_or_else(|| not_found(entry))?;
    let (leaf, text) = match position {
        ForkPosition::At => (entry.to_string(), None),
        ForkPosition::Before => {
            let text = match &chosen.body {
                EntryBody::Msg { wire, parts, .. } if wire.role == Role::User => user_text(wire, parts),
                _ => {
                    return Err(OxaError::new(codes::INVALID, "Invalid entry ID for forking")
                        .with_hint("fork from a user message"))
                }
            };
            let parent = chosen.parent.clone().ok_or_else(|| {
                OxaError::new(
                    codes::INVALID,
                    "cannot fork before the first entry of the session",
                )
                .with_hint("use /new")
            })?;
            (parent, Some(text))
        }
    };
    // `fork_from` has no event sink: what serving host requests emits meanwhile is dropped.
    let mut quiet = oxa_proto::VecSink::default();
    let pos = match position {
        ForkPosition::Before => "before",
        ForkPosition::At => "at",
    };
    for h in st.hooks.clone() {
        let payload = json!({"entryId": entry, "position": pos});
        match crate::host::fire_hook(st, &h, HookPoint::SessionBeforeFork, &payload, &mut quiet, ui) {
            HookReply::Continue => {}
            HookReply::Cancel(reason) => return Err(OxaError::new(codes::ABORTED, reason)),
            _ => break,
        }
    }
    let dst = fork_session(st.store.as_ref(), &st.sref, &leaf, &st.cwd)?;
    crate::fire::fork_switch(st, &dst, ui, |st| switch_file(st, &dst, &leaf))?;
    Ok(Forked { sref: dst, text })
}

/// The text of a user message as pi's `extractUserMessageText` gives it (the wire content).
fn user_text(wire: &WireMsg, parts: &[Part]) -> String {
    // The editor gets back what was typed, not the environment note oxa appended (only the
    // note recorded with the entry is dropped).
    crate::envnote::strip_note(wire.content.as_deref().unwrap_or_default(), parts).to_string()
}

/// `/clone`: the active branch copied into a new session (pi: a fork at the leaf, position `at`;
/// the `SessionBeforeFork` hook may cancel it), switch to it.
///
/// The new file holds the same branch with the same entry ids, so the wire is unchanged and
/// no rebase is logged.
pub fn clone_current(st: &mut SessionState, ui: &mut dyn Ui) -> Result<SessionRef, OxaError> {
    let leaf = st.tree.leaf().to_string();
    if leaf.is_empty() {
        return Err(OxaError::new(
            codes::INVALID,
            "nothing to clone: the session has no entries",
        ));
    }
    // pi: `clone` is `fork(leaf, {position: "at"})`, so `session_before_fork` fires.
    fork_from(st, &leaf, ForkPosition::At, ui).map(|f| f.sref)
}

/// The summary request on the current (old) wire. The wire is not changed. `thinking` off,
/// `TurnKind::BranchSummary`, the summary budget of the compaction policy. Foreground (F12):
/// the user asked for it and waits; only an abort cancels it.
fn generate_summary(
    st: &mut SessionState,
    instructions: Option<&str>,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) -> Result<String, OxaError> {
    let old_wire = st.transcript.wire().to_vec();
    let full = branch_summary_request(&old_wire, instructions);
    let extra = full[old_wire.len()..].to_vec();
    let opts = RequestOpts {
        kind: TurnKind::BranchSummary,
        max_tokens: Some(branch_summary_max(st)),
        thinking: Some(false),
        background: false,
        snapshot_keep: false,
        extra,
        events: false,
        idem_key: String::new(),
    };
    let turn = request(st, &opts, sink, ui)?;
    let usage = turn.usage;
    persist(
        st,
        EntryBody::Usage {
            kind: TurnKind::BranchSummary.as_str().to_string(),
            usage,
        },
        sink,
    )?;
    Ok(turn.msg.content.unwrap_or_default())
}

/// The branch summary's output budget: `branchSummary.reserveTokens` (pi's tokens reserved
/// when summarising branch history, default 16384) in the compaction policy's form,
/// `min(max_output, reserve * 4 / 5)`. The abandoned branch's wire is sent whole (W1: a pure
/// append), so the reserve bounds the summary instead of cutting the history.
pub(crate) fn branch_summary_max(st: &SessionState) -> u32 {
    let reserve = st.settings.u32("branchSummary.reserveTokens", 16384);
    let max_output = if st.info.max_output > 0 {
        st.info.max_output
    } else {
        st.compact.summary_max
    };
    max_output.min((u64::from(reserve) * 4 / 5) as u32).max(1)
}

/// Point the transcript at the active branch: one `leaf_move` rebase.
fn rebase_to_leaf(st: &mut SessionState) {
    let branch = branch_of_leaf(st);
    let head = Transcript::project(&branch).wire().to_vec();
    st.transcript.rebase(head, rebase_why::LEAF_MOVE);
}

/// The entries from the root to the active leaf, root first, owned.
fn branch_of_leaf(st: &SessionState) -> Vec<Entry> {
    st.tree.branch(st.tree.leaf()).into_iter().cloned().collect()
}

/// Switch the state to session file `dst`, whose active leaf is `leaf`. The transcript is
/// rebuilt from that branch; a changed wire is one `leaf_move` rebase, an identical one keeps
/// the transcript (and its cache expectation) as it is.
fn switch_file(st: &mut SessionState, dst: &SessionRef, leaf: &str) -> Result<(), OxaError> {
    st.store.acquire(dst)?;
    let loaded = (|| {
        let entries = st.store.load(dst)?;
        let mut tree = Tree::new(entries);
        if !tree.set_leaf(leaf) {
            return Err(not_found(leaf));
        }
        let branch: Vec<Entry> = tree.branch(leaf).into_iter().cloned().collect();
        Ok((tree, Transcript::project(&branch).wire().to_vec()))
    })();
    let (tree, head) = match loaded {
        Ok(value) => value,
        Err(error) => {
            st.store.abort_lock();
            return Err(error);
        }
    };
    // Adopt the destination and its writer lock together, releasing the old session.
    st.store.commit_lock();
    if head.as_slice() != st.transcript.wire() {
        st.transcript.rebase(head, rebase_why::LEAF_MOVE);
    }
    st.tree = tree;
    st.sref = dst.clone();
    Ok(())
}

/// An entry as pi's session file stores it (the hook events carry entries in that form).
fn entry_json(e: &Entry) -> Value {
    let mut bytes = Vec::new();
    oxa_session::encode_entry(e, &mut bytes);
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

/// The deepest entry on both `a`'s and `b`'s path to the root (pi `commonAncestorId`).
fn common_ancestor(tree: &Tree, a: &str, b: &str) -> Option<String> {
    let mut seen = std::collections::HashSet::new();
    let mut cur = Some(a.to_string());
    while let Some(id) = cur {
        if !seen.insert(id.clone()) {
            break;
        }
        cur = tree.get(&id).and_then(|e| e.parent.clone());
    }
    let mut cur = Some(b.to_string());
    let mut steps = 0;
    while let Some(id) = cur {
        if seen.contains(&id) {
            return Some(id);
        }
        steps += 1;
        if steps > seen.len() + tree.entries().len() {
            break;
        }
        cur = tree.get(&id).and_then(|e| e.parent.clone());
    }
    None
}

fn not_found(id: &str) -> OxaError {
    OxaError::new(codes::NOT_FOUND, format!("entry {id} is not in this session"))
}

#[cfg(test)]
mod hook_payload_tests {
    use super::*;

    fn user(id: &str, parent: Option<&str>) -> Entry {
        Entry {
            id: id.to_string(),
            parent: parent.map(str::to_string),
            ts_ms: 1,
            body: EntryBody::Msg {
                wire: WireMsg::user(id),
                parts: Vec::new(),
                tool: None,
            },
        }
    }

    /// pi `TreePreparation.commonAncestorId`: the deepest entry both paths share.
    #[test]
    fn common_ancestor_of_two_branches() {
        // r -> a -> { b -> c , d }
        let tree = Tree::new(vec![
            user("r", None),
            user("a", Some("r")),
            user("b", Some("a")),
            user("c", Some("b")),
            user("d", Some("a")),
        ]);
        assert_eq!(common_ancestor(&tree, "c", "d").as_deref(), Some("a"));
        assert_eq!(common_ancestor(&tree, "c", "b").as_deref(), Some("b"));
        assert_eq!(common_ancestor(&tree, "c", "r").as_deref(), Some("r"));
        assert_eq!(common_ancestor(&tree, "c", "zz"), None);
    }

    /// Entries reach the hooks in the session file's form.
    #[test]
    fn entry_json_is_the_file_form() {
        let v = entry_json(&user("u1", Some("p")));
        assert_eq!(v["type"], "message");
        assert_eq!(v["id"], "u1");
        assert_eq!(v["parentId"], "p");
        assert_eq!(v["message"]["role"], "user");
    }
}
