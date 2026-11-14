//! Architect-owned: the `Theme` value and its SGR emission (complete). Loading and colour
//! conversion are task B13 (`theme.rs`). Not a task file.
// ported from pi packages/coding-agent/src/modes/interactive/theme/theme.ts (class Theme) and theme-tokens.ts
use crate::ColorMode;
use std::collections::BTreeMap;

/// A resolved colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Color {
    /// The terminal's default colour (pi's empty string).
    Default,
    /// A 256-colour palette index.
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// pi's theme tokens in theme-file order: `(name, is_background, fallback token)`.
pub const THEME_TOKENS: [(&str, bool, Option<&str>); 59] = [
    ("accent", false, None),
    ("border", false, None),
    ("borderAccent", false, None),
    ("borderMuted", false, None),
    ("success", false, None),
    ("error", false, None),
    ("warning", false, None),
    ("muted", false, None),
    ("dim", false, None),
    ("text", false, None),
    ("thinkingText", false, None),
    ("scrollbarTrack", false, Some("muted")),
    ("scrollbarThumb", false, Some("text")),
    ("selectedBg", true, None),
    ("searchMatchBg", true, Some("selectedBg")),
    ("searchMatchText", false, Some("text")),
    ("userMessageBg", true, None),
    ("userMessageText", false, None),
    ("customMessageBg", true, None),
    ("customMessageText", false, None),
    ("customMessageLabel", false, None),
    ("toolPendingBg", true, None),
    ("toolSuccessBg", true, None),
    ("toolErrorBg", true, None),
    ("toolTitle", false, None),
    ("toolOutput", false, None),
    ("mdHeading", false, None),
    ("mdLink", false, None),
    ("mdLinkUrl", false, None),
    ("mdCode", false, None),
    ("mdCodeBlock", false, None),
    ("mdCodeBlockBorder", false, None),
    ("mdQuote", false, None),
    ("mdQuoteBorder", false, None),
    ("mdHr", false, None),
    ("mdListBullet", false, None),
    ("toolDiffAdded", false, None),
    ("toolDiffRemoved", false, None),
    ("toolDiffContext", false, None),
    ("syntaxComment", false, None),
    ("syntaxKeyword", false, None),
    ("syntaxFunction", false, None),
    ("syntaxVariable", false, None),
    ("syntaxString", false, None),
    ("syntaxNumber", false, None),
    ("syntaxType", false, None),
    ("syntaxOperator", false, None),
    ("syntaxPunctuation", false, None),
    ("thinkingOff", false, None),
    ("thinkingMinimal", false, None),
    ("thinkingLow", false, None),
    ("thinkingMedium", false, None),
    ("thinkingHigh", false, None),
    ("thinkingXhigh", false, None),
    ("thinkingMax", false, Some("thinkingXhigh")),
    ("bashMode", false, None),
    // oxa additions (not in pi theme files; always resolved through the fallback unless a
    // theme sets them): the permission-mode indicators of the footer.
    ("modePlan", false, Some("warning")),
    ("modeWorkspace", false, Some("muted")),
    ("modeAsk", false, Some("accent")),
];

/// `light` or `dark`, from the theme file's `appearance` or from the terminal background.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Appearance {
    Dark,
    Light,
}

/// A loaded theme: every token resolved to a [`Color`].
///
/// Components style text only through this type; nothing else in the workspace writes SGR
/// colour codes. A token that is not set resolves through its fallback in [`THEME_TOKENS`],
/// then to [`Color::Default`]. An unknown token name is [`Color::Default`] (never a panic).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    pub name: String,
    pub appearance: Appearance,
    pub mode: ColorMode,
    /// Resolved colours by token name (foreground and background tokens share the map).
    pub colors: BTreeMap<String, Color>,
    /// The theme file's `export` colours as CSS strings: `pageBg`, `cardBg`, `infoBg`.
    pub export: BTreeMap<String, String>,
    /// The file the theme was loaded from (`None` for built-ins and [`Theme::plain`]).
    pub source: Option<std::path::PathBuf>,
}

impl Theme {
    /// The theme with no colours at all: every method returns its text unchanged (attribute
    /// helpers included). The default in unit tests and for `NO_COLOR`.
    pub fn plain() -> Theme {
        Theme {
            name: "plain".to_string(),
            appearance: Appearance::Dark,
            mode: ColorMode::Ansi256,
            colors: BTreeMap::new(),
            export: BTreeMap::new(),
            source: None,
        }
    }

    /// `true` for [`Theme::plain`]-like themes (no colour set): attribute helpers are no-ops.
    pub fn is_plain(&self) -> bool {
        self.colors.is_empty()
    }

    /// The colour of `token` after fallback resolution.
    pub fn color(&self, token: &str) -> Color {
        let mut name = token;
        for _ in 0..4 {
            if let Some(c) = self.colors.get(name) {
                return *c;
            }
            match THEME_TOKENS
                .iter()
                .find(|(n, _, _)| *n == name)
                .and_then(|(_, _, f)| *f)
            {
                Some(f) => name = f,
                None => break,
            }
        }
        Color::Default
    }

    fn code(&self, c: Color, bg: bool) -> String {
        let base = if bg { 48 } else { 38 };
        match c {
            Color::Default => String::new(),
            Color::Indexed(n) => format!("\x1b[{base};5;{n}m"),
            Color::Rgb(r, g, b) => match self.mode {
                ColorMode::TrueColor => format!("\x1b[{base};2;{r};{g};{b}m"),
                ColorMode::Ansi256 => format!("\x1b[{base};5;{}m", crate::rgb_to_256(r, g, b)),
            },
        }
    }

    /// The SGR sequence that sets the foreground to `token` (empty for the default colour).
    pub fn fg_code(&self, token: &str) -> String {
        self.code(self.color(token), false)
    }
    /// The SGR sequence that sets the background to `token` (empty for the default colour).
    pub fn bg_code(&self, token: &str) -> String {
        self.code(self.color(token), true)
    }
    /// `text` in the foreground colour `token`, followed by a foreground reset (`ESC[39m`).
    pub fn fg(&self, token: &str, text: &str) -> String {
        let code = self.fg_code(token);
        if code.is_empty() {
            text.to_string()
        } else {
            format!("{code}{text}\x1b[39m")
        }
    }
    /// `text` on the background colour `token`, followed by a background reset (`ESC[49m`).
    pub fn bg(&self, token: &str, text: &str) -> String {
        let code = self.bg_code(token);
        if code.is_empty() {
            text.to_string()
        } else {
            format!("{code}{text}\x1b[49m")
        }
    }
    fn attr(&self, on: u8, off: u8, text: &str) -> String {
        if self.is_plain() {
            text.to_string()
        } else {
            format!("\x1b[{on}m{text}\x1b[{off}m")
        }
    }
    pub fn bold(&self, text: &str) -> String {
        self.attr(1, 22, text)
    }
    pub fn dim(&self, text: &str) -> String {
        self.attr(2, 22, text)
    }
    pub fn italic(&self, text: &str) -> String {
        self.attr(3, 23, text)
    }
    pub fn underline(&self, text: &str) -> String {
        self.attr(4, 24, text)
    }
    pub fn strikethrough(&self, text: &str) -> String {
        self.attr(9, 29, text)
    }
    pub fn inverse(&self, text: &str) -> String {
        self.attr(7, 27, text)
    }
    /// The border token for a thinking level name (`off` .. `max`): `thinkingOff` ..
    /// `thinkingMax`; unknown names give `border`.
    pub fn thinking_token(level: &str) -> &'static str {
        match level {
            "off" => "thinkingOff",
            "minimal" => "thinkingMinimal",
            "low" => "thinkingLow",
            "medium" => "thinkingMedium",
            "high" => "thinkingHigh",
            "xhigh" => "thinkingXhigh",
            "max" => "thinkingMax",
            _ => "border",
        }
    }
}
