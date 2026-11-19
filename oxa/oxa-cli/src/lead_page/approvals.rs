//! The "Waiting for your approval" panel (#1512 S6): the approval desk's pending requests.
//! Every field is escaped. Answer forms appear only when owner chat supplies the form credential.
use super::{escape, View};
use oxa_perm::ApprovalRecord;
use std::fmt::Write;
use std::path::Path;

/// The owner's answers, in the order the desk offers them.
const ANSWERS: [(&str, &str); 4] = [
    ("once", "Once"),
    ("session", "This session"),
    ("always", "Always"),
    ("deny", "Deny"),
];

/// Nothing renders when the caller supplied no desk reading, so pages without one are unchanged.
pub(super) fn render(html: &mut String, view: &View<'_>, now: u64) {
    let Some(waiting) = view.approvals else {
        return;
    };
    html.push_str(
        "<section id=approvals aria-labelledby=approvals-title><h2 id=approvals-title>Waiting for your approval</h2>",
    );
    if waiting.is_empty() {
        html.push_str("<p class=muted>Nothing is waiting for your approval.</p>");
    } else {
        html.push_str("<ol class=approvals>");
        for rec in waiting {
            record(html, view, rec, now);
        }
        html.push_str("</ol>");
    }
    if view.approval_csrf.is_none() {
        html.push_str(
            "<p class=muted>Answering needs owner chat enabled in user settings. In a terminal, the same answer is <code>oxa send &lt;session&gt; --approve &lt;id&gt; once|session|always|deny</code>.</p>",
        );
    }
    html.push_str("</section>");
}

fn record(html: &mut String, view: &View<'_>, rec: &ApprovalRecord, now: u64) {
    let folder = Path::new(&rec.cwd)
        .file_name()
        .map_or_else(|| "(root)".to_owned(), |name| name.to_string_lossy().into_owned());
    let _ = write!(
        html,
        "<li class=approval><p><code>{}</code> · session <code>{}</code> · <code>{}</code> · waiting {}</p><pre>{}</pre>",
        escape(&rec.id),
        escape(&rec.session),
        escape(&folder),
        age(now, rec.created_ms),
        escape(&rec.command),
    );
    html.push_str("<ul class=reasons>");
    for reason in &rec.reasons {
        let _ = write!(html, "<li>{}</li>", escape(reason));
    }
    html.push_str("</ul>");
    if let Some(csrf) = view.approval_csrf {
        let _ = write!(
            html,
            "<form method=post action=/approvals><input type=hidden name=csrf value=\"{}\"><input type=hidden name=id value=\"{}\">",
            escape(csrf),
            escape(&rec.id)
        );
        for (answer, label) in ANSWERS {
            let _ = write!(html, "<button name=answer value={answer}>{label}</button>");
        }
        html.push_str("</form>");
    }
    html.push_str("</li>");
}

/// Whole seconds, minutes or hours since the request was filed. A timestamp ahead of this clock reads as new.
fn age(now: u64, created_ms: u64) -> String {
    let seconds = now.saturating_sub(created_ms / 1000);
    match seconds {
        0..=59 => format!("{seconds} s"),
        60..=3599 => format!("{} min", seconds / 60),
        _ => format!("{} h", seconds / 3600),
    }
}

#[cfg(test)]
mod tests {
    use crate::lead_page::{content_for, Page, View};
    use oxa_perm::ApprovalRecord;

    #[test]
    fn board_page_lists_pending_approvals_escaped() {
        let waiting = vec![ApprovalRecord {
            id: "0123456789abcdef".into(),
            session: "s<1>".into(),
            cwd: "/work/<proj>".into(),
            command: "rm -rf \"<x>\" &".into(),
            keys: vec!["rm".into()],
            reasons: vec!["rm: <recursive>".into(), "password=opaque-example".into()],
            status: "pending".into(),
            used: false,
            created_ms: 0,
            answered_ms: None,
        }];
        let owner = View {
            approvals: Some(&waiting),
            approval_csrf: Some("tok"),
            ..Default::default()
        };
        let html = content_for(Page::Board, &owner, 120);
        assert!(html.contains("Waiting for your approval"));
        assert!(
            html.contains("<pre>rm -rf &quot;&lt;x&gt;&quot; &amp;</pre>"),
            "{html}"
        );
        assert!(html.contains("session <code>s&lt;1&gt;</code>"));
        assert!(html.contains("<code>&lt;proj&gt;</code>"));
        assert!(html.contains("<li>rm: &lt;recursive&gt;</li>"));
        assert!(html.contains("<li>password=&lt;secret&gt;</li>"));
        assert!(!html.contains("opaque-example"));
        assert!(html.contains("waiting 2 min"));
        assert!(html.contains("<button name=answer value=once>Once</button>"));
        assert!(html.contains("name=csrf value=\"tok\""));
        assert!(!html.contains("<x>"));
        assert!(!html.contains("<proj>"));

        let read_only = View {
            approvals: Some(&waiting),
            ..Default::default()
        };
        let html = content_for(Page::Board, &read_only, 120);
        assert!(!html.contains("Waiting for your approval"));
        assert!(!html.contains("s&lt;1&gt;"));
        assert!(!html.contains("action=/approvals"));
    }
}
