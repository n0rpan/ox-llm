//! The chat page attached to a running owner session (#2075): the newest part of its transcript
//! and the message form that sends through its inbox. Every text goes through credential masking
//! first (the approvals panel's masking for commands and arguments, the report scrubber for free
//! text) and then `escape`. The bounds here are display bounds, not limits on the session.
use super::approvals::{age, scrub_shell};
use super::{escape, AttachedView, SessionListView};
use crate::approve_cmd::one_line;
use crate::report_cmd::{scrub, ScrubCtx};
use oxa_proto::{EntryBody, Part, Role, Value, WireMsg};
use std::fmt::Write;

/// Newest entries one render shows.
const SHOWN_ENTRIES: usize = 120;
/// Characters of one message text shown.
const TEXT_CHARS: usize = 4000;
/// Characters of a one-line preview (a tool argument, a result line).
const LINE_CHARS: usize = 200;
/// Lines of a tool result or command output shown.
const RESULT_LINES: usize = 3;
/// Argument names whose value is never shown.
const SECRET_NAMES: [&str; 9] = [
    "token",
    "secret",
    "password",
    "passwd",
    "passphrase",
    "key",
    "authorization",
    "auth",
    "credential",
];

pub(super) fn render(html: &mut String, view: &AttachedView<'_>) {
    let _ = write!(
        html,
        "<section id=session aria-labelledby=session-title><h2 id=session-title>Lead session {}</h2>",
        escape(&view.short_id)
    );
    if view.live {
        let _ = write!(
            html,
            "<p class=muted>Status: running{}</p>",
            view.activity
                .map_or(String::new(), |activity| format!(" · {activity}"))
        );
    } else {
        html.push_str("<p class=muted>Status: not running (messages wait in its inbox)</p>");
    }
    if let Some(tokens) = view.context_tokens {
        let _ = write!(
            html,
            "<p class=muted>Context: {tokens} tokens in the newest request (the session does not record the model window)</p>"
        );
    }
    let skipped = view.entries.len().saturating_sub(SHOWN_ENTRIES);
    if view.truncated || skipped > 0 {
        html.push_str("<p class=muted>Showing the newest part of the session.</p>");
    }
    let ctx = ScrubCtx::default();
    html.push_str("<ol class=session-log role=log aria-label=\"Session transcript\">");
    let mut any = false;
    for entry in &view.entries[skipped..] {
        any |= item(html, &entry.body, &ctx);
    }
    if !any {
        html.push_str("<li class=note>Nothing to show yet.</li>");
    }
    html.push_str("</ol>");
    let _ = write!(
        html,
        r#"<form method=post action=/chat><input type=hidden name=csrf value="{}"><input type=hidden name=session value="{}"><label for=lead-message>Message</label><textarea id=lead-message name=message required></textarea><button>Send</button></form>"#,
        escape(view.csrf),
        escape(&view.id)
    );
    if view.sent {
        html.push_str("<p role=status>Sent. The session reads it at its next turn.</p>");
    }
    html.push_str("</section>");
}

/// The running sessions as CSRF forms. Only the id, the project directory's basename and the
/// time of last activity are shown, and only on the owner-verified chat page.
pub(super) fn render_list(html: &mut String, list: &SessionListView<'_>, now: u64) {
    html.push_str("<section id=sessions aria-labelledby=sessions-title><h2 id=sessions-title>Sessions</h2>");
    if list.rows.is_empty() {
        html.push_str("<p class=muted>No session of yours is running now.</p>");
    } else {
        html.push_str("<ul class=sessions>");
        for row in list.rows {
            let short: String = row.id.chars().take(8).collect();
            let _ = write!(
                html,
                "<li><code>{}</code> · {} · active {} ago ",
                escape(&short),
                escape(&one_line(&row.cwd_name)),
                age(now, row.modified_ms)
            );
            if row.attached {
                html.push_str("· viewing</li>");
            } else {
                let _ = write!(
                    html,
                    r#"<form method=post action=/chat><input type=hidden name=csrf value="{}"><button name=attach value="{}">View</button></form></li>"#,
                    escape(list.csrf),
                    escape(&row.id)
                );
            }
        }
        html.push_str("</ul>");
    }
    if let Some(label) = list.back {
        let _ = write!(
            html,
            r#"<form method=post action=/chat><input type=hidden name=csrf value="{}"><button name=attach value="">{}</button></form>"#,
            escape(list.csrf),
            escape(label)
        );
    }
    html.push_str("</section>");
}

fn item(html: &mut String, body: &EntryBody, ctx: &ScrubCtx) -> bool {
    match body {
        EntryBody::Msg { wire, parts, .. } => match wire.role {
            Role::User => text_item(html, "user", "You", &user_text(wire, parts), ctx),
            Role::Assistant => {
                let shown = text_item(
                    html,
                    "assistant",
                    "Assistant",
                    wire.content.as_deref().unwrap_or(""),
                    ctx,
                );
                for call in &wire.tool_calls {
                    let _ = write!(
                        html,
                        "<li class=tool><span class=who>Tool</span> <code>{}</code> {}</li>",
                        escape(&one_line(&call.name)),
                        escape(&call_preview(&call.args_raw, ctx))
                    );
                }
                shown || !wire.tool_calls.is_empty()
            }
            Role::Tool => {
                let _ = write!(
                    html,
                    "<li class=tool><span class=who>Result</span><pre>{}</pre></li>",
                    escape(&lines_preview(wire.content.as_deref().unwrap_or(""), ctx))
                );
                true
            }
            Role::System => false,
        },
        EntryBody::BashExecution { command, output, .. } => {
            let _ = write!(
                html,
                "<li class=tool><span class=who>Command</span> <code>{}</code><pre>{}</pre></li>",
                escape(&bounded(&one_line(&scrub_shell(command, ctx)), LINE_CHARS)),
                escape(&lines_preview(output, ctx))
            );
            true
        }
        EntryBody::Shown { text, .. } => text_item(html, "note", "Notice", text, ctx),
        EntryBody::Compaction { .. } => marker(html, "compaction"),
        EntryBody::BranchSummary { .. } => marker(html, "branch"),
        EntryBody::ContextEdit { .. } => marker(html, "edit"),
        EntryBody::Custom { .. } | EntryBody::CustomMessage { .. } => marker(html, "custom"),
        _ => false,
    }
}

fn marker(html: &mut String, word: &str) -> bool {
    let _ = write!(html, "<li class=note>[{word}]</li>");
    true
}

/// What the owner typed: the wire text minus the environment note oxa appended to it.
fn user_text(wire: &WireMsg, parts: &[Part]) -> String {
    let mut text = wire.content.clone().unwrap_or_default();
    for part in parts {
        if let Part::EnvNote(note) = part {
            if let Some(stripped) = text.strip_suffix(note.as_str()) {
                text = stripped.trim_end().to_owned();
            }
        }
    }
    text
}

fn text_item(html: &mut String, class: &str, who: &str, text: &str, ctx: &ScrubCtx) -> bool {
    if text.trim().is_empty() {
        return false;
    }
    let masked = scrub(text, ctx);
    // `escape` turns line breaks into spaces; each line is escaped on its own and the
    // list item keeps the breaks (`white-space: pre-wrap`).
    let body: Vec<String> = bounded(&masked, TEXT_CHARS).lines().map(escape).collect();
    let _ = write!(
        html,
        "<li class={class}><span class=who>{who}</span><div class=text>{}</div></li>",
        body.join("\n")
    );
    true
}

/// A tool call's arguments on one bounded line: a value under a secret-looking name is never
/// shown, every other value is masked like an approval command.
fn call_preview(args_raw: &str, ctx: &ScrubCtx) -> String {
    let text = match serde_json::from_str::<Value>(args_raw) {
        Ok(Value::Object(map)) => map
            .iter()
            .map(|(name, value)| {
                let lower = name.to_ascii_lowercase();
                let shown = if SECRET_NAMES.iter().any(|secret| lower.contains(secret)) {
                    "<secret>".to_owned()
                } else {
                    match value {
                        Value::String(text) => scrub_shell(text, ctx),
                        other => scrub_shell(&other.to_string(), ctx),
                    }
                };
                format!("{name}={shown}")
            })
            .collect::<Vec<_>>()
            .join(" "),
        _ => scrub_shell(args_raw, ctx),
    };
    bounded(&one_line(&text), LINE_CHARS)
}

/// The first lines of a tool result or command output, masked and bounded.
fn lines_preview(text: &str, ctx: &ScrubCtx) -> String {
    let mut shown: Vec<String> = text
        .lines()
        .take(RESULT_LINES)
        .map(|line| bounded(&one_line(&scrub_shell(line, ctx)), LINE_CHARS))
        .collect();
    if text.lines().nth(RESULT_LINES).is_some() {
        shown.push("...".into());
    }
    shown.join("\n")
}

fn bounded(text: &str, chars: usize) -> String {
    if text.chars().count() <= chars {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(chars).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lead_page::SessionRow;
    use oxa_proto::{Entry, ToolCall};

    fn entry(body: EntryBody) -> Entry {
        Entry {
            id: "a1b2c3d4".into(),
            parent: None,
            ts_ms: 0,
            body,
        }
    }

    fn msg(wire: WireMsg) -> Entry {
        entry(EntryBody::Msg {
            wire,
            parts: Vec::new(),
            tool: None,
        })
    }

    fn page(entries: &[Entry], live: bool, sent: bool) -> String {
        let mut html = String::new();
        render(
            &mut html,
            &AttachedView {
                csrf: "nonce",
                entries,
                truncated: false,
                live,
                short_id: "abcd1234".into(),
                id: "abcd1234-full-id".into(),
                sent,
                activity: live.then_some("working"),
                context_tokens: Some(1234),
            },
        );
        html
    }

    #[test]
    fn transcript_is_escaped_and_credentials_in_calls_and_results_are_masked() {
        let mut call = WireMsg::assistant("<b>working</b>");
        call.tool_calls.push(ToolCall {
            id: "1".into(),
            name: "bash".into(),
            args_raw: r#"{"command":"curl -H x token=SYNTHETICSECRET && ls","api_key":"SYNTHETICKEY"}"#
                .into(),
        });
        let entries = [
            msg(WireMsg::user("hello there")),
            msg(call),
            msg(WireMsg::tool_result(
                "1",
                "line one\npassword=SYNTHETICPASS\nthree\nfour",
            )),
        ];
        let html = page(&entries, true, false);
        assert!(html.contains("hello there") && html.contains("&lt;b&gt;working&lt;/b&gt;"));
        assert!(!html.contains("<b>working"));
        for secret in ["SYNTHETICSECRET", "SYNTHETICKEY", "SYNTHETICPASS"] {
            assert!(!html.contains(secret), "{secret} leaked: {html}");
        }
        assert!(html.contains("<secret>") || html.contains("&lt;secret&gt;"));
        assert!(html.contains("name=message") && html.contains("id=lead-message"));
        assert!(html.contains(r#"name=session value="abcd1234-full-id""#));
        assert!(html.contains("running") && !html.contains("not running"));
        assert!(!html.contains("Sent."));
        assert!(html.contains("running · working") && html.contains("Context: 1234 tokens"));
    }

    #[test]
    fn session_list_is_a_set_of_csrf_forms_with_basename_only() {
        let rows = [
            SessionRow {
                id: "abcd1234-ffff".into(),
                cwd_name: "<proj>".into(),
                modified_ms: 0,
                attached: false,
            },
            SessionRow {
                id: "eeee5555-ffff".into(),
                cwd_name: "other".into(),
                modified_ms: 0,
                attached: true,
            },
        ];
        let mut html = String::new();
        render_list(
            &mut html,
            &SessionListView {
                csrf: "nonce",
                rows: &rows,
                back: Some("Back to Lead chat"),
            },
            100,
        );
        assert!(html.contains("&lt;proj&gt;") && !html.contains("<proj>"));
        assert!(html.contains(r#"name=attach value="abcd1234-ffff""#));
        assert!(!html.contains(r#"value="eeee5555-ffff""#) && html.contains("viewing"));
        assert!(html.contains(r#"name=attach value="">Back to Lead chat"#));
        assert_eq!(html.matches("name=csrf").count(), 2);
    }

    #[test]
    fn stopped_session_and_sent_notice_are_stated() {
        let html = page(&[], false, true);
        assert!(html.contains("not running"));
        assert!(html.contains("Sent."));
    }

    #[test]
    fn long_text_and_many_entries_are_bounded() {
        let entries: Vec<Entry> = (0..SHOWN_ENTRIES + 10)
            .map(|i| {
                msg(WireMsg::user(format!(
                    "entry-{i:04} {}",
                    "word ".repeat(TEXT_CHARS)
                )))
            })
            .collect();
        let html = page(&entries, true, false);
        assert!(html.contains("newest part"));
        assert!(!html.contains("entry-0000") && html.contains(&format!("entry-{:04}", SHOWN_ENTRIES + 9)));
        assert!(html.contains("word word") && !html.contains(&"word ".repeat(TEXT_CHARS / 5 + 10)));
    }
}
