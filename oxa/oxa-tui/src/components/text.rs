//! B12: Text and TruncatedText components. Owner: task B12. Signatures below are frozen; bodies are yours.
// ported from pi packages/tui/src/components/text.ts and truncated-text.ts
use crate::{truncate_to_width, visible_width, wrap_ansi, Component, Theme};

const RESET: &str = "\x1b[0m";
const BG_CLOSE: &str = "\x1b[49m";
const FG_CLOSE: &str = "\x1b[39m";

/// Fit `line` to exactly `width` columns and paint it with the foreground code `fg` and the
/// background code `bg` (both from [`Theme::fg_code`] / [`Theme::bg_code`], empty = none).
///
/// The codes are re-applied after every SGR reset inside `line`, so a background block stays
/// unbroken across styled spans. The padding lies inside the background.
pub(super) fn paint_line(line: &str, width: usize, fg: &str, bg: &str) -> String {
    let line = truncate_to_width(line, width, "");
    let prefix = format!("{fg}{bg}");
    let body = if prefix.is_empty() {
        line.clone()
    } else {
        line.replace(RESET, &format!("{RESET}{prefix}"))
    };
    let pad = " ".repeat(width.saturating_sub(visible_width(&line)));
    let mut out = String::with_capacity(prefix.len() + body.len() + pad.len() + 8);
    out.push_str(&prefix);
    out.push_str(&body);
    out.push_str(&pad);
    if !bg.is_empty() {
        out.push_str(BG_CLOSE);
    }
    if !fg.is_empty() {
        out.push_str(FG_CLOSE);
    }
    out
}

/// Word-wrapped text with padding and an optional background.
pub struct Text {
    text: String,
    padding_x: u16,
    padding_y: u16,
    fg: Option<String>,
    bg: Option<String>,
}

impl Text {
    /// `text` may contain SGR sequences and newlines. `padding_x` columns left and right,
    /// `padding_y` empty lines above and below.
    pub fn new(text: &str, padding_x: u16, padding_y: u16) -> Text {
        Text {
            text: text.to_string(),
            padding_x,
            padding_y,
            fg: None,
            bg: None,
        }
    }
    pub fn set_text(&mut self, text: &str) {
        self.text = text.to_string();
    }
    /// Foreground token applied to the whole text (`None` = as given).
    pub fn set_fg(&mut self, token: Option<&str>) {
        self.fg = token.map(str::to_string);
    }
    /// Background token; lines are padded to the full width so the background is a block.
    pub fn set_bg(&mut self, token: Option<&str>) {
        self.bg = token.map(str::to_string);
    }
}

impl Component for Text {
    /// Empty text renders no lines (not even padding).
    fn render(&mut self, width: u16, theme: &Theme) -> Vec<String> {
        if self.text.trim().is_empty() {
            return Vec::new();
        }
        let w = width as usize;
        let fg = self.fg.as_deref().map(|t| theme.fg_code(t)).unwrap_or_default();
        let bg = self.bg.as_deref().map(|t| theme.bg_code(t)).unwrap_or_default();

        let text = self.text.replace('\t', "   ");
        // Margins shrink when the width cannot hold them (pi).
        let px = (self.padding_x as usize).min(w.saturating_sub(1) / 2);
        let content = w.saturating_sub(px * 2).max(1);
        let margin = " ".repeat(px);

        let blank = paint_line("", w, &fg, &bg);
        let mut out = Vec::new();
        for _ in 0..self.padding_y {
            out.push(blank.clone());
        }
        for line in wrap_ansi(&text, content) {
            out.push(paint_line(&format!("{margin}{line}{margin}"), w, &fg, &bg));
        }
        for _ in 0..self.padding_y {
            out.push(blank.clone());
        }
        out
    }
    fn invalidate(&mut self) {
        // Nothing is cached: every render is computed from the text.
    }
}

/// One line, truncated with `…` instead of wrapped (only the first line of the text is used).
pub struct TruncatedText {
    text: String,
    padding_x: u16,
}

impl TruncatedText {
    pub fn new(text: &str, padding_x: u16) -> TruncatedText {
        TruncatedText {
            text: text.to_string(),
            padding_x,
        }
    }
    pub fn set_text(&mut self, text: &str) {
        self.text = text.to_string();
    }
}

impl Component for TruncatedText {
    fn render(&mut self, width: u16, _theme: &Theme) -> Vec<String> {
        let w = width as usize;
        let px = (self.padding_x as usize).min(w.saturating_sub(1) / 2);
        let content = w.saturating_sub(px * 2).max(1);
        let first = self.text.split('\n').next().unwrap_or("").replace('\t', "   ");
        let cut = truncate_to_width(&first, content, "…");
        let margin = " ".repeat(px);
        vec![paint_line(&format!("{margin}{cut}"), w, "", "")]
    }
}
