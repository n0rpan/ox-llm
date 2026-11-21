//! The "Waiting for your approval" panel (#1512 S6): the approval desk's pending requests.
//! Only the bound owner's view renders the panel. Credential masking precedes HTML escaping, and
//! the record text goes through the CLI listing's one-line form first, so line breaks, control and
//! bidi or zero-width characters show as escapes: the owner reads the text the shell will run.
use super::{escape, View};
use crate::approve_cmd::one_line;
use crate::report_cmd::{scrub, ScrubCtx};
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
    let Some((waiting, csrf)) = view.approvals.zip(view.approval_csrf) else {
        return;
    };
    html.push_str(
        "<section id=approvals aria-labelledby=approvals-title><h2 id=approvals-title>Waiting for your approval</h2>",
    );
    if waiting.is_empty() {
        html.push_str("<p class=muted>Nothing is waiting for your approval.</p>");
    } else {
        html.push_str("<ol class=approvals>");
        let scrub_ctx = ScrubCtx::default();
        for rec in waiting {
            record(html, csrf, rec, now, &scrub_ctx);
        }
        html.push_str("</ol>");
    }
    html.push_str("</section>");
}

fn record(html: &mut String, csrf: &str, rec: &ApprovalRecord, now: u64, scrub_ctx: &ScrubCtx) {
    let folder = Path::new(&rec.cwd)
        .file_name()
        .map_or_else(|| "(root)".to_owned(), |name| name.to_string_lossy().into_owned());
    let _ = write!(
        html,
        "<li class=approval><p><code>{}</code> · session <code>{}</code> · <code>{}</code> · waiting {}</p><pre>{}</pre>",
        escape(&one_line(&rec.id)),
        escape(&one_line(&rec.session)),
        escape(&one_line(&folder)),
        age(now, rec.created_ms),
        escape(&one_line(&scrub_shell(&rec.command, scrub_ctx))),
    );
    html.push_str("<ul class=reasons>");
    for reason in &rec.reasons {
        let _ = write!(html, "<li>{}</li>", escape(&one_line(&scrub(reason, scrub_ctx))));
    }
    html.push_str("</ul>");
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
    html.push_str("</li>");
}

/// Credential masking that keeps every executable part of the command visible. The command is
/// split into shell words at unquoted whitespace and operators (`&`, `|`, `;`, `<`, `>`,
/// parentheses, line breaks); separators are kept verbatim, so a masked value never swallows
/// what follows it (`token=demo&&rm` keeps `&&rm`), and quoting or a backslash keeps a
/// metacharacter inside its word (`password="&value"` is one word). Each word is then
/// [`mask_word`]ed on its own.
pub(super) fn scrub_shell(command: &str, ctx: &ScrubCtx) -> String {
    let is_sep = |c: char| c.is_whitespace() || matches!(c, '&' | '|' | ';' | '<' | '>' | '(' | ')');
    let mut out = String::with_capacity(command.len());
    let (mut word, mut seps) = (String::new(), String::new());
    let (mut single, mut double, mut escaped) = (false, false, false);
    for c in command.chars() {
        if !single && !double && !escaped && is_sep(c) {
            if !word.is_empty() {
                out.push_str(&mask_word(&word, ctx));
                word.clear();
            }
            seps.push(c);
            continue;
        }
        if !seps.is_empty() {
            out.push_str(&seps);
            seps.clear();
        }
        word.push(c);
        if escaped {
            escaped = false;
        } else if c == '\\' && !single {
            escaped = true;
        } else if c == '\'' && !double {
            single = !single;
        } else if c == '"' && !single {
            double = !double;
        }
    }
    if !word.is_empty() {
        out.push_str(&mask_word(&word, ctx));
    }
    out.push_str(&seps);
    out
}

/// One shell word, masked for display. A real expansion (`$name`, `${...}`, `$(...)`, a
/// backtick) is kept visible: the shell runs what it holds, so the owner must see it, and the
/// literal credential text around it is still masked. A `$` that cannot start an expansion
/// (a trailing one, or one before a quote or a space) is literal text. A secret-named
/// assignment (`password=...`, `--api-key=...`, `TOKEN='...'`) shows its name and `<secret>`
/// for its literal value, quotes included, and `<secret>` for each literal run of a value
/// that also holds an expansion. Any other word goes through the report scrubber (known
/// token formats).
fn mask_word(word: &str, ctx: &ScrubCtx) -> String {
    let executable = has_executable_expansion(word);
    if let Some(eq) = word.find('=') {
        let name = word[..eq].trim_start_matches('-').to_ascii_lowercase();
        let named = !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
        let secret = [
            "token",
            "secret",
            "password",
            "passwd",
            "passphrase",
            "key",
            "authorization",
            "auth",
            "credential",
        ]
        .iter()
        .any(|k| name.contains(k));
        if named && secret && eq + 1 < word.len() {
            if !executable {
                return format!("{}<secret>", &word[..=eq]);
            }
            // The value also holds an expansion the shell runs: keep the expansion visible
            // and mask the literal credential runs around it.
            return format!("{}{}", &word[..=eq], mask_literal_runs(&word[eq + 1..]));
        }
    }
    if !executable {
        return scrub(word, ctx);
    }
    // The shell runs this word, so the owner must see it as written.
    word.to_string()
}

/// Whether the shell can act on an expansion marker in this word. Single quotes make both
/// markers literal, and a backslash makes the following marker literal; double quotes still
/// permit parameter and command substitution. A `$` starts an expansion only before a name
/// character, `{` or `(`; a trailing or otherwise bare `$` is literal text.
fn has_executable_expansion(word: &str) -> bool {
    let (mut single, mut double, mut escaped) = (false, false, false);
    let mut chars = word.chars().peekable();
    while let Some(c) = chars.next() {
        if escaped {
            escaped = false;
        } else if c == '\\' && !single {
            escaped = true;
        } else if c == '\'' && !double {
            single = !single;
        } else if c == '"' && !single {
            double = !double;
        } else if !single && c == '`' {
            return true;
        } else if !single && c == '$' && chars.peek().is_some_and(|next| expansion_start(*next)) {
            return true;
        }
    }
    false
}

/// The characters that can continue `$` into an expansion the shell runs.
fn expansion_start(next: char) -> bool {
    next.is_alphanumeric() || matches!(next, '_' | '{' | '(')
}

/// The literal runs of a value masked as `<secret>`, with the expansions the shell runs kept
/// as written, including the command text inside `$(...)`: the owner must see what runs.
/// A run that holds only quote or escape characters stays as it is, so a fully expanded
/// value still reads exactly as written.
fn mask_literal_runs(value: &str) -> String {
    let mut out = String::new();
    let mut literal = String::new();
    let (mut single, mut double, mut escaped) = (false, false, false);
    let mut chars = value.chars().peekable();
    let flush = |out: &mut String, literal: &mut String| {
        let maskable = literal.chars().any(|c| c != '"' && c != '\'' && c != '\\');
        out.push_str(if maskable { "<secret>" } else { literal });
        literal.clear();
    };
    while let Some(c) = chars.next() {
        if escaped {
            escaped = false;
            literal.push(c);
        } else if c == '\\' && !single {
            escaped = true;
            literal.push(c);
        } else if c == '\'' && !double {
            single = !single;
            literal.push(c);
        } else if c == '"' && !single {
            double = !double;
            literal.push(c);
        } else if !single && c == '$' && !chars.peek().is_some_and(|next| expansion_start(*next)) {
            literal.push(c);
        } else if !single && c == '$' {
            flush(&mut out, &mut literal);
            out.push(c);
            match chars.next() {
                Some('{') => {
                    out.push('{');
                    let mut depth = 1;
                    while depth > 0 {
                        match chars.next() {
                            Some('{') => {
                                depth += 1;
                                out.push('{');
                            }
                            Some('}') => {
                                depth -= 1;
                                out.push('}');
                            }
                            Some(other) => out.push(other),
                            None => break,
                        }
                    }
                }
                Some('(') => {
                    out.push('(');
                    let mut depth = 1;
                    while depth > 0 {
                        match chars.next() {
                            Some('(') => {
                                depth += 1;
                                out.push('(');
                            }
                            Some(')') => {
                                depth -= 1;
                                out.push(')');
                            }
                            Some(other) => out.push(other),
                            None => break,
                        }
                    }
                }
                Some(name) => {
                    out.push(name);
                    while let Some(next) = chars.peek().copied() {
                        if next.is_alphanumeric() || next == '_' {
                            out.push(next);
                            chars.next();
                        } else {
                            break;
                        }
                    }
                }
                None => {}
            }
        } else if !single && c == '`' {
            flush(&mut out, &mut literal);
            out.push('`');
            for next in chars.by_ref() {
                out.push(next);
                if next == '`' {
                    break;
                }
            }
        } else {
            literal.push(c);
        }
    }
    if !literal.is_empty() {
        flush(&mut out, &mut literal);
    }
    out
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
            run_nonce: "0123456789abcdef".into(),
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

    /// A multiline command or one with bidi or zero-width characters renders the escaped one-line
    /// form of the CLI listing, never a joined line that hides the second command in a comment.
    #[test]
    fn approval_panel_shows_line_breaks_and_format_characters() {
        let waiting = vec![ApprovalRecord {
            id: "0123456789abcdef".into(),
            session: "s\u{202e}1".into(),
            cwd: "/work/proj".into(),
            command: "echo safe # explanation\nrm -rf /srv/data\u{200b}".into(),
            keys: vec!["rm".into()],
            reasons: vec!["rm:\u{2066} recursive".into()],
            status: "pending".into(),
            used: false,
            run_nonce: "0123456789abcdef".into(),
            created_ms: 0,
            answered_ms: None,
        }];
        let owner = View {
            approvals: Some(&waiting),
            approval_csrf: Some("tok"),
            ..Default::default()
        };
        let html = content_for(Page::Board, &owner, 120);
        assert!(
            html.contains("<pre>echo safe # explanation\\nrm -rf /srv/data\\u{200b}</pre>"),
            "{html}"
        );
        assert!(html.contains("session <code>s\\u{202e}1</code>"), "{html}");
        assert!(html.contains("<li>rm:\\u{2066} recursive</li>"), "{html}");
        for hidden in ['\u{202e}', '\u{200b}', '\u{2066}'] {
            assert!(!html.contains(hidden), "{html}");
        }
        assert!(!html.contains("explanation rm"), "{html}");
    }

    /// Masking a secret-looking assignment keeps the shell operator after it, so the owner sees
    /// every command the line runs.
    #[test]
    fn approval_panel_masking_keeps_shell_operators() {
        let waiting = vec![ApprovalRecord {
            id: "0123456789abcdef".into(),
            session: "s1".into(),
            cwd: "/work/proj".into(),
            command: "echo token=demo&&rm -rf /srv/data;echo password=opaque-example|sh".into(),
            keys: vec!["rm".into()],
            reasons: vec![],
            status: "pending".into(),
            used: false,
            run_nonce: "0123456789abcdef".into(),
            created_ms: 0,
            answered_ms: None,
        }];
        let owner = View {
            approvals: Some(&waiting),
            approval_csrf: Some("tok"),
            ..Default::default()
        };
        let html = content_for(Page::Board, &owner, 120);
        assert!(
            html.contains("&amp;&amp;rm -rf /srv/data;echo password=&lt;secret&gt;|sh</pre>"),
            "{html}"
        );
        assert!(!html.contains("token=demo"), "{html}");
    }

    /// An expansion runs: a command substitution inside a quoted value is shown in full, never
    /// hidden as part of a masked credential.
    #[test]
    fn approval_panel_shows_command_substitutions() {
        let waiting = vec![ApprovalRecord {
            id: "0123456789abcdef".into(),
            session: "s1".into(),
            cwd: "/work/proj".into(),
            command: r#"password="$(rm -rf /srv/data)" git push --force origin main"#.into(),
            keys: vec!["git-push-force".into()],
            reasons: vec![],
            status: "pending".into(),
            used: false,
            run_nonce: "0123456789abcdef".into(),
            created_ms: 0,
            answered_ms: None,
        }];
        let owner = View {
            approvals: Some(&waiting),
            approval_csrf: Some("tok"),
            ..Default::default()
        };
        let html = content_for(Page::Board, &owner, 120);
        assert!(
            html.contains("password=&quot;$(rm -rf /srv/data)&quot; git push --force origin main</pre>"),
            "{html}"
        );
    }

    #[test]
    fn approval_panel_masks_literal_runs_around_expansions() {
        for (command, shown) in [
            // A trailing `$` cannot start an expansion, so the whole value is literal.
            (
                r#"password="opaque-example$" git push --force origin main"#,
                "password=&lt;secret&gt; git push",
            ),
            // A real expansion stays visible; the literal credential run around it is masked.
            (
                r#"password="opaque-example$USER" git push --force origin main"#,
                "password=&lt;secret&gt;$USER&quot; git push",
            ),
            (
                r#"password="opaque-example${USER}" git push --force origin main"#,
                "password=&lt;secret&gt;${USER}&quot; git push",
            ),
            (
                r#"TOKEN=opaque-example$USER git push --force origin main"#,
                "TOKEN=&lt;secret&gt;$USER git push",
            ),
        ] {
            let waiting = vec![ApprovalRecord {
                id: "0123456789abcdef".into(),
                session: "s1".into(),
                cwd: "/work/proj".into(),
                command: command.into(),
                keys: vec!["git-push-force".into()],
                reasons: vec![],
                status: "pending".into(),
                used: false,
                run_nonce: "0123456789abcdef".into(),
                created_ms: 0,
                answered_ms: None,
            }];
            let owner = View {
                approvals: Some(&waiting),
                approval_csrf: Some("tok"),
                ..Default::default()
            };
            let html = content_for(Page::Board, &owner, 120);
            assert!(html.contains(shown), "missing {shown:?} in {html}");
            assert!(
                !html.contains("opaque-example"),
                "literal credential text is displayed: {html}"
            );
        }
    }

    /// A shell metacharacter inside a quoted or escaped credential value is part of the value:
    /// the whole value stays masked.
    #[test]
    fn approval_panel_masking_respects_shell_quoting() {
        for (command, secret) in [
            (
                r#"password="&opaque-example" git push --force origin main"#,
                "opaque-example",
            ),
            (
                r#"password=x\&opaque-example git push --force origin main"#,
                "opaque-example",
            ),
            (
                r#"password='&opaque-example' git push --force origin main"#,
                "opaque-example",
            ),
            (
                r#"API_KEY='a b opaque-example' git push --force origin main"#,
                "opaque-example",
            ),
            (
                r#"password='opaque$example' git push --force origin main"#,
                "opaque$example",
            ),
            (
                r#"password=opaque\$example git push --force origin main"#,
                r#"opaque\$example"#,
            ),
        ] {
            let waiting = vec![ApprovalRecord {
                id: "0123456789abcdef".into(),
                session: "s1".into(),
                cwd: "/work/proj".into(),
                command: command.into(),
                keys: vec!["git-push-force".into()],
                reasons: vec![],
                status: "pending".into(),
                used: false,
                run_nonce: "0123456789abcdef".into(),
                created_ms: 0,
                answered_ms: None,
            }];
            let owner = View {
                approvals: Some(&waiting),
                approval_csrf: Some("tok"),
                ..Default::default()
            };
            let html = content_for(Page::Board, &owner, 120);
            assert!(!html.contains(secret), "{command}: {html}");
            assert!(
                html.contains("git push --force origin main</pre>"),
                "{command}: {html}"
            );
        }
    }
}
