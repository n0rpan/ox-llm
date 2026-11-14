//! A13: the Component trait and the line-diff renderer. Owner: task A13. Signatures below are frozen; bodies are yours.
// ported from pi packages/tui/src/tui.ts, tui-main-screen.ts and tui-alt-screen.ts
use crate::{truncate_to_width, CursorPos, Key, Mouse, TermMode, Terminal, Theme};

const RESET: &str = "\x1b[0m";
const SYNC_ON: &str = "\x1b[?2026h";
const SYNC_OFF: &str = "\x1b[?2026l";

/// The one UI abstraction (plan sec. 4): a component renders lines for a width; the renderer
/// diffs lines. See the crate docs for what a valid line is.
pub trait Component {
    /// Lines for `width` columns. Called on every frame; implementations cache internally and
    /// drop the cache in [`Component::invalidate`].
    fn render(&mut self, width: u16, theme: &Theme) -> Vec<String>;
    /// Handle a key; `true` = consumed (the frame is redrawn).
    fn input(&mut self, _key: &Key) -> bool {
        false
    }
    /// Drop cached render output (theme or content changed from outside).
    fn invalidate(&mut self) {}
}

/// Optional extras a component may offer. Kept separate so `Component` stays exactly as planned.
pub trait Interactive: Component {
    /// Where the hardware cursor should be, relative to this component's own lines, as of the
    /// last `render`. `None` = no cursor (hidden).
    fn cursor(&self) -> Option<CursorPos> {
        None
    }
    /// Bracketed paste; `true` = consumed.
    fn paste(&mut self, _text: &str) -> bool {
        false
    }
    /// A mouse event with `row` relative to this component's first line; `true` = consumed.
    fn mouse(&mut self, _m: &Mouse) -> bool {
        false
    }
    fn set_focused(&mut self, _focused: bool) {}
}

/// Which screen the renderer draws on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenMode {
    /// The normal screen: finished output is committed to the terminal's scrollback, a live
    /// region at the bottom is diff-rendered (pi's "regular" mode).
    Main,
    /// The alternate screen: every frame is exactly `rows` lines (pi's "fullscreen" mode).
    Alt,
}

/// One frame to draw.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Frame {
    /// `Alt`: at most `rows` lines, line 0 = top of the screen (missing lines are blank).
    /// `Main`: the live region, at most `rows` lines; it sits directly below the committed
    /// scrollback output.
    pub lines: Vec<String>,
    /// Hardware cursor (for IME placement); `None` hides the cursor.
    pub cursor: Option<CursorPos>,
}

/// The line-diff renderer.
///
/// Contract:
/// * Output for a frame = the **minimal writes** that turn the previous frame into the new one:
///   only changed lines are rewritten (cursor move + clear-to-end-of-line + line), wrapped in
///   synchronized output (`ESC[?2026h` .. `ESC[?2026l`); an unchanged frame writes nothing
///   except a cursor move when only the cursor changed. Golden byte streams are the tests.
/// * Each written line is truncated to the terminal width and followed by `ESC[0m`.
/// * **Shrink clear**: when the new frame has fewer lines than the previous one, the surplus
///   lines are cleared.
/// * **Resize**: after a size change the next `draw` repaints everything (`Alt`), or clears and
///   repaints the live region (`Main`; committed scrollback is left to the terminal's reflow).
/// * `Main`: [`Renderer::commit`] writes lines above the live region exactly once; they are
///   never rewritten or cleared. If the live region would exceed `rows`, only its last `rows`
///   lines are drawn.
/// * The renderer never reads from the terminal and holds no reference to it between calls.
#[derive(Debug)]
pub struct Renderer {
    mode: ScreenMode,
    /// The lines on screen as of the last draw: the live region in `Main`, exactly `rows` lines
    /// in `Alt`.
    prev: Vec<String>,
    /// The cursor as requested by the last draw (clamped to the region).
    prev_cursor: Option<CursorPos>,
    /// `Main`: the row the hardware cursor is on, relative to the first live row.
    cursor_row: usize,
    /// Last visibility set through `Terminal::set_mode`; `None` before the first draw.
    cursor_shown: Option<bool>,
    /// Terminal size at the last draw; `None` before the first draw.
    size: Option<(u16, u16)>,
    /// Set by `reset` and `clear`: the next draw repaints everything.
    stale: bool,
}

/// The bytes that move the hardware cursor from row `from` to row `to` (0-based, relative to
/// the first live row) and leave it in column 0. Moving down writes line feeds, which create
/// rows past the end of the region and scroll at the bottom of the screen.
fn goto(from: usize, to: usize) -> String {
    use std::cmp::Ordering::*;
    match to.cmp(&from) {
        Less => format!("\x1b[{}A\r", from - to),
        Greater => "\r\n".repeat(to - from),
        Equal => "\r".to_string(),
    }
}

/// `line` cut to the terminal width, without an ellipsis.
fn fit(line: &str, cols: u16) -> String {
    truncate_to_width(line, cols as usize, "")
}

impl Renderer {
    pub fn new(mode: ScreenMode) -> Renderer {
        Renderer {
            mode,
            prev: Vec::new(),
            prev_cursor: None,
            cursor_row: 0,
            cursor_shown: None,
            size: None,
            stale: true,
        }
    }

    pub fn mode(&self) -> ScreenMode {
        self.mode
    }

    /// Draw `frame`, writing the minimal byte stream to `term` and flushing once.
    pub fn draw(&mut self, frame: &Frame, term: &mut dyn Terminal) {
        let (cols, rows) = term.size();
        let resized = self.size.is_some_and(|s| s != (cols, rows));
        let full = self.stale || resized || self.size.is_none();
        let body = match self.mode {
            ScreenMode::Main => self.draw_main(frame, cols, rows, full, term),
            ScreenMode::Alt => self.draw_alt(frame, cols, rows, full, term),
        };
        if !body.is_empty() {
            term.write(format!("{SYNC_ON}{body}{SYNC_OFF}").as_bytes());
        }
        term.flush();
        self.size = Some((cols, rows));
        self.stale = false;
    }

    /// `Main` only (no-op in `Alt`): print `lines` permanently above the live region. The live
    /// region is cleared first and repainted by the next `draw`.
    pub fn commit(&mut self, lines: &[String], term: &mut dyn Terminal) {
        if self.mode == ScreenMode::Alt {
            return;
        }
        let (cols, _) = term.size();
        let mut body = goto(self.cursor_row, 0);
        body.push_str("\x1b[J");
        for line in lines {
            body.push_str(&fit(line, cols));
            body.push_str(RESET);
            body.push_str("\r\n");
        }
        term.write(format!("{SYNC_ON}{body}{SYNC_OFF}").as_bytes());
        term.flush();
        self.prev.clear();
        self.prev_cursor = None;
        self.cursor_row = 0;
    }

    /// Forget the previous frame; the next `draw` repaints everything. Call after a resize,
    /// after `Terminal::resume`, and after anything else wrote to the screen.
    pub fn reset(&mut self) {
        self.prev.clear();
        self.prev_cursor = None;
        self.cursor_row = 0;
        self.stale = true;
    }

    /// The next `draw` repaints the whole region from its first row (the cursor position stays
    /// known, unlike [`Renderer::reset`]): pi's `clearOnShrink` full re-render.
    pub fn repaint(&mut self) {
        self.stale = true;
    }

    /// Erase what the renderer drew (the live region in `Main`, the screen in `Alt`) and
    /// leave the cursor at its start, visible. Used on exit and before suspending.
    pub fn clear(&mut self, term: &mut dyn Terminal) {
        let body = match self.mode {
            ScreenMode::Main => {
                let mut b = goto(self.cursor_row, 0);
                b.push_str("\x1b[J");
                b
            }
            ScreenMode::Alt => "\x1b[H\x1b[2J".to_string(),
        };
        term.write(body.as_bytes());
        self.show_cursor(true, term);
        term.flush();
        self.prev.clear();
        self.prev_cursor = None;
        self.cursor_row = 0;
        self.stale = true;
    }

    /// The lines of the last drawn frame (for selection and search in fullscreen mode).
    pub fn last_lines(&self) -> &[String] {
        &self.prev
    }

    fn show_cursor(&mut self, on: bool, term: &mut dyn Terminal) {
        if self.cursor_shown != Some(on) {
            term.set_mode(TermMode::CursorVisible, on);
            self.cursor_shown = Some(on);
        }
    }

    /// The body of an `Alt` frame: changed rows only, each at its absolute position.
    fn draw_alt(
        &mut self,
        frame: &Frame,
        cols: u16,
        rows: u16,
        full: bool,
        term: &mut dyn Terminal,
    ) -> String {
        let n = rows as usize;
        let mut target: Vec<String> = frame.lines.iter().take(n).cloned().collect();
        target.resize(n, String::new());
        let mut body = String::new();
        for (r, line) in target.iter().enumerate() {
            if full || self.prev.get(r) != Some(line) {
                body.push_str(&format!("\x1b[{};1H\x1b[K{}{RESET}", r + 1, fit(line, cols)));
            }
        }
        let cursor = frame.cursor.filter(|c| c.row < n).map(|c| CursorPos {
            row: c.row,
            col: c.col.min(cols.saturating_sub(1)),
        });
        if let Some(cur) = cursor {
            if !body.is_empty() || cursor != self.prev_cursor {
                body.push_str(&format!("\x1b[{};{}H", cur.row + 1, cur.col as usize + 1));
            }
        }
        self.show_cursor(cursor.is_some(), term);
        self.prev = target;
        self.prev_cursor = cursor;
        body
    }

    /// The body of a `Main` frame: the live region is diffed against the previous one, from the
    /// first changed line to the last one, with a clear below when it shrank.
    fn draw_main(
        &mut self,
        frame: &Frame,
        cols: u16,
        rows: u16,
        full: bool,
        term: &mut dyn Terminal,
    ) -> String {
        let skip = frame.lines.len().saturating_sub(rows as usize);
        let target: Vec<String> = frame.lines[skip..].to_vec();
        let cursor = match frame.cursor {
            Some(c) if c.row >= skip && !target.is_empty() => Some(CursorPos {
                row: (c.row - skip).min(target.len() - 1),
                col: c.col.min(cols.saturating_sub(1)),
            }),
            _ => None,
        };
        let prev = if full {
            Vec::new()
        } else {
            std::mem::take(&mut self.prev)
        };
        let mut c = self.cursor_row;
        let mut body = String::new();
        if full {
            // Repaint from the first live row; the rows below are cleared unless this is the
            // very first frame (nothing was drawn yet, so there is nothing to clear).
            body.push_str(&goto(c, 0));
            if self.size.is_some() {
                body.push_str("\x1b[J");
            }
            c = 0;
        }
        let n = target.len();
        let m = prev.len();
        let differs = |i: usize| target.get(i) != prev.get(i);
        if let Some(first) = (0..n.max(m)).find(|&i| differs(i)) {
            if first < n {
                if !full {
                    body.push_str(&goto(c, first));
                }
                let last = (0..n.max(m))
                    .rev()
                    .find(|&i| differs(i))
                    .unwrap_or(first)
                    .min(n - 1);
                for (idx, line) in target[first..=last].iter().enumerate() {
                    if idx > 0 {
                        body.push_str("\r\n");
                    }
                    body.push_str("\x1b[K");
                    body.push_str(&fit(line, cols));
                    body.push_str(RESET);
                }
                c = last;
            }
            if n < m {
                // Shrink: everything from the new end of the region down is cleared.
                body.push_str(&goto(c, n));
                body.push_str("\x1b[J");
                c = n;
            }
        }
        if let Some(cur) = cursor {
            if !body.is_empty() || cursor != self.prev_cursor {
                body.push_str(&goto(c, cur.row));
                c = cur.row;
                body.push_str(&format!("\x1b[{}G", cur.col as usize + 1));
            }
        }
        self.show_cursor(cursor.is_some(), term);
        self.cursor_row = c;
        self.prev_cursor = cursor;
        self.prev = target;
        body
    }
}

/// Render `children` top to bottom into one line list (pi's `Container`).
pub fn render_stack(children: &mut [&mut dyn Component], width: u16, theme: &Theme) -> Vec<String> {
    let mut out = Vec::new();
    for child in children.iter_mut() {
        out.extend(child.render(width, theme));
    }
    out
}
