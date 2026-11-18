//! Fixed questions use the same observation as every page, without model calls or state.
use super::{Prompt, View};
use std::fmt::Write;

pub(super) fn render(html: &mut String, view: &View<'_>, prompt: Prompt) {
    if let Some(csrf) = view.fixed_prompt_csrf {
        let _ = write!(html, "<form class=fixed-prompts method=post action=/chat><input type=hidden name=csrf value=\"{}\"><fieldset><legend>Ask about this observation</legend><button name=prompt value=status>Status</button><button name=prompt value=workers>Are workers on goal?</button><button name=prompt value=attention>What needs me?</button></fieldset></form>", super::escape(csrf));
    }
    html.push_str("<section><h2>Snapshot prompts</h2><p><a href=/chat?prompt=status>Status</a> · <a href=/chat?prompt=workers>Are workers on goal?</a> · <a href=/chat?prompt=attention>What needs me?</a></p><div role=log aria-label=\"Lead replies\"><article>");
    let fact = match prompt {
        Prompt::Status => view.board.map(|board| {
            (
                format!(
                    "The board contained {} items at observation.",
                    board.entries.len()
                ),
                board.observed_at,
                "/board",
                "Board",
            )
        }),
        Prompt::Workers => view.situation.map(|snapshot| {
            (
                format!(
                    "The runner reported {} running tasks. Verified worker goals are unavailable.",
                    snapshot.running.len()
                ),
                snapshot.at_ms / 1000,
                "/workers",
                "Workers",
            )
        }),
        Prompt::Attention => view.situation.map(|snapshot| {
            (
                format!(
                    "The runner reported {} open owner decisions.",
                    snapshot.asks.len()
                ),
                snapshot.at_ms / 1000,
                "/asks",
                "Asks",
            )
        }),
    };
    if let Some((message, at, href, label)) = fact {
        let _ = write!(
            html,
            "<p>{message}</p><p><a href={href}>{label} · observed at {at} (Unix seconds)</a></p>"
        );
    } else {
        html.push_str("<p>Snapshot observations unavailable for this prompt.</p>");
    }
    html.push_str("<p class=muted>From the snapshot · 0 tokens</p></article></div><p>Messaging unavailable.</p></section>");
}
