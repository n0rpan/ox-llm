//! D2 acceptance tests: Fullscreen viewport. Owner: task D2.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_interactive::{compose_fullscreen, ScrollbarMode, Viewport, WheelAccel};
use oxa_tui::{
    mode_sequence, restore_sequence, Frame, Mouse, MouseButton, MouseKind, Renderer, ScreenMode, TermMode,
    Terminal, TestTerminal, Theme,
};

fn wheel(kind: MouseKind) -> Mouse {
    Mouse {
        kind,
        button: MouseButton::None,
        col: 0,
        row: 0,
        shift: false,
        alt: false,
        ctrl: false,
    }
}

fn at(kind: MouseKind, col: u16, row: u16) -> Mouse {
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

fn lines(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("line {i}")).collect()
}

/// sequences
#[test]
fn alt_screen_enter_leave() {
    assert_eq!(mode_sequence(TermMode::AltScreen, true), "\x1b[?1049h");
    assert_eq!(mode_sequence(TermMode::AltScreen, false), "\x1b[?1049l");
    assert!(
        restore_sequence().contains("\x1b[?1049l"),
        "a panic or exit must leave the alt screen"
    );

    // The mode switch is recorded by the terminal; the renderer draws rows at absolute positions
    // and clears the whole screen on `clear`.
    let mut term = TestTerminal::new(20, 3);
    term.set_mode(TermMode::AltScreen, true);
    assert!(term.mode_on(TermMode::AltScreen));
    let mut r = Renderer::new(ScreenMode::Alt);
    let frame = Frame {
        lines: vec!["hi".to_string()],
        cursor: None,
    };
    r.draw(&frame, &mut term);
    let out = term.take_output();
    assert!(out.contains("\x1b[1;1H\x1b[Khi"), "{out:?}");
    assert!(
        out.contains("\x1b[3;1H"),
        "every row of the screen is drawn: {out:?}"
    );
    r.clear(&mut term);
    assert!(term.take_output().contains("\x1b[H\x1b[2J"));
    term.set_mode(TermMode::AltScreen, false);
    assert!(!term.mode_on(TermMode::AltScreen));

    // The frame is exactly the viewport rows plus the bottom rows.
    let (frame, vp) = compose_fullscreen(&lines(5), &["editor".to_string(), "footer".to_string()], 4);
    assert_eq!(vp, 2);
    assert_eq!(
        frame,
        vec![
            "line 0".to_string(),
            "line 1".to_string(),
            "editor".to_string(),
            "footer".to_string()
        ]
    );
}

/// follow mode breaks on scroll up, resumes at bottom
#[test]
fn follow_and_scroll() {
    let mut v = Viewport::new(40, 5, ScrollbarMode::Auto, 3);
    v.set_content(10, vec![0, 4], vec![0]);
    assert!(v.follow);
    assert_eq!(v.offset, 5, "follow starts at the bottom");

    // New content while following keeps the bottom visible.
    v.set_content(15, vec![0, 4, 12], vec![0]);
    assert!(v.follow);
    assert_eq!(v.offset, 10);

    // Scrolling up one line leaves follow mode; appended content does not move the view.
    v.scroll_by(-1);
    assert!(!v.follow);
    assert_eq!(v.offset, 9);
    v.set_content(20, vec![0, 4, 12], vec![0]);
    assert!(!v.follow);
    assert_eq!(v.offset, 9);

    // Clamped at both ends; reaching the bottom re-enters follow.
    v.scroll_by(100);
    assert_eq!(v.offset, 15);
    assert!(v.follow);
    v.scroll_by(-100);
    assert_eq!(v.offset, 0);
    assert!(!v.follow);

    v.scroll_to_bottom();
    assert!(v.follow);
    assert_eq!(v.offset, 15);
    v.scroll_to_top();
    assert_eq!(v.offset, 0);
    assert!(!v.follow);

    // Pages move by the height minus the 4-line overlap, at least one line.
    v.scroll_to_top();
    v.set_content(100, vec![0], vec![]);
    v.page_down();
    assert_eq!(v.offset, 1);
    v.page_down();
    assert_eq!(v.offset, 2);
    v.scroll_to_bottom();
    v.page_up();
    assert_eq!(v.offset, 95 - 1);
}

/// settings values
#[test]
fn scrollbar_modes() {
    assert_eq!(ScrollbarMode::parse("auto"), ScrollbarMode::Auto);
    assert_eq!(ScrollbarMode::parse("always"), ScrollbarMode::Always);
    assert_eq!(ScrollbarMode::parse("never"), ScrollbarMode::Never);
    assert_eq!(ScrollbarMode::parse("hidden"), ScrollbarMode::Never);
    assert_eq!(ScrollbarMode::parse("bogus"), ScrollbarMode::Auto);

    // Auto and Always reserve the column; Never does not.
    assert_eq!(Viewport::new(40, 5, ScrollbarMode::Auto, 3).content_width(), 39);
    assert_eq!(Viewport::new(40, 5, ScrollbarMode::Always, 3).content_width(), 39);
    assert_eq!(Viewport::new(40, 5, ScrollbarMode::Never, 3).content_width(), 40);

    let mut auto = Viewport::new(40, 5, ScrollbarMode::Auto, 3);
    auto.set_content(4, vec![0], vec![]);
    assert!(
        !auto.scrollbar_visible(),
        "auto hides the bar when everything fits"
    );
    auto.set_content(10, vec![0], vec![]);
    assert!(auto.scrollbar_visible());

    let mut always = Viewport::new(40, 5, ScrollbarMode::Always, 3);
    always.set_content(4, vec![0], vec![]);
    assert!(always.scrollbar_visible());

    let mut never = Viewport::new(40, 5, ScrollbarMode::Never, 3);
    never.set_content(10, vec![0], vec![]);
    assert!(!never.scrollbar_visible());

    // Rendered rows are exactly `rows` lines of exactly `width` columns, with or without the bar.
    let theme = Theme::plain();
    let content = lines(10);
    for mode in [ScrollbarMode::Auto, ScrollbarMode::Always, ScrollbarMode::Never] {
        let mut v = Viewport::new(40, 5, mode, 3);
        v.set_content(content.len(), vec![0], vec![]);
        let out = v.render(&content, &theme);
        assert_eq!(out.len(), 5);
        for line in &out {
            assert_eq!(oxa_tui::visible_width(line), 40, "{line:?}");
        }
    }
}

/// wheel scroll amount
#[test]
fn wheel_lines() {
    let mut v = Viewport::new(40, 5, ScrollbarMode::Auto, 3);
    v.set_content(30, vec![0], vec![]);
    assert_eq!(v.offset, 25);
    assert!(v.mouse(&wheel(MouseKind::WheelUp)));
    assert_eq!(v.offset, 22);
    assert!(!v.follow);
    assert!(v.mouse(&wheel(MouseKind::WheelDown)));
    assert_eq!(v.offset, 25);
    assert!(v.follow);

    let mut one = Viewport::new(40, 5, ScrollbarMode::Auto, 1);
    one.set_content(30, vec![0], vec![]);
    one.mouse(&wheel(MouseKind::WheelUp));
    assert_eq!(one.offset, 24);

    // Other mouse events are not consumed by the wheel handler.
    assert!(!v.mouse(&at(MouseKind::Move, 0, 0)));
}

/// jump to previous/next prompt
#[test]
fn prompt_jump() {
    // Blocks: 0 user (line 0), 1 assistant (5), 2 user (10), 3 assistant (20), 4 user (25).
    let mut v = Viewport::new(40, 5, ScrollbarMode::Auto, 3);
    v.set_content(30, vec![0, 5, 10, 20, 25], vec![0, 2, 4]);
    assert_eq!(v.offset, 25);

    assert!(v.jump_prompt(false));
    assert_eq!(v.offset, 10);
    assert!(!v.follow);
    assert!(v.jump_prompt(false));
    assert_eq!(v.offset, 0);
    assert!(!v.jump_prompt(false), "no previous prompt above the first one");
    assert_eq!(v.offset, 0);

    assert!(v.jump_prompt(true));
    assert_eq!(v.offset, 10);
    assert!(v.jump_prompt(true));
    assert_eq!(v.offset, 25);
    assert!(v.follow);
    assert!(!v.jump_prompt(true), "no next prompt below the last one");

    // Rows of the viewport are the visible slice.
    v.scroll_to_top();
    let content = lines(30);
    let out = v.render(&content, &Theme::plain());
    assert!(out[0].starts_with("line 0"));
    assert_eq!(v.line_at(0), Some(0));
    assert_eq!(v.line_at(4), Some(4));
    assert_eq!(v.line_at(5), None);
}

/// anchor line stable
#[test]
fn resize_keeps_position() {
    let mut v = Viewport::new(80, 10, ScrollbarMode::Auto, 3);
    v.set_content(40, vec![0, 10, 25], vec![]);
    v.scroll_to_top();
    v.scroll_by(12);
    assert_eq!(v.offset, 12, "block 1 starts at 10, two lines in");

    // Narrower: the content re-wraps, block 1 now starts at 14 and is longer. The anchor is block 1,
    // two lines in, so the top line is 16.
    v.resize(40, 10);
    v.set_content(50, vec![0, 14, 33], vec![]);
    assert_eq!(v.offset, 16);
    assert!(!v.follow);

    // Rows change as well: the same anchor stays on top.
    v.resize(40, 8);
    v.set_content(50, vec![0, 14, 33], vec![]);
    assert_eq!(v.offset, 16);

    // In follow mode the bottom stays.
    let mut f = Viewport::new(80, 10, ScrollbarMode::Auto, 3);
    f.set_content(40, vec![0, 10, 25], vec![]);
    assert!(f.follow);
    f.resize(40, 10);
    f.set_content(50, vec![0, 14, 33], vec![]);
    assert!(f.follow);
    assert_eq!(f.offset, 40);
}

#[test]
fn reveal_and_scrollbar_drag() {
    let mut v = Viewport::new(40, 5, ScrollbarMode::Always, 3);
    v.set_content(50, vec![0], vec![]);
    v.scroll_to_top();
    v.reveal(7);
    assert_eq!(v.offset, 3);
    v.reveal(1);
    assert_eq!(v.offset, 1);
    v.reveal(20);
    assert_eq!(v.offset, 16);

    // Press on the rightmost column (the bar) jumps to the bottom row of the bar.
    assert!(v.mouse(&at(MouseKind::Press, 39, 4)));
    assert_eq!(v.offset, 45);
    assert!(v.follow);
    // A press elsewhere is not consumed.
    assert!(!v.mouse(&at(MouseKind::Press, 3, 2)));
}

#[test]
fn compose_bottom_wins_when_small() {
    let (frame, vp) = compose_fullscreen(&lines(10), &["a".into(), "b".into(), "c".into()], 2);
    assert_eq!(vp, 0);
    assert_eq!(frame, vec!["b".to_string(), "c".to_string()]);

    let (frame, vp) = compose_fullscreen(&lines(1), &[], 3);
    assert_eq!(vp, 3);
    assert_eq!(frame.len(), 3);
    assert_eq!(frame[0], "line 0");
    assert_eq!(frame[2], "");
}

/// pi's WheelScrollAccelerator: fixed counts as given; `auto` is one line per isolated notch
/// and scales with the event rate (100 ms -> 1, 50 ms -> 2, 20 ms -> 5, at most 6); bursts
/// under 5 ms and non-accelerating terminals move one line.
#[test]
fn wheel_acceleration_follows_pi() {
    let mut fixed = WheelAccel::new(Some(5), true);
    assert_eq!(fixed.next(1, 0), 5);
    assert_eq!(fixed.next(1, 1), 5);
    let mut auto = WheelAccel::new(None, true);
    assert_eq!(auto.next(1, 1000), 1, "an isolated notch");
    assert_eq!(auto.next(1, 1050), 2, "50 ms apart");
    assert_eq!(auto.next(1, 1052), 1, "a burst");
    let mut fast = WheelAccel::new(None, true);
    fast.next(-1, 0);
    let lines: Vec<u16> = (1..=4).map(|i| fast.next(-1, i * 20)).collect();
    assert_eq!(lines, vec![5, 5, 5, 5]);
    let mut spin = WheelAccel::new(None, true);
    spin.next(1, 0);
    assert_eq!(spin.next(1, 10), 6, "capped at six");
    assert_eq!(spin.next(-1, 20), 1, "a direction change starts over");
    assert_eq!(spin.next(-1, 400), 1, "a pause ends the gesture");
    let mut mac = WheelAccel::new(None, false);
    mac.next(1, 0);
    assert_eq!(
        mac.next(1, 10),
        1,
        "no acceleration where the terminal accelerates"
    );
}
