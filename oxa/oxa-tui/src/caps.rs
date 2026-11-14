//! Architect-owned: terminal capability data types (pure data, complete). Not a task file.
// ported from pi packages/tui/src/terminal-image.ts (TerminalCapabilities)

/// Inline image protocol of the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ImageProtocol {
    Kitty,
    Iterm2,
}

/// Colour depth used when emitting SGR colours.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ColorMode {
    TrueColor,
    Ansi256,
}

/// What the terminal can do. Detected once at start (`detect_caps`, C14), then fixed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TermCaps {
    pub images: Option<ImageProtocol>,
    pub color: ColorMode,
    /// OSC 8 hyperlinks.
    pub hyperlinks: bool,
    /// The kitty keyboard protocol was confirmed by the terminal.
    pub kitty_keyboard: bool,
    /// OSC 52 clipboard writes.
    pub osc52: bool,
    /// Pixel size of one cell `(w, h)`, when the terminal reported it.
    pub cell_px: Option<(u16, u16)>,
}

impl TermCaps {
    /// The conservative default: 256 colours, nothing optional.
    pub const BASIC: TermCaps = TermCaps {
        images: None,
        color: ColorMode::Ansi256,
        hyperlinks: false,
        kitty_keyboard: false,
        osc52: false,
        cell_px: None,
    };
}

impl Default for TermCaps {
    fn default() -> Self {
        TermCaps::BASIC
    }
}

/// A terminal mode that can be switched on and off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TermMode {
    AltScreen,
    /// SGR mouse reporting (press, release, drag, wheel).
    Mouse,
    BracketedPaste,
    KittyKeyboard,
    FocusReporting,
    CursorVisible,
}

/// Position of the hardware cursor inside a frame: 0-based row (index into the frame's lines)
/// and 0-based column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorPos {
    pub row: usize,
    pub col: u16,
}
