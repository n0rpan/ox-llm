//! The chat page: one header strip, one bubble thread, one sticky composer (#2341).
//!
//! Reading order is the page's job; the truth is not. Every piece of session text goes
//! through [`super::session`], which masks before it becomes markup. The fixed questions
//! keep their existing no-JS form and CSRF field, the owner message keeps the same POST,
//! and the action card keeps the same confirmation POST. The composer placeholder is the
//! only place that says whether messaging is on, so no state is invented as a positive.
use super::session::{self, Item};
use super::{escape, md, Prompt, View};
use crate::report_cmd::ScrubCtx;
use oxa_proto::Entry;
use std::fmt::Write;

const PROMPTS: [(Prompt, &str, &str); 3] = [
    (Prompt::Status, "status", "Status"),
    (Prompt::Workers, "workers", "Are workers on goal?"),
    (Prompt::Attention, "attention", "What needs me?"),
];

/// The newest request's context: the prompt tokens of the most recent reported usage.
fn context_tokens(entries: &[Entry]) -> Option<u32> {
    entries.iter().rev().find_map(|entry| match &entry.body {
        oxa_proto::EntryBody::Usage { usage, .. } if !usage.unreported => Some(usage.prompt),
        _ => None,
    })
}

/// The header strip's session switcher: the bound session as one popover line, never a
/// section above the transcript. Switching sessions is a user setting, not a request.
fn popover(html: &mut String, owner: Option<&super::OwnerChatView<'_>>, now: u64) {
    html.push_str("<details class=sessions><summary>Lead");
    match owner {
        Some(owner) => {
            if let Some(entries) = owner.entries {
                let working = entries
                    .last()
                    .is_some_and(|entry| entry.ts_ms / 1000 + session::RUNNING > now);
                let _ = write!(
                    html,
                    "<span class=chip>{}</span>",
                    if working { "running" } else { "idle" }
                );
                if let Some(tokens) = context_tokens(entries) {
                    let _ = write!(html, "<span class=\"num muted\">{tokens} tokens</span>");
                }
            }
        }
        None => {}
    }
    html.push_str("</summary><div class=sheet>");
    match owner {
        Some(owner) if owner.entries.is_some() => html
            .push_str("<p>The selected Lead Session. Choose another in <code>oxa.lead.sessionId</code>.</p>"),
        Some(_) => html.push_str("<p>The selected Lead Session's entries could not be read.</p>"),
        None => html.push_str(
            "<p>Owner chat is off: enable <code>oxa.lead.serve.owner_chat</code> to message lead.</p>",
        ),
    }
    html.push_str("</div></details>");
}

/// The chat page's one item page. The repository's owner goal lives at `/goal`, a page
/// outside this slice, so the chip opens only when that destination is more than the
/// one-line status Home shows. Until then it says where the goal is.
fn goal_chip(html: &mut String, view: &View<'_>) {
    match view.owner_goal {
        Some((goal, _)) => {
            let _ = write!(
                html,
                "<span class=\"chip quiet\">Goal {}</span>",
                escape(goal.status.as_str())
            );
        }
        None => {}
    }
}

/// The three fixed questions as chips, each a link to its answer at zero tokens.
fn prompt_chips(html: &mut String) {
    html.push_str("<div class=quick role=group aria-label=\"Snapshot questions\">");
    for (_, key, label) in PROMPTS {
        let _ = write!(html, "<a class=chip href=\"/chat?prompt={key}\">{label}</a>");
    }
    html.push_str("</div>");
}

/// The fixed questions as one no-JS form: the same CSRF field, the same POST, the same
/// `prompt` values the route already answers.
fn fixed_form(html: &mut String, view: &View<'_>) {
    let Some(csrf) = view.fixed_prompt_csrf else {
        return;
    };
    let _ = write!(
        html,
        "<form class=fixed-prompts method=post action=/chat><input type=hidden name=csrf value=\"{}\"><fieldset><legend>Ask about this snapshot · 0 tokens</legend>",
        escape(csrf)
    );
    for (_, key, label) in PROMPTS {
        let _ = write!(
            html,
            "<button class=chip name=prompt value={key}>{label}</button>"
        );
    }
    html.push_str("</fieldset></form>");
}

/// The fixed question's answer as a lead bubble, or its existing statement of what did
/// not reach this page. The answer itself is the quick reply's own text.
fn snapshot_bubble(html: &mut String, view: &View<'_>, prompt: Prompt, now: u64) {
    html.push_str("<div class=\"msg lead snapshot\"><div class=dg>From the snapshot · 0 tokens</div>");
    if let Some(reply) = crate::lead_chat::quick_reply(prompt, view.board, view.situation) {
        let _ = write!(html, "<div class=md>{}</div>", md::render(&reply.text));
        let _ = write!(
            html,
            "<div class=facts><a class=chip href=\"{}\">{}</a><span class=\"chip quiet\">observed {}</span></div><div class=by>answered without a model call</div></div>",
            escape(reply.href),
            escape(reply.source),
            super::human_time(reply.observed_at, now)
        );
    } else {
        let needed = match prompt {
            Prompt::Status => "board",
            Prompt::Workers | Prompt::Attention => "runner",
        };
        let _ = write!(
            html,
            "<div class=md><p>Snapshot observations unavailable for this prompt. This question needs a {needed} observation; none reached this page.</p></div></div>"
        );
    }
}

/// The opening lead bubble for a session with no entries: the three fixed answers as a
/// digest, or nothing at all when no observation reached this page.
fn digest(html: &mut String, view: &View<'_>, now: u64) {
    let answers: Vec<_> = PROMPTS
        .into_iter()
        .filter_map(|(prompt, _, _)| crate::lead_chat::quick_reply(prompt, view.board, view.situation))
        .collect();
    if answers.is_empty() {
        return;
    }
    html.push_str(
        "<div class=\"msg lead snapshot\"><div class=dg>Since you last looked · from the snapshot</div><ul>",
    );
    for reply in &answers {
        let _ = write!(html, "<li>{}</li>", escape(&reply.text));
    }
    html.push_str("</ul><div class=facts>");
    for reply in &answers {
        let _ = write!(
            html,
            "<a class=chip href=\"{}\">{}</a><span class=\"chip quiet\">observed {}</span>",
            escape(reply.href),
            escape(reply.source),
            super::human_time(reply.observed_at, now)
        );
    }
    html.push_str("</div><div class=by>0 tokens</div></div>");
}

/// The composer: the one owner message form, held above the nav over the thread. Its
/// states are said in words, and a message still posts the same form it always did.
fn composer(html: &mut String, view: &View<'_>) {
    html.push_str("<div class=composer2>");
    prompt_chips(html);
    match view.owner_chat.as_ref() {
        Some(owner) => {
            let _ = write!(
                html,
                "<form class=send method=post action=/chat><input type=hidden name=csrf value=\"{}\"><label class=sr for=lead-message>Message to lead</label><textarea id=lead-message name=message required rows=1 enterkeyhint=send autocomplete=off placeholder=\"Message lead…\"></textarea><button class=\"btn primary\" aria-label=Send><svg viewBox=\"0 0 24 24\" aria-hidden=true><path d=\"M5 12h13M13 6l6 6-6 6\"/></svg></button></form>",
                escape(owner.csrf)
            );
        }
        None => html.push_str(
            "<p class=muted>Chat is off: enable <code>oxa.lead.serve.owner_chat</code> to message lead.</p>",
        ),
    }
    html.push_str("</div>");
}

fn thread(html: &mut String, view: &View<'_>, prompt: Prompt, before: Option<[u8; 8]>, now: u64) {
    let ctx = ScrubCtx::default();
    html.push_str("<div class=thread role=log aria-label=\"Conversation with lead\">");
    let owner = view.owner_chat.as_ref();
    let rows: Vec<Item> = owner
        .and_then(|owner| owner.entries)
        .map(|entries| session::items(entries, &ctx))
        .unwrap_or_default();
    let bound = before.map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
    let (page, older) = session::page(rows, bound.as_deref());
    if older {
        if let Some(first) = page.first() {
            let _ = write!(
                html,
                "<a class=earlier href=\"/chat?before={}\">Show earlier messages</a>",
                escape(&first.id)
            );
        }
    }
    if page.is_empty() {
        digest(html, view, now);
    } else {
        let action: Option<(&str, &super::OwnerChatView<'_>)> =
            owner.and_then(|owner| owner.proposal.map(|proposal| (proposal.0.as_str(), owner)));
        session::render(html, &page, owner.and_then(|owner| owner.reply), action, now);
    }
    snapshot_bubble(html, view, prompt, now);
    html.push_str("</div>");
}

/// The whole page: header strip, thread, sticky composer. `before` opens an older page
/// of the same session; anything else about the request is ignored, as before.
pub(super) fn render(html: &mut String, view: &View<'_>, prompt: Prompt, before: Option<[u8; 8]>, now: u64) {
    html.push_str("<div class=chat>");
    html.push_str("<div class=chat-head>");
    popover(html, view.owner_chat.as_ref(), now);
    goal_chip(html, view);
    html.push_str("</div>");
    fixed_form(html, view);
    thread(html, view, prompt, before, now);
    composer(html, view);
    html.push_str("</div>");
}
