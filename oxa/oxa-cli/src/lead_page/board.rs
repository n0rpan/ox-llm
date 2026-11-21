//! Board view: counted filter chips, a desktop table and phone rows (#2350).
use super::pages;
use super::{action, escape, human_time, stale, Query, View};
use oxa_work::board::Entry;
use std::fmt::Write;

pub(super) fn render(html: &mut String, view: &View<'_>, now: u64, query: Query) {
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
            html.push_str("<p>Blocked observations unavailable. The runner observation is missing or cannot associate every block and ask with a PR.</p></section>");
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
        // The same observed rows as cards for a phone; the table stays for a wide screen.
        html.push_str("<ol class=board-rows aria-label=\"Observed board items\">");
        for row in rows.iter().skip(offset).take(pages::BOARD_ROWS) {
            let _ = write!(
                html,
                "<li><div class=row-head><a href=/items/{0}>#{0}</a><span>{1}</span></div><dl><dt>Next action</dt><dd>{2}</dd><dt>Lease</dt><dd>{3}</dd><dt>Attempts today</dt><dd>{4}</dd></dl></li>",
                row.number,
                if row.release_pinned { "Release" } else { "Normal" },
                action(row.action),
                lease(row, now),
                row.attempts_today
            );
        }
        html.push_str("</ol>");
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
        html.push_str(
            "<p>Board unavailable. No board observation reached this page; the source did not say why.</p>",
        );
    }
    html.push_str("</section>");
}

pub(super) fn lease(row: &Entry, now: u64) -> String {
    row.lease.as_ref().map_or("Unleased".into(), |l| {
        format!(
            "{} · expires {}{}",
            escape(&l.holder),
            human_time(l.expires_at, now),
            if l.expires_at <= now { " (expired)" } else { "" }
        )
    })
}
