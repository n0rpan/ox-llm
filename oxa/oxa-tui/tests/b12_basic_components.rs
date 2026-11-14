//! B12 acceptance tests: Basic components. Owner: task B12.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use std::collections::BTreeMap;

use oxa_config::Keymap;
use oxa_tui::components::*;
use oxa_tui::{visible_width, Color, Component, Interactive, Key, Theme};

fn plain() -> Theme {
    Theme::plain()
}

/// A theme with a few colours set, so the SGR paths run.
fn coloured() -> Theme {
    let mut colors = BTreeMap::new();
    colors.insert("accent".to_string(), Color::Indexed(39));
    colors.insert("muted".to_string(), Color::Indexed(244));
    colors.insert("selectedBg".to_string(), Color::Indexed(236));
    colors.insert("scrollbarThumb".to_string(), Color::Indexed(250));
    colors.insert("scrollbarTrack".to_string(), Color::Indexed(238));
    let mut theme = Theme::plain();
    theme.colors = colors;
    theme
}

fn keys() -> Keymap {
    Keymap::defaults()
}

/// Plain text of a rendered line (escape sequences removed).
fn text(line: &str) -> String {
    oxa_tui::strip_ansi(line)
}

fn labelled(n: usize) -> Vec<SelectItem> {
    (0..n)
        .map(|i| SelectItem {
            value: format!("v{i:02}"),
            label: format!("item{i:02}"),
            description: None,
        })
        .collect()
}

/// padding and wrap
#[test]
fn text_wrap_snapshots() {
    let theme = plain();
    let mut t = Text::new("alpha beta gamma delta", 1, 1);
    let lines = t.render(12, &theme);
    assert_eq!(lines.len(), 5, "{lines:?}");
    assert_eq!(lines[0], " ".repeat(12));
    assert_eq!(lines[1], " alpha beta ");
    assert_eq!(lines[2].trim(), "gamma");
    assert_eq!(lines[3].trim(), "delta");
    assert_eq!(lines[4], " ".repeat(12));
    for l in &lines {
        assert_eq!(visible_width(l), 12, "{l:?}");
    }

    // Empty text renders nothing, not even padding.
    let mut empty = Text::new("   ", 1, 1);
    assert!(empty.render(12, &theme).is_empty());

    // Zero padding: wrapped content only, no margin columns.
    let mut bare = Text::new("ab cd", 0, 0);
    assert_eq!(bare.render(3, &theme), vec!["ab ".to_string(), "cd ".to_string()]);

    // Set text replaces the content.
    t.set_text("x");
    let lines = t.render(12, &theme);
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[1], " x          ");
}

/// padding and background
#[test]
fn box_snapshots() {
    let theme = plain();
    let mut b = Boxed::new(2, 1, None);
    b.push(Box::new(Text::new("hi", 0, 0)));
    let lines = b.render(10, &theme);
    assert_eq!(
        lines,
        vec![" ".repeat(10), "  hi      ".to_string(), " ".repeat(10)]
    );

    // Coloured background: every line is a full-width block carrying the background code.
    let theme = coloured();
    let mut b = Boxed::new(1, 0, Some("selectedBg"));
    b.push(Box::new(Text::new("ok", 0, 0)));
    let lines = b.render(8, &theme);
    assert_eq!(lines.len(), 1);
    assert!(lines[0].contains("\x1b[48;5;236m"), "{:?}", lines[0]);
    assert_eq!(visible_width(&lines[0]), 8);

    // No children: no lines.
    let mut empty = Boxed::new(1, 1, Some("selectedBg"));
    assert!(empty.render(8, &theme).is_empty());
    b.clear();
    assert!(b.render(8, &theme).is_empty());
}

/// selection keeps in view; keys
#[test]
fn select_list_scroll_window() {
    let theme = plain();
    let km = keys();
    let mut list = SelectList::new(labelled(12), 5);

    let lines = list.render(40, &theme);
    assert_eq!(lines.len(), 6, "5 rows and the position line: {lines:?}");
    assert_eq!(lines[0], "→ item00");
    assert_eq!(lines[5], "  (1/12)");

    // Down moves the selection; the window follows once it passes the middle.
    for _ in 0..7 {
        assert_eq!(list.key(&Key::press("down"), &km), SelectAction::Moved);
    }
    assert_eq!(list.selected_index(), 7);
    let lines = list.render(40, &theme);
    assert_eq!(text(&lines[0]), "  item05");
    assert!(lines.iter().any(|l| l == "→ item07"), "{lines:?}");
    assert_eq!(lines[5], "  (8/12)");

    // Up on the first row wraps to the last; down on the last wraps to the first.
    list.set_selected_index(0);
    assert_eq!(list.key(&Key::press("up"), &km), SelectAction::Moved);
    assert_eq!(list.selected_index(), 11);
    assert_eq!(list.key(&Key::press("down"), &km), SelectAction::Moved);
    assert_eq!(list.selected_index(), 0);

    // Page keys move by one window and stop at the bounds.
    assert_eq!(list.key(&Key::press("pageDown"), &km), SelectAction::Moved);
    assert_eq!(list.selected_index(), 5);
    assert_eq!(list.key(&Key::press("pageUp"), &km), SelectAction::Moved);
    assert_eq!(list.selected_index(), 0);
    assert_eq!(list.key(&Key::press("pageUp"), &km), SelectAction::Ignored);

    // Confirm carries the value; cancel reports cancellation.
    list.set_selected_index(3);
    assert_eq!(
        list.key(&Key::press("enter"), &km),
        SelectAction::Selected("v03".to_string())
    );
    assert_eq!(list.key(&Key::press("escape"), &km), SelectAction::Cancelled);

    // The Component path records its action for take_action.
    assert!(list.input(&Key::press("down")));
    assert_eq!(list.take_action(), SelectAction::Moved);
    assert_eq!(list.take_action(), SelectAction::Ignored);

    // Short lists show no position line.
    let mut short = SelectList::new(labelled(2), 5);
    assert_eq!(short.render(40, &theme).len(), 2);
}

/// typed filter
#[test]
fn select_list_filter() {
    let theme = plain();
    let items = ["apple", "banana", "grape", "pineapple", "cherry"]
        .iter()
        .map(|s| SelectItem {
            value: s.to_string(),
            label: s.to_string(),
            description: None,
        })
        .collect();
    let mut list = SelectList::new(items, 5);

    list.set_filter("ap");
    assert_eq!(list.len(), 3);
    assert_eq!(list.selected().map(|i| i.label.as_str()), Some("apple"));
    // Best first: "apple" (contiguous), then "grape", then "pineapple".
    let order: Vec<String> = list.render(40, &theme).iter().map(|l| text(l)).collect();
    assert_eq!(order, vec!["→ apple", "  grape", "  pineapple"]);

    list.set_filter("zzz");
    assert!(list.is_empty());
    assert_eq!(list.render(40, &theme), vec!["No matching items".to_string()]);

    // An empty filter restores every item in the original order.
    list.set_filter("");
    assert_eq!(list.len(), 5);
    assert_eq!(list.selected().map(|i| i.label.as_str()), Some("apple"));
    assert_eq!(list.render(40, &theme)[1], "  banana");
}

/// single-line editing
#[test]
fn input_keys() {
    let theme = plain();
    let km = keys();
    let mut input = Input::new("type here");

    // Empty field shows the placeholder after the prompt.
    assert_eq!(input.render(20, &theme)[0].trim_end(), "> type here");

    for c in "hello world".chars() {
        assert_eq!(input.key(&Key::char(c), &km), InputAction::Changed);
    }
    assert_eq!(input.value(), "hello world");

    // Left moves the cursor; typing inserts at the cursor.
    assert_eq!(input.key(&Key::press("left"), &km), InputAction::Changed);
    assert_eq!(input.key(&Key::char('X'), &km), InputAction::Changed);
    assert_eq!(input.value(), "hello worlXd");

    // Word delete backward, then line kill and yank restores the killed text.
    input.set_value("hello world");
    assert_eq!(input.key(&Key::press("ctrl+w"), &km), InputAction::Changed);
    assert_eq!(input.value(), "hello ");
    input.set_value("abc def");
    assert_eq!(input.key(&Key::press("end"), &km), InputAction::Changed);
    assert_eq!(input.key(&Key::press("ctrl+u"), &km), InputAction::Changed);
    assert_eq!(input.value(), "");
    assert_eq!(input.key(&Key::press("ctrl+y"), &km), InputAction::Changed);
    assert_eq!(input.value(), "abc def");

    // Undo restores the value before the last edit.
    assert_eq!(input.key(&Key::press("ctrl+-"), &km), InputAction::Changed);
    assert_eq!(input.value(), "");

    // Nothing to delete at the start of the line.
    input.set_value("x");
    assert_eq!(input.key(&Key::press("end"), &km), InputAction::Changed);
    assert_eq!(input.key(&Key::press("home"), &km), InputAction::Changed);
    assert_eq!(input.key(&Key::press("home"), &km), InputAction::Ignored);
    assert_eq!(input.key(&Key::press("backspace"), &km), InputAction::Ignored);

    // Submit carries the value; escape cancels. Pasted newlines become spaces.
    input.set_value("go");
    assert_eq!(
        input.key(&Key::press("enter"), &km),
        InputAction::Submit("go".to_string())
    );
    assert_eq!(input.key(&Key::press("escape"), &km), InputAction::Cancelled);
    assert_eq!(input.key(&Key::press("end"), &km), InputAction::Changed);
    assert!(input.paste("a\r\nb\nc"));
    assert_eq!(input.value(), "goa b c");

    // Masked fields show one bullet per character.
    let mut secret = Input::new("");
    secret.set_masked(true);
    secret.set_value("pw");
    let line = secret.render(20, &theme)[0].clone();
    assert_eq!(text(&line).trim_end(), "> ••");
    assert_eq!(input.value(), "goa b c");
}

/// deterministic frame sequence
#[test]
fn loader_frames() {
    let theme = plain();
    let mut loader = Loader::new("Working");
    assert_eq!(
        loader.render(30, &theme),
        vec![String::new(), "⠋ Working".to_string()]
    );

    // Frames advance every SPINNER_INTERVAL_MS and wrap after the last one.
    assert!(!loader.tick(0));
    assert!(!loader.tick(SPINNER_INTERVAL_MS - 1));
    assert!(loader.tick(SPINNER_INTERVAL_MS));
    assert_eq!(loader.render(30, &theme)[1], "⠙ Working");
    assert!(loader.tick(3 * SPINNER_INTERVAL_MS));
    assert_eq!(loader.render(30, &theme)[1], "⠸ Working");
    assert!(loader.tick(10 * SPINNER_INTERVAL_MS));
    assert_eq!(loader.render(30, &theme)[1], "⠋ Working");

    // The sequence is a pure function of the elapsed time.
    let mut other = Loader::new("Working");
    other.tick(3 * SPINNER_INTERVAL_MS);
    assert_eq!(other.render(30, &theme)[1], "⠸ Working");

    loader.set_message("Done");
    loader.set_hint(Some("(esc to interrupt)"));
    assert_eq!(loader.render(60, &theme)[1], "⠋ Done (esc to interrupt)");
    loader.set_hint(None);
    assert_eq!(loader.render(60, &theme)[1], "⠋ Done");
}

/// all styles
#[test]
fn border_and_scrollbar_snapshots() {
    let theme = plain();

    // Border: a full rule, and a rule with a label.
    let mut rule = Border::new("border");
    assert_eq!(rule.render(10, &theme), vec!["─".repeat(10)]);
    rule.set_label(Some("x"));
    assert_eq!(rule.render(10, &theme), vec!["── x ─────".to_string()]);
    rule.set_label(None);
    rule.set_token("borderAccent");
    assert_eq!(rule.render(4, &theme), vec!["────".to_string()]);

    // Framed box: titled top edge, padded body rows, bottom edge; each exactly 10 columns.
    let lines = frame_lines(&["ab".to_string()], 10, Some("t"), "border", &theme);
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0], "╭─ t ────╮");
    assert_eq!(lines[1], "│ ab     │");
    assert_eq!(lines[2], "╰────────╯");
    for l in &lines {
        assert_eq!(visible_width(l), 10, "{l:?}");
    }
    let untitled = frame_lines(&[], 6, None, "border", &theme);
    assert_eq!(untitled, vec!["╭────╮".to_string(), "╰────╯".to_string()]);

    // Scrollbar: thumb geometry at the ends and the middle.
    assert_eq!(scrollbar_thumb(100, 10, 0), Some((0, 1)));
    assert_eq!(scrollbar_thumb(100, 10, 90), Some((9, 1)));
    assert_eq!(scrollbar_thumb(100, 10, 45), Some((4, 1)));
    assert_eq!(scrollbar_thumb(5, 10, 0), None);
    assert_eq!(scrollbar_thumb(5, 0, 0), None);

    // Cells: thumb glyph on the thumb row, track glyph elsewhere; blank when nothing scrolls.
    let cells = scrollbar_cells(100, 10, 0, &theme);
    assert_eq!(cells.len(), 10);
    assert_eq!(cells[0], "█");
    assert!(cells[1..].iter().all(|c| c == "│"));
    assert_eq!(scrollbar_cells(5, 10, 0, &theme), vec![" "; 10]);
    let coloured_cells = scrollbar_cells(100, 10, 0, &coloured());
    assert!(coloured_cells[0].contains("\x1b[38;5;250m"));
    assert!(coloured_cells[1].contains("\x1b[38;5;238m"));

    // Offset for a track row lands the thumb on that row (within one row where unreachable).
    assert_eq!(scrollbar_offset_for_row(100, 10, 0), 0);
    assert_eq!(scrollbar_offset_for_row(100, 10, 9), 90);
    let mut last = 0;
    for row in 0..10 {
        let off = scrollbar_offset_for_row(100, 10, row);
        let (top, _) = scrollbar_thumb(100, 10, off).expect("scrolls");
        assert!(
            top.abs_diff(row) <= 1,
            "row {row}: offset {off} puts thumb at {top}"
        );
        assert!(off >= last);
        last = off;
    }
}

/// The thumb is at least one row, sits at the top only at offset 0 and at the bottom only at the
/// last offset, and moves monotonically with the offset.
#[test]
fn scrollbar_thumb_invariants() {
    for rows in 3..=12 {
        for total in (rows + 1)..=(rows + 60) {
            let max = total - rows;
            let mut prev_top = 0;
            for offset in 0..=max {
                let (top, len) = scrollbar_thumb(total, rows, offset).expect("scrolls");
                assert!(
                    len >= 1 && top + len <= rows,
                    "rows {rows} total {total} offset {offset}"
                );
                assert_eq!(
                    top == 0,
                    offset == 0,
                    "top rule: rows {rows} total {total} offset {offset}"
                );
                assert_eq!(
                    top + len == rows,
                    offset == max,
                    "bottom rule: rows {rows} total {total} offset {offset}"
                );
                assert!(top >= prev_top, "monotonic");
                prev_top = top;
            }
        }
    }
}

/// Every component returns lines that fit the requested width, at narrow and wide widths.
#[test]
fn components_fit_their_width() {
    let theme = coloured();
    let km = keys();
    for width in [1u16, 4, 12, 41, 80] {
        let mut lines: Vec<Vec<String>> = vec![
            Text::new("the quick brown fox \x1b[31mjumps\x1b[0m over", 2, 1).render(width, &theme),
            TruncatedText::new("a very long single line of text", 1).render(width, &theme),
            Border::new("border").render(width, &theme),
            Loader::new("working on it").render(width, &theme),
        ];
        let mut boxed = Boxed::new(1, 1, Some("selectedBg"));
        boxed.push(Box::new(Text::new("boxed text that wraps", 0, 0)));
        lines.push(boxed.render(width, &theme));
        let mut list = SelectList::new(
            vec![SelectItem {
                value: "v".into(),
                label: "a label that is quite long".into(),
                description: Some("and a description that is also rather long".into()),
            }],
            3,
        );
        lines.push(list.render(width, &theme));
        let mut input = Input::new("placeholder text");
        input.key(&Key::char('x'), &km);
        lines.push(input.render(width, &theme));
        lines.push(frame_lines(
            &["inner".to_string()],
            width.max(4),
            Some("title"),
            "border",
            &theme,
        ));
        for group in lines {
            for line in group {
                assert!(!line.contains('\n'), "{line:?}");
                assert!(
                    visible_width(&line) <= width.max(4) as usize,
                    "width {width}: {line:?} is {} columns",
                    visible_width(&line)
                );
            }
        }
    }
}

/// Input wider than the field scrolls so that the cursor stays on the line.
#[test]
fn input_scrolls_to_cursor() {
    let theme = plain();
    let km = keys();
    let mut input = Input::new("");
    for c in "abcdefghijklmnopqrstuvwxyz".chars() {
        input.key(&Key::char(c), &km);
    }
    let line = input.render(10, &theme)[0].clone();
    assert!(visible_width(&line) <= 10, "{line:?}");
    assert!(line.starts_with("> "));
    // The cursor sits at the end of the text, inside the line.
    let pos = input.cursor().expect("cursor");
    assert_eq!(pos.row, 0);
    assert!((pos.col as usize) < 10, "{pos:?}");
}
