//! T5 (OXA-LOCAL-FIRST): `enable_thinking` by turn kind, `oxa.thinking.policy`.
//!
//! * `fixed`: every request carries the session level (`off` = false, any other level = true).
//! * `auto` (the default): thinking on for the first reply after a user
//!   prompt; off for tool follow-ups, turns after an appended non-user instruction (an extension
//!   message, a branch summary, a repair message), compaction and branch summaries, prefills and
//!   every other background request. The level `off` always wins: the policy never turns
//!   thinking on. An explicit `RequestOpts::thinking` still wins over both (summaries send
//!   `Some(false)`). W6 holds: every request carries the explicit boolean.
//!
//! Cache safety (W1/W2) was checked against the engine's chat templates: on Ornith and
//! Qwen3.6 `enable_thinking` changes only the generation opener after the history
//! (`<think>\n` vs `<think>\n\n</think>\n\n`), and the thinking-off opener is byte-equal to how
//! an assistant turn without reasoning renders in history, so a toggle changes no byte already
//! sent. The Qwen3.8-Flash-Next template (`qwen38-flash-next.jinja`) also puts its
//! reasoning-effort sentence at the START of the system message when thinking is on, so a
//! toggle there changes the first bytes of the prompt and re-prefills everything. `auto`
//! therefore acts as `fixed` on an engine that does not keep reasoning in history
//! (`EngineCaps::preserve_thinking == Some(false)`, the Flash engine), until the engine
//! advertises a toggle-safe template itself.
//!
use crate::SessionState;
use oxa_config::ThinkingPolicy;
use oxa_proto::{EntryBody, Role, TurnKind};
use oxa_session::Tree;

/// What a request answers, for the thinking policy. New kinds (a repair turn, ...) are added
/// here; [`ReplyKind::thinks`] says which ones think under `auto`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyKind {
    /// The first reply after a user prompt (also a steering or follow-up message the user typed).
    Prompt,
    /// A reply after tool results.
    ToolFollowUp,
    /// A reply after an appended non-user instruction (extension message, branch summary,
    /// `!cmd` output, a repair message).
    Instruction,
    /// A compaction or branch summary.
    Summary,
    /// A prefill or any other request in the background.
    Background,
}

impl ReplyKind {
    /// Whether this kind thinks under `auto` (before the level `off` is applied).
    pub fn thinks(self) -> bool {
        self == ReplyKind::Prompt
    }
}

/// The policy in force: the setting, else [`ThinkingPolicy::Auto`]; `auto` acts as `fixed`
/// where a toggle is not cache-safe (see the module docs).
pub fn policy(st: &SessionState) -> ThinkingPolicy {
    match st.settings.thinking_policy() {
        ThinkingPolicy::Auto if !toggle_safe(st) => ThinkingPolicy::Fixed,
        p => p,
    }
}

/// Whether switching `enable_thinking` between requests keeps the sent bytes (module docs).
pub fn toggle_safe(st: &SessionState) -> bool {
    st.info.caps.preserve_thinking != Some(false)
}

/// The kind of the next chat reply, from the active branch: the last entry that put something
/// on the wire decides (a user message = `Prompt`, a tool result = `ToolFollowUp`, any other
/// appended message = `Instruction`). Entries that never reach the wire and rebases
/// (compaction, a new epoch) are skipped, so a retry after an overflow compaction still answers
/// the prompt.
pub fn classify(st: &SessionState) -> ReplyKind {
    classify_tree(&st.tree)
}

/// [`classify`] over a session tree (its active leaf).
pub fn classify_tree(tree: &Tree) -> ReplyKind {
    let mut id = tree.leaf().to_string();
    while let Some(e) = tree.get(&id) {
        match &e.body {
            EntryBody::Msg { wire, .. } => {
                return match wire.role {
                    Role::User => ReplyKind::Prompt,
                    Role::Tool => ReplyKind::ToolFollowUp,
                    _ => ReplyKind::Instruction,
                }
            }
            EntryBody::CustomMessage { .. } | EntryBody::BranchSummary { .. } => {
                return ReplyKind::Instruction
            }
            EntryBody::BashExecution { excluded: false, .. } => return ReplyKind::Instruction,
            _ => {}
        }
        match &e.parent {
            Some(p) => id = p.clone(),
            None => break,
        }
    }
    ReplyKind::Prompt
}

/// `enable_thinking` for a reply of `kind` under the policy in force.
pub fn thinking_for(st: &SessionState, kind: ReplyKind) -> bool {
    let level = st.thinking.enabled();
    match policy(st) {
        ThinkingPolicy::Fixed => level,
        ThinkingPolicy::Auto => level && kind.thinks(),
    }
}

/// The value `exec::request` sends when the caller did not set one: chat (and subagent) turns
/// by [`classify`], summaries and prefills as background work.
pub(crate) fn request_thinking(st: &SessionState, kind: TurnKind) -> bool {
    let reply = match kind {
        TurnKind::Chat | TurnKind::Subagent => classify(st),
        TurnKind::Compaction | TurnKind::BranchSummary => ReplyKind::Summary,
        TurnKind::Prefill => ReplyKind::Background,
    };
    thinking_for(st, reply)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxa_proto::{Entry, FrozenPrefix, Usage, Value, WireMsg};

    fn tree(bodies: Vec<EntryBody>) -> Tree {
        let mut t = Tree::new(Vec::new());
        for (i, body) in bodies.into_iter().enumerate() {
            t.push(Entry {
                id: format!("{i:08x}"),
                parent: (i > 0).then(|| format!("{:08x}", i - 1)),
                ts_ms: 0,
                body,
            });
        }
        t
    }
    fn msg(w: WireMsg) -> EntryBody {
        EntryBody::Msg {
            wire: w,
            parts: Vec::new(),
        }
    }
    fn prefix() -> EntryBody {
        EntryBody::Prefix(FrozenPrefix {
            sections: Vec::new(),
            tools_json: String::from("[]"),
            sha256: [0; 32],
        })
    }
    fn usage() -> EntryBody {
        EntryBody::Usage {
            kind: "chat".to_string(),
            usage: Usage::default(),
        }
    }

    #[test]
    fn the_last_wire_entry_decides() {
        assert_eq!(classify_tree(&tree(vec![])), ReplyKind::Prompt);
        assert_eq!(
            classify_tree(&tree(vec![prefix(), msg(WireMsg::user("hi"))])),
            ReplyKind::Prompt
        );
        let after_tools = vec![
            prefix(),
            msg(WireMsg::user("hi")),
            msg(WireMsg::assistant("")),
            usage(),
            msg(WireMsg::tool_result("c1", "ok")),
        ];
        assert_eq!(classify_tree(&tree(after_tools.clone())), ReplyKind::ToolFollowUp);
        // An extension message appended after the results is an instruction, not a prompt.
        let mut instr = after_tools.clone();
        instr.push(EntryBody::CustomMessage {
            custom_type: "x".to_string(),
            content: "do more".to_string(),
            display: true,
            details: Value::Null,
        });
        assert_eq!(classify_tree(&tree(instr)), ReplyKind::Instruction);
        // A steering message the user typed is a prompt.
        let mut steer = after_tools.clone();
        steer.push(msg(WireMsg::user("also this")));
        assert_eq!(classify_tree(&tree(steer)), ReplyKind::Prompt);
        // Entries off the wire and rebases are skipped: a retry after an overflow compaction
        // still answers the prompt.
        let retry = vec![
            prefix(),
            msg(WireMsg::user("hi")),
            EntryBody::Compaction {
                summary: "s".to_string(),
                first_kept: "00000001".to_string(),
                tokens_before: 1,
                details: Value::Null,
                from_hook: false,
            },
            EntryBody::Custom {
                custom_type: "y".to_string(),
                data: Value::Null,
            },
        ];
        assert_eq!(classify_tree(&tree(retry)), ReplyKind::Prompt);
    }

    #[test]
    fn only_the_prompt_kind_thinks() {
        let all = [
            ReplyKind::Prompt,
            ReplyKind::ToolFollowUp,
            ReplyKind::Instruction,
            ReplyKind::Summary,
            ReplyKind::Background,
        ];
        let thinks: Vec<bool> = all.iter().map(|k| k.thinks()).collect();
        assert_eq!(thinks, vec![true, false, false, false, false]);
    }
}
