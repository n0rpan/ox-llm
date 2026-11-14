//! D3 acceptance tests: Selection, copy, search. Owner: task D3.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_interactive::{
    highlight_matches, highlight_selection, selection_text, Cell, SelectEvent, Selection, Selector,
    TranscriptSearch, ViewSettings,
};
use oxa_tui::{strip_ansi, wrap_ansi, Mouse, MouseButton, MouseKind, Theme};

/// A mouse report with the given kind, left button, column and row.
fn mouse(kind: MouseKind, col: u16, row: u16) -> Mouse {
    Mouse {
        kind,
        button: MouseButton::Left,
        col,
        row,
        shift: false,
        alt: false,
        ctrl: false,
    }
}

/// Viewport row = content line (no scrolling).
fn identity(row: usize) -> Option<usize> {
    Some(row)
}

/// selected text without ANSI
#[test]
fn mouse_selection_to_clipboard() {
    let content: Vec<String> = vec![
        "\x1b[1mHello\x1b[0m world   ".to_string(),
        String::new(),
        "second \x1b[31mline\x1b[39m".to_string(),
    ];
    let mut sel = Selector::new();
    assert_eq!(
        sel.mouse(&mouse(MouseKind::Press, 0, 0), &identity, &content, 0),
        SelectEvent::Changed
    );
    assert!(sel.dragging);
    assert_eq!(
        sel.mouse(&mouse(MouseKind::Drag, 5, 2), &identity, &content, 10),
        SelectEvent::Changed
    );
    let text = match sel.mouse(&mouse(MouseKind::Release, 5, 2), &identity, &content, 20) {
        SelectEvent::Finished(text) => text,
        other => panic!("expected Finished, got {other:?}"),
    };
    assert_eq!(text, "Hello world\n\nsecond");
    assert!(!text.contains('\x1b'));
    assert!(!sel.dragging);
    assert!(
        sel.selection.is_some(),
        "the selection stays visible after release"
    );
}

/// setting
#[test]
fn copy_on_select() {
    // The setting defaults to off; the app copies the Finished text only when it is on.
    let mut settings = ViewSettings::default();
    assert!(!settings.copy_on_select);
    settings.copy_on_select = true;
    assert!(settings.copy_on_select);

    let content: Vec<String> = vec!["copy me".to_string()];
    let mut sel = Selector::new();
    sel.mouse(&mouse(MouseKind::Press, 0, 0), &identity, &content, 0);
    sel.mouse(&mouse(MouseKind::Drag, 6, 0), &identity, &content, 1);
    let finished = sel.mouse(&mouse(MouseKind::Release, 6, 0), &identity, &content, 2);
    assert_eq!(finished, SelectEvent::Finished("copy me".to_string()));
}

/// wraps, highlights
#[test]
fn search_next_previous() {
    let content: Vec<String> = vec![
        "foo bar".to_string(),
        "nothing".to_string(),
        "bar foo bar".to_string(),
    ];
    let mut search = TranscriptSearch::new();
    search.set_query("bar", &content, 0);
    assert_eq!(search.matches.len(), 3);
    assert_eq!(search.position_text(), "1/3");

    assert_eq!(search.next_match(), Some(2));
    assert_eq!(search.position_text(), "2/3");
    assert_eq!(search.next_match(), Some(2));
    assert_eq!(search.position_text(), "3/3");
    assert_eq!(search.next_match(), Some(0), "next wraps to the first match");
    assert_eq!(search.position_text(), "1/3");
    assert_eq!(search.prev_match(), Some(2), "previous wraps to the last match");
    assert_eq!(search.position_text(), "3/3");

    // Matches are highlighted; the current one is drawn with the two colours swapped (plain
    // theme: inverse video, which marks every match).
    let theme = Theme::plain();
    let lines = highlight_matches(&content, 0, content.len(), &search, &theme);
    assert_eq!(strip_ansi(&lines[2]), "bar foo bar");
    assert_eq!(lines[2].matches("\x1b[7m").count(), 2);
    assert_eq!(strip_ansi(&lines[1]), "nothing");
    assert_eq!(lines[1], "nothing");

    assert_eq!(TranscriptSearch::new().position_text(), "0/0");
}

/// joins soft wraps
#[test]
fn selection_across_wrapped_lines() {
    let sentence = "The quick brown fox jumps over the lazy dog and keeps running far away";
    let content = wrap_ansi(sentence, 20);
    assert!(content.len() > 2, "the sentence wraps: {content:?}");

    let last = content.len() - 1;
    let sel = Selection {
        anchor: Cell { line: 0, col: 0 },
        head: Cell { line: last, col: 200 },
    };
    let text = selection_text(&content, &sel);
    assert!(
        !text.contains('\n'),
        "soft wraps must not become newlines: {text:?}"
    );
    assert_eq!(text, sentence);

    // A selection that starts mid-line and ends mid-line still joins the lines it crosses.
    let partial = Selection {
        anchor: Cell { line: 0, col: 4 },
        head: Cell { line: 1, col: 3 },
    };
    let part = selection_text(&content, &partial);
    assert!(!part.contains('\n'), "{part:?}");
    assert!(part.starts_with("quick"), "{part:?}");
}

/// smart case
#[test]
fn search_case_rules() {
    let content: Vec<String> = vec!["Hello hello HELLO".to_string()];

    // Lower-case query: case-insensitive, all three match.
    let mut search = TranscriptSearch::new();
    search.set_query("hello", &content, 0);
    assert_eq!(search.matches.len(), 3);

    // A query with an uppercase letter is case-sensitive: only "Hello" at column 0.
    search.set_query("Hello", &content, 0);
    assert_eq!(search.matches.len(), 1);
    assert_eq!((search.matches[0].col_start, search.matches[0].col_end), (0, 5));

    search.set_query("HELLO", &content, 0);
    assert_eq!(search.matches.len(), 1);
    assert_eq!(search.matches[0].col_start, 12);

    // Empty query clears everything.
    search.set_query("", &content, 0);
    assert!(search.matches.is_empty());
    assert_eq!(search.current, None);
}

#[test]
fn search_starts_at_visible_line_and_survives_refresh() {
    let content: Vec<String> = vec![
        "x one".to_string(),
        "one".to_string(),
        "one".to_string(),
        "one".to_string(),
    ];
    let mut search = TranscriptSearch::new();
    // Starting at line 2 selects the last match at or before it (line 2).
    search.set_query("one", &content, 2);
    assert_eq!(search.position_text(), "3/4");

    // Streaming: a new line is appended, the current match (line 2) is kept.
    let mut grown = content.clone();
    grown.push("one more".to_string());
    search.refresh(&grown);
    assert_eq!(search.position_text(), "3/5");
    assert_eq!(search.matches[search.current.unwrap()].line, 2);
}

#[test]
fn search_ignores_escape_sequences() {
    let content: Vec<String> = vec!["\x1b[31mred\x1b[0m text".to_string()];
    let mut search = TranscriptSearch::new();
    search.set_query("red text", &content, 0);
    assert_eq!(search.matches.len(), 1);
    assert_eq!((search.matches[0].col_start, search.matches[0].col_end), (0, 8));
}

#[test]
fn selection_highlight_keeps_text_and_respects_range() {
    let content: Vec<String> = vec![
        "\x1b[32mgreen\x1b[39m line".to_string(),
        "other".to_string(),
        "last".to_string(),
    ];
    let sel = Selection {
        anchor: Cell { line: 0, col: 2 },
        head: Cell { line: 1, col: 1 },
    };
    let theme = Theme::plain();
    let lines = highlight_selection(&content, 0, content.len(), &sel, &theme);
    assert_eq!(lines.len(), 3);
    for (before, after) in content.iter().zip(&lines) {
        assert_eq!(strip_ansi(before), strip_ansi(after));
    }
    assert!(lines[0].contains("\x1b[7m"));
    assert!(lines[1].contains("\x1b[7m"));
    assert_eq!(lines[2], "last", "lines outside the selection are unchanged");
}

#[test]
fn click_without_movement_clears() {
    let content: Vec<String> = vec!["abc".to_string()];
    let mut sel = Selector::new();
    sel.mouse(&mouse(MouseKind::Press, 1, 0), &identity, &content, 0);
    assert_eq!(
        sel.mouse(&mouse(MouseKind::Release, 1, 0), &identity, &content, 1),
        SelectEvent::Cleared
    );
    assert!(sel.selection.is_none());
}

#[test]
fn right_button_and_moves_are_ignored() {
    let content: Vec<String> = vec!["abc".to_string()];
    let mut sel = Selector::new();
    let right = Mouse {
        button: MouseButton::Right,
        ..mouse(MouseKind::Press, 0, 0)
    };
    assert_eq!(sel.mouse(&right, &identity, &content, 0), SelectEvent::Ignored);
    assert_eq!(
        sel.mouse(&mouse(MouseKind::Move, 0, 0), &identity, &content, 0),
        SelectEvent::Ignored
    );
    assert!(sel.selection.is_none());
}

#[test]
fn wide_characters_are_cut_at_cluster_boundaries() {
    // "日本" occupies columns 0..4; selecting only column 0..1 includes the whole first character.
    let content: Vec<String> = vec!["日本".to_string()];
    let sel = Selection {
        anchor: Cell { line: 0, col: 0 },
        head: Cell { line: 0, col: 0 },
    };
    assert_eq!(selection_text(&content, &sel), "日");
}

// ---- D-INT: click state and the soft-wrap rule ----------------------------------------------

/// A second press on the same cell within the interval selects the word, a third the line.
#[test]
fn double_click_selects_the_word_and_triple_click_the_line() {
    let content = vec!["alpha beta_2 gamma".to_string(), "second line".to_string()];
    let mut sel = Selector::new();
    let press = mouse(MouseKind::Press, 8, 0);
    let release = mouse(MouseKind::Release, 8, 0);
    assert_eq!(sel.mouse(&press, &identity, &content, 1000), SelectEvent::Changed);
    assert_eq!(
        sel.mouse(&release, &identity, &content, 1010),
        SelectEvent::Cleared
    );
    // Double click: the word under the pointer.
    sel.mouse(&press, &identity, &content, 1200);
    assert_eq!(
        sel.mouse(&release, &identity, &content, 1210),
        SelectEvent::Finished("beta_2".to_string())
    );
    // Triple click: the whole line.
    sel.mouse(&press, &identity, &content, 1400);
    assert_eq!(
        sel.mouse(&release, &identity, &content, 1410),
        SelectEvent::Finished("alpha beta_2 gamma".to_string())
    );
    // A fourth press starts over, and a slow second press is a new single click.
    sel.mouse(&press, &identity, &content, 1500);
    assert_eq!(
        sel.mouse(&release, &identity, &content, 1510),
        SelectEvent::Cleared
    );
    sel.mouse(&press, &identity, &content, 5000);
    assert_eq!(
        sel.mouse(&release, &identity, &content, 5010),
        SelectEvent::Cleared
    );
}

/// The soft-wrap rule is explicit: off = the lines as shown, a width = wraps at that width.
#[test]
fn soft_wrap_rule_is_explicit() {
    use oxa_interactive::{selection_text_with, SoftWrap};
    // Wrapped at 12 columns: "wrapped" did not fit after "these words".
    let content = vec![
        "these words".to_string(),
        "wrapped".to_string(),
        "end".to_string(),
    ];
    let all = Selection {
        anchor: Cell { line: 0, col: 0 },
        head: Cell { line: 2, col: 2 },
    };
    assert_eq!(
        selection_text_with(&content, &all, SoftWrap::Off),
        "these words\nwrapped\nend"
    );
    // At width 12 the first break is a wrap; "end" would have fitted after "wrapped", so the
    // second break is a real line end.
    assert_eq!(
        selection_text_with(&content, &all, SoftWrap::Width(12)),
        "these words wrapped\nend"
    );
    // At a wide width nothing was wrapped.
    assert_eq!(
        selection_text_with(&content, &all, SoftWrap::Width(80)),
        "these words\nwrapped\nend"
    );
    let mut sel = Selector::new();
    sel.wrap = SoftWrap::Off;
    sel.selection = Some(all);
    assert_eq!(sel.text(&content), "these words\nwrapped\nend");
}
