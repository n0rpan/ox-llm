//! Item view: next action and state, with the absent details named explicitly (#2350).
use super::board::lease;
use super::{action, human_time, View};
use std::fmt::Write;

pub(super) fn render(html: &mut String, view: &View<'_>, number: u64, now: u64) {
    html.push_str("<p><a href=/board>Back to Board</a></p>");
    let Some(board) = view.board else {
        html.push_str(
            "<p>Board unavailable. No board observation reached this page; the source did not say why.</p>",
        );
        return;
    };
    let Some(row) = board.entries.iter().find(|row| row.number == number) else {
        html.push_str(
            "<p>Item unavailable in this snapshot. The board observation does not include this item.</p>",
        );
        return;
    };
    let _ = write!(html,
        "<section><article><h2>Next action</h2><p>{}</p></article></section><section><article><h2>State</h2><dl><dt>Priority</dt><dd>{}</dd><dt>Lease</dt><dd>{}</dd><dt>Attempts today</dt><dd>{}</dd><dt>Observed at</dt><dd>{}</dd></dl></article></section>",
        action(row.action), if row.release_pinned { "Release" } else { "Normal priority" },
        lease(row, now), row.attempts_today, human_time(board.observed_at, now));
    html.push_str("<section><h2>Details and changes</h2><p>Title, checks, findings and diff observations unavailable. This board observation contains next actions and leases, without those details.</p><dl class=item-detail><dt>Title</dt><dd>Unavailable in this snapshot</dd><dt>Checks</dt><dd>Unavailable in this snapshot</dd><dt>Review state</dt><dd>Unavailable in this snapshot</dd><dt>Findings</dt><dd>Unavailable in this snapshot</dd><dt>Diff</dt><dd>Unavailable in this snapshot</dd></dl></section>");
}
