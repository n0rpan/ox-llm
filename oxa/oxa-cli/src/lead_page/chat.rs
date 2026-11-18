//! Fixed questions use the same observation as every page, without model calls or state.
use super::{Prompt, View};
use std::fmt::Write;

pub(super) fn render(html: &mut String, view: &View<'_>, prompt: Prompt) {
    html.push_str("<section><h2>Snapshot prompts</h2><p><a href=/chat?prompt=status>Status</a> · <a href=/chat?prompt=workers>Are workers on goal?</a> · <a href=/chat?prompt=attention>What needs me?</a></p><div role=log aria-label=\"Lead replies\"><article>");
    if let Some(reply) = crate::lead_chat::quick_reply(prompt, view.board, view.situation) {
        let _ = write!(
            html,
            "<p>{}</p><p><a href={}>{} · observed at {} (Unix seconds)</a></p>",
            reply.text, reply.href, reply.source, reply.observed_at
        );
    } else {
        html.push_str("<p>Snapshot observations unavailable for this prompt.</p>");
    }
    html.push_str("<p class=muted>From the snapshot · 0 tokens</p></article></div><p>Messaging unavailable.</p></section>");
}
