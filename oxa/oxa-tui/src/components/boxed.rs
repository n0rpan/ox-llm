//! B12: Boxed, a padded container with a background. Owner: task B12. Signatures below are frozen; bodies are yours.
// ported from pi packages/tui/src/components/box.ts
use super::text::paint_line;
use crate::{Component, Key, Theme};

/// Children stacked inside padding, every line padded to the full width on background `bg`.
pub struct Boxed {
    children: Vec<Box<dyn Component>>,
    padding_x: u16,
    padding_y: u16,
    bg: Option<String>,
}

impl Boxed {
    pub fn new(padding_x: u16, padding_y: u16, bg_token: Option<&str>) -> Boxed {
        Boxed {
            children: Vec::new(),
            padding_x,
            padding_y,
            bg: bg_token.map(str::to_string),
        }
    }
    pub fn push(&mut self, child: Box<dyn Component>) {
        self.children.push(child);
    }
    pub fn clear(&mut self) {
        self.children.clear();
    }
    pub fn set_bg(&mut self, token: Option<&str>) {
        self.bg = token.map(str::to_string);
    }
}

impl Component for Boxed {
    /// No children = no lines.
    fn render(&mut self, width: u16, theme: &Theme) -> Vec<String> {
        if self.children.is_empty() {
            return Vec::new();
        }
        let w = width as usize;
        let px = (self.padding_x as usize).min(w.saturating_sub(1) / 2);
        let content = w.saturating_sub(px * 2).max(1) as u16;
        let margin = " ".repeat(px);

        let mut child_lines = Vec::new();
        for child in &mut self.children {
            child_lines.extend(child.render(content, theme));
        }
        if child_lines.is_empty() {
            return Vec::new();
        }

        let bg = self.bg.as_deref().map(|t| theme.bg_code(t)).unwrap_or_default();
        let blank = paint_line("", w, "", &bg);
        let mut out = Vec::with_capacity(child_lines.len() + 2 * self.padding_y as usize);
        for _ in 0..self.padding_y {
            out.push(blank.clone());
        }
        for line in child_lines {
            out.push(paint_line(&format!("{margin}{line}"), w, "", &bg));
        }
        for _ in 0..self.padding_y {
            out.push(blank.clone());
        }
        out
    }
    /// Offered to the children in order; the first one that consumes it wins.
    fn input(&mut self, key: &Key) -> bool {
        self.children.iter_mut().any(|c| c.input(key))
    }
    fn invalidate(&mut self) {
        for child in &mut self.children {
            child.invalidate();
        }
    }
}
