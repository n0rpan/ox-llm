//! The stage frames (C3): golden rows at 50 and 80 columns, theme tokens only, no scene with a
//! plain theme, a held frame stays, and the step comes from the activity's elapsed time.
//! Public API only.
use oxa_interactive::scene_cells;
use oxa_interactive::skin::stage::render::{frame_index, render_frame, LoopMode, RowFrame};
use oxa_tui::{strip_ansi, ColorMode, Theme};

fn colour() -> Theme {
    oxa_tui::builtin_theme("dark", ColorMode::TrueColor, None).expect("builtin dark theme")
}

#[test]
fn frame_snapshot_at_50_columns() {
    // 50 columns: an 8-cell run stage, the `tool.bash.pipe` frame from design 4.1.
    let cells = scene_cells(50);
    assert_eq!(cells, 8);
    let frame = RowFrame {
        text: "▪────▐$▌",
        inks: "HDDDDCCC",
    };
    let theme = colour();
    let row = render_frame(frame, '$', "warning", false, &theme).expect("a frame at 50 columns");
    assert_eq!(strip_ansi(&row), "▪────▐$▌");
    let golden = format!(
        "{}{}{}",
        theme.fg("warning", "▪"),
        theme.fg("muted", "────"),
        theme.fg("accent", "▐$▌")
    );
    assert_eq!(row, golden);
}

#[test]
fn frame_snapshot_at_80_columns() {
    let cells = scene_cells(80);
    assert_eq!(cells, 12);
    let frame = RowFrame {
        text: "▪────────▐$▌",
        inks: "HDDDDDDDDCCC",
    };
    let theme = colour();
    let row = render_frame(frame, '$', "warning", false, &theme).expect("a frame at 80 columns");
    assert_eq!(strip_ansi(&row), "▪────────▐$▌");
    let golden = format!(
        "{}{}{}",
        theme.fg("warning", "▪"),
        theme.fg("muted", "────────"),
        theme.fg("accent", "▐$▌")
    );
    assert_eq!(row, golden);
}

#[test]
fn colour_comes_from_theme_tokens_only() {
    // Every ink role, and the `I` cell takes the icon, coloured by the tool token.
    let frame = RowFrame {
        text: "■●◆▲◐♥?✗",
        inks: "HDGCOBIB",
    };
    let theme = colour();
    let row = render_frame(frame, '$', "accent", false, &theme).expect("a frame");
    let golden = format!(
        "{}{}{}{}{}{}{}{}",
        theme.fg("accent", "■"),
        theme.fg("muted", "●"),
        theme.fg("warning", "◆"),
        theme.fg("accent", "▲"),
        theme.fg("success", "◐"),
        theme.fg("error", "♥"),
        theme.fg("warning", "$"),
        theme.fg("error", "✗"),
    );
    assert_eq!(row, golden);
    // A hot hero (YOLO) burns in the error token.
    let hot = render_frame(
        RowFrame {
            text: "■", inks: "H"
        },
        '$',
        "accent",
        true,
        &theme,
    )
    .unwrap();
    assert_eq!(hot, theme.fg("error", "■"));
}

#[test]
fn a_plain_theme_draws_no_scene() {
    let frame = RowFrame {
        text: "▪────▐$▌",
        inks: "HDDDDCCC",
    };
    assert_eq!(render_frame(frame, '$', "warning", false, &Theme::plain()), None);
}

#[test]
fn a_bad_frame_draws_nothing() {
    let theme = colour();
    // Lengths differ, an ink is outside the table, or the row is wider than the stage.
    assert_eq!(
        render_frame(
            RowFrame {
                text: "ab",
                inks: "H"
            },
            '$',
            "accent",
            false,
            &theme
        ),
        None
    );
    assert_eq!(
        render_frame(RowFrame { text: "a", inks: "X" }, '$', "accent", false, &theme),
        None
    );
    let wide = "a".repeat(13);
    let inks = "H".repeat(13);
    assert_eq!(
        render_frame(
            RowFrame {
                text: &wide,
                inks: &inks
            },
            '$',
            "accent",
            false,
            &theme
        ),
        None
    );
}

#[test]
fn hold_keeps_the_last_frame() {
    // Four frames of 125 ms, held: far past the end the last frame is still the one.
    assert_eq!(frame_index(4, 10_000, 125, LoopMode::Hold), Some(3));
    assert_eq!(frame_index(4, 0, 125, LoopMode::Hold), Some(0));
    // Once: played through, then nothing.
    assert_eq!(frame_index(4, 375, 125, LoopMode::Once), Some(3));
    assert_eq!(frame_index(4, 500, 125, LoopMode::Once), None);
    // Loop wraps, and the step comes from the elapsed time and the step length.
    assert_eq!(frame_index(4, 500, 125, LoopMode::Loop), Some(0));
    assert_eq!(frame_index(6, 250, 250, LoopMode::Loop), Some(1));
    // An empty scene and a zero step never panic.
    assert_eq!(frame_index(0, 500, 125, LoopMode::Loop), None);
    assert!(frame_index(3, 5, 0, LoopMode::Loop).is_some());
}
