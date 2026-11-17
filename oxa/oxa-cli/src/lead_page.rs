//! Read-only web projection. Never serialize runner records or arbitrary GitHub payloads.
use oxa_proto::work::{IncidentKind, PullRequest, Snapshot, Subject};
use oxa_work::board::{Action, Board};
use std::fmt::Write;

/// Borrowed presentation inputs, not a second runner state store. None means unavailable.
#[derive(Default)]
pub struct View<'a> {
    pub board: Option<&'a Board>,
    pub situation: Option<&'a Snapshot>,
    pub main_red: bool,
    pub queue: Option<&'a [PullRequest]>,
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
    format!("<!doctype html><html lang=en><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\"><title>oxa lead</title><link rel=stylesheet href=/style.css><script src=/live.js defer></script><header><a href=/>oxa <b>lead</b></a><span id=connection>Snapshot</span></header>{}</html>", content(view, now))
}

/// Allow-listed HTML fragment, also used by SSE. No worktrees, sessions, errors or headers.
pub fn content(view: &View<'_>, now: u64) -> String {
    let mut html = String::from("<main><h1>Work in motion</h1>");
    let ask_count = view.situation.map_or(0, |s| s.asks.len());
    if view.main_red || ask_count > 0 {
        html.push_str("<section id=attention><h2>Needs attention</h2><ul>");
        if view.main_red {
            html.push_str("<li>Checks on main are failing.</li>");
        }
        if ask_count > 0 {
            let _ = write!(
                html,
                "<li><a href=#asks>Unanswered owner decisions ({ask_count})</a></li>"
            );
        }
        html.push_str("</ul></section>");
    }
    html.push_str("<nav aria-label=Views><a href=#board>Board</a><a href=#lanes>Lanes</a><a href=#queue>Merge queue</a><a href=#release>Release</a><a href=#asks>Asks</a></nav><section id=board><h2>Board</h2>");
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
        html.push_str("<div class=scroll><table><thead><tr><th>PR</th><th>Next action</th><th>Priority</th><th>Lease</th><th>Attempts today</th></tr></thead><tbody>");
        for row in &board.entries {
            let lease = row.lease.as_ref().map_or("Unleased".into(), |l| {
                format!(
                    "{} · expires {}{}",
                    escape(&l.holder),
                    l.expires_at,
                    if l.expires_at <= now { " (expired)" } else { "" }
                )
            });
            let _ = write!(
                html,
                "<tr><td>#{}</td><td>{}</td><td>{}</td><td>{lease}</td><td>{}</td></tr>",
                row.number,
                action(row.action),
                if row.release_pinned { "Release" } else { "Normal" },
                row.attempts_today
            );
        }
        html.push_str("</tbody></table></div>");
        if board.entries.is_empty() {
            html.push_str("<p>No open PRs in this snapshot.</p>");
        }
    } else {
        html.push_str("<p>Board unavailable.</p>");
    }
    html.push_str("</section><section id=lanes><h2>Lanes</h2>");
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
    html.push_str("</section><section id=queue><h2>Merge queue</h2>");
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
    html.push_str("</section><section id=release><h2>Release checklist</h2>");
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
    html.push_str("</section><section id=asks><h2>For your decision</h2>");
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
    html.push_str("</section><footer>Read-only · Chat with the lead is not connected yet.</footer></main>");
    html
}

pub const STYLE: &str = r#"
:root{color-scheme:dark;font:16px/1.6 system-ui,sans-serif;background:#10171e;color:#e2e9ed}
body{max-width:1100px;margin:auto;padding:0 24px}header{display:flex;justify-content:space-between;padding:28px 0;border-bottom:1px solid #34414a}
a{color:#e9c785;text-decoration:none}a:focus-visible{outline:2px solid #e9c785}header a{font-size:22px;color:inherit}header b{color:#e9c785}
h1{font-size:clamp(32px,5vw,48px);letter-spacing:-.04em;margin:40px 0 0}h2{font-size:22px}h3{font-size:18px;margin:6px 0}
.muted,footer,#connection{color:#a3b4bf}nav{display:flex;gap:24px;overflow:auto;padding:20px 0}nav a{white-space:nowrap}
section{padding:16px 0 28px;border-top:1px solid #34414a;scroll-margin:16px}.scroll{overflow:auto}table{border-collapse:collapse;width:100%;text-align:left;font-size:14px}
th{color:#a3b4bf;font-weight:500}td,th{padding:12px;border-bottom:1px solid #28343d}article{background:#19242d;padding:16px 20px;margin:12px 0;border-radius:8px}article p{margin:4px 0}footer{padding:24px 0 40px}li{margin:10px 0}
@media(max-width:600px){body{padding:0 16px}nav{gap:18px}td,th{padding:10px}header{padding:20px 0}}
"#;

pub const SCRIPT: &str = r#"const badge=document.getElementById('connection');
const stream=new EventSource('/events');
stream.addEventListener('snapshot',event=>{document.querySelector('main').outerHTML=event.data;badge.textContent='Live';});
stream.onerror=()=>{badge.textContent='Disconnected · reconnecting';};
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use oxa_work::board::{GitHub, Mergeability, Policy, PullRequest as BoardPr, RequiredCi};
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
}
