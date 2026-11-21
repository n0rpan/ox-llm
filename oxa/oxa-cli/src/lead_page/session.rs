//! The chat thread: one reading of the bound Lead Session, rendered as messages.
//!
//! This module decides only how entries are shown. Masking happens before any markup:
//! text goes through the report scrubber, a shell command through the approval panel's
//! command masking, and control, bidi and zero-width characters through the CLI
//! listing's one-line form, so a hostile entry cannot reorder what the owner reads.
//! Lead text becomes markup only through [`super::md`]. Nothing here answers, approves,
//! steers or sends: the composer's form is the same same-origin, CSRF-checked POST it
//! was, and the running marker reads entry timestamps only.
use super::approvals::scrub_shell;
use super::{escape, md, ItemKind};
use crate::approve_cmd::one_line;
use crate::report_cmd::{scrub, ScrubCtx};
use oxa_proto::{Entry, EntryBody, Role};

/// One thread page: the newest messages shown at first. Older ones stay in the session
/// file and open with `?before=<entry id>`.
pub(super) const PAGE: usize = 30;
/// The most characters of a step's text shown.
const MAX_STEP_TEXT: usize = 600;
/// How long after its newest entry a turn still reads as running.
const RUNNING_SECS: u64 = 180;

const CONFIRMED: &str = "oxa.lead.proposal_confirmed";

/// One rendered thread piece: a bubble, a step or a one-line note. A step whose
/// [`ItemKind`] is `Step` folds with its neighbours; every other row stands alone.
pub(crate) struct Item {
    pub id: String,
    pub ts: u64,
    kind: ItemKind,
    /// Rendered markup for a bubble or note; empty for a folding step.
    body: String,
    /// Rendered text of a step that cannot fold.
    detail: String,
    /// The tool's name for a step's label.
    name: String,
    /// A failed tool result, shown as failed rather than as ordinary output.
    failed: bool,
}

/// A row that stands alone instead of folding: a bubble, or a step worth its own row
/// because it carries text of its own or failed.
fn standalone(item: &Item) -> bool {
    item.kind != ItemKind::Step && (!item.detail.is_empty() || item.failed)
}

/// The tool run a row belongs to: consecutive folding steps, closed by a `standalone`
/// row.
fn fold(rows: &[Item]) -> Vec<Vec<&Item>> {
    let mut groups: Vec<Vec<&Item>> = Vec::new();
    let mut run: Vec<&Item> = Vec::new();
    for row in rows {
        if row.kind == ItemKind::Step && !standalone(row) {
            run.push(row);
            continue;
        }
        if !run.is_empty() {
            groups.push(std::mem::take(&mut run));
        }
        groups.push(vec![row]);
    }
    if !run.is_empty() {
        groups.push(run);
    }
    groups
}

/// One `steps · bash ×4, read ×2 · 38 s` label for a folded run. Names are the tools'
/// own; the count and the duration are exact reads of this run.
fn tools_summary(run: &[&Item]) -> String {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for row in run {
        match counts.iter_mut().find(|(name, _)| *name == row.name) {
            Some((_, count)) => *count += 1,
            None => counts.push((row.name.clone(), 1)),
        }
    }
    let names = counts
        .iter()
        .map(|(name, count)| {
            if *count > 1 {
                format!("{name} ×{count}")
            } else {
                name.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let seconds = run
        .iter()
        .map(|row| row.ts)
        .fold((u64::MAX, 0), |(low, high), ts| (low.min(ts), high.max(ts)));
    let duration = seconds.1.saturating_sub(seconds.0);
    let steps = run.len();
    if duration > 0 {
        format!("{steps} steps · {names} · {duration} s")
    } else {
        format!("{steps} steps · {names}")
    }
}

fn step_html(html: &mut String, row: &Item) {
    let _ = write!(html, "<div class=step{}>", if row.failed { " bad" } else { "" });
    if !row.name.is_empty() {
        let _ = write!(html, "<b>{}</b> ", escape(&row.name));
    }
    if !row.detail.is_empty() {
        let _ = write!(html, "<span class=what>{}</span>", row.detail);
    }
    if row.failed {
        let _ = write!(html, " <span class=status>failed</span>");
    }
    if !row.body.is_empty() {
        let _ = write!(
            html,
            "<div class=\"out\">{}<button type=button class=copy data-copy=1>Copy</button></div>",
            row.body
        );
    }
    html.push_str("</div>");
}

/// The most characters of one rendered body shown; more is clipped with a note.
const MAX_BODY: usize = 16_384;

fn clipped(body: &str) -> String {
    if body.len() <= MAX_BODY {
        return body.to_owned();
    }
    let cut = body
        .char_indices()
        .take_while(|(at, _)| *at <= MAX_BODY)
        .last()
        .map_or(0, |(at, _)| at);
    format!("{}<p class=muted>Message clipped.</p>", &body[..cut])
}

/// What the thread shows under one lead message: fact chips from this process's reply
/// whose text is this bubble's own, and that reply's action card. A reply is matched by
/// its text, so the chips never land under a different message.
fn under_reply(
    html: &mut String,
    body: &str,
    row_is_lead: bool,
    latest: Option<&crate::lead_chat::conversation::ConversationReply>,
    action: Option<(&str, &crate::lead_page::OwnerChatView<'_>)>,
    now: u64,
) {
    let Some(reply) = latest.filter(|_| row_is_lead) else {
        return;
    };
    if !body.contains(&super::md::render(&reply.text)) {
        return;
    }
    if !reply.facts.is_empty() {
        html.push_str("<div class=facts>");
        for fact in &reply.facts {
            let _ = write!(
                html,
                "<a class=chip href=\"{}\">{}</a><span class=\"chip quiet\">observed {}</span>",
                escape(&fact.href),
                escape(&fact.label),
                super::human_time(fact.observed_at, now)
            );
        }
        html.push_str("</div>");
    }
    let (Some((id, view)), Some(action)) = (action, reply.action.as_ref()) else {
        return;
    };
    html.push_str("<div class=action>");
    super::proposals::render(html, &action.draft);
    if view.confirmed {
        html.push_str("<p class=ok>Confirmation recorded. Application is pending.</p>");
    } else {
        let _ = write!(
            html,
            "<form method=post action=/chat><input type=hidden name=csrf value=\"{}\"><input type=hidden name=proposal_id value=\"{}\"><button class=btn>Confirm proposal</button></form>",
            escape(view.csrf),
            escape(id)
        );
    }
    html.push_str("</div>");
}

/// The thread as bubbles. A run of folding steps becomes one `<details class=tools>`; a
/// step with its own editor text or a failure keeps its own row inside a details.
pub(super) fn render(
    html: &mut String,
    rows: &[Item],
    latest: Option<&crate::lead_chat::conversation::ConversationReply>,
    action: Option<(&str, &crate::lead_page::OwnerChatView<'_>)>,
    now: u64,
) {
    for group in &fold(rows) {
        let first = group[0];
        if first.kind == ItemKind::Step && !standalone(first) {
            let failed = if group.iter().any(|row| row.failed) {
                " data-failed=1"
            } else {
                ""
            };
            let _ = write!(
                html,
                "<details class=tools{failed} data-key=\"f{}\" open><summary>{}</summary>",
                first.id,
                escape(&tools_summary(group))
            );
            for row in group {
                step_html(html, row);
            }
            html.push_str("</details>");
            continue;
        }
        if first.kind == ItemKind::Tool {
            let _ = write!(
                html,
                "<details class=\"tools step{}\"{}{}><summary><b>{}</b> {}</summary>",
                if first.failed { " bad" } else { "" },
                if first.failed { " data-failed=1" } else { "" },
                format!(" data-key=\"f{}\" open", first.id),
                escape(&first.name),
                if first.detail.is_empty() {
                    "step".to_owned()
                } else {
                    first.detail.clone()
                }
            );
            step_html(html, first);
            html.push_str("</details>");
            continue;
        }
        let class = if first.kind == ItemKind::Owner {
            "me"
        } else {
            "lead"
        };
        let body = clipped(&first.body);
        let id = if first.id.is_empty() {
            String::new()
        } else {
            format!(" data-entry={}", escape(&first.id))
        };
        let _ = write!(html, "<div class=\"msg {class}\"{id}>{body}");
        under_reply(html, &body, class == "lead", latest, action, now);
        let by = if class == "me" {
            format!("you · {}", super::human_time(first.ts, now))
        } else {
            format!("lead · {}", super::human_time(first.ts, now))
        };
        let _ = write!(html, "<div class=by>{by}</div></div>");
    }
}

/// The page of thread rows the route shows: the newest [`PAGE`] messages and the steps
/// between them, or the page before the entry `before` names. An id the session does not
/// hold changes nothing, so a forged bound only shows the newest page again.
pub(super) fn page(rows: Vec<Item>, before: Option<&str>) -> (Vec<Item>, bool) {
    let end = before
        .and_then(|id| rows.iter().position(|row| row.id == id))
        .unwrap_or(rows.len());
    // Count messages, not steps: a page is a readable amount of conversation.
    let mut seen = 0usize;
    let mut start = end;
    while start > 0 {
        start -= 1;
        if rows[start].kind != ItemKind::Step && !standalone(&rows[start]) {
            seen += 1;
            if seen > PAGE {
                break;
            }
        }
    }
    let older = start > 0;
    (rows[start..end].to_vec(), older)
}

/// A synthetic session fixture for the render tests: the same entry shapes a real
/// session file holds, with no text of any real conversation.
#[cfg(test)]
pub(crate) fn fixture() -> Vec<Entry> {
    use oxa_proto::{Part, ToolCall, ToolMeta, Usage, Value, WireMsg};
    let mut entries = Vec::new();
    let push = |entries: &mut Vec<Entry>, id: &str, parent: Option<&str>, ts_ms: u64, body: EntryBody| {
        entries.push(Entry {
            id: id.into(),
            parent: parent.map(str::to_owned),
            ts_ms,
            body,
        });
    };
    let msg = |wire: WireMsg, tool: Option<ToolMeta>| EntryBody::Msg {
        wire,
        parts: vec![Part::Text("fixture".into())],
        tool,
    };
    push(
        &mut entries,
        "a0000000",
        None,
        (3_600_u64 - 120) * 1000,
        msg(WireMsg::user("Why is the fixture **stuck**?"), None),
    );
    let mut call_one = WireMsg::assistant("Checking the fixture **item**.");
    call_one.tool_calls = vec![ToolCall {
        id: "call-1".into(),
        name: "bash".into(),
        args_raw: "{\"command\":\"oxa status --project fixture\"}".into(),
    }];
    push(
        &mut entries,
        "a0000001",
        Some("a0000000"),
        (3_600_u64 - 100) * 1000,
        msg(call_one, None),
    );
    push(
        &mut entries,
        "a0000002",
        Some("a0000001"),
        (3_600_u64 - 96) * 1000,
        msg(
            WireMsg::tool_result("call-1", "fixture output line\nsecond line"),
            None,
        ),
    );
    for (index, (name, args)) in [
        ("read", "{\"path\":\"oxa/src/fixture.rs\"}"),
        ("bash", "{\"command\":\"cargo fmt --check\"}"),
        ("edit", "{\"path\":\"oxa/src/fixture.rs\"}"),
    ]
    .into_iter()
    .enumerate()
    {
        let mut call = WireMsg::assistant("");
        call.tool_calls = vec![ToolCall {
            id: format!("call-{}", index + 2),
            name: name.into(),
            args_raw: args.into(),
        }];
        push(
            &mut entries,
            &format!("a000000{}", 3 + index),
            Some("a0000002"),
            (3_600_u64 - 90 + index as u64) * 1000,
            msg(call, None),
        );
    }
    let reply = "Two findings are open:\n\n- the first one\n- the second one\n\n```rust\nlet path = \"<fixture>\";\n```\n\nSee #17.";
    push(
        &mut entries,
        "a0000006",
        Some("a0000003"),
        (3_600_u64 - 80) * 1000,
        msg(WireMsg::assistant(reply), None),
    );
    push(
        &mut entries,
        "a0000007",
        Some("a0000006"),
        (3_600_u64 - 80) * 1000,
        EntryBody::Usage {
            kind: "turn".into(),
            usage: Usage {
                prompt: 1_200,
                completion: 300,
                ..Usage::default()
            },
        },
    );
    push(
        &mut entries,
        "a0000008",
        Some("a0000007"),
        (3_600_u64 - 70) * 1000,
        EntryBody::Msg {
            wire: WireMsg::tool_result(
                "call-old",
                "token=demo-secret-value sk-abcdefghijklmnopqrstuvwxyz12 /home/synthetic-user/private/file",
            ),
            parts: Vec::new(),
            tool: Some(ToolMeta {
                is_error: true,
                ui: Value::Null,
            }),
        },
    );
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    fn built(entries: &[Entry]) -> Vec<Item> {
        let ctx = ScrubCtx::default();
        items(entries, &ctx)
    }

    fn rendered(entries: &[Entry]) -> String {
        let (page, older) = page(built(entries), None);
        assert!(!older);
        let mut html = String::new();
        render(&mut html, &page, None, None, 3_600);
        html
    }

    #[test]
    fn thread_is_bubbles_with_one_folded_tool_run_and_masked_failures() {
        let entries = fixture();
        let html = rendered(&entries);
        assert!(html.contains("class=\"msg me\""), "{html}");
        assert!(html.contains("class=\"msg lead\""), "{html}");
        assert!(html.contains("<strong>item</strong>"), "{html}");
        assert!(
            html.contains("<ul><li>the first one</li><li>the second one</li></ul>"),
            "{html}"
        );
        assert!(
            html.contains("<span class=lang>rust</span><pre><code>let path = &quot;&lt;fixture&gt;&quot;;"),
            "{html}"
        );
        assert!(html.contains("<a href=\"/items/17\">#17</a>"), "{html}");
        // bash, read, edit and the failed result are tool rows: the consecutive ones
        // fold into one details whose label counts the tools by their own names.
        assert!(html.contains("<details class=tools"), "{html}");
        assert!(html.contains("read, bash, edit"), "{html}");
        assert!(html.contains("data-failed=1"), "{html}");
        // A step that carries its own text shows the masked text and a Copy button: the
        // node holds text and the button, never an attribute holding the command.
        assert!(html.contains("oxa/src/fixture.rs</span>"), "{html}");
        assert!(html.contains("<button type=button class=copy"), "{html}");
        // A result holding a secret and a home path is masked before it is escaped.
        assert!(
            !html.contains("demo-secret-value") && !html.contains("sk-abcdef"),
            "{html}"
        );
        assert!(!html.contains("/home/synthetic-user"), "{html}");
        assert!(html.contains("<secret>"), "{html}");
    }

    #[test]
    fn a_page_holds_the_newest_messages_and_never_panics_on_an_unknown_id() {
        let entries = fixture();
        let (page, older) = page(built(&entries), None);
        assert!(!older);
        let first = page.first().map(|row| row.id.clone()).unwrap();
        let (earlier, older) = page(built(&entries), Some(&first));
        assert!(earlier.is_empty() && !older, "one page has nothing earlier");
        let (page, _) = page(built(&entries), Some("ffffffff"));
        assert_eq!(page.len(), built(&entries).len());
    }

    #[test]
    fn working_reads_only_the_newest_entry_time() {
        let items = built(&fixture());
        assert!(working(&items, 3_600));
        assert!(!working(&items, 3_600 + RUNNING_SECS));
        assert!(!working(&[], 3_600));
        assert_eq!(newest(&items), 3_600 - 70);
        assert_eq!(items[0].kind, ItemKind::Owner);
    }
}
