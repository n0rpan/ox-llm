//! Read-only web projection. Never serialize runner records or arbitrary GitHub payloads.
use oxa_proto::work::{IncidentKind, PullRequest, Snapshot, Subject};
use oxa_work::board::{Action, Board, Entry};
use std::fmt::Write;

mod chat;
mod filters;
mod pages;
mod quotas;
pub use filters::{Filter, Query};
pub use pages::{Page, Prompt};

/// Borrowed presentation inputs, not a second runner state store. None means unavailable.
#[derive(Default)]
pub struct View<'a> {
    /// Ephemeral form credential supplied only by the authorized HTTP caller.
    pub fixed_prompt_csrf: Option<&'a str>,
    /// Canonical goal and matching native observation from the authenticated caller.
    /// The caller retains full SessionRef/revision authority; identities are never rendered.
    pub owner_goal: Option<(&'a oxa_session::Goal, oxa_core::goal::GoalObservation<'a>)>,
    pub board: Option<&'a Board>,
    pub situation: Option<&'a Snapshot>,
    /// C's selected checkpoint observation; the renderer never acquires runner state.
    pub selected_task_usage: Option<&'a oxa_work::Result<Option<oxa_work::TaskUsage>>>,
    pub github_quotas: Option<&'a crate::lead_quota::GitHubQuotas>,
    pub model_plan: Option<&'a oxc_gateway::status::chatgpt_usage::Usage>,
    pub main_red: bool,
    pub main_merge_stalled: bool,
    pub stalled_runs: &'a [u64],
    pub queue: Option<&'a [PullRequest]>,
    /// Exact PR heads for which the landing policy raised its queue age alarm.
    pub overdue_queue: &'a [PullRequest],
    /// Source issue and its reported checkbox states; these do not establish validation.
    pub release: Option<(u64, &'a [(bool, String)])>,
}

fn escape(text: &str) -> String {
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
fn human_time(timestamp: u64, now: u64) -> String {
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
    let refresh = if page == Page::Home {
        "<noscript><meta http-equiv=refresh content=60></noscript>"
    } else {
        ""
    };
    format!("<!doctype html><html lang=en><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1,viewport-fit=cover\"><meta name=color-scheme content=\"light dark\"><title>oxa lead</title><link rel=stylesheet href=/style.css><script src=/live.js defer></script>{refresh}<a class=skip href=#main>Skip to content</a><header class=top><a href=/>oxa <b>lead</b></a><span id=connection class=age>Snapshot</span></header>{}</html>", content_for(page, view, now))
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
    if view.board.is_some_and(|board| stale(board, now)) {
        html.push_str("<aside class=stale-notice role=status><h2>Board observation needs refreshing</h2><p>The Board timestamp is stale or ahead of this clock. <a href=\"\">Retry</a></p></aside>");
    }
    match page {
        Page::Home => {
            html.push_str("<div class=home-layout><div class=home-decisions>");
            attention(&mut html, view);
            asks(&mut html, view, now);
            html.push_str("</div><div class=home-overview><section><h2>Snapshot overview</h2>");
            if let Some(board) = view.board {
                let _ = write!(
                    html,
                    "<p><a href=/board>{} observed items</a></p><p class=muted>{} · observed {}</p>",
                    board.entries.len(),
                    if stale(board, now) {
                        "Stale snapshot"
                    } else {
                        "Board snapshot"
                    },
                    human_time(board.observed_at, now)
                );
            } else {
                html.push_str("<p>Board unavailable.</p>");
            }
            html.push_str("</section><details class=home-work><summary>Observed work</summary>");
            board(&mut html, view, now, Query::default());
            workers(&mut html, view, now);
            queue(&mut html, view);
            release(&mut html, view);
            html.push_str("</details></div></div>");
        }
        Page::Board | Page::BoardQuery(_) => {
            let query = if let Page::BoardQuery(query) = page {
                query
            } else {
                Query::default()
            };
            board(&mut html, view, now, query);
            queue(&mut html, view);
        }
        Page::Workers => workers(&mut html, view, now),
        Page::Asks => {
            html.push_str("<div class=asks-layout>");
            asks(&mut html, view, now);
            html.push_str("</div>");
        }
        Page::Item(number) => item(&mut html, view, number, now),
        Page::Chat(prompt) => chat::render(&mut html, view, prompt, now),
        Page::Usage => {
            usage(&mut html, view);
            quotas::render(&mut html, view, now);
        }
    }
    html.push_str("<footer>Read-only · Chat with the lead is not connected yet.</footer></main>");
    html
}

fn attention(html: &mut String, view: &View<'_>) {
    let ask_count = view.situation.map_or(0, |s| s.asks.len());
    let overdue: Vec<_> = view
        .queue
        .into_iter()
        .flatten()
        .filter(|pr| view.overdue_queue.contains(pr))
        .collect();
    let alarm =
        view.main_red || view.main_merge_stalled || !view.stalled_runs.is_empty() || !overdue.is_empty();
    if alarm || ask_count > 0 {
        let (role, icon, label) = if alarm {
            ("alert", "!", "Alarm")
        } else {
            ("status", "?", "Needs you")
        };
        let _ = write!(
            html,
            "<section id=attention role={role}><p class=severity><span aria-hidden=true>{icon}</span> {label}</p><h2>Needs attention</h2><ul>"
        );
        if ask_count > 0 {
            let _ = write!(
                html,
                "<li><a href=#asks>Unanswered owner decisions ({ask_count})</a></li>"
            );
        }
        if view.main_red {
            html.push_str("<li>Checks on main are failing.</li>");
        }
        if view.main_merge_stalled {
            html.push_str("<li>No PR merged into main for 45 minutes or more at the last observation.</li>");
        }
        for pr in overdue {
            let _ = write!(
                html,
                "<li><a href=/board#queue>PR #{} has waited too long in the merge queue</a></li>",
                pr.number
            );
        }
        if !view.stalled_runs.is_empty() {
            let _ = write!(
                html,
                "<li><details><summary>Workflow runs queued for more than 15 minutes ({})</summary><ul>",
                view.stalled_runs.len()
            );
            for id in view.stalled_runs {
                let _ = write!(html, "<li>Run {id}</li>");
            }
            html.push_str("</ul></details></li>");
        }
        html.push_str("</ul>");
        if alarm {
            html.push_str("<p class=muted>Lead response observations unavailable.</p>");
        }
        html.push_str("</section>");
    }
}

fn stale(board: &Board, now: u64) -> bool {
    board.observed_at > now.saturating_add(60) || now.saturating_sub(board.observed_at) > 900
}

fn board(html: &mut String, view: &View<'_>, now: u64, query: Query) {
    html.push_str("<section id=board><h2>Board</h2>");
    if let Some(board) = view.board {
        let _ = write!(
            html,
            "<p class=muted>{} · observed {}</p>",
            if stale(board, now) {
                "Stale snapshot"
            } else {
                "GitHub snapshot"
            },
            human_time(board.observed_at, now)
        );
        query.navigation(html, board, view);
        if !query.filter.available(view) {
            html.push_str("<p>Blocked observations unavailable.</p></section>");
            return;
        }
        let rows = query.rows(board, view);
        html.push_str("<div class=scroll role=region aria-labelledby=board-table-title tabindex=0><table><caption id=board-table-title>Observed board items</caption><thead><tr><th scope=col>PR</th><th scope=col>Next action</th><th scope=col>Priority</th><th scope=col>Lease</th><th scope=col>Attempts today</th></tr></thead><tbody>");
        let offset = query.page.saturating_mul(pages::BOARD_ROWS);
        for row in rows.iter().skip(offset).take(pages::BOARD_ROWS) {
            let lease = lease(row, now);
            let _ = write!(
                html,
                "<tr><th scope=row><a href=/items/{0}>#{0}</a></th><td>{1}</td><td>{2}</td><td>{lease}</td><td>{3}</td></tr>",
                row.number,
                action(row.action),
                if row.release_pinned { "Release" } else { "Normal" },
                row.attempts_today
            );
        }
        html.push_str("</tbody></table></div>");
        if board.entries.is_empty() {
            html.push_str("<p>No open PRs in this snapshot.</p>");
        } else {
            let shown = rows.len().saturating_sub(offset).min(pages::BOARD_ROWS);
            let _ = write!(html, "<p>Showing {shown} of {} items</p>", rows.len());
            html.push_str("<nav class=pages aria-label=\"Board pages\">");
            if query.page > 0 {
                let path = escape(&Query { page: 0, ..query }.url());
                let _ = write!(html, "<a href=\"{path}\">First page</a>");
            }
            if offset.saturating_add(pages::BOARD_ROWS) < rows.len() {
                let path = escape(
                    &Query {
                        page: query.page + 1,
                        ..query
                    }
                    .url(),
                );
                let _ = write!(html, "<a href=\"{path}\">Show more</a>");
            }
            html.push_str("</nav>");
        }
    } else {
        html.push_str("<p>Board unavailable.</p>");
    }
    html.push_str("</section>");
}

fn lease(row: &Entry, now: u64) -> String {
    row.lease.as_ref().map_or("Unleased".into(), |l| {
        format!(
            "{} · expires {}{}",
            escape(&l.holder),
            human_time(l.expires_at, now),
            if l.expires_at <= now { " (expired)" } else { "" }
        )
    })
}

fn item(html: &mut String, view: &View<'_>, number: u64, now: u64) {
    html.push_str("<p><a href=/board>Back to Board</a></p>");
    let Some(board) = view.board else {
        html.push_str("<p>Board unavailable.</p>");
        return;
    };
    let Some(row) = board.entries.iter().find(|row| row.number == number) else {
        html.push_str("<p>Item unavailable in this snapshot.</p>");
        return;
    };
    let _ = write!(html,
        "<section><article><h2>Next action</h2><p>{}</p></article></section><section><article><h2>State</h2><dl><dt>Priority</dt><dd>{}</dd><dt>Lease</dt><dd>{}</dd><dt>Attempts today</dt><dd>{}</dd><dt>Observed at</dt><dd>{}</dd></dl></article></section>",
        action(row.action), if row.release_pinned { "Release" } else { "Normal priority" },
        lease(row, now), row.attempts_today, human_time(board.observed_at, now));
    html.push_str("<section><h2>Details and changes</h2><p>Title, checks, findings and diff observations unavailable.</p></section>");
}

fn usage(html: &mut String, view: &View<'_>) {
    html.push_str("<section id=task-usage><h2>Task usage</h2>");
    match view.selected_task_usage {
        None | Some(Err(_)) => html.push_str("<p>Task usage observations unavailable.</p>"),
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
                html.push_str("<p>Saved token aggregate unknown.</p>");
            }
            html.push_str(
                "<p class=muted>Task-scoped checkpoint; merged-PR attribution unavailable.</p></article>",
            );
        }
    }
    html.push_str("</section>");
}

fn workers(html: &mut String, view: &View<'_>, now: u64) {
    html.push_str("<section id=lanes><h2>Workers</h2>");
    if let Some(s) = view.situation {
        let _ = write!(
            html,
            "<p class=muted>Runner facts · sequence {} · observed {}</p>",
            s.as_of_seq,
            human_time(s.at_ms / 1000, now)
        );
        let mut ordered: Vec<_> = s
            .running
            .iter()
            .map(|running| {
                let stalled = s.incidents.iter().any(|i| {
                    i.kind == IncidentKind::WorkerStalled && i.subject == Subject::Task(running.task.clone())
                });
                (stalled, running)
            })
            .collect();
        ordered.sort_by_key(|(stalled, _)| !stalled);
        for (stalled, running) in ordered {
            let _ = write!(
                html,
                "<article><b>{}</b><p>{}</p></article>",
                escape(&running.task),
                if stalled {
                    "Stall reported"
                } else {
                    "Busy at observation"
                }
            );
        }
        if s.running.is_empty() {
            html.push_str("<p>No running tasks reported. Idle worker inventory is unavailable.</p>");
        }
        for incident in &s.incidents {
            if let Subject::Host(host) = &incident.subject {
                if incident.kind == IncidentKind::HostIdle && !s.running.iter().any(|r| &r.host == host) {
                    let _ = write!(
                        html,
                        "<article><b>{}</b><p>Idle reported</p></article>",
                        escape(host)
                    );
                }
            }
        }
    } else {
        html.push_str("<p>Lane status unavailable.</p>");
    }
    html.push_str("</section>");
}

fn queue(html: &mut String, view: &View<'_>) {
    html.push_str("<section id=queue><h2>Merge queue</h2>");
    if let Some(queue) = view.queue {
        for (index, pr) in queue.iter().enumerate() {
            let _ = write!(html, "<article>{}. PR #{}</article>", index + 1, pr.number);
        }
        if queue.is_empty() {
            html.push_str("<p>Queue empty at observation.</p>");
        }
    } else {
        html.push_str("<p>Merge queue unavailable. Ready to land does not establish queue membership.</p>");
    }
    html.push_str("</section>");
}

fn release(html: &mut String, view: &View<'_>) {
    html.push_str("<section id=release><h2>Release checklist</h2>");
    if let Some((number, items)) = view.release {
        let _ = write!(html, "<p class=muted>Issue #{number} · {} / {} reported complete; validation requires merge evidence.</p><ul>", items.iter().filter(|i| i.0).count(), items.len());
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
        html.push_str("<p>Release checklist unavailable.</p>");
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
            let _ = write!(html, "<article><p class=muted>Ask {} · {}</p><h3>{}</h3><p>Options, recommendation and meanwhile are unavailable in these facts.</p></article>", escape(&ask.id), subject(&ask.subject), escape(&ask.question));
        }
        html.push_str("</div>");
        if s.asks.is_empty() {
            html.push_str("<p>No open asks in these runner facts.</p>");
        }
    } else {
        html.push_str("<p>Owner asks unavailable.</p>");
    }
    html.push_str("</section>");
}

pub const STYLE: &str = concat!(
    include_str!("lead_page/style.css"),
    r#"
:root { --nav-height:3.875rem; }
.scroll caption { text-align:left; padding:12px; font-weight:600; }
.scroll th[scope=row]>a { display:inline-flex; align-items:center; min-height:44px; }
.stale-notice { padding:16px; margin:16px 0; border:1px dashed var(--muted); border-radius:var(--radius); background:var(--raised); }
:root { --bad:#b62c24; --bad-soft:#fde7e4; }
@media(prefers-color-scheme:dark) { :root { --bad:#ff8a80; --bad-soft:#331917; } }
#attention .severity { font-weight:700; color:var(--warn); }
#attention[role=alert] { background:var(--bad-soft); }
#attention[role=alert] h2,#attention[role=alert] .severity { color:var(--bad); }
nav.tabs a { position:relative; flex-direction:column; gap:.125rem; font-size:.75rem; line-height:1.25; }
nav.tabs svg { order:-1; flex:none; width:1.375rem; height:1.375rem; }
nav.tabs .badge {
  position:absolute; top:.25rem; right:.25rem; max-width:calc(100% - .5rem);
  padding:.125rem .375rem; border-radius:1rem; font-size:.71875rem;
  color:var(--warn); background:var(--warn-soft); white-space:nowrap;
  overflow:hidden; text-overflow:ellipsis;
}
nav.tabs .badge.quiet { color:var(--muted); background:var(--raised); }
.home-decisions,.home-overview,.ask-cards>article { min-width:0; }
.home-work { border:1px solid var(--line); border-radius:var(--radius); padding:16px; background:var(--surface); }
.home-work>summary { min-height:44px; font-weight:600; }
.ask-cards { display:grid; gap:14px; }
.ask-cards>article { margin:0; }
@media(min-width:900px) {
  body { padding-left:5.75rem; }
  nav.tabs { width:5.75rem; }
  .home-layout { display:grid; grid-template-columns:minmax(0,1.25fr) minmax(0,1fr); gap:24px; align-items:start; }
  .asks-layout .ask-cards { grid-template-columns:repeat(2,minmax(0,1fr)); }
  nav.tabs a { min-height:4.5rem; padding:.625rem .25rem; justify-content:center; font-size:.78125rem; }
}
.quota-windows dd { margin:0 0 16px; overflow-wrap:anywhere; }
.quota-windows meter { display:block; width:100%; max-width:24rem; margin-top:6px; }
"#
);

pub const SCRIPT: &str = r#"const badge=document.getElementById('connection');
let timer,pending;
function readerActive(){return document.querySelector('main').contains(document.activeElement)&&document.activeElement.matches('a,button,input,textarea,select,summary,[tabindex]');}
async function refresh(){
  clearTimeout(timer);
  if(document.hidden)return;
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
    if(pending===active&&!readerActive()){
      const current=document.querySelector('main'),work=current.querySelector('details.home-work');
      const replacement=next.querySelector('details.home-work');
      if(work&&replacement)replacement.open=work.open;
      current.replaceWith(next);badge.textContent='Connected';
    }
  }catch(error){if(!active.signal.aborted){
    badge.textContent='Refresh unavailable';
    const notice=document.getElementById('refresh-error');
    if(notice)notice.hidden=false;
  }}
  finally{if(pending===active){pending=null;if(!document.hidden)timer=setTimeout(refresh,15000);}}
}
document.addEventListener('visibilitychange',()=>{
  clearTimeout(timer);if(pending)pending.abort();if(!document.hidden)refresh();
});
timer=setTimeout(refresh,15000);
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use oxa_work::board::{GitHub, Mergeability, Policy, PullRequest as BoardPr, RequiredCi};

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
        for page in [Page::Home, Page::Chat(Prompt::Status)] {
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
        for page in [Page::Home, Page::Chat(Prompt::Status)] {
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
        let before = owner_goal_html(Page::Home, &goal, &reference, None);
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
            }],
        };
        let ready = owner_goal_html(Page::Home, &goal, &reference, Some(&board));
        assert_eq!(owner_goal_section(&before), owner_goal_section(&ready));
        let plan = goal.plan.as_mut().unwrap();
        plan.acceptance[0].result = plan.subgoals[1].acceptance[0].result.clone();
        plan.subgoals[0].acceptance[0].result.as_mut().unwrap().passed = true;
        assert!(plan.verified());
        let all_pass = owner_goal_html(Page::Home, &goal, &reference, Some(&board));
        let section = owner_goal_section(&all_pass);
        assert!(section.contains("Every acceptance check has passing evidence recorded"));
        assert!(section.contains("Recorded state: pursuing") && !section.contains("Recorded state: done"));
    }

    #[test]
    fn missing_or_inconsistent_goal_observations_never_promote_acceptance() {
        let absent = document_for(Page::Home, &View::default(), 100);
        assert!(owner_goal_section(&absent).contains("No bound native goal observation reached this page"));
        let (mut goal, reference) = owner_goal_fixture();
        for (revision, status) in [
            (0, Some(goal.status)),
            (7, None),
            (7, Some(oxa_session::GoalStatus::Paused)),
        ] {
            let html = document_for(
                Page::Home,
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
        let invalid = owner_goal_html(Page::Home, &goal, &reference, None);
        assert!(owner_goal_section(&invalid).contains("Goal observation unavailable"));
        goal.plan = None;
        goal.status = oxa_session::GoalStatus::Done;
        goal.evidence = Some("Legacy outcome reported".into());
        let legacy = owner_goal_html(Page::Home, &goal, &reference, None);
        let section = owner_goal_section(&legacy);
        assert!(section.contains("done reported; acceptance plan unavailable"));
        assert!(section.contains("The native goal has no saved acceptance plan"));
        assert!(!section.contains("Every acceptance check has passing evidence recorded"));
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
            assert_eq!(nav.matches("<svg ").count(), 5);
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
        let unknown = content_for(Page::Asks, &View::default(), 100);
        assert!(unknown.contains("aria-label=\"Asks, observations unavailable\""));
        assert!(!unknown.contains("Asks, 0 open in snapshot"));
        assert!(unknown.contains("Owner asks unavailable"));
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
                main_red: true,
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
        for ask in &snapshot.asks {
            assert!(html.contains(&escape(&ask.question)));
        }
        assert!(html.contains("No open PRs in this snapshot"));
        assert!(html.contains("href=/board>0 observed items"));
        assert!(html.contains("Options, recommendation and meanwhile are unavailable"));
        assert!(!html.contains("<form") && !html.contains("onclick="));
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
        let mut runtime = oxa_script::Runtime::new(oxa_script::Limits::EXTENSION).unwrap();
        runtime
            .eval_script(
                "dom-fixture",
                r#"
let current, supplied, closeDuringFetch, failureMode='ok', focusDuringFetch=false, focused='', inside=false, fetches=0;
const connectionBadge={textContent:'Snapshot'};
function makeMain(open) {
  const work={open}, notice={hidden:true};
  return {work, notice, contains:()=>inside, querySelector:selector=>selector==='details.home-work'?work:null,
    replaceWith:next=>{current=next;}};
}
const document={hidden:false, activeElement:{matches:selectors=>selectors.split(',').includes(focused)},
  getElementById:id=>id==='connection'?connectionBadge:id==='refresh-error'?current.notice:null,
  querySelector:()=>current, addEventListener:()=>{}};
const setTimeout=()=>1, clearTimeout=()=>{};
class AbortController { constructor(){this.signal={aborted:false};} abort(){this.signal.aborted=true;} }
class URL { constructor(){this.searchParams={set(){}};} }
const location={href:'http://example.invalid/'};
class DOMParser { parseFromString(){return {querySelector:()=>failureMode==='missing'?null:supplied};} }
async function fetch(){
  fetches++;
  if(focusDuringFetch){inside=true; focused='[tabindex]';}
  if(failureMode==='network')throw new Error('private-refresh-marker');
  if(failureMode==='abort'){pending.abort();throw new Error('aborted');}
  if(closeDuringFetch)current.work.open=false;
  return {ok:failureMode!=='refused', text:async()=>''};
}
"#,
            )
            .unwrap();
        runtime.eval_script("live.js", SCRIPT).unwrap();
        runtime
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
        let render = |queue, overdue_queue| {
            content(
                &View {
                    queue,
                    overdue_queue,
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
        for (queue, alarms) in [
            (None, overdue.as_slice()),
            (Some(queue.as_slice()), stale.as_slice()),
            (Some(queue.as_slice()), other.as_slice()),
            (Some(queue.as_slice()), &[]),
        ] {
            assert!(!render(queue, alarms).contains("Needs attention"));
        }
        let page = render(Some(&queue), &overdue);
        assert!(page.contains("PR #17 has waited too long in the merge queue"));
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
        let queue = [PullRequest {
            number: 17,
            head: "a".repeat(40),
        }];
        let runs = [29];
        for (view, evidence) in [
            (
                View {
                    main_red: true,
                    ..View::default()
                },
                "Checks on main are failing.",
            ),
            (
                View {
                    main_merge_stalled: true,
                    ..View::default()
                },
                "No PR merged into main for 45 minutes",
            ),
            (
                View {
                    stalled_runs: &runs,
                    ..View::default()
                },
                "Run 29",
            ),
            (
                View {
                    queue: Some(&queue),
                    overdue_queue: &queue,
                    ..View::default()
                },
                "PR #17 has waited too long",
            ),
        ] {
            let html = content_for(Page::Home, &view, 100);
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
                main_red: true,
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
        for situation in [None, Some(&snapshot)] {
            for (queue, overdue_queue) in [
                (None, queue.as_slice()),
                (Some(queue.as_slice()), old_head.as_slice()),
                (Some(queue.as_slice()), &[]),
            ] {
                let unknown = content_for(
                    Page::Home,
                    &View {
                        situation,
                        queue,
                        overdue_queue,
                        ..View::default()
                    },
                    100,
                );
                assert!(!unknown.contains("id=attention"));
                assert!(!unknown.contains("role=alert") && !unknown.contains("All good"));
            }
        }
    }

    #[test]
    fn only_failed_main_rollups_render_an_alarm() {
        for (state, red) in [
            ("ERROR", true),
            ("FAILURE", true),
            ("SUCCESS", false),
            ("PENDING", false),
            ("EXPECTED", false),
            ("", false),
        ] {
            let response = serde_json::json!({"data":{"repository":{
                "main":{"target":{"statusCheckRollup":{"state":state}}},
                "mergeQueue":null
            }}});
            let panels = crate::lead_panels::parse(&serde_json::to_vec(&response).unwrap(), None).unwrap();
            let page = content(
                &View {
                    main_red: panels.main_red,
                    ..View::default()
                },
                100,
            );
            assert_eq!(page.contains("Checks on main are failing."), red, "{state}");
            assert_eq!(page.contains("Needs attention"), red, "{state}");
        }
    }
    #[test]
    fn merge_alarm_uses_the_main_merge_and_sample_time() {
        use serde_json::{json, Value};

        let head = "c".repeat(40);
        let response = json!({"data":{"repository":{
            "id":"example-repo",
            "main":{"target":{"oid":head, "associatedPullRequests":{
                "pageInfo":{"hasNextPage":false},
                "nodes":[{"repository":{"id":"example-repo"}, "baseRefName":"main",
                    "mergeCommit":{"oid":head}, "mergedAt":"1970-01-02T00:00:00Z"}]
            }}},
            "mergeQueue":null
        }}});
        let render_at = |payload: &Value, sampled_at| {
            let panels = crate::lead_panels::parse(&serde_json::to_vec(payload).unwrap(), None).unwrap();
            content(
                &View {
                    main_merge_stalled: panels.main_merge_stalled(sampled_at),
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
        assert!(page.contains("0 / 1 reported complete"));
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
        assert_eq!(page.matches("<dt>").count(), 4);
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
}
