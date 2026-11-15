//! D8 acceptance tests: Session picker.
//! One test per acceptance bullet of WAVES.toml, plus unit tests of the picker. The picker is
//! pure: no terminal, no engine, no file system. Keys come from `Keymap::defaults()`.
//! Isolation: nothing here reads the real home; sessions are built in memory.

use oxa_config::Keymap;
use oxa_interactive::selectors::{relative_age, SessionPick, SessionPicker};
use oxa_interactive::Outcome;
use oxa_session::{SessionMeta, SessionRef};
use oxa_tui::{Key, KeyKind, Theme};
use std::path::PathBuf;

const NOW: u64 = 1_800_000_000_000;
const MIN: u64 = 60_000;

fn meta(
    id: &str,
    name: Option<&str>,
    first: &str,
    cwd: &str,
    modified_ms: u64,
    parent: Option<&str>,
) -> SessionMeta {
    SessionMeta {
        sref: SessionRef {
            path: PathBuf::from(format!("/sessions/{id}.jsonl")),
            id: id.to_string(),
        },
        cwd: PathBuf::from(cwd),
        name: name.map(str::to_string),
        created_ms: modified_ms.saturating_sub(MIN),
        modified_ms,
        message_count: 3,
        first_message: first.to_string(),
        all_text: first.to_string(),
        parent_session: parent.map(|p| format!("/sessions/{p}.jsonl")),
    }
}

fn press(p: &mut SessionPicker, keymap: &Keymap, id: &str) -> Outcome<SessionPick> {
    p.key(&Key::press(id), keymap)
}

fn type_text(p: &mut SessionPicker, keymap: &Keymap, text: &str) {
    for c in text.chars() {
        let out = p.key(&Key::char(c), keymap);
        assert!(
            matches!(out, Outcome::Pending),
            "typing {c:?} keeps the picker open"
        );
    }
}

fn screen(p: &mut SessionPicker, width: u16) -> String {
    p.render(width, &Theme::plain()).join("\n")
}

fn open_id(out: Outcome<SessionPick>) -> String {
    match out {
        Outcome::Done(SessionPick::Open(s)) => s.id,
        _ => panic!("expected Open"),
    }
}

/// Three sessions, newest first in the default order.
fn sample() -> Vec<SessionMeta> {
    let mut a = meta("a", None, "fix the parser bug", "/work/alpha", NOW - MIN, None);
    a.message_count = 4;
    vec![
        a,
        meta("b", None, "write the docs", "/work/alpha", NOW - 2 * MIN, None),
        meta("c", None, "parser benchmarks", "/work/beta", NOW - 3 * MIN, None),
    ]
}

/// filters as typed
#[test]
fn search() {
    let km = Keymap::defaults();
    let mut p = SessionPicker::new(sample(), None, false, NOW);
    let all = screen(&mut p, 120);
    assert!(all.contains("write the docs"));

    type_text(&mut p, &km, "parser");
    let filtered = screen(&mut p, 120);
    assert!(filtered.contains("> parser"), "query is shown");
    assert!(filtered.contains("fix the parser bug"));
    assert!(filtered.contains("parser benchmarks"));
    assert!(
        !filtered.contains("write the docs"),
        "non-matching sessions are hidden"
    );

    // The first match is selected: Enter opens the newest one.
    assert_eq!(open_id(press(&mut p, &km, "enter")), "a");

    // Backspace widens the filter again.
    for _ in 0..6 {
        press(&mut p, &km, "backspace");
    }
    assert!(screen(&mut p, 120).contains("write the docs"));

    type_text(&mut p, &km, "zzz");
    assert!(screen(&mut p, 120).contains("No sessions in current folder. Press Tab to view all."));
}

/// three modes
#[test]
fn sort_cycle() {
    let km = Keymap::defaults();
    let mut p = SessionPicker::new(sample(), None, false, NOW);
    assert!(screen(&mut p, 120).contains("Sort: Threaded"));
    press(&mut p, &km, "ctrl+s");
    assert!(screen(&mut p, 120).contains("Sort: Recent"));
    press(&mut p, &km, "ctrl+s");
    assert!(screen(&mut p, 120).contains("Sort: Fuzzy"));
    press(&mut p, &km, "ctrl+s");
    assert!(
        screen(&mut p, 120).contains("Sort: Threaded"),
        "cycle returns to the first mode"
    );
}

/// toggle
#[test]
fn named_filter() {
    let km = Keymap::defaults();
    let mut sessions = sample();
    sessions[2].name = Some("release notes".to_string());
    let mut p = SessionPicker::new(sessions, None, false, NOW);
    assert!(screen(&mut p, 120).contains("Name: All"));
    assert!(screen(&mut p, 120).contains("write the docs"));

    press(&mut p, &km, "ctrl+n");
    let named = screen(&mut p, 120);
    assert!(named.contains("Name: Named"));
    assert!(named.contains("release notes"));
    assert!(!named.contains("write the docs"), "unnamed sessions are hidden");

    press(&mut p, &km, "ctrl+n");
    assert!(screen(&mut p, 120).contains("write the docs"));
}

/// show paths
#[test]
fn paths_toggle() {
    let km = Keymap::defaults();
    let mut p = SessionPicker::new(sample(), None, false, NOW);
    let off = screen(&mut p, 160);
    assert!(off.contains("path (off)"));
    assert!(!off.contains("/work/beta"));

    press(&mut p, &km, "ctrl+p");
    let on = screen(&mut p, 160);
    assert!(on.contains("path (on)"));
    // pi: Ctrl+P shows each session's file path.
    assert!(on.contains(".jsonl"), "the file path of each session is shown");

    // pi: the All scope shows each session's cwd; the path toggle stays off.
    let all = SessionPicker::new(sample(), None, true, NOW)
        .render(160, &Theme::plain())
        .join("\n");
    assert!(all.contains("/work/beta"));
    assert!(all.contains("path (off)"));
}

/// appends SessionInfo
#[test]
fn rename_in_place() {
    let km = Keymap::defaults();
    let mut sessions = sample();
    sessions[0].name = Some("old name".to_string());
    let mut p = SessionPicker::new(sessions, None, false, NOW);

    press(&mut p, &km, "ctrl+r");
    let panel = screen(&mut p, 120);
    assert!(panel.contains("Rename Session"));
    assert!(
        panel.contains("> old name"),
        "the input starts with the current name"
    );

    for _ in 0.."old name".len() {
        press(&mut p, &km, "backspace");
    }
    type_text(&mut p, &km, "  new name ");
    match press(&mut p, &km, "enter") {
        Outcome::Done(SessionPick::Rename { session, name }) => {
            assert_eq!(session.id, "a");
            assert_eq!(name, "new name", "the name is trimmed");
        }
        _ => panic!("expected Rename"),
    }

    // The picker stays open; the app appends the SessionInfo and feeds the list back.
    assert!(!screen(&mut p, 120).contains("Rename Session"));
    assert!(matches!(press(&mut p, &km, "down"), Outcome::Pending));
    let mut renamed = sample();
    renamed[0].name = Some("new name".to_string());
    p.set_sessions(renamed, false);
    let after = screen(&mut p, 120);
    assert!(after.contains("new name"));
    assert!(!after.contains("old name"));
}

/// trash, list updated
#[test]
fn delete_with_confirm() {
    let km = Keymap::defaults();
    let mut p = SessionPicker::new(sample(), Some("a"), false, NOW);

    // The current session cannot be deleted.
    assert!(matches!(press(&mut p, &km, "ctrl+d"), Outcome::Pending));
    assert!(screen(&mut p, 120).contains("Cannot delete the currently active session"));

    press(&mut p, &km, "down");
    press(&mut p, &km, "ctrl+d");
    assert!(screen(&mut p, 120).contains("Delete session?"));

    // Escape cancels the delete, not the picker.
    assert!(matches!(press(&mut p, &km, "escape"), Outcome::Pending));
    assert!(!screen(&mut p, 120).contains("Delete session?"));

    press(&mut p, &km, "ctrl+d");
    match press(&mut p, &km, "enter") {
        Outcome::Done(SessionPick::Delete(s)) => assert_eq!(s.id, "b"),
        _ => panic!("expected Delete"),
    }
    let rest: Vec<SessionMeta> = sample().into_iter().filter(|m| m.sref.id != "b").collect();
    p.set_sessions(rest, false);
    let after = screen(&mut p, 120);
    assert!(!after.contains("write the docs"));
    assert!(after.contains("parser benchmarks"));
}

/// all sessions scope
#[test]
fn other_directory_tab() {
    let km = Keymap::defaults();
    let mut local = sample();
    local.retain(|m| m.cwd.to_string_lossy() == "/work/alpha");
    let mut p = SessionPicker::new(local, None, false, NOW);
    assert!(screen(&mut p, 120).contains("Resume Session (Current Folder)"));
    assert!(!screen(&mut p, 120).contains("/work/beta"));

    match press(&mut p, &km, "tab") {
        Outcome::Done(SessionPick::ToggleScope) => {}
        _ => panic!("expected ToggleScope"),
    }
    // The app loads every directory and feeds the list back.
    p.set_sessions(sample(), true);
    let all = screen(&mut p, 160);
    assert!(all.contains("Resume Session (All)"));
    assert!(
        all.contains("/work/beta"),
        "other directories are listed with their cwd"
    );
    assert!(all.contains("parser benchmarks"));
}

// ----- further checks --------------------------------------------------------------------

#[test]
fn escape_cancels_the_picker() {
    let km = Keymap::defaults();
    let mut p = SessionPicker::new(sample(), None, false, NOW);
    assert!(matches!(press(&mut p, &km, "escape"), Outcome::Cancelled));
}

#[test]
fn navigation_then_open() {
    let km = Keymap::defaults();
    let mut p = SessionPicker::new(sample(), None, false, NOW);
    press(&mut p, &km, "down");
    press(&mut p, &km, "down");
    press(&mut p, &km, "down"); // clamped at the last row
    assert_eq!(open_id(press(&mut p, &km, "enter")), "c");
    press(&mut p, &km, "up");
    assert_eq!(open_id(press(&mut p, &km, "enter")), "b");
}

#[test]
fn empty_list_enter_keeps_open() {
    let km = Keymap::defaults();
    let mut p = SessionPicker::new(Vec::new(), None, false, NOW);
    assert!(matches!(press(&mut p, &km, "enter"), Outcome::Pending));
    assert!(screen(&mut p, 80).contains("No sessions in current folder"));
}

#[test]
fn release_events_are_ignored() {
    let km = Keymap::defaults();
    let mut p = SessionPicker::new(sample(), None, false, NOW);
    let release = Key {
        kind: KeyKind::Release,
        ..Key::char('x')
    };
    assert!(matches!(p.key(&release, &km), Outcome::Ignored));
    assert!(!screen(&mut p, 120).contains("> x"));
}

#[test]
fn paste_goes_into_the_query_on_one_line() {
    let mut p = SessionPicker::new(sample(), None, false, NOW);
    p.paste("parser\nbench");
    let s = screen(&mut p, 120);
    assert!(s.contains("> parser bench"));
    assert!(s.contains("parser benchmarks"));
    assert!(!s.contains("fix the parser bug"));
}

#[test]
fn threaded_view_places_children_after_their_parent() {
    let km = Keymap::defaults();
    let sessions = vec![
        meta("p", None, "parent session", "/w", NOW - 10 * MIN, None),
        meta("k", None, "child session", "/w", NOW - MIN, Some("p")),
        meta("u", None, "unrelated session", "/w", NOW - 5 * MIN, None),
    ];
    let mut p = SessionPicker::new(sessions, None, false, NOW);
    let s = screen(&mut p, 120);
    let parent_at = s.find("parent session").expect("parent listed");
    let child_at = s.find("child session").expect("child listed");
    let other_at = s.find("unrelated session").expect("unrelated listed");
    assert!(parent_at < child_at && child_at < other_at, "thread order: {s}");
    assert!(s.contains("└─ child session"), "child uses pi's connector: {s}");

    // Recent order ignores the thread: the child is still the newest.
    press(&mut p, &km, "ctrl+s");
    assert!(!screen(&mut p, 120).contains("└─"));
}

#[test]
fn selection_survives_set_sessions() {
    let km = Keymap::defaults();
    let mut p = SessionPicker::new(sample(), None, false, NOW);
    press(&mut p, &km, "down"); // on "b"
    let mut next = sample();
    next.insert(0, meta("z", None, "brand new", "/work/alpha", NOW, None));
    p.set_sessions(next, false);
    assert_eq!(open_id(press(&mut p, &km, "enter")), "b");
}

#[test]
fn rename_escape_cancels_the_rename_only() {
    let km = Keymap::defaults();
    let mut p = SessionPicker::new(sample(), None, false, NOW);
    press(&mut p, &km, "ctrl+r");
    assert!(matches!(press(&mut p, &km, "escape"), Outcome::Pending));
    let s = screen(&mut p, 120);
    assert!(!s.contains("Rename Session"));
    assert!(s.contains("Resume Session"));
}

#[test]
fn empty_rename_is_not_submitted() {
    let km = Keymap::defaults();
    let mut p = SessionPicker::new(sample(), None, false, NOW);
    press(&mut p, &km, "ctrl+r");
    for _ in 0..4 {
        press(&mut p, &km, "backspace");
    }
    assert!(matches!(press(&mut p, &km, "enter"), Outcome::Pending));
    assert!(screen(&mut p, 120).contains("Rename Session"));
}

#[test]
fn rows_fit_the_width() {
    let mut p = SessionPicker::new(sample(), None, true, NOW);
    for line in p.render(40, &Theme::plain()) {
        assert!(oxa_tui::visible_width(&line) <= 40, "line too wide: {line:?}");
    }
}

#[test]
fn relative_age_formats() {
    // pi session-selector.ts `formatSessionDate`.
    assert_eq!(relative_age(NOW, NOW), "now");
    assert_eq!(
        relative_age(NOW + MIN, NOW),
        "now",
        "a future timestamp reads as now"
    );
    assert_eq!(relative_age(NOW - 3 * MIN, NOW), "3m");
    assert_eq!(relative_age(NOW - 2 * 3_600_000, NOW), "2h");
    assert_eq!(relative_age(NOW - 5 * 86_400_000, NOW), "5d");
    assert_eq!(relative_age(NOW - 6 * 86_400_000, NOW), "6d");
}

#[test]
fn relative_age_weeks_months_years() {
    // pi: under 30 days in weeks, under 365 in 30-day months, then years.
    assert_eq!(relative_age(0, 7 * 86_400_000), "1w");
    assert_eq!(relative_age(0, 29 * 86_400_000), "4w");
    assert_eq!(relative_age(0, 40 * 86_400_000), "1mo");
    assert_eq!(relative_age(0, 364 * 86_400_000), "12mo");
    assert_eq!(relative_age(0, 800 * 86_400_000), "2y");
}

/// Wave H: pi's frame (spacer, accent border, spacer, header, ..., spacer, border), the
/// `<count> <age>` column, and a status message in place of the hints until the next key.
#[test]
fn frame_count_age_and_status() {
    let km = Keymap::defaults();
    let mut p = SessionPicker::new(sample(), None, false, NOW);
    let lines = p.render(100, &Theme::plain());
    assert_eq!(lines[0], "");
    assert_eq!(lines[1], "─".repeat(100));
    assert_eq!(lines[2], "");
    assert!(lines[3].starts_with("Resume Session (Current Folder)"));
    assert_eq!(*lines.last().unwrap(), "─".repeat(100));
    assert_eq!(lines[lines.len() - 2], "");
    let row = lines.iter().find(|l| l.contains("fix the parser bug")).unwrap();
    assert!(row.trim_end().ends_with("4 1m"), "{row:?}");
    p.set_status(false, "Session moved to trash");
    let s = screen(&mut p, 100);
    assert!(
        s.contains("Session moved to trash") && !s.contains("ctrl+s sort"),
        "{s}"
    );
    press(&mut p, &km, "down");
    assert!(!screen(&mut p, 100).contains("Session moved to trash"));
}
