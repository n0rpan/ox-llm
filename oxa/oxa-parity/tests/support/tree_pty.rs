//! The `/tree` navigator driven through the pty (lane tui, wave H): open it, move the
//! selection to a row, answer pi's summary question. Included by `tree.rs` and `sessions.rs`
//! next to `tui_rig.rs`.
#![allow(dead_code)]

use super::rig::{editor_text, Rig, Screen, WAIT};

/// The selected row of a selector (pi's `› ` cursor), trimmed.
pub fn selected(s: &Screen) -> String {
    s.rows()
        .into_iter()
        .find(|r| r.starts_with("› "))
        .map(|r| r.trim().to_string())
        .unwrap_or_default()
}

/// The rows of the navigator's list: between the blank line after the search border and the
/// position line `(i/n)`.
pub fn nav_rows(s: &Screen) -> Vec<String> {
    let rows = s.rows();
    let Some(end) = rows.iter().rposition(|r| {
        let t = r.trim();
        t.starts_with('(') && t.contains('/') && t.contains(')')
    }) else {
        return Vec::new();
    };
    let start = rows[..end]
        .iter()
        .rposition(|r| r.contains("Type to search:") || r.contains("Label (empty to remove)"))
        .map(|i| i + 3)
        .unwrap_or(end);
    rows[start.min(end)..end].to_vec()
}

/// The position line `(i/n) [filter]`.
pub fn position(s: &Screen) -> String {
    s.rows()
        .into_iter()
        .rev()
        .find(|r| {
            let t = r.trim();
            t.starts_with('(') && t.contains('/')
        })
        .map(|r| r.trim().to_string())
        .unwrap_or_default()
}

/// `/tree` and wait for the navigator with the leaf selected.
pub fn open_tree(rig: &Rig) -> Screen {
    rig.slash("/tree");
    rig.pty
        .wait_for_named(
            "the /tree navigator",
            |s| s.contains("Session Tree") && !selected(s).is_empty(),
            WAIT,
        )
        .unwrap()
}

/// Move the selection up (or down) until the selected row contains `needle`.
pub fn select_row(rig: &Rig, needle: &str, key: &str) -> Screen {
    for _ in 0..40 {
        let s = rig.screen();
        let cur = selected(&s);
        if cur.contains(needle) {
            return s;
        }
        rig.pty.send_key(key);
        rig.pty
            .wait_for_named(
                &format!("selection moved from {cur:?}"),
                |s| selected(s) != cur,
                WAIT,
            )
            .unwrap();
    }
    panic!("no row {needle:?} in the navigator\n{:?}", rig.screen());
}

/// Select the row with `needle` and press Enter; with `answer` = the summary question's option
/// (0 = No summary, 1 = Summarize, 2 = custom prompt) wait for the question and choose it.
/// Returns the raw-output mark taken before the first Enter (for [`navigated`]).
pub fn pick(rig: &Rig, needle: &str, answer: Option<usize>) -> usize {
    select_row(rig, needle, "up");
    let mark = rig.pty.mark();
    rig.pty.send_key("enter");
    if let Some(n) = answer {
        rig.pty.wait_for_text("Summarize branch?").unwrap();
        for _ in 0..n {
            rig.pty.send_key("down");
        }
        let want = ["No summary", "Summarize", "Summarize with custom prompt"][n];
        rig.pty
            .wait_for_named(&format!("{want:?} chosen"), |s| dialog_choice(s) == want, WAIT)
            .unwrap();
        rig.pty.send_key("enter");
    }
    mark
}

/// Wait until the navigation shows: the notice on screen, no dialog, and `shown` (the new
/// branch's transcript) holds; return the settled screen. (The raw output cannot tell: closing
/// a dialog repaints an older notice of the same text.)
pub fn navigated(rig: &Rig, shown: impl Fn(&Screen) -> bool) -> Screen {
    rig.pty
        .wait_for_named(
            "the navigation on screen",
            |s| s.contains("Navigated to selected point") && !s.contains("Summarize branch?") && shown(s),
            WAIT,
        )
        .unwrap();
    rig.settle()
}

/// Clear the editor (Ctrl+C) and wait until it is empty.
pub fn clear_editor(rig: &Rig) {
    rig.pty.send_key("ctrl+c");
    rig.pty
        .wait_for_named("an empty editor", |s| editor_text(s).is_empty(), WAIT)
        .unwrap();
}

/// The highlighted option of a dialog (`│ → No summary  │` -> `No summary`).
pub fn dialog_choice(s: &Screen) -> String {
    s.rows()
        .into_iter()
        .find_map(|r| {
            let t = r.trim().trim_matches('│').trim();
            t.strip_prefix("→ ").map(|x| x.trim().to_string())
        })
        .unwrap_or_default()
}

/// Move a list's selection (`› ` rows, no wrap) to the row containing `needle`, choosing the
/// direction from the rows on screen.
pub fn select_listed(rig: &Rig, needle: &str) -> Screen {
    for _ in 0..40 {
        let s = rig.screen();
        let rows = s.rows();
        let cur = rows.iter().position(|r| r.starts_with("› "));
        let want = rows
            .iter()
            .position(|r| r.contains(needle) && (r.starts_with("› ") || r.starts_with("  ")));
        let (Some(cur), Some(want)) = (cur, want) else {
            panic!("no row {needle:?} or no selection\n{s:?}");
        };
        if cur == want {
            return s;
        }
        let key = if want < cur { "up" } else { "down" };
        let before = selected(&s);
        rig.pty.send_key(key);
        rig.pty
            .wait_for_named(
                &format!("selection moved from {before:?}"),
                |s| selected(s) != before,
                WAIT,
            )
            .unwrap();
    }
    panic!("could not select {needle:?}");
}
