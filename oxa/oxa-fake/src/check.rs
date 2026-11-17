//! B15: the invariant checker.
use crate::Recorded;
use oxa_proto::{Role, Value, WireMsg};
use std::collections::HashMap;

/// One broken rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// `W1`, `W2`, `W3`, `W5`, `W6`, `H1`..`H6`.
    pub rule: &'static str,
    /// `Recorded::seq` of the offending request.
    pub seq: usize,
    pub detail: String,
}

/// Rebase marks: `(seq, why)` = "the request with this seq is the first after a logged rebase
/// with this cause" (from `Transcript::rebases` / `PrefixEpoch` events). Requests are checked
/// per session id; `prefill` and summary requests take part like any other.
pub type Rebases<'a> = &'a [(usize, &'static str)];

// ---------- shared helpers ----------

/// Requests sorted by `seq`, plus each request's predecessor in the same session (an index
/// into the sorted vector, `None` for the first request of a session).
fn sequence(reqs: &[Recorded]) -> (Vec<&Recorded>, Vec<Option<usize>>) {
    let mut v: Vec<&Recorded> = reqs.iter().collect();
    v.sort_by_key(|r| r.seq);
    let mut last: HashMap<&str, usize> = HashMap::new();
    let mut prev = Vec::with_capacity(v.len());
    for (i, r) in v.iter().copied().enumerate() {
        prev.push(last.get(r.session.as_str()).copied());
        last.insert(r.session.as_str(), i);
    }
    (v, prev)
}

/// Byte equality of two wire messages (canonical JSON, the only serialiser of a message).
fn same(a: &WireMsg, b: &WireMsg) -> bool {
    a.to_json() == b.to_json()
}

/// `None` when `base` is a byte prefix of `msgs`; otherwise the index of the first message
/// that is missing or differs.
fn first_mismatch(base: &[WireMsg], msgs: &[WireMsg]) -> Option<usize> {
    (0..base.len()).find(|&i| i >= msgs.len() || !same(&base[i], &msgs[i]))
}

fn rebase_why(rebases: Rebases, seq: usize) -> Option<&'static str> {
    rebases.iter().find(|(s, _)| *s == seq).map(|(_, why)| *why)
}

fn is_summary(kind: &str) -> bool {
    matches!(kind, "compaction" | "branch_summary")
}

/// The wire that the next request of the same session must extend. A summary request's
/// trailing instruction and its reply are not part of the wire; any other request contributes
/// its messages plus its reply when one was completed.
fn next_wire(r: &Recorded) -> Vec<WireMsg> {
    if is_summary(&r.kind) {
        let n = r.messages.len().saturating_sub(1);
        return r.messages[..n].to_vec();
    }
    let mut v = r.messages.clone();
    if let Some(reply) = &r.reply {
        v.push(reply.clone());
    }
    v
}

fn violation(rule: &'static str, seq: usize, detail: impl Into<String>) -> Violation {
    Violation {
        rule,
        seq,
        detail: detail.into(),
    }
}

// ---------- W1 ----------

/// W1: within a session, each request's messages have the previous request's messages + its
/// reply as a prefix (canonical bytes), except at a marked rebase. A summary request's extra
/// instruction is not part of the next request (it must be exactly one trailing user message
/// on a `compaction` / `branch_summary` kind request).
pub fn check_w1(reqs: &[Recorded], rebases: Rebases) -> Vec<Violation> {
    let (v, prev) = sequence(reqs);
    let mut out = Vec::new();
    for (i, r) in v.iter().copied().enumerate() {
        let Some(p) = prev[i] else { continue };
        if rebase_why(rebases, r.seq).is_some() {
            continue;
        }
        let before = v[p];
        let base = next_wire(before);
        if let Some(at) = first_mismatch(&base, &r.messages) {
            out.push(violation(
                "W1",
                r.seq,
                format!(
                    "wire of request {} is not a prefix of this request: message {at} changed or is missing",
                    before.seq
                ),
            ));
        }
    }
    out
}

// ---------- W2 ----------

/// W2: every assistant message on the wire equals, byte for byte, a reply this engine
/// generated earlier in the session (or predates the recording: sessions resumed in the test
/// declare their imported messages by starting with a marked rebase); a reply that failed or
/// was cut never appears.
pub fn check_w2(reqs: &[Recorded], rebases: Rebases) -> Vec<Violation> {
    let (v, prev) = sequence(reqs);
    let mut out = Vec::new();
    // A request that opens its session with a marked rebase declares its messages as imported.
    let imported_root = |i: usize| prev[i].is_none() && rebase_why(rebases, v[i].seq).is_some();
    for i in 0..v.len() {
        if imported_root(i) {
            continue;
        }
        for (idx, m) in v[i].messages.iter().enumerate() {
            if m.role != Role::Assistant {
                continue;
            }
            let json = m.to_json();
            let mut ok = false;
            let mut j = prev[i];
            while let Some(k) = j {
                let earlier = v[k];
                let echoed = earlier.reply.as_ref().is_some_and(|rep| rep.to_json() == json);
                let declared = imported_root(k)
                    && earlier
                        .messages
                        .iter()
                        .any(|x| x.role == Role::Assistant && x.to_json() == json);
                if echoed || declared {
                    ok = true;
                    break;
                }
                j = prev[k];
            }
            if !ok {
                out.push(violation(
                    "W2",
                    v[i].seq,
                    format!(
                        "assistant message {idx} is not a reply this engine generated earlier in the session"
                    ),
                ));
            }
        }
    }
    out
}

// ---------- W3 ----------

const MONTHS: [&str; 12] = [
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
];

/// `\d{4}-\d{2}-\d{2}` anywhere in `s`.
fn has_iso_date(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() < 10 {
        return false;
    }
    (0..=b.len() - 10).any(|i| {
        let w = &b[i..i + 10];
        w[4] == b'-'
            && w[7] == b'-'
            && w.iter()
                .enumerate()
                .all(|(k, c)| k == 4 || k == 7 || c.is_ascii_digit())
    })
}

fn is_year(t: &str) -> bool {
    t.len() == 4 && t.bytes().all(|c| c.is_ascii_digit())
}

/// The text right after a month name is ` 2026` or ` 12 2026` / ` 12, 2026`.
fn year_after(rest: &str) -> bool {
    if !(rest.starts_with(' ') || rest.starts_with(',')) {
        return false;
    }
    let mut toks = rest.split([' ', ',']).filter(|t| !t.is_empty());
    match toks.next() {
        Some(t) if is_year(t) => true,
        Some(t) if t.len() <= 2 && t.bytes().all(|c| c.is_ascii_digit()) => toks.next().is_some_and(is_year),
        _ => false,
    }
}

/// A month name (whole word, any case) followed by a year.
fn has_month_year(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    let b = lower.as_bytes();
    MONTHS.iter().any(|&m| {
        let mut start = 0;
        while let Some(off) = lower[start..].find(m) {
            let idx = start + off;
            let end = idx + m.len();
            let word_before = idx == 0 || !b[idx - 1].is_ascii_alphabetic();
            let word_after = end == b.len() || !b[end].is_ascii_alphabetic();
            if word_before && word_after && year_after(&lower[end..]) {
                return true;
            }
            start = idx + 1;
        }
        false
    })
}

fn is_git_state_line(l: &str) -> bool {
    let l = l.to_ascii_lowercase();
    [
        "git status",
        "git state",
        "git branch",
        "current branch",
        "uncommitted",
    ]
    .iter()
    .any(|k| l.contains(k))
}

fn is_mode_line(l: &str) -> bool {
    let l = l.trim().to_ascii_lowercase();
    matches!(l.as_str(), "plan" | "workspace" | "ask")
        || l.starts_with("mode:")
        || l.starts_with("permission mode")
        || l.starts_with("permissions:")
}

fn is_cwd_line(l: &str) -> bool {
    let l = l.trim().to_ascii_lowercase();
    l.starts_with("current working directory") || l.starts_with("cwd:")
}

/// The cwd line, when present, is the last non-empty line.
fn cwd_is_last(system: &str) -> bool {
    let lines: Vec<&str> = system.lines().filter(|l| !l.trim().is_empty()).collect();
    lines
        .iter()
        .enumerate()
        .all(|(i, l)| !is_cwd_line(l) || i + 1 == lines.len())
}

/// W3: `system` + `tools_json` are constant within a session except at a rebase marked
/// `epoch`; the system text contains no date (`\d{4}-\d{2}-\d{2}`, month names with a year),
/// no git state line and no permission-mode word line; the cwd section is last.
pub fn check_w3(reqs: &[Recorded], rebases: Rebases) -> Vec<Violation> {
    let (v, prev) = sequence(reqs);
    let mut out = Vec::new();
    for (i, r) in v.iter().copied().enumerate() {
        if has_iso_date(&r.system) {
            out.push(violation("W3", r.seq, "system text contains a date (YYYY-MM-DD)"));
        }
        if has_month_year(&r.system) {
            out.push(violation(
                "W3",
                r.seq,
                "system text contains a month name with a year",
            ));
        }
        if r.system.lines().any(is_git_state_line) {
            out.push(violation("W3", r.seq, "system text contains a git state line"));
        }
        if r.system.lines().any(is_mode_line) {
            out.push(violation(
                "W3",
                r.seq,
                "system text contains a permission-mode line",
            ));
        }
        if !cwd_is_last(&r.system) {
            out.push(violation(
                "W3",
                r.seq,
                "the cwd section is not the last line of the system text",
            ));
        }
        if let Some(p) = prev[i] {
            if rebase_why(rebases, r.seq) != Some("epoch") {
                let before = v[p];
                if before.system != r.system {
                    out.push(violation("W3", r.seq, "system text changed within an epoch"));
                }
                if before.tools_json != r.tools_json {
                    out.push(violation("W3", r.seq, "tool list changed within an epoch"));
                }
            }
        }
    }
    out
}

// ---------- W5 ----------

/// W5: a `compaction` kind request = the previous wire + exactly one user message, thinking
/// off, `background`; the first request after a `compaction` rebase has the shape
/// `[user:<summary>, tail..]`; the user's prompt of a mid-run compaction is present verbatim
/// as its own message.
pub fn check_w5(reqs: &[Recorded], rebases: Rebases) -> Vec<Violation> {
    let (v, prev) = sequence(reqs);
    let mut out = Vec::new();
    for (i, r) in v.iter().copied().enumerate() {
        if r.kind == "compaction" {
            if r.thinking {
                out.push(violation("W5", r.seq, "compaction request has thinking on"));
            }
            // F12: the priority is not part of W5. Only the threshold summary is background;
            // `/compact`, overflow recovery and the pre-switch compaction are foreground.
            if let Some(p) = prev[i] {
                let base = next_wire(v[p]);
                let ends_with_user = r.messages.last().is_some_and(|m| m.role == Role::User);
                let prefix_ok = r.messages.len() > base.len() && first_mismatch(&base, &r.messages).is_none();
                if !prefix_ok {
                    out.push(violation(
                        "W5",
                        r.seq,
                        "compaction request is not the previous wire plus an appended instruction",
                    ));
                } else if !ends_with_user {
                    out.push(violation(
                        "W5",
                        r.seq,
                        "compaction request does not end with one user instruction",
                    ));
                } else {
                    let extra = &r.messages[base.len()..r.messages.len() - 1];
                    if extra.iter().any(|m| m.role == Role::Assistant) {
                        out.push(violation(
                            "W5",
                            r.seq,
                            "non-append summary request: assistant message inserted",
                        ));
                    }
                }
            }
        }

        if rebase_why(rebases, r.seq) == Some("compaction") {
            if r.messages.first().map(|m| m.role) != Some(Role::User) {
                out.push(violation(
                    "W5",
                    r.seq,
                    "head after a compaction rebase is not a user message",
                ));
            }
            // The summary request this rebase follows.
            let mut j = prev[i];
            let summary = loop {
                match j {
                    None => break None,
                    Some(k) if v[k].kind == "compaction" => break Some(v[k]),
                    Some(k) => j = prev[k],
                }
            };
            if let Some(c) = summary {
                let base = &c.messages[..c.messages.len().saturating_sub(1)];
                // Mid-run: tool results follow the last user message of the run.
                if let Some(u) = base.iter().rposition(|m| m.role == Role::User) {
                    let mid_run = base[u + 1..].iter().any(|m| m.role == Role::Tool);
                    if mid_run {
                        let prompt = base[u].to_json();
                        let kept = r
                            .messages
                            .iter()
                            .skip(1)
                            .any(|m| m.role == Role::User && m.to_json() == prompt);
                        if !kept {
                            out.push(violation(
                                "W5",
                                r.seq,
                                "mid-run prompt is not present verbatim as its own message after the summary",
                            ));
                        }
                    }
                }
            }
        }
    }
    out
}

// ---------- W6 ----------

/// W6: every body has `temperature: 0`, `cache_prompt: true`,
/// `chat_template_kwargs.preserve_thinking: true` and a boolean `enable_thinking`. Needs
/// `Recorded::body` (HTTP records only; scripted records are skipped).
pub fn check_w6(reqs: &[Recorded]) -> Vec<Violation> {
    let mut out = Vec::new();
    for r in reqs {
        if r.body.is_empty() {
            continue;
        }
        let body: Value = match serde_json::from_slice(&r.body) {
            Ok(v) => v,
            Err(e) => {
                out.push(violation("W6", r.seq, format!("body is not JSON: {e}")));
                continue;
            }
        };
        let kwargs = body.get("chat_template_kwargs");
        if body.get("temperature").and_then(Value::as_f64) != Some(0.0) {
            out.push(violation("W6", r.seq, "temperature is not 0"));
        }
        if body.get("cache_prompt").and_then(Value::as_bool) != Some(true) {
            out.push(violation("W6", r.seq, "cache_prompt is not true"));
        }
        if kwargs
            .and_then(|k| k.get("preserve_thinking"))
            .and_then(Value::as_bool)
            != Some(true)
        {
            out.push(violation(
                "W6",
                r.seq,
                "chat_template_kwargs.preserve_thinking is not true",
            ));
        }
        if kwargs
            .and_then(|k| k.get("enable_thinking"))
            .and_then(Value::as_bool)
            .is_none()
        {
            out.push(violation(
                "W6",
                r.seq,
                "chat_template_kwargs.enable_thinking is not a boolean",
            ));
        }
    }
    out
}

// ---------- hints ----------

const KINDS: [&str; 5] = ["chat", "compaction", "branch_summary", "prefill", "subagent"];

/// Hints (plan sec. 3): H1 session header present and constant per session; H2 a key on every
/// request, equal only for byte-identical bodies; H3 `background` on compaction, branch
/// summary, prefill and subagent kinds and never on `chat`; H4 `snapshot: keep` on prefill
/// requests; H5 kind present; H6 `max_tokens: 0` exactly on `prefill` kind requests.
pub fn check_hints(reqs: &[Recorded]) -> Vec<Violation> {
    let (v, _) = sequence(reqs);
    let mut out = Vec::new();
    for r in v.iter().copied() {
        if r.session.is_empty() {
            out.push(violation("H1", r.seq, "session header is missing"));
        }
        if r.idem_key.is_empty() {
            out.push(violation("H2", r.seq, "idempotency key is missing"));
        }
        if r.kind.is_empty() {
            out.push(violation("H5", r.seq, "turn kind is missing"));
        } else if !KINDS.contains(&r.kind.as_str()) {
            out.push(violation("H5", r.seq, format!("unknown turn kind `{}`", r.kind)));
        }
        match r.kind.as_str() {
            "chat" if r.background => out.push(violation("H3", r.seq, "background set on a chat request")),
            "compaction" | "branch_summary" | "prefill" | "subagent" if !r.background => {
                out.push(violation(
                    "H3",
                    r.seq,
                    format!("background missing on a {} request", r.kind),
                ));
            }
            _ => {}
        }
        if r.kind == "prefill" && !r.snapshot_keep {
            out.push(violation(
                "H4",
                r.seq,
                "snapshot keep missing on a prefill request",
            ));
        }
        if r.kind == "prefill" && r.max_tokens != 0 {
            out.push(violation("H6", r.seq, "prefill request must have max_tokens 0"));
        }
        if r.kind != "prefill" && r.max_tokens == 0 {
            out.push(violation("H6", r.seq, "max_tokens 0 outside a prefill request"));
        }
    }
    // H2: a key shared by two requests means byte-identical bodies.
    for (i, a) in v.iter().copied().enumerate() {
        for b in v[i + 1..].iter().copied() {
            if !a.idem_key.is_empty() && a.idem_key == b.idem_key && a.body != b.body {
                out.push(violation(
                    "H2",
                    b.seq,
                    "idempotency key reused for a different body",
                ));
            }
        }
    }
    out
}

/// All rules. Empty = green. Each rule has a negative control in B15's tests: a recorded
/// session with one deliberate fault must produce exactly that rule's violation.
pub fn check_all(reqs: &[Recorded], rebases: Rebases) -> Vec<Violation> {
    let mut out = Vec::new();
    out.extend(check_w1(reqs, rebases));
    out.extend(check_w2(reqs, rebases));
    out.extend(check_w3(reqs, rebases));
    out.extend(check_w5(reqs, rebases));
    out.extend(check_w6(reqs));
    out.extend(check_hints(reqs));
    out.sort_by_key(|x| x.seq);
    out
}

/// A5: the largest `max_tokens` a request that is not a main turn may carry (a summary, a
/// background request): bounded, never the model's whole output limit by default.
pub const SIDE_MAX_TOKENS: u32 = 16384;

/// A5 (tiny mechanics A/B, "keep: no background requests"): the request discipline oxa keeps.
/// * `A5a` every request is one of oxa's documented kinds (`chat`, `compaction`,
///   `branch_summary`, `prefill`, `subagent`); a `chat` request is never background (no
///   title, tag or naming request rides the main kind in the background).
/// * `A5b` every background request, and every request that is not a main turn, carries an
///   explicit bounded `max_tokens`: `prefill` exactly 0 (H6), `compaction` / `branch_summary`
///   / background requests at most [`SIDE_MAX_TOKENS`]; and every `prefill` that is not the
///   park (`snapshot_keep`, foreground) and every `subagent` request is background.
pub fn check_discipline(reqs: &[Recorded]) -> Vec<Violation> {
    let (v, _) = sequence(reqs);
    let mut out = Vec::new();
    for r in v.iter().copied() {
        if !KINDS.contains(&r.kind.as_str()) {
            out.push(violation(
                "A5a",
                r.seq,
                format!("undocumented request kind `{}`", r.kind),
            ));
        }
        if r.kind == "chat" && r.background {
            out.push(violation(
                "A5a",
                r.seq,
                "a background chat request (not a main turn)",
            ));
        }
        match r.kind.as_str() {
            "prefill" if r.max_tokens != 0 => {
                out.push(violation(
                    "A5b",
                    r.seq,
                    format!("prefill with max_tokens {}", r.max_tokens),
                ));
            }
            "compaction" | "branch_summary" if r.max_tokens > SIDE_MAX_TOKENS => out.push(violation(
                "A5b",
                r.seq,
                format!(
                    "{} with max_tokens {} (> {SIDE_MAX_TOKENS})",
                    r.kind, r.max_tokens
                ),
            )),
            _ => {}
        }
        if r.background && r.max_tokens > SIDE_MAX_TOKENS {
            out.push(violation(
                "A5b",
                r.seq,
                format!("background request with max_tokens {}", r.max_tokens),
            ));
        }
        let park = r.kind == "prefill" && r.snapshot_keep;
        if (r.kind == "subagent" || (r.kind == "prefill" && !park)) && !r.background {
            out.push(violation(
                "A5b",
                r.seq,
                format!("{} request without background priority", r.kind),
            ));
        }
    }
    out
}
