//! B12: Border, a horizontal rule that follows the width.
// ported from pi packages/coding-agent/src/modes/interactive/components/dynamic-border.ts
use crate::{pad_to_width, truncate_to_width, visible_width, Component, Theme};

/// One line of `─` across the full width in a theme token, with an optional label:
/// `── label ─────`.
pub struct Border {
    token: String,
    label: Option<String>,
}

impl Border {
    /// `token` = `border`, `borderAccent`, `borderMuted`, a thinking token, `bashMode`.
    pub fn new(token: &str) -> Border {
        Border {
            token: token.to_string(),
            label: None,
        }
    }
    pub fn set_token(&mut self, token: &str) {
        self.token = token.to_string();
    }
    pub fn set_label(&mut self, label: Option<&str>) {
        self.label = label.map(str::to_string);
    }
}

impl Component for Border {
    fn render(&mut self, width: u16, theme: &Theme) -> Vec<String> {
        let w = (width as usize).max(1);
        let line = match &self.label {
            None => "─".repeat(w),
            Some(label) => {
                let head = truncate_to_width(&format!("── {label} "), w, "");
                let rest = w.saturating_sub(visible_width(&head));
                format!("{head}{}", "─".repeat(rest))
            }
        };
        vec![theme.fg(&self.token, &line)]
    }
}

/// A box frame around `lines` (already fitted to `width - 4`): `╭─ title ─╮`, `│ line │`,
/// `╰────────╯`, in `token`. Used by dialogs and overlays. Returns lines of exactly `width`
/// columns.
pub fn frame_lines(
    lines: &[String],
    width: u16,
    title: Option<&str>,
    token: &str,
    theme: &Theme,
) -> Vec<String> {
    let w = width as usize;
    let paint = |s: &str| theme.fg(token, s);
    if w < 4 {
        return vec![paint(&"─".repeat(w))];
    }
    let inner = w - 2;
    let inner_content = w - 4;

    // Top edge: the optional title sits after one dash.
    let head = match title {
        Some(t) => {
            let t = truncate_to_width(t, inner.saturating_sub(4), "…");
            truncate_to_width(&format!("─ {t} "), inner, "")
        }
        None => String::new(),
    };
    let rest = inner.saturating_sub(visible_width(&head));
    let mut out = Vec::with_capacity(lines.len() + 2);
    out.push(format!(
        "{}{}{}{}",
        paint("╭"),
        head,
        paint(&"─".repeat(rest)),
        paint("╮")
    ));
    for line in lines {
        let body = pad_to_width(&truncate_to_width(line, inner_content, ""), inner_content, false);
        out.push(format!("{} {body} {}", paint("│"), paint("│")));
    }
    out.push(format!(
        "{}{}{}",
        paint("╰"),
        paint(&"─".repeat(inner)),
        paint("╯")
    ));
    out
}
