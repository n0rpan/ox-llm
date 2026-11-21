//! The Home above-fold projection (#2345): one banner that answers "is anything
//! wrong?", then four read-only navigation tiles. Every value comes from a typed
//! observation already in the View; an absent source is named, never shown healthy.
use super::{alarm_state, stale, View};
use std::fmt::Write;

/// The single above-fold sentence. An active alarm is an alert; waiting owner
/// decisions are a status; a fully observed, quiet, fresh system is one green
/// sentence; anything less says it cannot tell yet and points at the gaps below.
/// The banner never carries the attention id: the rows below own that block.
pub(super) fn banner(html: &mut String, view: &View<'_>, now: u64) {
    let (alarm, missing) = alarm_state(view);
    let asks = view.situation.map_or(0, |s| s.asks.len()) + view.durable_asks.map_or(0, |a| a.len());
    let approvals = view.approvals.map_or(0, |a| a.len());
    let healthy = view.alarms.is_some()
        && missing.is_empty()
        && !view.board_retained
        && view.board.is_some_and(|board| !stale(board, now))
        && asks == 0
        && approvals == 0;
    let (tone, role, label, text) = if alarm {
        (
            "bad",
            "alert",
            "Alarm",
            "An alarm is active. The attention rows below name each firing source.",
        )
    } else if asks > 0 || approvals > 0 {
        (
            "warn",
            "status",
            "Needs you",
            "Owner decisions are waiting. Answer them below.",
        )
    } else if healthy {
        (
            "ok",
            "status",
            "All clear",
            "All observed sources are healthy and fresh.",
        )
    } else {
        (
            "neutral",
            "status",
            "Can't tell yet",
            "Not every source has been observed. The gaps are named below.",
        )
    };
    let _ = write!(
        html,
        "<section id=home-banner data-tone={tone} role={role}><p class=banner-label>{label}</p><p class=banner-text>{text}</p></section>"
    );
}

/// Four navigation tiles to the full pages. Each carries one factual line from
/// the same observation the destination renders; a missing source says so. No
/// form, button or inline handler lives here, and no worker identity is leaked.
pub(super) fn tiles(html: &mut String, view: &View<'_>, now: u64) {
    html.push_str("<div class=tiles>");
    let (value, detail): (String, String) = match view.board {
        Some(board) => {
            let state = if view.board_retained {
                "Last-good snapshot"
            } else if stale(board, now) {
                "Stale snapshot"
            } else {
                "Board snapshot"
            };
            (
                board.entries.len().to_string(),
                format!("{} observed items · {state}", board.entries.len()),
            )
        }
        None => ("—".into(), "No board observation reached this page".into()),
    };
    tile(html, "/board", "Board", &value, &detail);

    let (value, detail): (String, String) = if let Some(snapshot) = view.situation {
        (
            snapshot.running.len().to_string(),
            "running at observation".into(),
        )
    } else if matches!(view.workers, Some(Ok(_))) {
        ("—".into(), "Live workers of the Lead session".into())
    } else {
        ("—".into(), "Lane status unavailable".into())
    };
    tile(html, "/workers", "Workers", &value, &detail);

    let (value, detail): (String, String) = match view.alarms.and_then(|a| a.queue.as_deref()) {
        Some(queue) => (queue.len().to_string(), "in the merge queue".into()),
        None => ("—".into(), "No queue observation reached this page".into()),
    };
    tile(html, "/board#queue", "Merge queue", &value, &detail);

    let (value, detail): (String, String) = match view.release {
        Some((_, items)) => (
            format!("{}/{}", items.iter().filter(|item| item.0).count(), items.len()),
            "release checklist items".into(),
        ),
        None => ("—".into(), "No release checklist supplied".into()),
    };
    tile(html, "/board#release", "Release", &value, &detail);
    html.push_str("</div>");
}

fn tile(html: &mut String, href: &str, label: &str, value: &str, detail: &str) {
    let _ = write!(
        html,
        "<a class=tile href={href}><span class=k>{label}</span><span class=v>{value}</span><span class=d>{detail}</span></a>"
    );
}
