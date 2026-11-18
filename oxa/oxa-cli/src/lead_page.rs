//! Read-only web projection. Never serialize runner records or arbitrary GitHub payloads.
use oxa_proto::work::{IncidentKind, PullRequest, Snapshot, Subject};
use oxa_work::board::{Action, Board, Entry};
use std::fmt::Write;

mod chat;
mod filters;
mod pages;
pub use filters::{Filter, Query};
pub use pages::{Page, Prompt};

/// Borrowed presentation inputs, not a second runner state store. None means unavailable.
#[derive(Default)]
pub struct View<'a> {
    pub board: Option<&'a Board>,
    pub situation: Option<&'a Snapshot>,
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
    if page == Page::Home {
        attention(&mut html, view);
    }
    html.push_str(&page.navigation());
    match page {
        Page::Home => {
            board(&mut html, view, now, Query::default());
            workers(&mut html, view);
            queue(&mut html, view);
            release(&mut html, view);
            asks(&mut html, view);
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
        Page::Workers => workers(&mut html, view),
        Page::Asks => asks(&mut html, view),
        Page::Item(number) => item(&mut html, view, number, now),
        Page::Chat(prompt) => chat::render(&mut html, view, prompt),
        Page::Usage => html
            .push_str("<section><h2>Plans and quotas</h2><p>Usage observations unavailable.</p></section>"),
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
    if view.main_red
        || view.main_merge_stalled
        || !view.stalled_runs.is_empty()
        || !overdue.is_empty()
        || ask_count > 0
    {
        html.push_str("<section id=attention><h2>Needs attention</h2><ul>");
        if view.main_red {
            html.push_str("<li>Checks on main are failing.</li>");
        }
        if view.main_merge_stalled {
            html.push_str("<li>No PR merged into main for 45 minutes or more at the last observation.</li>");
        }
        for pr in overdue {
            let _ = write!(
                html,
                "<li><a href=#queue>PR #{} has waited too long in the merge queue</a></li>",
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
        if ask_count > 0 {
            let _ = write!(
                html,
                "<li><a href=#asks>Unanswered owner decisions ({ask_count})</a></li>"
            );
        }
        html.push_str("</ul></section>");
    }
}

fn board(html: &mut String, view: &View<'_>, now: u64, query: Query) {
    html.push_str("<section id=board><h2>Board</h2>");
    if let Some(board) = view.board {
        let stale = board.observed_at > now.saturating_add(60) || now.saturating_sub(board.observed_at) > 900;
        let _ = write!(
            html,
            "<p class=muted>{} · observed at {} (Unix seconds)</p>",
            if stale {
                "Stale snapshot"
            } else {
                "GitHub snapshot"
            },
            board.observed_at
        );
        query.navigation(html, board, view);
        if !query.filter.available(view) {
            html.push_str("<p>Blocked observations unavailable.</p></section>");
            return;
        }
        let rows = query.rows(board, view);
        html.push_str("<div class=scroll><table><thead><tr><th>PR</th><th>Next action</th><th>Priority</th><th>Lease</th><th>Attempts today</th></tr></thead><tbody>");
        let offset = query.page.saturating_mul(pages::BOARD_ROWS);
        for row in rows.iter().skip(offset).take(pages::BOARD_ROWS) {
            let lease = lease(row, now);
            let _ = write!(
                html,
                "<tr><td><a href=/items/{0}>#{0}</a></td><td>{1}</td><td>{2}</td><td>{lease}</td><td>{3}</td></tr>",
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
            l.expires_at,
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
        "<section><h2>Next action</h2><p>{}</p></section><section><h2>State</h2><p>{}</p><p>{}</p><p>Attempts today: {}</p><p class=muted>Observed at {} (Unix seconds)</p></section>",
        action(row.action), if row.release_pinned { "Release" } else { "Normal priority" },
        lease(row, now), row.attempts_today, board.observed_at);
    html.push_str("<section><h2>Details and changes</h2><p>Title, checks, findings and diff observations unavailable.</p></section>");
}

fn workers(html: &mut String, view: &View<'_>) {
    html.push_str("<section id=lanes><h2>Workers</h2>");
    if let Some(s) = view.situation {
        let _ = write!(
            html,
            "<p class=muted>Runner facts · sequence {} · observed at {} (Unix milliseconds)</p>",
            s.as_of_seq, s.at_ms
        );
        for running in &s.running {
            let stalled = s.incidents.iter().any(|i| {
                i.kind == IncidentKind::WorkerStalled && i.subject == Subject::Task(running.task.clone())
            });
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

fn asks(html: &mut String, view: &View<'_>) {
    html.push_str("<section id=asks><h2>For your decision</h2>");
    if let Some(s) = view.situation {
        for ask in &s.asks {
            let _ = write!(html, "<article><p class=muted>{}</p><h3>{}</h3><p>Options, recommendation and meanwhile are unavailable in these facts.</p></article>", subject(&ask.subject), escape(&ask.question));
        }
        if s.asks.is_empty() {
            html.push_str("<p>No open asks in these runner facts.</p>");
        }
    } else {
        html.push_str("<p>Owner asks unavailable.</p>");
    }
    html.push_str("</section>");
}

pub const STYLE: &str = include_str!("lead_page/style.css");

pub const SCRIPT: &str = r#"const badge=document.getElementById('connection');
let timer,pending;
async function refresh(){
  clearTimeout(timer);
  if(document.hidden)return;
  if(document.querySelector('main').contains(document.activeElement)&&document.activeElement.matches('a,button,input,textarea,select,summary')){timer=setTimeout(refresh,15000);return;}
  if(pending)pending.abort();
  const active=new AbortController();pending=active;
  const target=new URL(location.href);target.searchParams.set('fragment','1');
  try{
    const response=await fetch(target,{cache:'no-store',signal:active.signal});
    if(!response.ok)throw new Error('refresh refused');
    const doc=new DOMParser().parseFromString(await response.text(),'text/html');
    const next=doc.querySelector('main');
    if(!next)throw new Error('refresh unavailable');
    if(pending===active){document.querySelector('main').replaceWith(next);badge.textContent='Live';}
  }catch(error){if(!active.signal.aborted)badge.textContent='Refresh unavailable';}
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
        let render = |board| {
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
        for expected in [
            "<h1 id=main tabindex=-1>Item #17</h1>",
            "href=/board>Back to Board</a>",
            "aria-current=page>Board",
            "<h2>Next action</h2><p>Fix findings</p>",
            "<h2>State</h2><dl>",
            "<dt>Priority</dt><dd>Release</dd>",
            "<dt>Lease</dt><dd>worker-example · expires 200 (expired)</dd>",
            "<dt>Attempts today</dt><dd>2</dd>",
            "<dt>Observed at</dt><dd>100 (Unix seconds)</dd>",
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
        assert!(leased.contains("&lt;worker&gt;&amp;example · expires 300</dd>"));
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
