//! Read-only web projection. Never serialize runner records or arbitrary GitHub payloads.
use oxa_proto::work::{PullRequest, Snapshot, Subject};
use oxa_work::board::{Action, Board};
use std::fmt::Write;

mod approvals;
mod board;
mod chat;
mod filters;
mod goal;
mod home;
mod item;
mod pages;
mod proposals;
mod quotas;
mod session;
mod ui;
mod workers;
pub use filters::{Filter, Query};
pub use pages::{Page, Prompt};

/// One protected stuck-work observation, decoded from the Reader's record. `None`
/// on a field means that source was not observed; an absent source is never
/// rendered healthy or alarming, and the page says so explicitly (#1391).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Alarms {
    /// Main required-CI failure at the observed head; optional job failures,
    /// pending states and unobserved requirements stay quiet (#1634).
    pub main_red: Option<bool>,
    /// Verified-merge stall at the record's observation time. `None` when no
    /// verified merge establishes the age.
    pub main_merge_stalled: Option<bool>,
    /// Stalled queued-run ids from one complete fresh observation.
    pub stalled_runs: Option<Vec<u64>>,
    /// Merge-queue membership with exact heads; `None` when unobserved.
    pub queue: Option<Vec<PullRequest>>,
    /// Queue-age alarms the landing policy raised, bound to board heads.
    pub overdue_queue: Option<Vec<PullRequest>>,
}

impl Alarms {
    /// The single rule for a queue-age alarm: the alarm intersected with queue
    /// membership at the exact (number, head). `None` when either source is
    /// unobserved, so neither the page nor the loop can act on half a record.
    pub fn overdue_in_queue(&self) -> Option<Vec<&PullRequest>> {
        let (queue, overdue) = (self.queue.as_deref()?, self.overdue_queue.as_deref()?);
        Some(queue.iter().filter(|pr| overdue.contains(pr)).collect())
    }
}

/// Borrowed presentation inputs, not a second runner state store. None means unavailable.
#[derive(Default)]
pub struct View<'a> {
    /// Ephemeral form credential supplied only by the authorized HTTP caller.
    pub owner_chat: Option<OwnerChatView<'a>>,
    pub fixed_prompt_csrf: Option<&'a str>,
    /// Canonical goal and matching native observation from the authenticated caller.
    /// The caller retains full SessionRef/revision authority; identities are never rendered.
    pub owner_goal: Option<(&'a oxa_session::Goal, oxa_core::goal::GoalObservation<'a>)>,
    pub board: Option<&'a Board>,
    /// Why the latest board read failed (#2342), from the allow-listed `BoardCause`.
    /// `None` on a fresh read; present when the board is missing or retained last-good.
    pub board_cause: Option<crate::lead_input::BoardCause>,
    /// True when the board shown is a retained last-good value, not a fresh read (#2342).
    pub board_retained: bool,
    pub situation: Option<&'a Snapshot>,
    /// The owner's configured per-host worker capacity (#2369), read live from the
    /// user settings. `None` when the settings cannot be read (shown unavailable);
    /// an empty map means the owner stated no capacity.
    pub capacity: Option<&'a crate::lead::capacity::Capacity>,
    /// The Lead session's workers from its registry and their status files. An error is
    /// an explicit state: no workers recorded, or a registry that was refused.
    pub workers: Option<&'a Result<Vec<crate::workers::tool::Binding>, crate::workers::tool::BindingsError>>,
    /// Optional GitHub work the owner's App reserve deferred in the latest sample.
    pub quota_deferred: &'a [oxa_work::shadow::Deferral],
    /// Open owner asks read live from the authorized durable Lead Session, with
    /// their original revisions. Distinct from the runner situation: an absent
    /// runner source never hides these, and vice versa.
    pub durable_asks: Option<&'a [oxa_session::OpenGoalAsk]>,
    /// C's selected checkpoint observation; the renderer never acquires runner state.
    pub selected_task_usage: Option<&'a oxa_work::Result<Option<oxa_work::TaskUsage>>>,
    pub github_quotas: Option<&'a crate::lead_quota::GitHubQuotas>,
    pub model_plan: Option<&'a oxc_gateway::status::chatgpt_usage::Usage>,
    /// Stuck-work signals from one protected observation. `None` when no alarm
    /// record reached this page.
    pub alarms: Option<&'a Alarms>,
    /// Throughput and stuck-work projection of the same observation (#2346),
    /// built once by the caller; `None` when no board sample reached this page.
    pub work: Option<&'a crate::lead_projection::Work>,
    /// Source issue and its reported checkbox states; these do not establish validation.
    pub release: Option<(u64, &'a [(bool, String)])>,
    /// Serving process self-image digest, supplied by the authenticated native caller.
    pub running_sha256: Option<&'a str>,
    /// True only when this listener supplies the agreed authenticated live route.
    pub live_events: bool,
    /// Requests the approval desk still holds for the owner; None when the desk cannot be read.
    pub approvals: Option<&'a [oxa_perm::ApprovalRecord]>,
    /// The owner form credential for answering approvals; present only with owner chat.
    pub approval_csrf: Option<&'a str>,
    /// The running owner session this chat page is attached to (#2075); replaces the Lead chat.
    pub attached: Option<AttachedView<'a>>,
    /// The owner's running sessions to choose from, on the chat page only (#1469).
    pub sessions: Option<SessionListView<'a>>,
}

/// The sessions of this user that are running now, as the owner's chat page lists them.
pub struct SessionListView<'a> {
    pub csrf: &'a str,
    pub rows: &'a [SessionRow],
    /// Label of the button that returns to the start-up target, when the page is not on it.
    pub back: Option<&'static str>,
}

pub struct SessionRow {
    pub id: String,
    /// The project directory's basename only.
    pub cwd_name: String,
    pub modified_ms: u64,
    pub attached: bool,
}

/// The attached session as the page shows it: the newest part of its transcript, whether it is
/// running, and the owner form credential for sending through its inbox.
pub struct AttachedView<'a> {
    pub csrf: &'a str,
    pub entries: &'a [oxa_proto::Entry],
    /// Older history exists in the session file but is not shown.
    pub truncated: bool,
    pub live: bool,
    pub short_id: String,
    /// The full id the message form names, so a stale page cannot send to another session.
    pub id: String,
    /// A message was accepted since the last render.
    pub sent: bool,
    /// `working` or `idle`, from the newest message; only meaningful while running.
    pub activity: Option<&'static str>,
    /// Prompt tokens of the newest request with recorded usage.
    pub context_tokens: Option<u32>,
}

pub struct OwnerChatView<'a> {
    pub csrf: &'a str,
    pub(crate) reply: Option<&'a crate::lead_chat::conversation::ConversationReply>,
    pub proposal: Option<&'a (String, serde_json::Value)>,
    pub confirmed: bool,
}

pub(crate) fn escape(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '&' => "&amp;".into(),
            '<' => "&lt;".into(),
            '>' => "&gt;".into(),
            '"' => "&quot;".into(),
            '\'' => "&#39;".into(),
            c if c.is_control() => " ".into(),
            c => c.to_string(),
        })
        .collect()
}

/// Presentation follows the serving host's zone; native observations retain UTC timestamps.
pub(crate) fn human_time(timestamp: u64, now: u64) -> String {
    let local = |value| i64::try_from(value).ok().and_then(oxa_session::local_time);
    let Some(at) = local(timestamp) else {
        return "time not available (the host clock cannot format this observation)".into();
    };
    let date = if local(now).is_some_and(|today| today.date == at.date) {
        String::new()
    } else {
        format!("{} ", at.date)
    };
    escape(&format!("{date}{} {}", at.hm, at.zone_label()))
}

/// S3 (#2342): the age of a value as a client-ticking `<time data-at>` element.
/// The server renders an initial label; live.js re-labels it every 10 s with no
/// request, so a quiet page never shows a stale age. Like `human_time`, an
/// observation the host clock cannot format shows no raw value, only a reason.
pub(crate) fn age(at: u64, now: u64) -> String {
    if i64::try_from(at).ok().and_then(oxa_session::local_time).is_none() {
        return "age not available (the host clock cannot format this observation)".into();
    }
    format!("<time data-at=\"{at}\">{}</time>", escape(&age_text(at, now)))
}

fn age_text(at: u64, now: u64) -> String {
    if at > now.saturating_add(60) {
        return "ahead of this clock".into();
    }
    let delta = now.saturating_sub(at);
    if delta < 60 {
        format!("{delta} s old")
    } else if delta < 3600 {
        format!("{} min old", delta / 60)
    } else if delta < 86400 {
        format!("{} h old", delta / 3600)
    } else {
        format!("{} d old", delta / 86400)
    }
}

/// Capitalize the first character of an allow-listed cause or hint so it reads as
/// a sentence; the rest of the fixed text is never altered.
pub(crate) fn sentence(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// S3 (#2342): one missing observation state. Names the source, the allow-listed
/// cause, the age of the last good value (ticking client-side) and the next step.
/// Never a bare "unavailable", never a raw diagnostic, path or file content.
pub(crate) fn say_why(
    html: &mut String,
    source: &str,
    cause: &str,
    next: &str,
    last_good_at: Option<u64>,
    now: u64,
) {
    let _ = write!(
        html,
        "<p class=\"muted say-why\"><b>{} unavailable.</b> {}.",
        escape(source),
        escape(&sentence(cause))
    );
    match last_good_at {
        Some(at) => {
            html.push_str(" Last good value ");
            html.push_str(&age(at, now));
            html.push('.');
        }
        None => html.push_str(" No good value has been read yet."),
    }
    let _ = write!(html, " {}.</p>", escape(&sentence(next)));
}

/// The allow-listed cause and next step for a board gap (#2342). A view with no
/// cause (no read attempted) keeps the generic observation-gap text.
pub(crate) fn board_gap(view: &View<'_>) -> (&'static str, &'static str) {
    match view.board_cause {
        Some(cause) => (cause.text(), cause.hint()),
        None => (
            "no board observation reached this page",
            "rerun the Lead loop to read a board observation",
        ),
    }
}

fn action(value: Action) -> &'static str {
    match value {
        Action::FixFindings => "Fix findings",
        Action::FixRequiredCi => "Fix required CI",
        Action::ResolveConflict => "Resolve conflict",
        Action::AddRemoves => "Add removal rationale",
        Action::BuildIssue => "Build approved issue",
        Action::PromoteDraft => "Promote green draft",
        Action::Ready => "Ready to land",
        Action::WaitRequiredCi => "Wait for required CI",
        Action::WaitMergeability => "Wait for mergeability",
        Action::WaitReview => "Wait for review",
        Action::WaitIssue => "Wait for issue intake",
        Action::EvidenceIncomplete => "Evidence incomplete",
    }
}
fn subject(value: &Subject) -> String {
    match value {
        Subject::Repository => "Repository".into(),
        Subject::PullRequest(n) => format!("PR #{n}"),
        Subject::Task(id) => format!("Task {}", escape(id)),
        Subject::Host(_) => "Worker".into(),
        Subject::Inbox(_) => "Inbox".into(),
    }
}

/// Full server-rendered page; scripts/styles are same-origin resources, without a CDN.
pub fn document(view: &View<'_>, now: u64) -> String {
    document_for(Page::Home, view, now)
}

pub fn document_for(page: Page, view: &View<'_>, now: u64) -> String {
    let live = if view.live_events {
        "<meta name=oxa-live-events content=1>"
    } else {
        ""
    };
    let digest = view
        .running_sha256
        .filter(|value| {
            value.len() == 64
                && value
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        })
        .map_or_else(String::new, |digest| {
            format!("<meta name=oxa-running-sha256 content=\"{digest}\">")
        });
    let refresh = if page == Page::Home {
        "<noscript><meta http-equiv=refresh content=60></noscript>"
    } else {
        ""
    };
    format!("<!doctype html><html lang=en><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1,viewport-fit=cover\"><meta name=color-scheme content=\"light dark\">{live}{digest}<title>oxa lead</title><link rel=stylesheet href={style}><script src={script} defer></script>{refresh}<a class=skip href=#main>Skip to content</a><header class=top><a href=/>oxa <b>lead</b></a><span class=age>{}<span id=connection>Snapshot</span></span>{}</header>{}</html>", ui::dot(), ui::theme_toggle(), content_for(page, view, now), style = crate::lead_assets::style_path(), script = crate::lead_assets::script_path())
}

/// Allow-listed HTML fragment, also used by SSE. No worktrees, sessions, errors or headers.
pub fn content(view: &View<'_>, now: u64) -> String {
    content_for(Page::Home, view, now)
}

pub fn content_for(page: Page, view: &View<'_>, now: u64) -> String {
    let title = match page {
        Page::Item(number) => format!("Item #{number}"),
        _ => page.title().into(),
    };
    let mut html = format!("<main><h1 id=main tabindex=-1>{title}</h1>");
    html.push_str(&page.navigation(view.situation.map(|snapshot| snapshot.asks.len())));
    html.push_str("<aside id=refresh-error class=stale-notice role=status hidden><h2>Refresh unavailable</h2><p>The last rendered observations are retained. <a href=\"\">Retry</a></p></aside>");
    if view.board_retained {
        if let (Some(board), Some(cause)) = (view.board, view.board_cause) {
            let _ = write!(
                html,
                "<aside class=\"stale-notice say-why\" role=status><h2>Board is last-good</h2><p>{}. Last good value {}. {} <a href=\"\">Retry</a></p></aside>",
                escape(&sentence(cause.text())),
                age(board.observed_at, now),
                escape(&sentence(cause.hint()))
            );
        }
    } else if view.board.is_some_and(|board| stale(board, now)) {
        html.push_str("<aside class=stale-notice role=status><h2>Board observation needs refreshing</h2><p>The Board timestamp is stale or ahead of this clock. <a href=\"\">Retry</a></p></aside>");
    }
    match page {
        Page::Home => {
            home::banner(&mut html, view, now);
            html.push_str("<div class=home-layout><div class=home-decisions>");
            attention(&mut html, view, now);
            approvals::render(&mut html, view, now);
            asks(&mut html, view, now);
            html.push_str("</div><div class=home-overview>");
            goal::row(&mut html, view, now);
            home::tiles(&mut html, view, now);
            crate::lead_projection::render(&mut html, view, now);
            workers::render(&mut html, view, now);
            capacity(&mut html, view);
            html.push_str("<details class=home-work><summary>Observed work</summary>");
            board::render(&mut html, view, now, Query::default());
            queue(&mut html, view, now);
            release(&mut html, view, now);
            html.push_str("</details></div></div>");
        }
        Page::Goal => goal::render(&mut html, view, now),
        Page::Board | Page::BoardQuery(_) => {
            let query = if let Page::BoardQuery(query) = page {
                query
            } else {
                Query::default()
            };
            approvals::render(&mut html, view, now);
            board::render(&mut html, view, now, query);
            queue(&mut html, view, now);
        }
        Page::Workers => {
            capacity(&mut html, view);
            workers::render(&mut html, view, now);
        }
        Page::Asks => {
            html.push_str("<div class=asks-layout>");
            asks(&mut html, view, now);
            html.push_str("</div>");
        }
        Page::Item(number) => item::render(&mut html, view, number, now),
        Page::Chat(prompt) => chat::render(&mut html, view, prompt, now),
        Page::Usage => {
            usage(&mut html, view, now);
            quotas::render(&mut html, view, now);
        }
    }
    html.push_str("</main>");
    html
}

/// The single alarm decision shared by the attention rows and the Home banner
/// (#2345): whether a protected observation raises an alarm, and which alarm
/// sources were not observed. An absent source is never healthy (#1391, #2342).
pub(super) fn alarm_state(view: &View<'_>) -> (bool, Vec<&'static str>) {
    let alarms = view.alarms;
    let overdue = alarms.and_then(Alarms::overdue_in_queue).unwrap_or_default();
    let alarm = alarms.and_then(|a| a.main_red) == Some(true)
        || alarms.and_then(|a| a.main_merge_stalled) == Some(true)
        || alarms
            .and_then(|a| a.stalled_runs.as_deref())
            .is_some_and(|runs| !runs.is_empty())
        || !overdue.is_empty();
    let missing = alarms.map_or(Vec::new(), |a| {
        let mut names = Vec::new();
        if a.main_red.is_none() {
            names.push("main checks");
        }
        if a.main_merge_stalled.is_none() {
            names.push("no-merge alarm");
        }
        if a.queue.is_none() {
            names.push("merge queue membership");
        }
        if a.overdue_queue.is_none() {
            names.push("queue age");
        }
        if a.stalled_runs.is_none() {
            names.push("queued workflow runs");
        }
        names
    });
    (alarm, missing)
}

fn attention(html: &mut String, view: &View<'_>, now: u64) {
    let asks = view.situation.map_or(0, |s| s.asks.len()) + view.durable_asks.map_or(0, |asks| asks.len());
    let alarms = view.alarms;
    // The queue-age alarm binds each PR to its exact observed head: membership
    // and landing age must come from the same protected record.
    let overdue: Vec<&PullRequest> = alarms.and_then(Alarms::overdue_in_queue).unwrap_or_default();
    let main_red = alarms.and_then(|a| a.main_red);
    let merge_stalled = alarms.and_then(|a| a.main_merge_stalled);
    let stalled_runs = alarms.and_then(|a| a.stalled_runs.as_deref());
    // The banner and these rows share one alarm decision. #2342: the missing
    // sources collapse into one line naming them, with the cause, the age of the
    // last good board value and the next step, instead of one bullet per source.
    // A page with no alarm record at all says nothing here; the missing Board
    // section carries it.
    let (alarm, missing) = alarm_state(view);
    if alarm || asks > 0 || !missing.is_empty() {
        let (role, icon, label) = if alarm {
            ("alert", "!", "Alarm")
        } else if asks > 0 {
            ("status", "?", "Needs you")
        } else {
            ("status", "?", "Unavailable")
        };
        let _ = write!(
            html,
            "<section id=attention role={role}><p class=severity><span aria-hidden=true>{icon}</span> {label}</p><h2>Needs attention</h2><ul>"
        );
        if asks > 0 {
            let _ = write!(
                html,
                "<li><a href=#asks>Unanswered owner decisions ({asks})</a></li>"
            );
        }
        if main_red == Some(true) {
            html.push_str("<li>Checks on main are failing.</li>");
        }
        if merge_stalled == Some(true) {
            html.push_str("<li>No PR merged into main for 45 minutes or more at the last observation.</li>");
        }
        for pr in overdue {
            let _ = write!(
                html,
                "<li><a href=/board#queue>PR #{} has waited too long in the merge queue</a></li>",
                pr.number
            );
        }
        if let Some(runs) = stalled_runs.filter(|runs| !runs.is_empty()) {
            let _ = write!(
                html,
                "<li><details><summary>Workflow runs queued for more than 15 minutes ({})</summary><ul>",
                runs.len()
            );
            for id in runs {
                let _ = write!(html, "<li>Run {id}</li>");
            }
            html.push_str("</ul></details></li>");
        }
        html.push_str("</ul>");
        if !missing.is_empty() {
            let _ = write!(
                html,
                "<p class=\"muted say-why\"><b>Not in this sample:</b> {}. The latest board sample did not carry them.",
                escape(&missing.join(", "))
            );
            match view.board.map(|b| b.observed_at) {
                Some(at) => {
                    html.push_str(" Last good value ");
                    html.push_str(&age(at, now));
                    html.push('.');
                }
                None => html.push_str(" No good value has been read yet."),
            }
            html.push_str(" Refresh to read a newer sample.</p>");
        }
        if alarm {
            html.push_str("<p class=muted>Lead response observations unavailable. These observations report the alarm, without a Lead response.</p>");
        }
        html.push_str("</section>");
    }
}

pub(crate) fn stale(board: &Board, now: u64) -> bool {
    board.observed_at > now.saturating_add(60) || now.saturating_sub(board.observed_at) > 900
}

fn usage(html: &mut String, view: &View<'_>, now: u64) {
    html.push_str("<section id=task-usage><h2>Task usage</h2>");
    match view.selected_task_usage {
        None => say_why(
            html,
            "Task usage observations",
            "no selected-task usage observation reached this page",
            "select a task and rerun the Lead loop to read its checkpoint",
            None,
            now,
        ),
        Some(Err(_)) => say_why(
            html,
            "Task usage observations",
            "the selected task usage could not be read",
            "rerun the Lead loop to read the checkpoint again",
            None,
            now,
        ),
        Some(Ok(None)) => html.push_str("<p>No saved checkpoint for the selected task.</p>"),
        Some(Ok(Some(usage))) => {
            let _ = write!(
                html,
                "<article><h3>Task {}</h3><p class=muted>Checkpoint revision {}</p>",
                escape(&usage.task),
                usage.checkpoint_revision
            );
            if let Some(tokens) = &usage.task_tokens {
                let count =
                    |value: Option<u64>| value.map_or_else(|| "Unknown".into(), |value| value.to_string());
                let _ = write!(html,
                    "<p>Usage coverage: {}</p><dl><dt>Task attempts</dt><dd>{}</dd><dt>Attempts with complete usage</dt><dd>{}</dd><dt>Prompt tokens</dt><dd>{}</dd><dt>Completion tokens</dt><dd>{}</dd><dt>Cached input tokens</dt><dd>{}</dd></dl><p class=muted>Cached tokens are included in prompt when both are reported.</p>",
                    if tokens.complete { "complete" } else { "partial" }, tokens.attempts, tokens.complete_attempts,
                    count(tokens.prompt), count(tokens.completion), count(tokens.cached));
            } else {
                html.push_str("<p>Saved token aggregate unknown. The checkpoint has no token aggregate; this does not mean zero usage.</p>");
            }
            html.push_str(
                "<p class=muted>Task-scoped checkpoint; merged-PR attribution unavailable. This checkpoint does not link usage to merged PRs.</p></article>",
            );
        }
    }
    html.push_str("</section>");
}

fn capacity(html: &mut String, view: &View<'_>) {
    html.push_str("<section id=capacity><h2>Capacity</h2>");
    let Some(configured) = view.capacity else {
        html.push_str("<p>Capacity unavailable. The owner's user settings could not be read.</p></section>");
        return;
    };
    let facts = crate::lead::capacity::derive(configured, view.board, view.situation);
    if facts.hosts.is_empty() {
        html.push_str("<p>No per-host capacity configured. Set <code>oxa.lead.capacity</code> in user settings, for example <code>{\"host-a\": 6}</code>.</p>");
    } else {
        html.push_str("<p class=muted>running workers / configured capacity</p><ul>");
        for row in &facts.hosts {
            let used = row.used.map_or("unknown".to_owned(), |n| n.to_string());
            let cap = row.capacity.map_or("unknown".to_owned(), |n| n.to_string());
            let _ = write!(html, "<li>{} {used}/{cap}</li>", escape(&row.host));
        }
        html.push_str("</ul>");
        if configured.is_empty() {
            html.push_str("<p class=muted>Observed use only; no per-host capacity is configured.</p>");
        }
    }
    if configured.ignored() > 0 {
        let _ = write!(
            html,
            "<p class=muted>{} capacity entries could not be read and were ignored.</p>",
            configured.ignored()
        );
    }
    match facts.ready {
        Some(n) => {
            let _ = write!(html, "<p>Ready: {n} items</p>");
        }
        None => html.push_str("<p>Ready depth unavailable. No board observation reached this page.</p>"),
    }
    match &facts.stuck {
        Some(list) if list.is_empty() => html.push_str("<p>Stuck: none reported.</p>"),
        Some(list) => {
            html.push_str("<p>Stuck:</p><ul>");
            for label in list {
                let _ = write!(html, "<li>{}</li>", escape(label));
            }
            html.push_str("</ul>");
        }
        None => html.push_str("<p>Stuck work unavailable. No runner observation reached this page.</p>"),
    }
    html.push_str("</section>");
}

fn queue(html: &mut String, view: &View<'_>, now: u64) {
    html.push_str("<section id=queue><h2>Merge queue</h2>");
    if let Some(queue) = view.alarms.and_then(|a| a.queue.as_deref()) {
        for (index, pr) in queue.iter().enumerate() {
            let _ = write!(html, "<article>{}. PR #{}</article>", index + 1, pr.number);
        }
        if queue.is_empty() {
            html.push_str("<p>Queue empty at observation.</p>");
        }
    } else {
        say_why(
            html,
            "Merge queue",
            "no queue observation reached this page; ready to land does not establish queue membership",
            "rerun the Lead loop to read a fresh sample",
            None,
            now,
        );
    }
    html.push_str("</section>");
}

fn release(html: &mut String, view: &View<'_>, now: u64) {
    html.push_str("<section id=release><h2>Release checklist</h2>");
    if let Some((number, items)) = view.release {
        let _ = write!(html, "<p class=muted>Issue #{number} · {} / {} checkboxes reported checked.</p><p class=muted>Owner outcome verification unavailable. This checklist does not include release or installability evidence.</p><ul>", items.iter().filter(|i| i.0).count(), items.len());
        for (checked, text) in items {
            let _ = write!(
                html,
                "<li>{} {}</li>",
                if *checked { "☑" } else { "☐" },
                escape(text)
            );
        }
        html.push_str("</ul>");
    } else {
        say_why(
            html,
            "Release checklist",
            "no release issue and checklist were supplied",
            "supply a release issue with its checklist to show it here",
            None,
            now,
        );
    }
    html.push_str("</section>");
}

fn asks(html: &mut String, view: &View<'_>, now: u64) {
    html.push_str("<section id=asks><h2>For your decision</h2>");
    if let Some(s) = view.situation {
        let _ = write!(
            html,
            "<p class=muted>Runner facts · observed {}</p>",
            human_time(s.at_ms / 1000, now)
        );
        html.push_str("<div class=ask-cards>");
        for ask in &s.asks {
            let _ = write!(html, "<article class=\"card ask\"><p class=\"muted ask-meta\">Ask {} · {}</p><h3>{}</h3><p class=ask-unknown>Options, recommendation and meanwhile are unavailable in these facts.</p></article>", escape(&ask.id), subject(&ask.subject), escape(&ask.question));
        }
        html.push_str("</div>");
        if s.asks.is_empty() {
            html.push_str("<p>No open asks in these runner facts.</p>");
        }
    } else if view.durable_asks.is_some() {
        say_why(
            html,
            "Runner asks",
            "no runner observation reached this page; the Lead Session asks below are a separate source",
            "rerun the Lead loop to read a runner situation",
            None,
            now,
        );
    } else {
        say_why(
            html,
            "Owner asks",
            "no runner observation reached this page and no Lead Session asks reached it",
            "rerun the Lead loop to read open decisions",
            None,
            now,
        );
    }
    if let Some(durable) = view.durable_asks {
        html.push_str("<p class=muted>Lead Session asks · read live from the authorized durable Session</p>");
        html.push_str("<div class=ask-cards>");
        for ask in durable {
            let _ = write!(
                html,
                "<article class=\"card ask\"><p class=\"muted ask-meta\">Ask {} · revision {}</p><h3>{}</h3><p class=ask-unknown>Options, recommendation and meanwhile are unavailable in these facts; this page grants no permission to resolve an ask.</p></article>",
                escape(&ask.id),
                ask.revision,
                escape(&ask.question)
            );
        }
        html.push_str("</div>");
        if durable.is_empty() {
            html.push_str("<p>No open asks in the Lead Session.</p>");
        }
    }
    html.push_str("</section>");
}

pub const STYLE: &str = concat!(
    include_str!("lead_page/style.css"),
    r#"
/* Board phone rows; the desktop table stays on a wide screen. */
.board-rows{display:none;list-style:none;margin:8px 0;padding:0;gap:8px}
.board-rows li{padding:12px;border:1px solid var(--line);border-radius:var(--r);background:var(--surface)}
.board-rows .row-head{display:flex;justify-content:space-between;align-items:center;gap:8px}
.board-rows dl{display:grid;grid-template-columns:auto 1fr;gap:4px 12px;margin:8px 0 0}
.board-rows dt{color:var(--muted);font-size:var(--t-sm)}.board-rows dd{margin:0}
@media(max-width:899.98px){.board-rows{display:grid}#board .scroll{display:none}}
/* Workers host groups. */
#lanes .host{margin:14px 0 6px;font-size:var(--t-md);color:var(--muted)}
/* Item detail list. */
.item-detail{display:grid;grid-template-columns:auto 1fr;gap:4px 12px;margin:8px 0 0}
.item-detail dt{color:var(--muted);font-size:var(--t-sm)}.item-detail dd{margin:0}
"#
);

pub const SCRIPT: &str = r#"const themeBtn=document.getElementById('theme');
if(themeBtn){
  const root=document.documentElement;
  let saved=null;try{saved=localStorage.getItem('lead-theme')}catch(e){}
  if(saved==='light'||saved==='dark')root.dataset.theme=saved;
  const dark=()=>root.dataset.theme?root.dataset.theme==='dark':matchMedia('(prefers-color-scheme:dark)').matches;
  themeBtn.setAttribute('aria-pressed',String(dark()));
  themeBtn.addEventListener('click',()=>{
    const next=dark()?'light':'dark';root.dataset.theme=next;
    themeBtn.setAttribute('aria-pressed',String(next==='dark'));
    try{localStorage.setItem('lead-theme',next)}catch(e){}
  });
}
const badge=document.getElementById('connection');
const live=!!document.querySelector('meta[name=oxa-live-events]');
const digest=value=>typeof value==='string'&&/^[0-9a-f]{64}$/.test(value);
let runningSha=document.querySelector('meta[name=oxa-running-sha256]')?.content;
if(!digest(runningSha))runningSha=null;
let timer,pending,stream,reconnectTimer,streamOk=false,reloadNeeded=false;
let ageTimer=null;
function ageText(at,now){if(at>now+60)return 'ahead of this clock';const d=Math.max(0,now-at);if(d<60)return d+' s old';if(d<3600)return Math.floor(d/60)+' min old';if(d<86400)return Math.floor(d/3600)+' h old';return Math.floor(d/86400)+' d old';}
function tickAges(){if(typeof document.querySelectorAll!=='function')return;const now=Math.floor(Date.now()/1000);document.querySelectorAll('time[data-at]').forEach(el=>{const at=Number(el.getAttribute('data-at'));if(Number.isFinite(at))el.textContent=ageText(at,now);});}
function scheduleAges(){if(typeof document.querySelectorAll!=='function')return;tickAges();ageTimer=setTimeout(()=>{ageTimer=null;if(!document.hidden)scheduleAges();},10000);}
function unavailable(){
  badge.textContent='Refresh unavailable';
  const notice=document.getElementById('refresh-error');if(notice)notice.hidden=false;
}
function reloadWhenIdle(){
  clearTimeout(timer);if(document.hidden)return;
  if(readerActive()||document.querySelector('#lead-message')?.value){
    badge.textContent='Update ready';timer=setTimeout(reloadWhenIdle,15000);
  }else location.reload();
}
function readerActive(){return document.querySelector('main').contains(document.activeElement)&&document.activeElement.matches('a,button,input,textarea,select,summary,[tabindex]');}
async function refresh(){
  clearTimeout(timer);
  if(document.hidden||(stream&&!streamOk))return;
  if(reloadNeeded){reloadWhenIdle();return;}
  if(readerActive()){timer=setTimeout(refresh,15000);return;}
  if(pending)pending.abort();
  const active=new AbortController();pending=active;
  const target=new URL(location.href);target.searchParams.set('fragment','1');
  try{
    const response=await fetch(target,{cache:'no-store',signal:active.signal});
    if(!response.ok)throw new Error('refresh refused');
    const doc=new DOMParser().parseFromString(await response.text(),'text/html');
    const next=doc.querySelector('main');
    if(!next)throw new Error('refresh unavailable');
    if(pending===active&&!active.signal.aborted&&!document.hidden&&!readerActive()&&(!stream||streamOk)){
      const current=document.querySelector('main'),work=current.querySelector('details.home-work');
      const replacement=next.querySelector('details.home-work');
      if(work&&replacement)replacement.open=work.open;
      const draft=current.querySelector('#lead-message');
      if(draft?.value){
        const nextDraft=next.querySelector('#lead-message');
        if(!nextDraft)return;
        nextDraft.value=draft.value;
      }
      current.replaceWith(next);badge.textContent='Connected';
    }
  }catch(error){if(!active.signal.aborted)unavailable();}
  finally{if(pending===active){pending=null;if(!document.hidden&&(!stream||readerActive()))timer=setTimeout(refresh,15000);}}
}
function connect(){
  stream=new EventSource('/events/live');streamOk=false;
  const activeStream=stream;
  const waitForEvent=()=>{reconnectTimer=null;if(stream===activeStream&&!document.hidden&&!streamOk)unavailable();};
  reconnectTimer=setTimeout(waitForEvent,15000);
  stream.addEventListener('lead-update',async event=>{
    if(document.hidden||stream!==activeStream)return;
    let value;
    try{
      if(typeof event.data!=='string'||event.data.length>128||/[\r\n]/.test(event.data))throw new Error();
      value=JSON.parse(event.data);
      if(!value||Array.isArray(value)||Object.keys(value).length!==1||!Object.prototype.hasOwnProperty.call(value,'running_sha256')||
        (value.running_sha256!==null&&!digest(value.running_sha256)))throw new Error();
    }catch{streamOk=false;unavailable();return;}
    streamOk=true;clearTimeout(reconnectTimer);reconnectTimer=null;
    if(value.running_sha256!==null){
      if(runningSha&&runningSha!==value.running_sha256)reloadNeeded=true;
      if(!reloadNeeded)runningSha=value.running_sha256;
    }
    if(reloadNeeded)reloadWhenIdle();else await refresh();
  });
  stream.onerror=()=>{
    if(stream!==activeStream)return;
    streamOk=false;if(!reconnectTimer)reconnectTimer=setTimeout(waitForEvent,15000);
  };
}
document.addEventListener('visibilitychange',()=>{
  clearTimeout(timer);clearTimeout(reconnectTimer);reconnectTimer=null;if(pending)pending.abort();
  if(ageTimer){clearTimeout(ageTimer);ageTimer=null;}
  if(stream){stream.close();stream=null;streamOk=false;}
  if(!document.hidden){if(live&&typeof EventSource==='function')connect();else refresh();scheduleAges();}
});
if(live&&typeof EventSource==='function')connect();else timer=setTimeout(refresh,15000);
scheduleAges();
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use oxa_proto::work::IncidentKind;
    use oxa_work::board::{Entry, GitHub, Mergeability, Policy, PullRequest as BoardPr, RequiredCi};

    fn observed_asks() -> Snapshot {
        include_str!("../../oxa-proto/tests/fixtures/work/evening.jsonl")
            .lines()
            .filter_map(|line| Snapshot::from_json(line.as_bytes()).ok())
            .find(|snapshot| !snapshot.asks.is_empty())
            .unwrap()
    }

    #[test]
    fn timestamps_use_the_host_zone_on_every_page() {
        const CHILD: &str = "OXA_TEST_LEAD_TIME_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "lead_page::tests::timestamps_use_the_host_zone_on_every_page",
                    "--nocapture",
                ])
                .env_clear()
                .env(CHILD, "1")
                .env("TZ", "CET-1")
                .env("RUST_TEST_THREADS", "1")
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(stdout.contains("running 1 test"), "{stdout}");
            assert!(output.status.success(), "{stdout}");
            return;
        }
        let mut situation = observed_asks();
        situation.at_ms = 0;
        let mut board = Board {
            observed_at: 0,
            entries: vec![Entry {
                number: 17,
                head: "synthetic-head".into(),
                release_pinned: false,
                action: Action::Ready,
                lease: Some(oxa_work::board::Lease {
                    holder: "worker-example".into(),
                    expires_at: 3600,
                    claimed_at: None,
                    generation: (0, 1),
                }),
                attempts_today: 0,
                eligible: true,
                evidence_incomplete: None,
            }],
        };
        let mut quota = crate::lead_quota::GitHubQuotas::default();
        assert!(quota.observe(
            crate::lead_quota::GitHubIdentity::App,
            crate::lead_quota::GitHubResource::Core,
            "x-ratelimit-resource: core\nx-ratelimit-limit: 10\nx-ratelimit-remaining: 7\nx-ratelimit-reset: 3600",
        ));
        let model = oxc_gateway::status::chatgpt_usage::parse(&serde_json::json!({
            "rate_limit": {"primary_window": {"used_percent": 0, "reset_at": 3600}}
        }));
        let (goal, reference) = owner_goal_fixture();
        let observation = oxa_core::goal::GoalObservation {
            session: &reference,
            revision: 7,
            status: Some(goal.status),
        };
        let view = View {
            owner_goal: Some((&goal, observation)),
            board: Some(&board),
            situation: Some(&situation),
            github_quotas: Some(&quota),
            model_plan: Some(&model),
            ..Default::default()
        };
        for page in [
            Page::Home,
            Page::Board,
            Page::Workers,
            Page::Asks,
            Page::Item(17),
            Page::Chat(Prompt::Status),
        ] {
            let html = document_for(page, &view, 100);
            assert!(html.contains("01:00 CET"), "{page:?}");
            assert!(
                !html.contains("Unix seconds") && !html.contains("Unix milliseconds"),
                "{page:?}"
            );
            assert!(!html.contains("1970-01-01"), "today needs no date: {page:?}");
        }
        for page in [Page::Board, Page::Item(17), Page::Usage] {
            let html = document_for(page, &view, 100);
            assert!(html.contains("02:00 CET"), "lease or quota reset: {page:?}");
            assert!(!html.contains("expires 3600") && !html.contains("resets at 3600"));
        }
        for page in [Page::Goal, Page::Chat(Prompt::Status)] {
            let html = document_for(page, &view, 100);
            let section = owner_goal_section(&html);
            assert!(section.contains("Started 01:00 CET") && section.contains("Observed 02:00 CET"));
            assert!(!section.contains("1970-01-01"));
        }
        let older = document_for(Page::Board, &view, 86_400);
        assert!(older.contains("1970-01-01 01:00 CET"));
        board.observed_at = 82_800; // 23:00 UTC is midnight of the next host-local day.
        let midnight = document_for(
            Page::Board,
            &View {
                board: Some(&board),
                ..Default::default()
            },
            86_400,
        );
        assert!(midnight.contains("00:00 CET") && !midnight.contains("1970-01-02"));
        board.observed_at = u64::MAX;
        let invalid = document_for(
            Page::Board,
            &View {
                board: Some(&board),
                ..Default::default()
            },
            100,
        );
        assert!(!invalid.contains(&u64::MAX.to_string()));
    }

    #[test]
    fn capacity_section_shows_configured_and_observed_truth() {
        use crate::lead::capacity::Capacity;
        use oxa_proto::work::{Running, TaskStep};
        let running = |task: &str, host: &str| Running {
            task: task.into(),
            host: host.into(),
            branch: format!("agent/{task}"),
            worktree: format!("~/project/{task}"),
            session_id: None,
            step: TaskStep::Worker,
        };
        let situation = Snapshot {
            as_of_seq: 1,
            at_ms: 1_000,
            standing_revision: 1,
            goal: None,
            running: vec![
                running("t1", "host-a"),
                running("t2", "host-a"),
                running("t3", "<b>host"),
            ],
            local_accepted: Vec::new(),
            published: Vec::new(),
            landed: Vec::new(),
            blocked: Vec::new(),
            asks: Vec::new(),
            incidents: Vec::new(),
            history_truncated: false,
        };
        let board = Board {
            observed_at: 1,
            entries: vec![Entry {
                number: 1,
                head: "synthetic-head".into(),
                release_pinned: false,
                action: Action::Ready,
                lease: None,
                attempts_today: 0,
                eligible: true,
                evidence_incomplete: None,
            }],
        };
        let capacity = Capacity::parse(Some(&serde_json::json!({"host-a": 6})));
        let view = View {
            board: Some(&board),
            situation: Some(&situation),
            capacity: Some(&capacity),
            ..Default::default()
        };
        for page in [Page::Home, Page::Workers] {
            let html = content_for(page, &view, 100);
            assert!(html.contains("id=capacity"), "{page:?}");
            assert!(html.contains("host-a 2/6"), "{page:?}");
            // An observed but unconfigured host keeps an unknown capacity, and its
            // name is escaped rather than rendered as markup.
            assert!(html.contains("&lt;b&gt;host 1/unknown"), "{page:?}");
            assert!(!html.contains("<b>host"), "{page:?}");
            assert!(html.contains("Ready: 1 items"), "{page:?}");
            assert!(html.contains("Stuck: none reported."), "{page:?}");
        }
    }

    #[test]
    fn capacity_unknown_states_are_explicit() {
        use crate::lead::capacity::Capacity;
        // No capacity source at all: unavailable, not "not configured".
        let html = content_for(Page::Workers, &View::default(), 100);
        assert!(html.contains("Capacity unavailable"));
        // Configured but no situation or board: use and ready stay unknown, never zero.
        let capacity = Capacity::parse(Some(&serde_json::json!({"host-a": 6})));
        let view = View {
            capacity: Some(&capacity),
            ..Default::default()
        };
        let html = content_for(Page::Workers, &view, 100);
        assert!(html.contains("host-a unknown/6"));
        assert!(html.contains("Ready depth unavailable"));
        assert!(html.contains("Stuck work unavailable"));
        // An empty configuration with no observation offers the set-it hint.
        let empty = Capacity::default();
        let view = View {
            capacity: Some(&empty),
            ..Default::default()
        };
        assert!(content_for(Page::Workers, &view, 100).contains("No per-host capacity configured"));
    }

    fn owner_goal_fixture() -> (oxa_session::Goal, oxa_session::SessionRef) {
        use oxa_session::{CheckResult, Goal, GoalBudget, GoalCheck, GoalPlan, Subgoal};
        let check = |id: &str, passed: Option<bool>| GoalCheck {
            id: id.into(),
            acceptance: format!("Acceptance {id} <example>"),
            done_check: format!("check-{id}"),
            result: passed.map(|passed| CheckResult {
                passed,
                receipt: format!("<receipt-{id}>&"),
                observed_at_ms: 3_600_000,
            }),
        };
        let mut goal = Goal::new("Ship <example> & outcome".into(), GoalBudget::default(), 0).unwrap();
        goal.plan = Some(GoalPlan {
            acceptance: vec![check("owner", None)],
            subgoals: vec![
                Subgoal {
                    id: "install".into(),
                    objective: "Install <example>".into(),
                    acceptance: vec![check("install", Some(false))],
                    board_items: vec![17],
                },
                Subgoal {
                    id: "build".into(),
                    objective: "Build example".into(),
                    acceptance: vec![check("build", Some(true))],
                    board_items: vec![18],
                },
            ],
        });
        goal.lead = Some(oxa_session::LeadGoalState {
            pending_steer: Some(oxa_session::PendingGoalSteer {
                entry_id: "0123abcd".into(),
                revision: 7,
                text: "private-continuation-marker".into(),
                lease: None,
            }),
            ..Default::default()
        });
        goal.validate().unwrap();
        (
            goal,
            oxa_session::SessionRef {
                path: "/project/private-session-reference.jsonl".into(),
                id: "private-session-id-marker".into(),
            },
        )
    }

    fn owner_goal_html(
        page: Page,
        goal: &oxa_session::Goal,
        reference: &oxa_session::SessionRef,
        board: Option<&Board>,
    ) -> String {
        let observation = oxa_core::goal::GoalObservation {
            session: reference,
            revision: 7,
            status: Some(goal.status),
        };
        document_for(
            page,
            &View {
                owner_goal: Some((goal, observation)),
                board,
                ..Default::default()
            },
            100,
        )
    }

    fn owner_goal_section(html: &str) -> &str {
        html.split("<section id=owner-goal>")
            .nth(1)
            .unwrap()
            .split("</section>")
            .next()
            .unwrap()
    }

    #[test]
    fn canonical_goal_checks_render_without_private_binding_or_new_controls() {
        let (goal, reference) = owner_goal_fixture();
        for page in [Page::Goal, Page::Chat(Prompt::Status)] {
            let html = owner_goal_html(page, &goal, &reference, None);
            let section = owner_goal_section(&html);
            for expected in [
                "Owner goal",
                "Ship &lt;example&gt; &amp; outcome",
                "revision 7",
                "Recorded state: pursuing",
                "Acceptance owner &lt;example&gt;",
                "check-owner",
                "Install &lt;example&gt;",
                "Build example",
                "Not checked",
                "Failed check",
                "Pass recorded",
                "&lt;receipt-install&gt;&amp;",
                "href=/items/17",
                "href=/items/18",
            ] {
                assert!(section.contains(expected), "{page:?}: {expected}");
            }
            for private in [
                reference.id.as_str(),
                reference.path.to_str().unwrap(),
                "private-continuation-marker",
                "<receipt-install>",
            ] {
                assert!(!html.contains(private), "{page:?}: {private}");
            }
            assert!(
                !section.contains("<form") && !section.contains("<button") && !section.contains("<script")
            );
            assert!(!section.contains("Every acceptance check has passing evidence recorded"));
        }
    }

    #[test]
    fn canonical_goal_acceptance_never_comes_from_board_readiness_or_check_counts() {
        let (mut goal, reference) = owner_goal_fixture();
        let before = owner_goal_html(Page::Goal, &goal, &reference, None);
        let board = Board {
            observed_at: 100,
            entries: vec![Entry {
                number: 17,
                head: "synthetic-head".into(),
                release_pinned: false,
                action: Action::Ready,
                lease: None,
                attempts_today: 0,
                eligible: true,
                evidence_incomplete: None,
            }],
        };
        let ready = owner_goal_html(Page::Goal, &goal, &reference, Some(&board));
        assert_eq!(owner_goal_section(&before), owner_goal_section(&ready));
        let plan = goal.plan.as_mut().unwrap();
        plan.acceptance[0].result = plan.subgoals[1].acceptance[0].result.clone();
        plan.subgoals[0].acceptance[0].result.as_mut().unwrap().passed = true;
        assert!(plan.verified());
        let all_pass = owner_goal_html(Page::Goal, &goal, &reference, Some(&board));
        let section = owner_goal_section(&all_pass);
        assert!(section.contains("Every acceptance check has passing evidence recorded"));
        assert!(section.contains("Recorded state: pursuing") && !section.contains("Recorded state: done"));
    }

    #[test]
    fn missing_or_inconsistent_goal_observations_never_promote_acceptance() {
        let absent = document_for(Page::Goal, &View::default(), 100);
        assert!(owner_goal_section(&absent).contains("No bound native goal observation reached this page"));
        let (mut goal, reference) = owner_goal_fixture();
        for (revision, status) in [
            (0, Some(goal.status)),
            (7, None),
            (7, Some(oxa_session::GoalStatus::Paused)),
        ] {
            let html = document_for(
                Page::Goal,
                &View {
                    owner_goal: Some((
                        &goal,
                        oxa_core::goal::GoalObservation {
                            session: &reference,
                            revision,
                            status,
                        },
                    )),
                    ..Default::default()
                },
                100,
            );
            let section = owner_goal_section(&html);
            assert!(section.contains("Goal observation unavailable"));
            assert!(!section.contains("Ship &lt;example&gt;"));
        }
        goal.plan.as_mut().unwrap().subgoals[0].board_items = vec![0];
        let invalid = owner_goal_html(Page::Goal, &goal, &reference, None);
        assert!(owner_goal_section(&invalid).contains("Goal observation unavailable"));
        goal.plan = None;
        goal.status = oxa_session::GoalStatus::Done;
        goal.evidence = Some("Legacy outcome reported".into());
        let legacy = owner_goal_html(Page::Goal, &goal, &reference, None);
        let section = owner_goal_section(&legacy);
        assert!(section.contains("done reported; acceptance plan unavailable"));
        assert!(section.contains("The native goal has no saved acceptance plan"));
        assert!(!section.contains("Every acceptance check has passing evidence recorded"));
    }

    #[test]
    fn missing_sections_explain_the_observation_gap_without_inventing_a_cause() {
        for (page, explanations) in [
            (
                Page::Home,
                vec![
                    "No board observation reached this page",
                    "No runner observation reached this page",
                    "No queue observation reached this page",
                    "No release issue and checklist were supplied",
                ],
            ),
            (Page::Board, vec!["No board observation reached this page"]),
            (Page::Workers, vec!["No runner observation reached this page"]),
            (Page::Asks, vec!["No runner observation reached this page"]),
            (Page::Item(17), vec!["No board observation reached this page"]),
            (
                Page::Usage,
                vec![
                    "No selected-task usage observation reached this page",
                    "No GitHub quota observation reached this page",
                    "No model plan observation reached this page",
                ],
            ),
            (
                Page::Chat(Prompt::Status),
                vec!["This question needs a board observation"],
            ),
            (
                Page::Chat(Prompt::Workers),
                vec!["This question needs a runner observation"],
            ),
            (
                Page::Chat(Prompt::Attention),
                vec!["This question needs a runner observation"],
            ),
        ] {
            let html = document_for(page, &View::default(), 100);
            for explanation in explanations {
                assert!(html.contains(explanation), "{page:?}: {explanation}");
            }
            for invented in ["Sign in again", "not configured", "Permission denied"] {
                assert!(!html.contains(invented), "{page:?}");
            }
        }
        let failed = Err("private-error-marker".into());
        let failed_page = usage_page(Some(&failed));
        assert!(failed_page.contains("The selected task usage could not be read"));
        assert!(!failed_page.contains("private-error-marker"));
        assert!(!failed_page.contains("No selected-task usage observation reached this page"));
        let absent = usage_page(Some(&Ok(None)));
        assert!(absent.contains("No saved checkpoint for the selected task"));
        assert!(!absent.contains("could not be read"));

        let board = Board::default();
        let situation = observed_asks();
        // A complete fresh healthy observation stays quiet: no alarm and no
        // unavailable state renders in the attention block.
        let alarms = Alarms {
            main_red: Some(false),
            main_merge_stalled: Some(false),
            stalled_runs: Some(vec![]),
            queue: Some(vec![]),
            overdue_queue: Some(vec![]),
        };
        let empty = View {
            board: Some(&board),
            situation: Some(&situation),
            alarms: Some(&alarms),
            ..Default::default()
        };
        let home = document_for(Page::Home, &empty, 100);
        assert!(home.contains("No open PRs in this snapshot"));
        assert!(home.contains("Queue empty at observation"));
        assert!(!home.contains("Checks on main are failing"));
        assert!(!home.contains("role=alert"));
        assert!(!home.contains("No board observation reached this page"));
        assert!(!home.contains("No runner observation reached this page"));
        let missing_item = document_for(Page::Item(17), &empty, 100);
        assert!(missing_item.contains("The board observation does not include this item"));
        let quotas = crate::lead_quota::GitHubQuotas::default();
        let model = oxc_gateway::status::chatgpt_usage::parse(&serde_json::json!({}));
        let usage = document_for(
            Page::Usage,
            &View {
                github_quotas: Some(&quotas),
                model_plan: Some(&model),
                ..Default::default()
            },
            100,
        );
        assert!(usage.contains("No valid current quota window was reported"));
        assert!(usage.contains("No valid model plan was reported"));
        assert!(usage.contains("No valid current model window was reported"));
        assert!(!usage.contains("<meter "));
    }

    #[test]
    fn release_checkboxes_do_not_claim_owner_outcome_acceptance() {
        for (checked, total) in [(0, 0), (0, 1), (2, 2)] {
            let items: Vec<_> = (0..total)
                .map(|index| (index < checked, format!("Reported step {index}")))
                .collect();
            let html = document_for(
                Page::Home,
                &View {
                    release: Some((19, &items)),
                    ..Default::default()
                },
                100,
            );
            let checklist = html
                .split("<section id=release>")
                .nth(1)
                .unwrap()
                .split("</section>")
                .next()
                .unwrap();
            assert!(checklist.contains(&format!("{checked} / {total} checkboxes reported checked")));
            assert!(checklist.contains("Owner outcome verification unavailable"));
            assert!(checklist.contains("release or installability evidence"));
            assert_eq!(checklist.matches('☑').count(), checked);
            assert_eq!(checklist.matches('☐').count(), total - checked);
            for unverified in [
                "reported complete",
                "validation requires merge evidence",
                "Goal complete",
            ] {
                assert!(!checklist.contains(unverified), "{unverified}");
            }
        }
    }

    #[test]
    fn navigation_has_real_urls_one_current_destination_and_observed_ask_counts() {
        let mut snapshot = observed_asks();
        for (page, selected) in [
            (Page::Home, "/"),
            (Page::Asks, "/asks"),
            (Page::Board, "/board"),
            (Page::Workers, "/workers"),
            (Page::Usage, "/usage"),
            (Page::Item(17), "/board"),
            (
                Page::BoardQuery(Query {
                    filter: Filter::Fix,
                    page: 1,
                }),
                "/board",
            ),
            (Page::Chat(Prompt::Status), "/chat"),
        ] {
            let html = document_for(
                page,
                &View {
                    situation: Some(&snapshot),
                    ..View::default()
                },
                100,
            );
            let nav = html
                .split("<nav class=tabs")
                .nth(1)
                .unwrap()
                .split("</nav>")
                .next()
                .unwrap();
            assert_eq!(nav.matches("aria-current=page").count(), 1);
            let current = nav
                .split("<a ")
                .find(|anchor| anchor.split('>').next().unwrap().contains("aria-current=page"))
                .unwrap();
            assert!(
                current
                    .split('>')
                    .next()
                    .unwrap()
                    .split_whitespace()
                    .any(|attribute| attribute == format!("href={selected}")),
                "{page:?}"
            );
            for path in ["/", "/asks", "/board", "/workers", "/usage", "/chat"] {
                assert!(nav.contains(&format!("href={path}")), "{page:?} {path}");
            }
            assert!(nav.contains(&format!(
                "aria-label=\"Asks, {} open in snapshot\"",
                snapshot.asks.len()
            )));
            assert_eq!(nav.matches("<svg ").count(), 6);
            assert!(nav.contains("class=rail-only href=/chat"));
            assert!(nav.contains("<span class=badge aria-hidden=true>"));
            if matches!(page, Page::Chat(_)) {
                assert!(!html.contains("class=fab"));
            } else {
                assert!(html.contains("<a class=fab href=/chat"));
            }
            assert_eq!(html.matches("<h1 ").count(), 1);
            assert!(html.contains("href=#main>Skip to content"));
            assert!(html.contains("id=main tabindex=-1"));
            assert!(!nav.contains("https://") && !nav.contains("onclick="));
        }
        snapshot.asks.clear();
        let quiet = content_for(
            Page::Asks,
            &View {
                situation: Some(&snapshot),
                ..View::default()
            },
            100,
        );
        assert!(quiet.contains("aria-label=\"Asks, 0 open in snapshot\""));
        assert!(!quiet.contains("class=badge"));
        let unknown = content_for(Page::Asks, &View::default(), 100);
        assert!(unknown.contains("aria-label=\"Asks, observations unavailable\""));
        assert!(!unknown.contains("Asks, 0 open in snapshot"));
        assert!(!unknown.contains("class=badge"));
        assert!(unknown.contains("Owner asks unavailable"));
    }

    #[test]
    fn capability_footer_is_gone_and_the_theme_toggle_is_document_chrome() {
        for page in [
            Page::Home,
            Page::Asks,
            Page::Board,
            Page::Workers,
            Page::Usage,
            Page::Item(17),
            Page::Chat(Prompt::Status),
        ] {
            let document = document_for(page, &View::default(), 100);
            let fragment = content_for(page, &View::default(), 100);
            assert!(!document.contains("not connected yet"), "{page:?}");
            assert!(!fragment.contains("not connected yet"), "{page:?}");
            assert!(!document.contains("<footer"), "{page:?}");
            assert!(document.contains("<button id=theme class=iconbtn type=button aria-label=\"Switch colour theme\" aria-pressed=false>"));
            // The fragment swaps into a document that already carries the toggle.
            assert!(!fragment.contains("id=theme"), "{page:?}");
            assert!(document.contains("<span class=dot aria-hidden=true></span>"));
        }
    }

    #[test]
    fn style_keeps_colour_literals_inside_the_token_block() {
        let end = STYLE
            .find("/* End of design tokens.")
            .expect("the token block is marked");
        let rest = &STYLE[end..];
        assert!(!rest.contains("rgb(") && !rest.contains("rgba(") && !rest.contains("hsl("));
        let bytes = rest.as_bytes();
        for (index, byte) in bytes.iter().enumerate() {
            if *byte != b'#' {
                continue;
            }
            let run = bytes[index + 1..]
                .iter()
                .take_while(|b| b.is_ascii_hexdigit())
                .count();
            let next = bytes.get(index + 1 + run).copied().unwrap_or(b' ');
            if (3..=8).contains(&run) && !next.is_ascii_alphanumeric() && next != b'-' && next != b'_' {
                panic!(
                    "colour literal outside the token block: {}",
                    &rest[index..index + 1 + run]
                );
            }
        }
        for token in [
            "--ink2:",
            "--on-accent:",
            "--ok:",
            "--bad:",
            "--shadow:",
            "--on-bad:",
        ] {
            assert!(STYLE[..end].contains(token), "missing token {token}");
        }
    }

    #[test]
    fn theme_toggle_follows_the_system_and_persists_an_explicit_choice() {
        let mut runtime = oxa_script::Runtime::new(oxa_script::Limits::EXTENSION).unwrap();
        runtime.eval_script("dom-fixture", DOM_FIXTURE).unwrap();
        runtime
            .eval_script(
                "theme-setup",
                "themeState.systemDark=true;themeState.store['lead-theme']='light';",
            )
            .unwrap();
        runtime.eval_script("live.js", SCRIPT).unwrap();
        let result = runtime
            .eval_async_body(
                "theme-choice",
                r#"
const root=document.documentElement;
if(root.dataset.theme!=='light'||themeState.pressed!=='false')throw new Error('saved choice did not win over the system');
themeState.click();
if(root.dataset.theme!=='dark'||themeState.pressed!=='true'||themeState.store['lead-theme']!=='dark')throw new Error('toggle did not flip and persist');
themeState.click();
if(root.dataset.theme!=='light'||themeState.pressed!=='false'||themeState.store['lead-theme']!=='light')throw new Error('second toggle did not return to light');
return true;
"#,
            )
            .unwrap();
        assert_eq!(result, serde_json::json!(true));

        let mut runtime = oxa_script::Runtime::new(oxa_script::Limits::EXTENSION).unwrap();
        runtime.eval_script("dom-fixture", DOM_FIXTURE).unwrap();
        runtime
            .eval_script(
                "theme-setup",
                "themeState.systemDark=true;themeState.store['lead-theme']='banana';",
            )
            .unwrap();
        runtime.eval_script("live.js", SCRIPT).unwrap();
        let result = runtime
            .eval_async_body(
                "theme-invalid",
                r#"
if(document.documentElement.dataset.theme)throw new Error('invalid saved theme was applied');
if(themeState.pressed!=='true')throw new Error('system dark was not reported');
themeState.click();
if(document.documentElement.dataset.theme!=='light'||themeState.store['lead-theme']!=='light')throw new Error('toggle from system dark did not choose light');
return true;
"#,
            )
            .unwrap();
        assert_eq!(result, serde_json::json!(true));
    }

    #[test]
    fn home_prioritizes_decisions_and_keeps_observed_work_in_native_details() {
        let snapshot = observed_asks();
        let board = Board::default();
        let html = content_for(
            Page::Home,
            &View {
                board: Some(&board),
                situation: Some(&snapshot),
                alarms: Some(&Alarms {
                    main_red: Some(true),
                    ..Alarms::default()
                }),
                ..View::default()
            },
            100,
        );
        assert!(html.contains("<div class=home-layout>"));
        assert!(html.find("id=attention").unwrap() < html.find("id=asks").unwrap());
        assert!(html.find("id=asks").unwrap() < html.find("<details class=home-work>").unwrap());
        assert!(html.contains("<details class=home-work><summary>Observed work</summary>"));
        assert!(!html.contains("<details class=home-work open"));
        assert!(html.contains("role=alert"));
        // A partial observation keeps the active alarm visible and names the
        // sources that did not reach the page (#1391).
        assert!(html.contains("Checks on main are failing."));
        assert!(html.contains("merge queue membership"));
        for ask in &snapshot.asks {
            assert!(html.contains(&escape(&ask.question)));
        }
        assert!(html.contains("No open PRs in this snapshot"));
        assert!(html.contains("href=/board>"));
        assert!(html.contains("0 observed items"));
        assert!(html.contains("Options, recommendation and meanwhile are unavailable"));
        assert!(!html.contains("<form") && !html.contains("onclick="));
    }

    #[test]
    fn home_banner_answers_is_anything_wrong_above_the_fold_without_inventing_health() {
        let banner_of = |view: &View<'_>| {
            let html = content_for(Page::Home, view, 100);
            // #2345: the banner is the first block, above the decision/overview layout.
            assert!(html.find("id=home-banner").unwrap() < html.find("class=home-layout").unwrap());
            let banner = html
                .split("<section id=home-banner")
                .nth(1)
                .unwrap()
                .split("</section>")
                .next()
                .unwrap()
                .to_string();
            // The rows below own the attention block; the banner never carries its id.
            assert!(!banner.contains("id=attention"));
            assert!(!banner.contains("<form") && !banner.contains("<button") && !banner.contains("onclick="));
            assert!(!banner.contains("All good"));
            banner
        };
        let healthy_alarms = Alarms {
            main_red: Some(false),
            main_merge_stalled: Some(false),
            stalled_runs: Some(vec![]),
            queue: Some(vec![]),
            overdue_queue: Some(vec![]),
        };
        let board = Board {
            observed_at: 100,
            ..Board::default()
        };
        let alarm = banner_of(&View {
            alarms: Some(&Alarms {
                main_red: Some(true),
                ..Alarms::default()
            }),
            ..View::default()
        });
        assert!(alarm.contains("data-tone=bad") && alarm.contains("role=alert") && alarm.contains("Alarm"));
        let snapshot = observed_asks();
        let needs = banner_of(&View {
            situation: Some(&snapshot),
            ..View::default()
        });
        assert!(
            needs.contains("data-tone=warn") && needs.contains("role=status") && needs.contains("Needs you")
        );
        let clear = banner_of(&View {
            board: Some(&board),
            alarms: Some(&healthy_alarms),
            ..View::default()
        });
        assert!(
            clear.contains("data-tone=ok") && clear.contains("role=status") && clear.contains("All clear")
        );
        // An absent source is never healthy: a missing board or no alarm record at
        // all says it cannot tell yet instead of claiming all clear.
        let partial = banner_of(&View {
            alarms: Some(&healthy_alarms),
            ..View::default()
        });
        assert!(
            partial.contains("data-tone=neutral")
                && partial.contains("Can't tell yet")
                && !partial.contains("All clear")
        );
        let unknown = banner_of(&View::default());
        assert!(unknown.contains("data-tone=neutral") && !unknown.contains("All clear"));
    }

    #[test]
    fn home_tiles_navigate_to_full_pages_with_observed_values_and_name_missing_sources() {
        let tiles_of = |view: &View<'_>| {
            let html = content_for(Page::Home, view, 100);
            let tiles = html
                .split("<div class=tiles>")
                .nth(1)
                .unwrap()
                .split("</div>")
                .next()
                .unwrap()
                .to_string();
            assert!(!tiles.contains("<form") && !tiles.contains("<button") && !tiles.contains("onclick="));
            tiles
        };
        let snapshot = observed_asks();
        let items = vec![
            (true, "Reported step".to_string()),
            (false, "Other step".to_string()),
        ];
        let board = Board {
            observed_at: 100,
            ..Board::default()
        };
        let observed = tiles_of(&View {
            board: Some(&board),
            situation: Some(&snapshot),
            alarms: Some(&Alarms {
                queue: Some(vec![]),
                ..Alarms::default()
            }),
            release: Some((19, &items)),
            ..View::default()
        });
        for href in [
            "href=/board>",
            "href=/workers>",
            "href=/board#queue>",
            "href=/board#release>",
        ] {
            assert!(observed.contains(href), "{href}");
        }
        assert!(observed.contains("0 observed items"));
        assert!(observed.contains("running at observation"));
        assert!(observed.contains("in the merge queue"));
        assert!(observed.contains("1/2") && observed.contains("release checklist items"));
        // A navigation tile carries no checklist glyphs and no worker identity.
        assert!(!observed.contains('☑') && !observed.contains('☐'));
        let missing = tiles_of(&View::default());
        for gap in [
            "No board observation reached this page",
            "Lane status unavailable",
            "No queue observation reached this page",
            "No release checklist supplied",
        ] {
            assert!(missing.contains(gap), "{gap}");
        }
        assert!(missing.contains(">—<"));
    }

    #[test]
    fn goal_collapses_to_a_home_row_and_the_acceptance_tree_lives_on_the_goal_page() {
        let (goal, reference) = owner_goal_fixture();
        let observation = oxa_core::goal::GoalObservation {
            session: &reference,
            revision: 7,
            status: Some(goal.status),
        };
        let view = View {
            owner_goal: Some((&goal, observation)),
            ..View::default()
        };
        let home = content_for(Page::Home, &view, 100);
        assert!(home.contains("class=goal-row") && home.contains("href=/goal") && home.contains("Started"));
        // The full acceptance section and tree stay off Home.
        assert!(!home.contains("id=owner-goal"));
        assert!(!home.contains("Owner acceptance") && !home.contains("Sub-goals"));
        assert!(!home.contains("<form") && !home.contains("<button"));
        // The dedicated read-only page carries the full section and highlights Home.
        let page = document_for(Page::Goal, &view, 100);
        assert!(page.contains(">Owner goal</h1>") && page.contains("id=owner-goal"));
        assert!(page.contains("Owner acceptance") && page.contains("Sub-goals"));
        assert!(page.contains("aria-current=page>Home"));
        assert_eq!(Page::request("/goal"), Some((Page::Goal, false)));
        assert_eq!(Page::Goal.title(), "Owner goal");
        // An absent observation says so on both surfaces and still links to /goal.
        let absent_home = content_for(Page::Home, &View::default(), 100);
        assert!(absent_home.contains("class=goal-row") && absent_home.contains("href=/goal"));
        assert!(!absent_home.contains("id=owner-goal"));
        assert!(content_for(Page::Goal, &View::default(), 100)
            .contains("No bound native goal observation reached this page"));
    }

    #[test]
    fn asks_layout_keeps_questions_and_unknown_options_read_only() {
        let snapshot = observed_asks();
        let html = content_for(
            Page::Asks,
            &View {
                situation: Some(&snapshot),
                ..View::default()
            },
            100,
        );
        assert!(html.contains("<div class=asks-layout>"));
        assert!(html.contains("<div class=ask-cards>"));
        for ask in &snapshot.asks {
            assert!(html.contains(&escape(&ask.question)));
            assert!(html.contains(&escape(&ask.id)));
        }
        assert!(html.contains("Options, recommendation and meanwhile are unavailable"));
        assert!(!html.contains("<form") && !html.contains("<button"));
    }

    fn refresh_runtime() -> oxa_script::Runtime {
        refresh_runtime_config(false, None, false)
    }

    const DOM_FIXTURE: &str = r#"
let current, supplied, closeDuringFetch, failureMode='ok', focusDuringFetch=false, hideDuringFetch=false, focused='', inside=false, fetches=0;
let initialDigest=null, liveEnabled=false, reloads=0, visibilityListener, serial=0;
const timers=new Map(), requested=[], streams=[];
const connectionBadge={textContent:'Snapshot'};
const themeState={store:{},pressed:null,click:null,systemDark:false};
const themeButton={setAttribute:(k,v)=>{if(k==='aria-pressed')themeState.pressed=v;},addEventListener:(n,fn)=>{if(n==='click')themeState.click=fn;}};
const localStorage={getItem:k=>Object.prototype.hasOwnProperty.call(themeState.store,k)?themeState.store[k]:null,setItem:(k,v)=>{themeState.store[k]=String(v);}};
const matchMedia=()=>({matches:themeState.systemDark});
function makeMain(open, message=null) {
  const work={open}, notice={hidden:true}, draft=message===null?null:{value:message};
  return {work, notice, draft, contains:()=>inside, querySelector:selector=>selector==='details.home-work'?work:selector==='#lead-message'?draft:null,
    replaceWith:next=>{current=next;}};
}
const document={hidden:false, documentElement:{dataset:{}}, activeElement:{matches:selectors=>selectors.split(',').includes(focused)},
  getElementById:id=>id==='theme'?themeButton:id==='connection'?connectionBadge:id==='refresh-error'?current.notice:null,
  querySelector:selector=>selector==='meta[name=oxa-live-events]'?(liveEnabled?{}:null):selector.startsWith('meta')?(initialDigest?{content:initialDigest}:null):selector==='#lead-message'?current?.draft:current,
  addEventListener:(name,fn)=>{if(name==='visibilitychange')visibilityListener=fn;}};
const setTimeout=(fn,ms)=>{const id=++serial;timers.set(id,{fn:()=>{timers.delete(id);return fn();},ms});return id;}, clearTimeout=id=>timers.delete(id);
class AbortController { constructor(){this.signal={aborted:false};} abort(){this.signal.aborted=true;} }
class URL { constructor(href){this.href=href;this.searchParams={set:(key,value)=>{this.href+=(href.includes('?')?'&':'?')+key+'='+value;}};} }
const location={href:'http://example.invalid/board?f=fix&p=2',reload:()=>{reloads++;}};
class DOMParser { parseFromString(){return {querySelector:()=>failureMode==='missing'?null:supplied};} }
async function fetch(target){
  fetches++;
  requested.push(target.href);
  if(focusDuringFetch){inside=true; focused='[tabindex]';}
  if(hideDuringFetch){document.hidden=true;visibilityListener();}
  if(failureMode==='network')throw new Error('private-refresh-marker');
  if(failureMode==='abort'){pending.abort();throw new Error('aborted');}
  if(closeDuringFetch)current.work.open=false;
  return {ok:failureMode!=='refused', text:async()=>''};
}
"#;

    fn refresh_runtime_config(events: bool, digest: Option<&str>, enabled: bool) -> oxa_script::Runtime {
        let mut runtime = oxa_script::Runtime::new(oxa_script::Limits::EXTENSION).unwrap();
        runtime.eval_script("dom-fixture", DOM_FIXTURE).unwrap();
        if events {
            runtime
                .eval_script(
                    "event-source-fixture",
                    &format!(
                        r#"
initialDigest={};liveEnabled={enabled};
class EventSource {{
  constructor(url){{this.url=url;this.listeners={{}};this.closed=false;streams.push(this);}}
  addEventListener(name,fn){{this.listeners[name]=fn;}}
  close(){{this.closed=true;}}
}}
"#,
                        serde_json::to_string(&digest).unwrap()
                    ),
                )
                .unwrap();
        }
        runtime.eval_script("live.js", SCRIPT).unwrap();
        runtime
    }

    #[test]
    fn serving_digest_is_validated_and_only_in_full_document_metadata() {
        let digest = "ab".repeat(32);
        for page in [
            Page::Home,
            Page::Asks,
            Page::Board,
            Page::Workers,
            Page::Usage,
            Page::Chat(Prompt::Status),
        ] {
            let view = View {
                running_sha256: Some(&digest),
                live_events: true,
                ..Default::default()
            };
            let expected = format!("<meta name=oxa-running-sha256 content=\"{digest}\">");
            assert_eq!(document_for(page, &view, 0).matches(&expected).count(), 1);
            assert!(document_for(page, &view, 0).contains("<meta name=oxa-live-events content=1>"));
            assert!(!content_for(page, &view, 0).contains("oxa-running-sha256"));
            assert!(!content_for(page, &view, 0).contains("oxa-live-events"));
        }
        let uppercase = "AB".repeat(32);
        for digest in [
            None,
            Some(""),
            Some("abcd"),
            Some(uppercase.as_str()),
            Some("\"><script>private-marker</script>"),
        ] {
            let html = document_for(
                Page::Home,
                &View {
                    running_sha256: digest,
                    ..Default::default()
                },
                0,
            );
            assert!(!html.contains("oxa-running-sha256") && !html.contains("private-marker"));
            assert!(!html.contains("oxa-live-events"));
        }
    }

    #[test]
    fn listeners_without_the_live_route_keep_the_existing_refresh_path() {
        let mut runtime = refresh_runtime_config(true, None, false);
        let result = runtime
            .eval_async_body(
                "no-live-route",
                r#"
current=makeMain(true);supplied=makeMain(false);
if(streams.length!==0)throw new Error('listener without live capability was subscribed');
await refresh();
if(current!==supplied||!current.work.open||fetches!==1)throw new Error('existing listener lost refresh');
return true;
"#,
            )
            .unwrap();
        assert_eq!(result, serde_json::json!(true));
    }

    #[test]
    fn stream_refresh_uses_current_tab_and_preserves_reader_and_digest_authority() {
        let mut runtime = refresh_runtime_config(true, Some(&"a".repeat(64)), true);
        let result = runtime.eval_async_body("stream-reader", r#"
current=makeMain(true,'unsent <draft>'); supplied=makeMain(false,'');
if(streams.length!==1||streams[0].url!=='/events/live')throw new Error('missing authenticated live route');
const source=streams[0], send=value=>source.listeners['lead-update']({data:JSON.stringify(value)});
await send({running_sha256:'a'.repeat(64)});
if(current!==supplied||!current.work.open||current.draft.value!=='unsent <draft>'||reloads!==0||requested[0]!=='http://example.invalid/board?f=fix&p=2&fragment=1')
  throw new Error('same-image event lost current tab or reader state');
inside=true;focused='textarea';supplied=makeMain(false);
const retained=current, before=fetches;
await send({running_sha256:'a'.repeat(64)});
if(current!==retained||fetches!==before)throw new Error('stream replaced focused composer');
inside=false;await send({running_sha256:null});
if(current!==retained||reloads!==0)throw new Error('missing composer dropped the retained draft');
supplied=makeMain(false,'');await send({running_sha256:null});
if(current!==supplied||current.draft.value!=='unsent <draft>'||reloads!==0)throw new Error('null image must refresh without losing draft');
for(const data of ['not-json','{}','{"running_sha256":3}','{"running_sha256":"AB"}','{"running_sha256":null,"extra":1}']) {
  const before=fetches;
  await source.listeners['lead-update']({data});
  if(fetches!==before||reloads!==0||current.notice.hidden)throw new Error('invalid event acquired authority');
}
supplied=makeMain(false,'');await send({running_sha256:'a'.repeat(64)});
if(!current.notice.hidden)throw new Error('valid event and fragment did not recover');
inside=true;focused='textarea';
await send({running_sha256:'b'.repeat(64)});
if(reloads!==0)throw new Error('image change discarded focused input');
inside=false;
current=makeMain(true,'unsent draft');
await send({running_sha256:null});
if(reloads!==0)throw new Error('image change discarded an unfocused draft');
current.draft.value='';
await send({running_sha256:null});
if(reloads!==1)throw new Error('null event erased pending verified image change');
return true;
"#).unwrap();
        assert_eq!(result, serde_json::json!(true));
    }

    #[test]
    fn normal_stream_rotation_retains_state_but_failures_require_valid_recovery() {
        let mut runtime = refresh_runtime_config(true, None, true);
        let result = runtime.eval_async_body("stream-rotation", r#"
current=makeMain(true);supplied=makeMain(false);
const source=streams[0], send=value=>source.listeners['lead-update']({data:JSON.stringify(value)});
await send({running_sha256:null});
await send({running_sha256:'a'.repeat(64)});
if(reloads!==0)throw new Error('unknown document image cannot authorize reload');
source.onerror();
if(!current.notice.hidden)throw new Error('normal finite stream rotation flashed failure');
await send({running_sha256:'a'.repeat(64)});
if(!current.notice.hidden||timers.size!==0)throw new Error('normal reconnect failed to retain successful state');
source.onerror();
for(const {fn} of [...timers.values()])fn();
if(current.notice.hidden)throw new Error('transport failure never surfaced');
failureMode='network';const retained=current;supplied=makeMain(false);
await send({running_sha256:'a'.repeat(64)});
if(current!==retained||current.notice.hidden)throw new Error('failed read erased last view or failure');
failureMode='ok';await send({running_sha256:'a'.repeat(64)});
if(current!==supplied||!current.notice.hidden)throw new Error('valid event and read must recover');
document.hidden=true;visibilityListener();
if(!source.closed)throw new Error('hidden page retained stream');
const before=fetches;await refresh();
if(fetches!==before)throw new Error('hidden page fetched');
document.hidden=false;visibilityListener();
if(streams.length!==2)throw new Error('visible page failed to reauthenticate');
await send({running_sha256:'c'.repeat(64)});
if(reloads!==0)throw new Error('closed stream retained image authority');
await streams[1].listeners['lead-update']({data:JSON.stringify({running_sha256:'b'.repeat(64)})});
if(reloads!==1)throw new Error('reconnect missed changed image with unchanged board');
return true;
"#).unwrap();
        assert_eq!(result, serde_json::json!(true));
    }

    #[test]
    fn live_refresh_keeps_the_readers_latest_work_disclosure_choice() {
        let mut runtime = refresh_runtime();
        let result = runtime
            .eval_async_body(
                "refresh",
                r#"
for(const [open,close,expected] of [[false,false,false],[true,false,true],[true,true,false]]) {
  current=makeMain(open); supplied=makeMain(false); closeDuringFetch=close;
  await refresh();
  if(current!==supplied||current.work.open!==expected)throw new Error('reader disclosure state lost');
}
closeDuringFetch=false; inside=true;
for(focused of ['summary','[tabindex]']) {
  current=makeMain(true); supplied=makeMain(false);
  const before=fetches;
  await refresh();
  if(current===supplied||fetches!==before)throw new Error('focused reader region was replaced');
}
inside=false;
await refresh();
if(current!==supplied)throw new Error('refresh did not resume after leaving main');
current=makeMain(true); supplied=makeMain(false); focusDuringFetch=true;
await refresh();
if(current===supplied)throw new Error('focus acquired during fetch was lost');
focusDuringFetch=false;inside=false;hideDuringFetch=true;
current=makeMain(true);supplied=makeMain(false);
await refresh();
if(current===supplied)throw new Error('late fragment replaced a hidden page after abort');
return true;
"#,
            )
            .unwrap();
        assert_eq!(result, serde_json::json!(true));
    }

    #[test]
    fn live_refresh_preserves_an_unfocused_unsent_chat_draft() {
        let mut runtime = refresh_runtime();
        let result = runtime
            .eval_async_body(
                "chat-draft",
                r#"
current=makeMain(false,'unsent owner message'); supplied=makeMain(false,'');
await refresh();
if(current!==supplied||current.draft.value!=='unsent owner message')
  throw new Error('fragment replacement lost the draft');
const retained=current;
supplied=makeMain(false);
await refresh();
if(current!==retained||current.draft.value!=='unsent owner message')
  throw new Error('missing composer dropped the draft');
current.draft.value='';
await refresh();
if(current!==supplied)throw new Error('refresh did not resume after clearing the draft');
return true;
"#,
            )
            .unwrap();
        assert_eq!(result, serde_json::json!(true));
    }

    #[test]
    fn failed_refresh_keeps_the_last_content_and_exposes_retry_until_recovery() {
        let mut runtime = refresh_runtime();
        let result = runtime
            .eval_async_body(
                "refresh-recovery",
                r#"
for(failureMode of ['network','refused','missing']) {
  current=makeMain(true); supplied=makeMain(false);
  const retained=current;
  await refresh();
  if(current!==retained||current.notice.hidden||connectionBadge.textContent!=='Refresh unavailable')
    throw new Error('refresh failure must retain content and expose retry');
  failureMode='ok';
  await refresh();
  if(current!==supplied||!current.notice.hidden||connectionBadge.textContent!=='Connected')
    throw new Error('successful refresh must clear error without claiming source freshness');
}
current=makeMain(true); supplied=makeMain(false); failureMode='abort';
await refresh();
if(!current.notice.hidden)throw new Error('aborted refresh must not expose error');
return true;
"#,
            )
            .unwrap();
        assert_eq!(result, serde_json::json!(true));
    }

    #[test]
    fn stale_board_observations_offer_an_ssr_retry_without_inventing_an_interval() {
        for page in [
            Page::Home,
            Page::BoardQuery(Query {
                filter: Filter::Fix,
                page: 1,
            }),
            Page::Item(17),
            Page::Asks,
            Page::Workers,
            Page::Usage,
            Page::Chat(Prompt::Workers),
        ] {
            let render = |observed_at, now| {
                document_for(
                    page,
                    &View {
                        board: Some(&Board {
                            observed_at,
                            ..Board::default()
                        }),
                        ..View::default()
                    },
                    now,
                )
            };
            for (observed_at, now) in [(100, 1001), (161, 100)] {
                let html = render(observed_at, now);
                assert!(html.contains("<h2>Board observation needs refreshing</h2>"));
                assert!(html.contains("<a href=\"\">Retry</a>"));
                assert_eq!(html.matches("id=refresh-error").count(), 1);
                assert!(html.contains("id=refresh-error class=stale-notice role=status hidden"));
                assert!(!html.contains("onclick=") && !html.contains("<form"));
                assert!(!html.contains("sweep every") && !html.contains("Can't reach lead"));
            }
            for (observed_at, now) in [(100, 1000), (160, 100)] {
                assert!(!render(observed_at, now).contains("Board observation needs refreshing"));
            }
            let unavailable = document_for(page, &View::default(), 1001);
            assert!(!unavailable.contains("Board observation needs refreshing"));
            assert!(unavailable.contains("id=refresh-error class=stale-notice role=status hidden"));
        }
    }

    #[test]
    fn queue_alarm_requires_membership_at_the_exact_head() {
        let active = PullRequest {
            number: 17,
            head: "a".repeat(40),
        };
        let queue = [active.clone()];
        let older = PullRequest {
            head: "b".repeat(40),
            ..active.clone()
        };
        let render = |queue: Option<&[PullRequest]>, overdue: Option<&[PullRequest]>| {
            content(
                &View {
                    alarms: Some(&Alarms {
                        queue: queue.map(<[PullRequest]>::to_vec),
                        overdue_queue: overdue.map(<[PullRequest]>::to_vec),
                        ..Alarms::default()
                    }),
                    ..View::default()
                },
                100,
            )
        };
        let overdue = [active];
        let stale = [older];
        let other = [PullRequest {
            number: 18,
            head: "a".repeat(40),
        }];
        // An alarm binds the PR to its exact observed head: an older head, another
        // PR and an observed-empty landing record all stay quiet.
        for (queue, overdue) in [
            (Some(queue.as_slice()), Some(stale.as_slice())),
            (Some(queue.as_slice()), Some(other.as_slice())),
            (Some(queue.as_slice()), Some(&[])),
        ] {
            let page = render(queue, overdue);
            assert!(!page.contains("waited too long"), "{overdue:?}");
            assert!(!page.contains("role=alert"));
            assert!(!page.contains("membership unavailable"));
            assert!(!page.contains("Queue age unavailable"));
        }
        // Unobserved membership never becomes a quiet alarm: the page says the
        // queue was not observed, even when the landing record raised the age.
        let unobserved = render(None, Some(overdue.as_slice()));
        assert!(unobserved.contains("merge queue membership"));
        assert!(!unobserved.contains("waited too long"));
        assert!(!unobserved.contains("role=alert"));
        // A landing record with no observation of its own is explicit as well.
        let no_landing = render(Some(queue.as_slice()), None);
        assert!(no_landing.contains("queue age"));
        assert!(!no_landing.contains("waited too long"));
        let page = render(Some(&queue), Some(&overdue));
        assert!(page.contains("PR #17 has waited too long in the merge queue"));
        assert!(page.contains("role=alert"));
        assert!(!page.contains(&queue[0].head));
    }

    #[test]
    fn shared_runner_fixture_renders_asks_without_private_record_fields() {
        let fixture = include_str!("../../oxa-proto/tests/fixtures/work/evening.jsonl");
        let snapshots: Vec<_> = fixture
            .lines()
            .filter_map(|line| Snapshot::from_json(line.as_bytes()).ok())
            .collect();
        assert!(!snapshots.is_empty());
        assert!(snapshots.iter().any(|s| !s.asks.is_empty()));
        assert!(snapshots.iter().any(|s| s.asks.is_empty()));
        for snapshot in snapshots {
            let page = document(
                &View {
                    situation: Some(&snapshot),
                    ..View::default()
                },
                snapshot.at_ms / 1000,
            );
            for ask in &snapshot.asks {
                assert!(page.contains(&escape(&ask.question)));
            }
            assert_eq!(page.contains("Needs attention"), !snapshot.asks.is_empty());
            if !snapshot.asks.is_empty() {
                assert!(page.contains(&format!("Unanswered owner decisions ({})", snapshot.asks.len())));
            }
            for running in &snapshot.running {
                assert!(!page.contains(&running.worktree));
                if let Some(id) = &running.session_id {
                    assert!(!page.contains(id));
                }
            }
            assert!(
                page.contains("Options, recommendation and meanwhile are unavailable")
                    || snapshot.asks.is_empty()
            );
        }
    }
    #[test]
    fn known_alarms_have_visible_severity_and_keep_their_observed_evidence() {
        let queue = vec![PullRequest {
            number: 17,
            head: "a".repeat(40),
        }];
        let runs = vec![29];
        for (alarms, evidence) in [
            (
                Alarms {
                    main_red: Some(true),
                    ..Alarms::default()
                },
                "Checks on main are failing.",
            ),
            (
                Alarms {
                    main_merge_stalled: Some(true),
                    ..Alarms::default()
                },
                "No PR merged into main for 45 minutes",
            ),
            (
                Alarms {
                    stalled_runs: Some(runs.clone()),
                    ..Alarms::default()
                },
                "Run 29",
            ),
            (
                Alarms {
                    queue: Some(queue.clone()),
                    overdue_queue: Some(queue.clone()),
                    ..Alarms::default()
                },
                "PR #17 has waited too long",
            ),
        ] {
            let html = content_for(
                Page::Home,
                &View {
                    alarms: Some(&alarms),
                    ..View::default()
                },
                100,
            );
            let banner = html
                .split("<section id=attention")
                .nth(1)
                .unwrap()
                .split("</section>")
                .next()
                .unwrap();
            assert!(banner.contains("role=alert"));
            assert!(banner.contains("aria-hidden=true>!</span> Alarm</p>"));
            assert!(banner.contains(evidence));
            assert!(banner.contains("Lead response observations unavailable."));
            assert!(!banner.contains("All good"));
            assert!(!banner.contains("<form") && !banner.contains("<button"));
        }
    }

    #[test]
    fn owner_decisions_are_distinct_from_alarms_and_unknown_is_not_healthy() {
        let mut snapshot = observed_asks();
        let ask_only = content_for(
            Page::Home,
            &View {
                situation: Some(&snapshot),
                ..View::default()
            },
            100,
        );
        let banner = ask_only
            .split("<section id=attention")
            .nth(1)
            .unwrap()
            .split("</section>")
            .next()
            .unwrap();
        assert!(banner.contains("role=status"));
        assert!(banner.contains("aria-hidden=true>?</span> Needs you</p>"));
        assert!(banner.contains(&format!("Unanswered owner decisions ({})", snapshot.asks.len())));
        assert!(!banner.contains("Alarm</p>") && !banner.contains("Lead response"));
        let mixed = content_for(
            Page::Home,
            &View {
                situation: Some(&snapshot),
                alarms: Some(&Alarms {
                    main_red: Some(true),
                    ..Alarms::default()
                }),
                ..View::default()
            },
            100,
        );
        assert!(mixed.contains("role=alert") && mixed.contains(" Alarm</p>"));
        assert!(
            mixed.find("Unanswered owner decisions").unwrap()
                < mixed.find("Checks on main are failing").unwrap()
        );
        snapshot.asks.clear();
        let queue = [PullRequest {
            number: 17,
            head: "a".repeat(40),
        }];
        let old_head = [PullRequest {
            number: 17,
            head: "b".repeat(40),
        }];
        // Unknown is not healthy: observed membership without a landing record, or
        // a landing record without membership, stays explicit and never alarms.
        for situation in [None, Some(&snapshot)] {
            for (queue, overdue_queue, expected) in [
                (None, Some(queue.as_slice()), "merge queue membership"),
                (Some(queue.as_slice()), None, "queue age"),
            ] {
                let unknown = content_for(
                    Page::Home,
                    &View {
                        situation,
                        alarms: Some(&Alarms {
                            queue: queue.map(<[PullRequest]>::to_vec),
                            overdue_queue: overdue_queue.map(<[PullRequest]>::to_vec),
                            ..Alarms::default()
                        }),
                        ..View::default()
                    },
                    100,
                );
                let banner = unknown
                    .split("<section id=attention")
                    .nth(1)
                    .unwrap()
                    .split("</section>")
                    .next()
                    .unwrap();
                assert!(banner.contains(expected));
                assert!(banner.contains("role=status"));
                assert!(!banner.contains("role=alert") && !banner.contains("All good"));
            }
            // Complete healthy observations stay quiet with and without asks.
            let healthy = |queue: Option<&[PullRequest]>, overdue: Option<&[PullRequest]>| Alarms {
                main_red: Some(false),
                main_merge_stalled: Some(false),
                stalled_runs: Some(vec![]),
                queue: queue.map(<[PullRequest]>::to_vec),
                overdue_queue: overdue.map(<[PullRequest]>::to_vec),
            };
            for (queue, overdue_queue) in [
                (Some(queue.as_slice()), Some(old_head.as_slice())),
                (Some(queue.as_slice()), Some(&[])),
            ] {
                let alarms = healthy(queue, overdue_queue);
                let quiet = content_for(
                    Page::Home,
                    &View {
                        situation,
                        alarms: Some(&alarms),
                        ..View::default()
                    },
                    100,
                );
                assert!(!quiet.contains("id=attention"));
                assert!(!quiet.contains("role=alert") && !quiet.contains("All good"));
            }
        }
    }

    #[test]
    fn main_alarm_pages_only_on_a_failed_required_check_from_the_panels_record() {
        // #1634: required-success with optional failure, pending with optional
        // failure, and a true required failure; the alarm shows only for the third.
        let head = "c".repeat(40);
        let repository = |rows: serde_json::Value| {
            serde_json::json!({"data":{"repository":{
                "id":"example-repo",
                "main":{
                    "refUpdateRule":{"requiredStatusCheckContexts":["gate"]},
                    "rules":{"totalCount":0,"nodes":[]},
                    "target":{"oid":head, "statusCheckRollup":{"contexts":{
                        "pageInfo":{"hasNextPage":false}, "nodes":rows
                    }}}
                },
                "mergeQueue":null
            }}})
        };
        let check = |name: &str, status: &str, conclusion: &str| serde_json::json!({"name":name, "status":status, "conclusion":conclusion});
        let optional = check("macos", "COMPLETED", "FAILURE");
        let healthy = |main_red: bool| Alarms {
            main_red: Some(main_red),
            main_merge_stalled: Some(false),
            stalled_runs: Some(vec![]),
            queue: Some(vec![]),
            overdue_queue: Some(vec![]),
        };
        for (rows, red) in [
            (
                vec![check("gate", "COMPLETED", "SUCCESS"), optional.clone()],
                false,
            ),
            (
                vec![check("gate", "in_progress", "FAILURE"), optional.clone()],
                false,
            ),
            (vec![check("gate", "COMPLETED", "FAILURE"), optional], true),
        ] {
            let panels = crate::lead_panels::parse(
                &serde_json::to_vec(&repository(serde_json::json!(rows))).unwrap(),
                None,
            )
            .unwrap();
            assert_eq!(panels.main_red, red, "{rows:?}");
            let alarms = healthy(panels.main_red);
            let page = content(
                &View {
                    alarms: Some(&alarms),
                    ..View::default()
                },
                100,
            );
            assert_eq!(page.contains("Checks on main are failing."), red, "{rows:?}");
            assert_eq!(page.contains("role=alert"), red, "{rows:?}");
        }
    }
    #[test]
    fn merge_alarm_uses_the_main_merge_and_sample_time() {
        use serde_json::{json, Value};

        let head = "c".repeat(40);
        let response = json!({"data":{"repository":{
            "id":"example-repo",
            "main":{"refUpdateRule":null, "rules":{"totalCount":0,"nodes":[]},
                "target":{"oid":head, "associatedPullRequests":{
                "pageInfo":{"hasNextPage":false},
                "nodes":[{"repository":{"id":"example-repo"}, "baseRefName":"main",
                    "mergeCommit":{"oid":head}, "mergedAt":"1970-01-02T00:00:00Z"}]
            }}},
            "mergeQueue":null
        }}});
        let render_at = |payload: &Value, sampled_at| {
            let panels = crate::lead_panels::parse(&serde_json::to_vec(payload).unwrap(), None).unwrap();
            let alarms = Alarms {
                main_merge_stalled: Some(panels.main_merge_stalled(sampled_at)),
                main_red: Some(panels.main_red),
                stalled_runs: Some(vec![]),
                queue: panels.queue.clone(),
                overdue_queue: Some(vec![]),
            };
            content(
                &View {
                    alarms: Some(&alarms),
                    ..View::default()
                },
                sampled_at + 3600,
            )
            .contains("No PR merged into main for 45 minutes")
        };
        const MERGED: u64 = 86_400;
        assert!(!render_at(&response, MERGED - 1));
        assert!(!render_at(&response, MERGED + 45 * 60 - 1));
        assert!(render_at(&response, MERGED + 45 * 60));
        let base = "/data/repository/main/target/associatedPullRequests";
        for (field, value) in [
            ("nodes/0/baseRefName", json!("elsewhere")),
            ("nodes/0/repository/id", json!("example-fork")),
            ("nodes/0/mergeCommit/oid", json!("d".repeat(40))),
            ("nodes/0/mergedAt", json!("1970-02-30T00:00:00Z")),
            ("pageInfo/hasNextPage", json!(true)),
            ("nodes", json!([])),
        ] {
            let mut payload = response.clone();
            *payload.pointer_mut(&format!("{base}/{field}")).unwrap() = value;
            assert!(!render_at(&payload, MERGED + 45 * 60), "{field}");
        }
    }
    #[test]
    fn live_worker_rows_render_and_each_unavailable_state_is_explicit() {
        use crate::workers::{
            registry::WorkerState,
            tool::{Binding, BindingsError},
        };
        let binding = |name: &str, state, age| Binding {
            name: name.into(),
            branch: format!("agent/{name}"),
            model: "provider/model".into(),
            state,
            started_ms: 0,
            status_age_secs: age,
        };
        let found: Result<Vec<Binding>, BindingsError> = Ok(vec![
            binding("alive", WorkerState::Working, Some(5)),
            binding("gone", WorkerState::Dead, None),
            binding("slow", WorkerState::Idle, Some(900)),
        ]);
        let page = |workers| {
            document_for(
                Page::Workers,
                &View {
                    workers,
                    ..Default::default()
                },
                100,
            )
        };
        let html = page(Some(&found));
        assert!(html.contains("agent/alive") && html.contains("Working"));
        assert!(html.contains("<h3 class=host>This host</h3>"));
        assert!(html.contains("<span class=chip>Working</span>"));
        assert!(html.contains("Dead: its process is gone"));
        assert!(html.contains("status stale (900 s old)"));
        assert!(!html.contains("Lane status unavailable"));
        let refused = Err(BindingsError::Refused);
        assert!(page(Some(&refused)).contains("Live workers refused"));
        let absent = Err(BindingsError::Absent);
        assert!(page(Some(&absent)).contains("Lane status unavailable"));
        assert!(page(None).contains("Lane status unavailable"));
    }

    #[test]
    fn workers_put_reported_stalls_first_and_keep_observation_order_within_groups() {
        use oxa_proto::work::{Event, Fact, Incident};
        let fixture = include_str!("../../oxa-proto/tests/fixtures/work/evening.jsonl");
        let mut snapshot = fixture
            .lines()
            .find_map(|line| Snapshot::from_json(line.as_bytes()).ok())
            .unwrap();
        let template = fixture
            .lines()
            .filter_map(|line| Event::from_json(line.as_bytes()).ok())
            .find_map(|event| match event.fact {
                Fact::TaskStarted(running) => Some(running),
                _ => None,
            })
            .unwrap();
        let tasks = [
            "worker-ready-a",
            "worker-stalled-b",
            "worker-ready-c",
            "worker-stalled-d",
        ];
        snapshot.running = tasks
            .iter()
            .map(|task| oxa_proto::work::Running {
                task: (*task).into(),
                ..template.clone()
            })
            .collect();
        snapshot.incidents = [
            (
                IncidentKind::WorkerStalled,
                Subject::Task("worker-stalled-d".into()),
            ),
            (
                IncidentKind::WorkerStalled,
                Subject::Task("worker-stalled-b".into()),
            ),
            (IncidentKind::MainFailed, Subject::Task("worker-ready-c".into())),
            (
                IncidentKind::WorkerStalled,
                Subject::Host("worker-ready-a".into()),
            ),
            (IncidentKind::WorkerStalled, Subject::Task("not-running".into())),
            (IncidentKind::HostIdle, Subject::Host(template.host.clone())),
            (IncidentKind::HostIdle, Subject::Host("idle-example".into())),
        ]
        .into_iter()
        .enumerate()
        .map(|(index, (kind, subject))| Incident {
            id: format!("incident-{index}"),
            kind,
            subject,
            evidence: None,
        })
        .collect();
        let render = |page, snapshot: &Snapshot| {
            content_for(
                page,
                &View {
                    situation: Some(snapshot),
                    ..View::default()
                },
                100,
            )
        };
        for page in [Page::Home, Page::Workers] {
            let html = render(page, &snapshot);
            let expected = [
                "worker-stalled-b",
                "worker-stalled-d",
                "worker-ready-a",
                "worker-ready-c",
                "idle-example",
            ];
            let positions: Vec<_> = expected
                .iter()
                .map(|task| html.find(&format!("<b>{task}</b>")).unwrap())
                .collect();
            assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
            assert_eq!(html.matches("Stall reported").count(), 2);
            assert_eq!(html.matches("Busy at observation").count(), 2);
            assert_eq!(html.matches("Idle reported").count(), 1);
            assert!(!html.contains("not-running") && !html.contains("Verified"));
            assert!(!html.contains(&format!("<b>{}</b>", template.host)));
            assert!(!html.contains(&template.worktree));
            assert!(!html.contains(template.session_id.as_ref().unwrap()));
        }
        snapshot.incidents.clear();
        let page = render(Page::Workers, &snapshot);
        let positions: Vec<_> = tasks
            .iter()
            .map(|task| page.find(&format!("<b>{task}</b>")).unwrap())
            .collect();
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(!page.contains("Stall reported") && !page.contains("Idle reported"));
        snapshot.running.clear();
        assert!(render(Page::Workers, &snapshot).contains("Idle worker inventory is unavailable"));
        assert!(content_for(Page::Workers, &View::default(), 100).contains("Lane status unavailable"));
    }

    #[test]
    fn workers_group_runner_tasks_by_host_and_chip_their_state() {
        use oxa_proto::work::{Event, Fact, Incident};
        let fixture = include_str!("../../oxa-proto/tests/fixtures/work/evening.jsonl");
        let mut snapshot = fixture
            .lines()
            .find_map(|line| Snapshot::from_json(line.as_bytes()).ok())
            .unwrap();
        let template = fixture
            .lines()
            .filter_map(|line| Event::from_json(line.as_bytes()).ok())
            .find_map(|event| match event.fact {
                Fact::TaskStarted(running) => Some(running),
                _ => None,
            })
            .unwrap();
        // Two hosts interleaved in observation order, one reported stall on each.
        let running = |task: &str, host: &str| oxa_proto::work::Running {
            task: task.into(),
            host: host.into(),
            ..template.clone()
        };
        snapshot.running = vec![
            running("ready-a1", "host-a"),
            running("stalled-b1", "host-b"),
            running("stalled-a2", "host-a"),
            running("ready-b2", "host-b"),
        ];
        snapshot.incidents = [
            (IncidentKind::WorkerStalled, Subject::Task("stalled-a2".into())),
            (IncidentKind::WorkerStalled, Subject::Task("stalled-b1".into())),
        ]
        .into_iter()
        .enumerate()
        .map(|(index, (kind, subject))| Incident {
            id: format!("incident-{index}"),
            kind,
            subject,
            evidence: None,
        })
        .collect();
        let html = content_for(
            Page::Workers,
            &View {
                situation: Some(&snapshot),
                ..View::default()
            },
            100,
        );
        // Host headings keep first-seen order; within a host the stall comes first.
        let order = [
            "<h3 class=host>host-a</h3>",
            "<b>stalled-a2</b>",
            "<b>ready-a1</b>",
            "<h3 class=host>host-b</h3>",
            "<b>stalled-b1</b>",
            "<b>ready-b2</b>",
        ];
        let positions: Vec<_> = order.iter().map(|needle| html.find(needle).unwrap()).collect();
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(html.matches("class=\"chip bad\"").count(), 2);
        assert_eq!(
            html.matches("<span class=chip>Busy at observation</span>")
                .count(),
            2
        );
    }

    #[test]
    fn board_scroll_region_and_table_headers_are_available_without_javascript() {
        let board = Board {
            observed_at: 100,
            entries: vec![Entry {
                number: 17,
                head: "private-head-marker".into(),
                release_pinned: false,
                action: Action::WaitReview,
                lease: None,
                attempts_today: 2,
                eligible: true,
                evidence_incomplete: None,
            }],
        };
        for page in [Page::Home, Page::Board, Page::BoardQuery(Query::default())] {
            let html = content_for(
                page,
                &View {
                    board: Some(&board),
                    ..View::default()
                },
                100,
            );
            assert!(html.contains("class=scroll role=region aria-labelledby=board-table-title tabindex=0"));
            assert_eq!(html.matches("id=board-table-title").count(), 1);
            assert!(html.contains("<caption id=board-table-title>Observed board items</caption>"));
            for label in ["PR", "Next action", "Priority", "Lease", "Attempts today"] {
                assert!(html.contains(&format!("<th scope=col>{label}</th>")));
            }
            assert!(html.contains("<th scope=row><a href=/items/17>#17</a></th>"));
            for value in ["Wait for review", "Normal", "Unleased", "2"] {
                assert!(html.contains(&format!("<td>{value}</td>")));
            }
            assert!(html.contains("class=board-rows"));
            assert!(html.contains("<dt>Next action</dt><dd>Wait for review</dd>"));
            assert!(!html.contains("private-head-marker") && !html.contains("onclick="));
        }
        let empty = content_for(
            Page::Board,
            &View {
                board: Some(&Board::default()),
                ..View::default()
            },
            100,
        );
        assert!(empty.contains("No open PRs in this snapshot."));
        assert!(empty.contains("<caption id=board-table-title>Observed board items</caption>"));
        let unavailable = content_for(Page::Board, &View::default(), 100);
        assert!(unavailable.contains("Board unavailable."));
        assert!(!unavailable.contains("role=region") && !unavailable.contains("<table"));
        assert!(!unavailable.contains("board-rows"));
    }

    #[test]
    fn fake_board_uses_policy_and_does_not_render_source_payloads() {
        struct Fake;
        impl GitHub for Fake {
            fn open_pull_requests(&mut self) -> oxa_work::Result<Vec<BoardPr>> {
                Ok(vec![BoardPr {
                    number: 17,
                    head: "private-head-marker".into(),
                    release_pinned: true,
                    findings: true,
                    required_ci: RequiredCi::Passed,
                    mergeability: Mergeability::Mergeable,
                    body: "private-body-marker".into(),
                    removes_source: false,
                    draft: false,
                    reviewed: false,
                }])
            }
        }
        let board = Board::read(
            &mut Fake,
            &Policy {
                enabled: true,
                ..Policy::default()
            },
            &Default::default(),
            100,
        )
        .unwrap();
        let page = document(
            &View {
                board: Some(&board),
                ..View::default()
            },
            100,
        );
        for expected in ["#17", "Fix findings", "Release", "Unleased"] {
            assert!(page.contains(expected));
        }
        for private in ["private-head-marker", "private-body-marker"] {
            assert!(!page.contains(private));
        }
        assert!(document(
            &View {
                board: Some(&board),
                ..View::default()
            },
            2000
        )
        .contains("Stale snapshot"));
    }
    #[test]
    fn escapes_text_and_does_not_infer_missing_facts() {
        let items = vec![(false, "<script>example</script> & work".into())];
        let page = document(
            &View {
                release: Some((19, &items)),
                ..View::default()
            },
            100,
        );
        assert!(page.contains("&lt;script&gt;example&lt;/script&gt; &amp; work"));
        assert!(!page.contains("<script>example"));
        for unknown in [
            "Board unavailable",
            "Lane status unavailable",
            "Merge queue unavailable",
            "Owner asks unavailable",
        ] {
            assert!(page.contains(unknown));
        }
        assert!(page.contains("0 / 1 checkboxes reported checked"));
    }

    fn usage_page(observation: Option<&oxa_work::Result<Option<oxa_work::TaskUsage>>>) -> String {
        content_for(
            Page::Usage,
            &View {
                selected_task_usage: observation,
                ..View::default()
            },
            100,
        )
    }

    #[test]
    fn selected_usage_preserves_unavailable_absent_and_saved_unknown_states() {
        let failure = Err("private-error-marker".into());
        for observation in [None, Some(&failure)] {
            let page = usage_page(observation);
            assert!(page.contains("Task usage observations unavailable."));
            assert!(!page.contains("No saved checkpoint") && !page.contains("private-error-marker"));
        }
        let absent = Ok(None);
        let page = usage_page(Some(&absent));
        assert!(page.contains("No saved checkpoint for the selected task."));
        assert!(!page.contains("Task usage observations unavailable."));
        let unknown = Ok(Some(oxa_work::TaskUsage {
            task: "<task>&example".into(),
            checkpoint_revision: 41,
            task_tokens: None,
        }));
        let page = usage_page(Some(&unknown));
        assert!(page.contains("Task &lt;task&gt;&amp;example"));
        assert!(page.contains("Checkpoint revision 41"));
        assert!(page.contains("Saved token aggregate unknown."));
        assert!(!page.contains("<task>") && !page.contains("<dd>0</dd>"));
        assert!(!page.contains("Prompt tokens"));
    }

    #[test]
    fn selected_usage_keeps_measured_zero_and_partial_counters_distinct() {
        let complete_zero = Ok(Some(oxa_work::TaskUsage {
            task: "selected-example".into(),
            checkpoint_revision: 42,
            task_tokens: Some(oxa_work::TaskTokens {
                attempts: 1,
                complete_attempts: 1,
                prompt: Some(0),
                completion: Some(0),
                cached: Some(0),
                complete: true,
            }),
        }));
        let page = usage_page(Some(&complete_zero));
        assert!(page.contains("Usage coverage: complete"));
        assert!(page.contains("<dt>Task attempts</dt><dd>1</dd>"));
        assert!(page.contains("<dt>Attempts with complete usage</dt><dd>1</dd>"));
        assert!(page.contains("<dt>Prompt tokens</dt><dd>0</dd>"));
        assert!(page.contains("<dt>Completion tokens</dt><dd>0</dd>"));
        assert!(page.contains("<dt>Cached input tokens</dt><dd>0</dd>"));
        assert!(page.contains("Task-scoped checkpoint; merged-PR attribution unavailable."));
        assert!(page.contains("GitHub App · GraphQL</dt><dd>Unknown"));
        assert!(page.contains("GitHub Owner · REST core</dt><dd>Unknown"));
        assert!(page.contains("Primary model window</dt><dd>Unknown"));
        assert!(page.contains("aria-current=page>Usage"));
        assert!(!page.contains("<form") && !page.contains("<button"));
        let partial = Ok(Some(oxa_work::TaskUsage {
            task: "selected-example".into(),
            checkpoint_revision: 43,
            task_tokens: Some(oxa_work::TaskTokens {
                attempts: 2,
                complete_attempts: 1,
                prompt: None,
                completion: Some(0),
                cached: None,
                complete: false,
            }),
        }));
        let page = usage_page(Some(&partial));
        assert!(page.contains("Usage coverage: partial"));
        assert!(!page.contains("Usage coverage: complete"));
        assert!(page.contains("<dt>Task attempts</dt><dd>2</dd>"));
        assert!(page.contains("<dt>Prompt tokens</dt><dd>Unknown</dd>"));
        assert!(page.contains("<dt>Completion tokens</dt><dd>0</dd>"));
        assert!(page.contains("<dt>Cached input tokens</dt><dd>Unknown</dd>"));
        assert!(page.contains("Cached tokens are included in prompt when both are reported."));
    }

    #[test]
    fn selected_partial_usage_keeps_large_known_fields_without_inventing_totals() {
        let usage = Ok(Some(oxa_work::TaskUsage {
            task: "selected-example".into(),
            checkpoint_revision: 44,
            task_tokens: Some(oxa_work::TaskTokens {
                attempts: 2,
                complete_attempts: 1,
                prompt: Some(u64::MAX),
                completion: Some(u64::MAX),
                cached: Some(1),
                complete: false,
            }),
        }));
        let page = usage_page(Some(&usage));
        assert!(page.contains("Usage coverage: partial"));
        for label in ["Prompt tokens", "Completion tokens"] {
            assert!(page.contains(&format!("<dt>{label}</dt><dd>{}</dd>", u64::MAX)));
        }
        assert!(!page.contains("Total tokens") && !page.contains("Estimated cost"));
        assert!(page.contains("merged-PR attribution unavailable"));
    }

    #[test]
    fn item_state_pairs_native_labels_with_observed_values_and_keeps_unknowns_explicit() {
        let mut board = Board {
            observed_at: 100,
            entries: vec![Entry {
                number: 17,
                head: "private-head-marker".into(),
                release_pinned: true,
                action: Action::FixFindings,
                lease: Some(oxa_work::board::Lease {
                    holder: "worker-example".into(),
                    expires_at: 200,
                    claimed_at: Some(90),
                    generation: (0, 1),
                }),
                attempts_today: 2,
                eligible: true,
                evidence_incomplete: None,
            }],
        };
        let render = |board: Option<&Board>| {
            content_for(
                Page::Item(17),
                &View {
                    board,
                    ..View::default()
                },
                201,
            )
        };
        let page = render(Some(&board));
        let lease_time = format!(
            "<dt>Lease</dt><dd>worker-example · expires {} (expired)</dd>",
            human_time(200, 201)
        );
        let observed_time = format!("<dt>Observed at</dt><dd>{}</dd>", human_time(100, 201));
        for expected in [
            "<h1 id=main tabindex=-1>Item #17</h1>",
            "href=/board>Back to Board</a>",
            "aria-current=page>Board",
            "<h2>Next action</h2><p>Fix findings</p>",
            "<h2>State</h2><dl>",
            "<dt>Priority</dt><dd>Release</dd>",
            lease_time.as_str(),
            "<dt>Attempts today</dt><dd>2</dd>",
            observed_time.as_str(),
            "Title, checks, findings and diff observations unavailable.",
        ] {
            assert!(page.contains(expected), "{expected}");
        }
        for label in [
            "Priority",
            "Lease",
            "Attempts today",
            "Observed at",
            "Title",
            "Checks",
            "Review state",
            "Findings",
            "Diff",
        ] {
            assert!(page.contains(&format!("<dt>{label}</dt>")), "{label}");
        }
        assert!(page.contains("Unavailable in this snapshot"));
        assert!(!page.contains("private-head-marker"));
        assert!(!page.contains("<form") && !page.contains("<button"));
        board.entries[0].release_pinned = false;
        board.entries[0].lease = None;
        let unleased = render(Some(&board));
        assert!(unleased.contains("<dt>Priority</dt><dd>Normal priority</dd>"));
        assert!(unleased.contains("<dt>Lease</dt><dd>Unleased</dd>"));
        board.entries[0].lease = Some(oxa_work::board::Lease {
            holder: "<worker>&example".into(),
            expires_at: 300,
            claimed_at: Some(90),
            generation: (0, 1),
        });
        let leased = render(Some(&board));
        assert!(leased.contains(&format!(
            "&lt;worker&gt;&amp;example · expires {}</dd>",
            human_time(300, 201)
        )));
        assert!(!leased.contains("(expired)") && !leased.contains("<worker>"));
        board.entries.clear();
        for (source, expected) in [
            (None, "Board unavailable."),
            (Some(&board), "Item unavailable in this snapshot."),
        ] {
            let missing = render(source);
            assert!(missing.contains(expected));
            assert!(!missing.contains("<dl>") && !missing.contains("<dt>"));
        }
    }

    #[test]
    fn missing_states_name_source_cause_age_and_next_step() {
        // #2342: every observation gap names its source, an allow-listed cause,
        // the age of the last good value and the next step; never a bare
        // "unavailable" and never "the source did not say why".
        let html = document_for(Page::Home, &View::default(), 100);
        assert!(!html.contains("did not say why"));
        for source in [
            "Board unavailable.",
            "Lane status unavailable.",
            "Merge queue unavailable.",
            "Owner asks unavailable.",
        ] {
            assert!(html.contains(source), "{source}");
        }
        assert!(html.contains("No good value has been read yet."));
        assert!(html.contains("say-why"));
        // A read cause reaches the view from the allowlist and is shown verbatim.
        let caused = document_for(
            Page::Board,
            &View {
                board_cause: Some(crate::lead_input::BoardCause::Unreadable),
                ..View::default()
            },
            100,
        );
        assert!(caused.contains("not readable by this owner"));
        assert!(caused.contains("owner-only"));
        assert!(!caused.contains("did not say why"));
        for invented in ["Sign in again", "not configured", "Permission denied"] {
            assert!(!caused.contains(invented));
        }
    }

    #[test]
    fn five_missing_alarm_sources_collapse_into_one_line() {
        let html = content_for(
            Page::Home,
            &View {
                alarms: Some(&Alarms::default()),
                ..View::default()
            },
            100,
        );
        let banner = html
            .split("<section id=attention")
            .nth(1)
            .unwrap()
            .split("</section>")
            .next()
            .unwrap();
        assert_eq!(banner.matches("Not in this sample:").count(), 1);
        for name in [
            "main checks",
            "no-merge alarm",
            "merge queue membership",
            "queue age",
            "queued workflow runs",
        ] {
            assert!(banner.contains(name), "{name}");
        }
        assert!(banner.contains("role=status"));
        assert!(!banner.contains("role=alert"));
        // A complete healthy observation stays quiet.
        let healthy = Alarms {
            main_red: Some(false),
            main_merge_stalled: Some(false),
            stalled_runs: Some(vec![]),
            queue: Some(vec![]),
            overdue_queue: Some(vec![]),
        };
        let quiet = content_for(
            Page::Home,
            &View {
                alarms: Some(&healthy),
                ..View::default()
            },
            100,
        );
        assert!(!quiet.contains("id=attention"));
    }

    #[test]
    fn ages_render_as_client_ticking_time_elements() {
        let board = Board {
            observed_at: 40,
            ..Board::default()
        };
        let html = content_for(
            Page::Board,
            &View {
                board: Some(&board),
                ..View::default()
            },
            100,
        );
        assert!(html.contains("<time data-at=\"40\">1 min old</time>"));
        // The script re-labels ages every 10 s with no request.
        assert!(SCRIPT.contains("time[data-at]"));
        assert!(SCRIPT.contains("scheduleAges"));
        // A retained board shows its age and cause in the banner.
        let retained = document_for(
            Page::Board,
            &View {
                board: Some(&board),
                board_retained: true,
                board_cause: Some(crate::lead_input::BoardCause::Changed),
                ..View::default()
            },
            100,
        );
        assert!(retained.contains("Board is last-good"));
        assert!(retained.contains("changed while it was being read"));
        assert!(retained.contains("<time data-at=\"40\">"));
        assert!(retained.contains("Last-good snapshot"));
        // An observation the host clock cannot format leaks no raw value into the
        // age (the host-zone test guards this across pages; here it guards age()).
        let invalid = content_for(
            Page::Board,
            &View {
                board: Some(&Board {
                    observed_at: u64::MAX,
                    ..Board::default()
                }),
                ..View::default()
            },
            100,
        );
        assert!(!invalid.contains(&u64::MAX.to_string()));
        assert!(invalid.contains("age not available"));
    }

    #[test]
    fn retained_board_keeps_last_good_rows_with_cause_and_age() {
        let board = Board {
            observed_at: 40,
            entries: vec![Entry {
                number: 17,
                head: "synthetic-head".into(),
                release_pinned: false,
                action: Action::Ready,
                lease: None,
                attempts_today: 0,
                eligible: true,
                evidence_incomplete: None,
            }],
        };
        let html = content_for(
            Page::Board,
            &View {
                board: Some(&board),
                board_retained: true,
                board_cause: Some(crate::lead_input::BoardCause::Changed),
                ..View::default()
            },
            100,
        );
        // The last good row stays visible; the cause and age are named.
        assert!(html.contains("href=/items/17"));
        assert!(html.contains("Board is last-good"));
        assert!(html.contains("changed while it was being read"));
        assert!(html.contains("Last good value"));
        assert!(!html.contains("Board unavailable."));
    }
}
