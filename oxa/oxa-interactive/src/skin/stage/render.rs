//! Rendering of one data frame (design 3.1 and 4.1). A frame is a text row and an ink row of
//! the same length; the ink row names the colour role of each cell, and `I` puts the current
//! tool's icon in its cell. The row is styled by the same `Canvas::paint` as the built-in
//! scenes, so colour comes from theme tokens only.
//!
//! Pure: no clock, no I/O. The frame is picked from `activity_ms` and the scene's `step_ms`.

use crate::skin::arcade::{Canvas, Ink, MAX_CELLS};
use oxa_tui::Theme;

/// How a scene plays its frames (`loop` in the scene file).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopMode {
    /// Repeats for as long as the activity runs.
    Loop,
    /// Plays once, then is gone.
    Once,
    /// Plays once, then the last frame stays.
    Hold,
}

/// One frame: the text row and its ink row, both as many characters as the stage has cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowFrame<'a> {
    pub text: &'a str,
    pub inks: &'a str,
}

/// The frame to show `activity_ms` into a scene of `len` frames, each `step_ms` long.
/// `None` when there is no frame: an empty scene, or a `once` scene that has played.
pub fn frame_index(len: usize, activity_ms: u64, step_ms: u64, mode: LoopMode) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let step = usize::try_from(activity_ms / step_ms.max(1)).unwrap_or(usize::MAX);
    match mode {
        LoopMode::Loop => Some(step % len),
        LoopMode::Once => (step < len).then_some(step),
        LoopMode::Hold => Some(step.min(len - 1)),
    }
}

/// The frame as a styled row, `icon` in the `I` cells, `hero` the activity's token (`error`
/// while `hot`, as the ox does under YOLO). `None` for a plain theme (no scene at all), for
/// rows that do not match in length, for an ink outside the table, or for a row wider than the
/// stage can draw.
pub fn render_frame(frame: RowFrame<'_>, icon: char, hero: &str, hot: bool, theme: &Theme) -> Option<String> {
    if theme.is_plain() {
        return None;
    }
    let cells = frame.text.chars().count();
    if cells == 0 || cells != frame.inks.chars().count() || cells > MAX_CELLS {
        return None;
    }
    let mut canvas = Canvas::new(cells);
    for (x, (ch, role)) in frame.text.chars().zip(frame.inks.chars()).enumerate() {
        let (ch, ink) = match role {
            'H' => (ch, Ink::Hero),
            'D' => (ch, Ink::Dim),
            'G' => (ch, Ink::Gold),
            'C' => (ch, Ink::Crate),
            'O' => (ch, Ink::Good),
            'B' => (ch, Ink::Bad),
            'I' => (icon, Ink::Icon),
            '.' => (' ', Ink::Dim),
            _ => return None,
        };
        canvas.put(x, ch, ink);
    }
    Some(canvas.paint(hero, hot, theme))
}
