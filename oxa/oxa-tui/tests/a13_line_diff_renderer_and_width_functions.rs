//! A13 acceptance tests: Line-diff renderer and width functions. Owner: task A13.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_tui::{
    cluster_boundaries, compose_overlay, pad_to_width, slice_by_column, strip_ansi, truncate_to_width,
    visible_width, wrap_ansi, CursorPos, Frame, OverlayAnchor, Renderer, ScreenMode, TermMode, TestTerminal,
    Theme,
};

const SYNC_ON: &str = "\x1b[?2026h";
const SYNC_OFF: &str = "\x1b[?2026l";

fn frame(lines: &[&str]) -> Frame {
    Frame {
        lines: lines.iter().map(|s| s.to_string()).collect(),
        cursor: None,
    }
}

fn synced(body: &str) -> String {
    format!("{SYNC_ON}{body}{SYNC_OFF}")
}

/// changed line only is rewritten (golden byte stream)
#[test]
fn line_diff_minimal_writes_golden() {
    let mut term = TestTerminal::new(20, 10);
    let mut r = Renderer::new(ScreenMode::Main);

    r.draw(&frame(&["a", "b", "c"]), &mut term);
    assert_eq!(
        term.take_output(),
        synced("\r\x1b[Ka\x1b[0m\r\n\x1b[Kb\x1b[0m\r\n\x1b[Kc\x1b[0m"),
        "first frame writes every line once"
    );

    r.draw(&frame(&["a", "B", "c"]), &mut term);
    assert_eq!(
        term.take_output(),
        synced("\x1b[1A\r\x1b[KB\x1b[0m"),
        "only the changed line is rewritten; the cursor moves up from the last line"
    );

    r.draw(&frame(&["a", "B", "c"]), &mut term);
    assert_eq!(term.take_output(), "", "an unchanged frame writes nothing");
    assert_eq!(term.flushes, 3, "one flush per draw");
}

/// CJK and emoji widths
#[test]
fn wide_chars_width() {
    assert_eq!(visible_width("中文"), 4);
    assert_eq!(visible_width("a中b"), 4);
    assert_eq!(visible_width("😀"), 2);
    assert_eq!(
        visible_width("👨\u{200d}👩\u{200d}👧"),
        2,
        "ZWJ family is one wide cluster"
    );
    assert_eq!(visible_width("🇯🇵"), 2, "flag is one wide cluster");
    assert_eq!(visible_width("\x1b[31m中\x1b[0m"), 2, "escapes have width 0");
    assert_eq!(visible_width("\t"), 3, "a tab is 3 columns");
    assert_eq!(
        truncate_to_width("中文字", 3, ""),
        "中",
        "a wide cluster is not split"
    );
    assert_eq!(
        slice_by_column("中文", 1, 2),
        "  ",
        "a wide cluster cut by both edges becomes spaces"
    );
}

/// SGR state carried across wrapped lines
#[test]
fn ansi_aware_wrap() {
    let lines = wrap_ansi("\x1b[31mhello world foo\x1b[0m", 6);
    assert_eq!(
        lines,
        vec![
            "\x1b[31mhello\x1b[0m".to_string(),
            "\x1b[31mworld\x1b[0m".to_string(),
            "\x1b[31mfoo\x1b[0m".to_string(),
        ]
    );
    for line in &lines {
        assert!(visible_width(line) <= 6);
        assert!(line.starts_with("\x1b[31m"), "style re-opened on every line");
    }
    assert_eq!(
        wrap_ansi("a\n\nb", 10),
        vec!["a", "", "b"],
        "empty paragraph gives an empty line"
    );
    assert_eq!(
        wrap_ansi("abcdefgh", 3),
        vec!["abc", "def", "gh"],
        "long word breaks at clusters"
    );
    assert_eq!(wrap_ansi("abc", 0), vec!["abc"], "width 0 does not wrap");
}

/// ellipsis and reset
#[test]
fn ansi_aware_truncate() {
    assert_eq!(
        truncate_to_width("\x1b[31mhello world\x1b[0m", 8, "…"),
        "\x1b[31mhello w…\x1b[0m"
    );
    assert_eq!(
        truncate_to_width("abc", 5, "…"),
        "abc",
        "a fitting line is unchanged"
    );
    let cut = truncate_to_width("中文字符abc", 5, "");
    assert_eq!(cut, "中文", "cut keeps whole clusters and fits");
    assert!(visible_width(&cut) <= 5);
}

/// width change
#[test]
fn resize_full_redraw() {
    // Main: a width change clears and repaints the whole live region.
    let mut term = TestTerminal::new(20, 10);
    let mut r = Renderer::new(ScreenMode::Main);
    r.draw(&frame(&["one", "two"]), &mut term);
    term.take_output();
    term.resize(30, 10);
    r.draw(&frame(&["one", "two"]), &mut term);
    let out = term.take_output();
    assert!(out.contains("\x1b[J"), "region cleared below the cursor");
    assert!(out.contains("\x1b[Kone"), "first line repainted");
    assert!(out.contains("\x1b[Ktwo"), "second line repainted");

    // Alt: every row is rewritten after a size change, even unchanged ones.
    let mut term = TestTerminal::new(20, 5);
    let mut r = Renderer::new(ScreenMode::Alt);
    r.draw(&frame(&["one", "two"]), &mut term);
    term.take_output();
    term.resize(30, 5);
    r.draw(&frame(&["one", "two"]), &mut term);
    let out = term.take_output();
    for row in 1..=5 {
        assert!(out.contains(&format!("\x1b[{row};1H")), "row {row} rewritten");
    }
}

/// fewer lines than before
#[test]
fn shrink_clears_removed_lines() {
    let mut term = TestTerminal::new(20, 10);
    let mut r = Renderer::new(ScreenMode::Main);
    r.draw(&frame(&["a", "b", "c"]), &mut term);
    term.take_output();
    r.draw(&frame(&["a", "b"]), &mut term);
    assert_eq!(
        term.take_output(),
        synced("\r\x1b[J"),
        "surplus line cleared, nothing rewritten"
    );

    // Alt: the removed row is blanked.
    let mut term = TestTerminal::new(20, 3);
    let mut r = Renderer::new(ScreenMode::Alt);
    r.draw(&frame(&["a", "b", "c"]), &mut term);
    term.take_output();
    r.draw(&frame(&["a"]), &mut term);
    let out = term.take_output();
    assert_eq!(out, synced("\x1b[2;1H\x1b[K\x1b[0m\x1b[3;1H\x1b[K\x1b[0m"));
}

/// appended lines do not rewrite earlier ones
#[test]
fn first_render_and_append_only() {
    let mut term = TestTerminal::new(20, 10);
    let mut r = Renderer::new(ScreenMode::Main);
    r.draw(&frame(&["a"]), &mut term);
    assert_eq!(term.take_output(), synced("\r\x1b[Ka\x1b[0m"));
    r.draw(&frame(&["a", "b"]), &mut term);
    assert_eq!(
        term.take_output(),
        synced("\r\n\x1b[Kb\x1b[0m"),
        "earlier line not rewritten"
    );
    r.draw(&frame(&["a", "b", "c"]), &mut term);
    assert_eq!(term.take_output(), synced("\r\n\x1b[Kc\x1b[0m"));

    // Committed lines are written once, above the live region, and the region is repainted below.
    r.commit(&["done".to_string()], &mut term);
    assert_eq!(term.take_output(), synced("\x1b[2A\r\x1b[Jdone\x1b[0m\r\n"));
    r.draw(&frame(&["x"]), &mut term);
    assert_eq!(term.take_output(), synced("\r\x1b[Kx\x1b[0m"));
}

/// width 0 code points
#[test]
fn zero_width_and_combining() {
    assert_eq!(visible_width("e\u{301}"), 1, "combining mark adds no column");
    assert_eq!(visible_width("\u{301}"), 0, "lone combining mark has width 0");
    assert_eq!(visible_width("a\u{200b}b"), 2, "zero-width space has width 0");
    assert_eq!(visible_width("\u{200d}"), 0, "lone joiner has width 0");
    assert_eq!(visible_width("\u{0007}"), 0, "control characters have width 0");
    assert_eq!(strip_ansi("\x1b[1me\u{301}\x1b[0m"), "e\u{301}");
    assert_eq!(cluster_boundaries("e\u{301}a"), vec![0, 3, 4]);
    assert_eq!(
        truncate_to_width("e\u{301}e\u{301}e\u{301}", 2, ""),
        "e\u{301}e\u{301}"
    );
    assert_eq!(visible_width(&pad_to_width("e\u{301}", 3, false)), 3);
}

// ---- additional coverage ---------------------------------------------------------------------

#[test]
fn slice_keeps_style_at_start() {
    let line = "\x1b[1mbold text\x1b[0m plain";
    assert_eq!(slice_by_column(line, 5, 4), "\x1b[1mtext\x1b[0m");
    assert_eq!(slice_by_column("abc", 10, 2), "", "past the end is empty");
}

#[test]
fn pad_inside_style_goes_before_reset() {
    assert_eq!(pad_to_width("\x1b[44mx\x1b[0m", 3, true), "\x1b[44mx  \x1b[0m");
    assert_eq!(pad_to_width("\x1b[44mx\x1b[0m", 3, false), "\x1b[44mx\x1b[0m  ");
}

#[test]
fn overlay_replaces_cells_and_keeps_styles() {
    let base = vec!["\x1b[31mabcdef\x1b[0m".to_string(), "uvwxyz".to_string()];
    let overlay = vec!["XY".to_string()];
    let out = compose_overlay(&base, &overlay, 2, OverlayAnchor::At(0, 2), 6, 2);
    assert_eq!(out[0], "\x1b[31mab\x1b[0mXY\x1b[0m\x1b[31mef\x1b[0m");
    assert_eq!(out[1], "uvwxyz", "rows without overlay are untouched");
    for line in &out {
        assert!(visible_width(line) <= 6);
    }
}

#[test]
fn overlay_clipped_to_area() {
    let base = vec!["abc".to_string()];
    let overlay = vec!["XXXX".to_string(), "YY".to_string()];
    let out = compose_overlay(&base, &overlay, 4, OverlayAnchor::TopRight, 5, 1);
    assert_eq!(out.len(), 1, "rows are not added");
    assert_eq!(visible_width(&out[0]), 5);
    assert!(out[0].contains("XXXX"));
}

#[test]
fn cursor_is_placed_and_hidden() {
    let mut term = TestTerminal::new(20, 10);
    let mut r = Renderer::new(ScreenMode::Main);
    let mut f = frame(&["prompt> "]);
    f.cursor = Some(CursorPos { row: 0, col: 8 });
    r.draw(&f, &mut term);
    assert!(term.mode_on(TermMode::CursorVisible));
    term.take_output();

    // Same lines, cursor moved: only the cursor is placed.
    f.cursor = Some(CursorPos { row: 0, col: 3 });
    r.draw(&f, &mut term);
    assert_eq!(
        term.take_output(),
        synced("\r\x1b[4G"),
        "cursor moved, lines untouched"
    );

    // No cursor: hidden.
    f.cursor = None;
    r.draw(&f, &mut term);
    assert!(!term.mode_on(TermMode::CursorVisible));
}

#[test]
fn main_keeps_only_last_rows() {
    let mut term = TestTerminal::new(20, 2);
    let mut r = Renderer::new(ScreenMode::Main);
    r.draw(&frame(&["1", "2", "3"]), &mut term);
    assert_eq!(r.last_lines(), &["2".to_string(), "3".to_string()]);
}

#[test]
fn alt_frame_has_exactly_rows_lines() {
    let mut term = TestTerminal::new(20, 3);
    let mut r = Renderer::new(ScreenMode::Alt);
    r.draw(&frame(&["a", "b", "c", "d"]), &mut term);
    assert_eq!(r.last_lines().len(), 3);
    assert_eq!(
        r.last_lines(),
        &["a".to_string(), "b".to_string(), "c".to_string()]
    );
}

#[test]
fn clear_erases_and_resets_state() {
    let mut term = TestTerminal::new(20, 10);
    let mut r = Renderer::new(ScreenMode::Main);
    r.draw(&frame(&["a"]), &mut term);
    term.take_output();
    r.clear(&mut term);
    assert_eq!(term.take_output(), "\r\x1b[J");
    assert!(r.last_lines().is_empty());
    assert!(term.mode_on(TermMode::CursorVisible));
}

struct Fixed(Vec<String>);

impl oxa_tui::Component for Fixed {
    fn render(&mut self, _width: u16, _theme: &Theme) -> Vec<String> {
        self.0.clone()
    }
}

#[test]
fn render_stack_concatenates_children() {
    let mut a = Fixed(vec!["x".into()]);
    let mut b = Fixed(vec!["y".into(), "z".into()]);
    let theme = Theme::plain();
    let out = oxa_tui::render_stack(&mut [&mut a, &mut b], 10, &theme);
    assert_eq!(out, vec!["x", "y", "z"]);
}

/// The frame's cursor is the hardware cursor: positioned (input methods place their candidate
/// window there) and shown; a frame without a cursor hides it; `clear` (exit, suspend) leaves
/// it visible.
#[test]
fn hardware_cursor_is_positioned_and_shown() {
    for mode in [ScreenMode::Alt, ScreenMode::Main] {
        let at = |cursor| Frame {
            lines: vec!["one".to_string(), "two".to_string()],
            cursor,
        };
        let mut term = TestTerminal::new(20, 5);
        let mut r = Renderer::new(mode);
        r.draw(&at(Some(CursorPos { row: 1, col: 2 })), &mut term);
        let out = term.take_output();
        let moved = match mode {
            ScreenMode::Alt => out.contains("\x1b[2;3H"),
            ScreenMode::Main => out.contains("\x1b[3G"),
        };
        assert!(moved, "{mode:?}: positioned: {out:?}");
        assert!(term.mode_on(TermMode::CursorVisible), "{mode:?}: shown");
        r.draw(&at(None), &mut term);
        assert!(
            !term.mode_on(TermMode::CursorVisible),
            "{mode:?}: no cursor, hidden"
        );
        r.clear(&mut term);
        assert!(
            term.mode_on(TermMode::CursorVisible),
            "{mode:?}: visible after clear"
        );
    }
}

/// `repaint` (pi's clearOnShrink full re-render): the next frame rewrites the whole live
/// region from its first row, moving up from where the cursor really is.
#[test]
fn repaint_rewrites_the_region_from_its_first_row() {
    let mut term = TestTerminal::new(20, 10);
    let mut r = Renderer::new(ScreenMode::Main);
    r.draw(&frame(&["a", "b", "c"]), &mut term);
    term.take_output();
    r.draw(&frame(&["a", "b"]), &mut term);
    let diffed = term.take_output();
    assert!(
        !diffed.contains("\x1b[Ka"),
        "a plain shrink only clears: {diffed:?}"
    );
    r.draw(&frame(&["a", "b", "c"]), &mut term);
    term.take_output();
    r.repaint();
    r.draw(&frame(&["a", "b"]), &mut term);
    assert_eq!(
        term.take_output(),
        synced("\x1b[2A\r\x1b[J\x1b[Ka\x1b[0m\r\n\x1b[Kb\x1b[0m"),
        "up two rows from the last line, clear, rewrite"
    );
}

/// narrowing reflows the old live lines into more rows; the clear must start above all of them
#[test]
fn narrowing_clears_reflowed_height() {
    let mut term = TestTerminal::new(20, 20);
    let mut r = Renderer::new(ScreenMode::Main);
    r.set_reflow(true);
    let full = "x".repeat(20);
    r.draw(&frame(&[&full, &full, &full]), &mut term);
    term.take_output();
    term.resize(10, 20);
    r.draw(&frame(&[&full[..10], &full[..10], &full[..10]]), &mut term);
    let out = term.take_output();
    assert!(
        out.contains("\x1b[5A\r\x1b[J"),
        "moves up over the 6 reflowed rows: {out:?}"
    );
}

/// the cursor partway through a reflowed line: only the rows above the cursor are climbed
#[test]
fn narrowing_with_cursor_at_line_start_does_not_climb() {
    let mut term = TestTerminal::new(20, 20);
    let mut r = Renderer::new(ScreenMode::Main);
    r.set_reflow(true);
    let full = "x".repeat(20);
    let mut f = frame(&[&full]);
    f.cursor = Some(CursorPos { row: 0, col: 0 });
    r.draw(&f, &mut term);
    term.take_output();
    term.resize(10, 20);
    let mut f = frame(&[&full[..10]]);
    f.cursor = Some(CursorPos { row: 0, col: 0 });
    r.draw(&f, &mut term);
    let out = term.take_output();
    assert!(!out.contains("A\r"), "no climb above the region: {out:?}");
    assert!(out.contains("\r\x1b[J"), "{out:?}");
}

/// the cursor on the first cell of a reflowed row: that row is above the cursor's line start
#[test]
fn narrowing_with_cursor_on_row_boundary_climbs_to_its_row() {
    let mut term = TestTerminal::new(20, 20);
    let mut r = Renderer::new(ScreenMode::Main);
    r.set_reflow(true);
    let full = "x".repeat(20);
    let mut f = frame(&[&full]);
    f.cursor = Some(CursorPos { row: 0, col: 10 });
    r.draw(&f, &mut term);
    term.take_output();
    term.resize(10, 20);
    let mut f = frame(&[&full[..10]]);
    f.cursor = Some(CursorPos { row: 0, col: 0 });
    r.draw(&f, &mut term);
    let out = term.take_output();
    assert!(
        out.contains("\x1b[1A\r"),
        "climb one row to the region start: {out:?}"
    );
}

/// a wide character that does not fit at a row end moves to the next row
#[test]
fn narrowing_counts_wide_character_wrap() {
    let mut term = TestTerminal::new(20, 20);
    let mut r = Renderer::new(ScreenMode::Main);
    r.set_reflow(true);
    let line = "あ".repeat(5); // 10 columns
    r.draw(&frame(&[&line, "z"]), &mut term);
    term.take_output();
    term.resize(5, 20);
    r.draw(&frame(&[&line, "z"]), &mut term);
    let out = term.take_output();
    // 5 columns hold two wide chars (4 cols): 3 rows, plus "z" row; cursor on last line
    assert!(out.contains("\x1b[4A\r\x1b[J"), "{out:?}");
}

/// a terminal that does not reflow must never be climbed past the live region
#[test]
fn narrowing_without_reflow_never_climbs_past_the_region() {
    let mut term = TestTerminal::new(20, 20);
    let mut r = Renderer::new(ScreenMode::Main);
    r.set_reflow(false);
    let full = "x".repeat(20);
    r.draw(&frame(&[&full, &full, &full]), &mut term);
    term.take_output();
    term.resize(10, 20);
    r.draw(&frame(&[&full[..10], &full[..10], &full[..10]]), &mut term);
    let out = term.take_output();
    assert!(out.contains("\x1b[2A\r\x1b[J"), "{out:?}");
}

/// the cursor row counts wide-character carry: the cursor on the 5th wide char (col 8) at 3 columns is row 4, not 8/3
#[test]
fn narrowing_cursor_row_counts_wide_carry() {
    let mut term = TestTerminal::new(20, 20);
    let mut r = Renderer::new(ScreenMode::Main);
    r.set_reflow(true);
    let line = "あ".repeat(5);
    let mut f = frame(&[&line]);
    f.cursor = Some(CursorPos { row: 0, col: 8 });
    r.draw(&f, &mut term);
    term.take_output();
    term.resize(3, 20);
    r.draw(&f, &mut term);
    let out = term.take_output();
    assert!(out.contains("\x1b[4A\r\x1b[J"), "{out:?}");
}
