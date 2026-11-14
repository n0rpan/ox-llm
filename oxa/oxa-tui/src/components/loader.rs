//! B12: Loader, the working spinner line. Owner: task B12. Signatures below are frozen; bodies are yours.
// ported from pi packages/tui/src/components/loader.ts and cancellable-loader.ts
use crate::{truncate_to_width, Component, Theme};

/// The spinner frames (pi's braille set), advanced by [`Loader::tick`].
pub const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// Milliseconds between spinner frames.
pub const SPINNER_INTERVAL_MS: u64 = 80;

/// One empty line, then `<spinner> <message>` (spinner in `accent`, message in `muted`).
/// There is no timer inside: the app calls [`Loader::tick`] from its loop.
pub struct Loader {
    message: String,
    hint: Option<String>,
    frame: usize,
}

impl Loader {
    pub fn new(message: &str) -> Loader {
        Loader {
            message: message.to_string(),
            hint: None,
            frame: 0,
        }
    }
    pub fn set_message(&mut self, message: &str) {
        self.message = message.to_string();
    }
    /// A dimmed hint after the message (`(esc to interrupt)`), `None` to remove.
    pub fn set_hint(&mut self, hint: Option<&str>) {
        self.hint = hint.map(str::to_string);
    }
    /// Advance to the frame for `elapsed_ms` since the loader started; `true` when the frame
    /// changed (a redraw is needed).
    pub fn tick(&mut self, elapsed_ms: u64) -> bool {
        let frame = (elapsed_ms / SPINNER_INTERVAL_MS) as usize % SPINNER_FRAMES.len();
        let changed = frame != self.frame;
        self.frame = frame;
        changed
    }
}

impl Component for Loader {
    fn render(&mut self, width: u16, theme: &Theme) -> Vec<String> {
        let spinner = theme.fg("accent", SPINNER_FRAMES[self.frame]);
        let mut line = format!("{spinner} {}", theme.fg("muted", &self.message));
        if let Some(hint) = &self.hint {
            line.push(' ');
            line.push_str(&theme.dim(hint));
        }
        vec![String::new(), truncate_to_width(&line, width as usize, "…")]
    }
}
