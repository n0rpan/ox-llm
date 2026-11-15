//! B1: the summary request and the rebased head.
// adapted from pi packages/coding-agent/src/core/compaction/compaction.ts (findCutPoint, prepareCompaction)
use crate::prompt::{self, SummaryKind};
use crate::{CompactPolicy, When};
use oxa_proto::{Role, WireMsg};
use oxa_session::Transcript;

/// The messages of the summary request: `t.wire()` unchanged + one user message holding
/// `prompt::summary_instruction(SummaryKind::.., instructions)` (the update kind when the wire already starts with a
/// summary; `instructions` = `/compact <instructions>`). The result has
/// `t.wire()` as a prefix (acceptance: "summary request is a pure append").
pub fn summary_request(t: &Transcript, instructions: Option<&str>) -> Vec<WireMsg> {
    summary_request_with_target(t, instructions, 0)
}

/// [`summary_request`] whose instruction carries the soft length target of `target_words`
/// words (`prompt::summary_instruction_with_target`; 0 = none, the same bytes as
/// [`summary_request`]).
pub fn summary_request_with_target(
    t: &Transcript,
    instructions: Option<&str>,
    target_words: u32,
) -> Vec<WireMsg> {
    let wire = t.wire();
    let kind = if wire.first().is_some_and(is_summary_head) {
        SummaryKind::Update
    } else {
        SummaryKind::Initial
    };
    let mut req = Vec::with_capacity(wire.len() + 1);
    req.extend_from_slice(wire);
    req.push(WireMsg::user(prompt::summary_instruction_with_target(
        kind,
        instructions,
        target_words,
    )));
    req
}

/// Index into `t.wire()` of the first message kept verbatim: walk back from the end
/// accumulating `count(message text)` until `keep_tokens` is reached, then move the cut
/// forward to pi's valid cut points (never between an assistant message and its tool results;
/// a cut lands on a user message or on an assistant message). `MidRun`: the last user message
/// (the current prompt) and everything after it is always kept. Returns `wire.len()` when
/// nothing fits.
pub fn first_kept_index(t: &Transcript, w: When, p: &CompactPolicy, count: &dyn Fn(&str) -> u32) -> usize {
    let wire = t.wire();
    let n = wire.len();
    // The previous compaction summary (repeated compaction) is summarised, never kept.
    let lower = usize::from(wire.first().is_some_and(is_summary_head));
    if n <= lower {
        return n;
    }

    let mut cut: Option<usize> = None;
    let mut acc: u64 = 0;
    let keep = u64::from(p.keep_tokens);
    let mut budget_reached = false;
    for i in (lower..n).rev() {
        acc += u64::from(count(&message_text(&wire[i])));
        if acc >= keep {
            budget_reached = true;
            // Prefer the closest valid cut point at or after this message; if trailing tool
            // results exceed the budget by themselves, fall back to the last valid point
            // (its tool call keeps its results with it).
            cut = (i..n)
                .find(|&j| is_valid_cut(&wire[j]))
                .or_else(|| (lower..n).rev().find(|&j| is_valid_cut(&wire[j])));
            break;
        }
    }
    if !budget_reached {
        // The whole wire fits the budget: keep it from the first valid cut point.
        cut = (lower..n).find(|&j| is_valid_cut(&wire[j]));
    }

    let Some(mut cut) = cut else {
        return n;
    };
    if w == When::MidRun {
        if let Some(last_user) = (lower..n).rev().find(|&j| wire[j].role == Role::User) {
            cut = cut.min(last_user);
        }
    }
    cut
}

/// The new wire after compaction: `[user:prompt::summary_user_message(summary)]` + the kept
/// tail from [`first_kept_index`]. Repeated compaction: the previous summary message is part
/// of what was summarised and is not kept. `w == When::No` panics (a bug).
pub fn head_after(
    t: &Transcript,
    w: When,
    summary: &str,
    p: &CompactPolicy,
    count: &dyn Fn(&str) -> u32,
) -> Vec<WireMsg> {
    assert!(w != When::No, "head_after called without a compaction decision");
    let wire = t.wire();
    let cut = first_kept_index(t, w, p, count);
    let mut head = Vec::with_capacity(1 + wire.len() - cut);
    head.push(WireMsg::user(prompt::summary_user_message(summary)));
    let mut tail = wire[cut..].to_vec();
    for (k, text) in tail_elisions(&tail, p.keep_tokens, count) {
        tail[k].content = Some(text);
    }
    head.extend(tail);
    head
}

/// A1: a kept tool result of at most this many tokens is never elided.
pub const ELIDE_MIN_TOKENS: u32 = 1000;

/// A1: the tool results of a kept tail that are too big to keep verbatim: one result above
/// half of `keep_tokens` would fill the room the tail is meant to have (and keep the next
/// prompt near the trigger, so the next task compacts again). Each is `(index in tail,
/// prompt::elided_tool_result(its bytes))`; the message itself stays (its call keeps a result,
/// pi's pairing) and the summary request, built before the cut, saw the whole result. Results
/// up to [`ELIDE_MIN_TOKENS`] are always kept (a tiny `keepRecentTokens` elides nothing small).
/// #459: an older `read` result that a later call of the tail supersedes (see
/// [`superseded_read`]) is replaced by `prompt::superseded_read(path)` whatever its size.
/// A pure function of the tail and the policy (W4).
pub fn tail_elisions(
    tail: &[WireMsg],
    keep_tokens: u32,
    count: &dyn Fn(&str) -> u32,
) -> Vec<(usize, String)> {
    let limit = (keep_tokens / 2).max(ELIDE_MIN_TOKENS);
    tail.iter()
        .enumerate()
        .filter(|(_, m)| m.role == Role::Tool)
        .filter_map(|(k, m)| {
            let c = m.content.as_deref()?;
            if let Some(path) = superseded_read(tail, k) {
                return Some((k, prompt::superseded_read(&path)));
            }
            (count(c) > limit).then(|| (k, prompt::elided_tool_result(c.len())))
        })
        .collect()
}

/// The call (name, parsed args) that the tool result at `k` answers: the nearest earlier
/// assistant call with its id.
fn call_of(tail: &[WireMsg], k: usize) -> Option<(&str, serde_json::Value)> {
    let id = tail[k].tool_call_id.as_deref()?;
    let call = tail[..k]
        .iter()
        .rev()
        .filter(|m| m.role == Role::Assistant)
        .find_map(|m| m.tool_calls.iter().find(|c| c.id == id))?;
    let args = serde_json::from_str(&call.args_raw).ok()?;
    Some((call.name.as_str(), args))
}

/// #459: `Some(path)` when the tool result at `k` is a `read` of `path` that a later call of
/// the tail supersedes: a `read` of the same page (same `offset` and `limit`) or an `edit` /
/// `write` of the same path.
fn superseded_read(tail: &[WireMsg], k: usize) -> Option<String> {
    let (name, args) = call_of(tail, k)?;
    if name != "read" {
        return None;
    }
    let path = args.get("path")?.as_str()?;
    let page = (args.get("offset"), args.get("limit"));
    let later = tail[k + 1..]
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .flat_map(|m| &m.tool_calls)
        .filter_map(|c| {
            Some((
                c.name.as_str(),
                serde_json::from_str::<serde_json::Value>(&c.args_raw).ok()?,
            ))
        });
    for (n, a) in later {
        if a.get("path").and_then(|p| p.as_str()) != Some(path) {
            continue;
        }
        let hit = match n {
            "edit" | "write" => true,
            "read" => (a.get("offset"), a.get("limit")) == page,
            _ => false,
        };
        if hit {
            return Some(path.to_string());
        }
    }
    None
}

/// `true` for the user message that carries a compaction summary at the head of a wire.
fn is_summary_head(m: &WireMsg) -> bool {
    m.role == Role::User
        && m.content.as_deref().is_some_and(|c| {
            c.starts_with(prompt::COMPACTION_SUMMARY_PREFIX) && c.ends_with(prompt::COMPACTION_SUMMARY_SUFFIX)
        })
}

/// pi's valid cut points: user and assistant messages. Tool results never start a kept tail.
fn is_valid_cut(m: &WireMsg) -> bool {
    matches!(m.role, Role::User | Role::Assistant)
}

/// The text whose tokens are counted for one message: content, reasoning and tool calls.
fn message_text(m: &WireMsg) -> String {
    let mut s = String::new();
    if let Some(c) = &m.content {
        s.push_str(c);
    }
    if let Some(r) = &m.reasoning {
        s.push('\n');
        s.push_str(r);
    }
    for call in &m.tool_calls {
        s.push('\n');
        s.push_str(&call.name);
        s.push('\n');
        s.push_str(&call.args_raw);
    }
    s
}
