//! Fixed questions use the same observation as every page, without model calls or state.
use super::{Prompt, View};
use std::fmt::Write;

pub(super) fn render(html: &mut String, view: &View<'_>, prompt: Prompt, now: u64) {
    if let Some(csrf) = view.fixed_prompt_csrf {
        let _ = write!(html, "<form class=fixed-prompts method=post action=/chat><input type=hidden name=csrf value=\"{}\"><fieldset><legend>Ask about this observation</legend><button name=prompt value=status>Status</button><button name=prompt value=workers>Are workers on goal?</button><button name=prompt value=attention>What needs me?</button></fieldset></form>", super::escape(csrf));
    }
    html.push_str("<section><h2>Snapshot prompts</h2><p><a href=/chat?prompt=status>Status</a> · <a href=/chat?prompt=workers>Are workers on goal?</a> · <a href=/chat?prompt=attention>What needs me?</a></p><div role=log aria-label=\"Lead replies\"><article>");
    if let Some(reply) = crate::lead_chat::quick_reply(prompt, view.board, view.situation) {
        let _ = write!(
            html,
            "<p>{}</p><p><a href={}>{} · observed {}</a></p>",
            reply.text,
            reply.href,
            reply.source,
            super::human_time(reply.observed_at, now)
        );
    } else {
        let needed = match prompt {
            Prompt::Status => "board",
            Prompt::Workers | Prompt::Attention => "runner",
        };
        let _ = write!(html, "<p>Snapshot observations unavailable for this prompt. This question needs a {needed} observation; none reached this page.</p>");
    }
    html.push_str("<p class=muted>From the snapshot · 0 tokens</p></article></div><p>Messaging unavailable. This page supports snapshot questions only.</p></section>");
}
