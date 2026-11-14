//! D2: the fullscreen (alternate screen) viewport. Owner: task D2. Signatures below are frozen; bodies are yours.
// ported from pi packages/coding-agent/src/modes/interactive/chat-viewport.ts and packages/tui/src/{tui-alt-screen.ts,wheel-scroll.ts}
use oxa_tui::{
    scrollbar_cells, scrollbar_offset_for_row, truncate_to_width, visible_width, Mouse, MouseKind, Theme,
};

/// Lines kept on screen when paging (pi's `PAGE_SCROLL_OVERLAP`).
const PAGE_SCROLL_OVERLAP: usize = 4;

/// When the scrollbar column is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollbarMode {
    /// Only when the content is taller than the viewport.
    Auto,
    Always,
    Never,
}

impl ScrollbarMode {
    /// `"auto"`, `"always"`, `"never"`; anything else = `Auto`. `"hidden"` (the settings schema
    /// value of `fullscreenScrollbar`) is the same as `"never"`.
    pub fn parse(s: &str) -> ScrollbarMode {
        match s {
            "always" => ScrollbarMode::Always,
            "never" | "hidden" => ScrollbarMode::Never,
            _ => ScrollbarMode::Auto,
        }
    }
}

/// A scrolling window over the transcript lines. Pure state; no terminal.
///
/// Contract (acceptance, plan D2):
/// * **follow**: while at the bottom, new content keeps the bottom visible; scrolling up leaves
///   follow mode; reaching the bottom again re-enters it.
/// * **scroll** by lines, pages, to top / bottom; offsets are clamped.
/// * **wheel**: `wheel_lines` per notch.
/// * **prompt jump**: to the previous / next user block start (`block_starts` + which are user).
/// * **resize keeps position**: after a width change (content re-wrapped), the line that was at
///   the top stays at the top, tracked by block index + offset within the block; in follow mode
///   the bottom stays.
/// * the scrollbar column is the rightmost column; content is rendered at `content_width()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Viewport {
    /// Index of the first visible content line.
    pub offset: usize,
    pub follow: bool,
    pub rows: usize,
    pub width: u16,
    pub scrollbar: ScrollbarMode,
    pub wheel_lines: u16,
    /// Number of content lines as of the last `set_content`.
    pub total: usize,
    /// First line index of every block, ascending, as of the last `set_content`.
    pub block_starts: Vec<usize>,
    /// Indices (into `block_starts`) of user blocks.
    pub user_blocks: Vec<usize>,
    /// `Auto` draws the bar only while this is set: pi's transient scrollbar, shown for a
    /// moment after scrolling (the app sets it per frame). Default `true`.
    pub auto_bar_visible: bool,
}

/// The block containing line `off` as `(block index, line offset within the block)`.
fn locate(starts: &[usize], off: usize) -> Option<(usize, usize)> {
    let n = starts.partition_point(|&s| s <= off);
    if n == 0 {
        None
    } else {
        Some((n - 1, off - starts[n - 1]))
    }
}

/// Number of lines of block `b` given the block starts and the total line count.
fn block_len(starts: &[usize], b: usize, total: usize) -> usize {
    let end = starts.get(b + 1).copied().unwrap_or(total);
    end.saturating_sub(starts[b])
}

/// `line` cut to `cols` visible columns and padded with spaces to exactly `cols`.
fn fit(line: &str, cols: usize) -> String {
    let cut = truncate_to_width(line, cols, "");
    let pad = cols.saturating_sub(visible_width(&cut));
    if cut.contains('\x1b') {
        format!("{cut}\x1b[0m{}", " ".repeat(pad))
    } else {
        format!("{cut}{}", " ".repeat(pad))
    }
}

impl Viewport {
    pub fn new(width: u16, rows: usize, scrollbar: ScrollbarMode, wheel_lines: u16) -> Viewport {
        Viewport {
            offset: 0,
            follow: true,
            rows,
            width,
            scrollbar,
            wheel_lines,
            total: 0,
            block_starts: Vec::new(),
            user_blocks: Vec::new(),
            auto_bar_visible: true,
        }
    }

    /// The largest valid offset (the bottom position).
    fn max_offset(&self) -> usize {
        self.total.saturating_sub(self.rows)
    }

    /// The offset clamped to the valid range.
    fn pos(&self) -> usize {
        self.offset.min(self.max_offset())
    }

    /// Lines per wheel notch (at least one).
    fn wheel_step(&self) -> isize {
        self.wheel_lines.max(1) as isize
    }

    /// The width content must be rendered at (the full width minus the scrollbar column when
    /// the mode can draw one: `Always`, or `Auto`, which reserves the column permanently so
    /// the text does not reflow when the bar appears).
    pub fn content_width(&self) -> u16 {
        match self.scrollbar {
            ScrollbarMode::Never => self.width,
            ScrollbarMode::Auto | ScrollbarMode::Always => self.width.saturating_sub(1),
        }
    }

    /// New content metrics after a re-render. Keeps the position per the contract above.
    ///
    /// The previous offset is located in the previous block layout (`block_starts`, `total`,
    /// still held by `self` at this point) and mapped to the same block and line within it in
    /// the new layout. Follow mode pins the bottom instead.
    pub fn set_content(&mut self, total: usize, block_starts: Vec<usize>, user_blocks: Vec<usize>) {
        let old_total = self.total;
        let old_starts = std::mem::replace(&mut self.block_starts, block_starts);
        self.total = total;
        self.user_blocks = user_blocks;
        if !self.follow {
            let old_off = self.offset.min(old_total.saturating_sub(1));
            if let Some((b, within)) = locate(&old_starts, old_off) {
                if let Some(&start) = self.block_starts.get(b) {
                    let len = block_len(&self.block_starts, b, total);
                    self.offset = start + within.min(len.saturating_sub(1));
                }
            }
        }
        if self.follow {
            self.offset = self.max_offset();
        } else {
            self.offset = self.pos();
        }
        self.follow = self.offset >= self.max_offset();
    }

    /// New terminal size. The caller re-renders content at the new `content_width()` and calls
    /// `set_content`; the position is restored there.
    pub fn resize(&mut self, width: u16, rows: usize) {
        self.width = width;
        self.rows = rows;
        if self.follow {
            self.offset = self.max_offset();
        }
    }

    /// Scroll by `delta` lines (negative = up).
    pub fn scroll_by(&mut self, delta: isize) {
        let max = self.max_offset();
        let next = (self.pos() as isize + delta).clamp(0, max as isize) as usize;
        self.offset = next;
        self.follow = next >= max;
    }

    pub fn page_up(&mut self) {
        let page = self.rows.saturating_sub(PAGE_SCROLL_OVERLAP).max(1);
        self.scroll_by(-(page as isize));
    }

    pub fn page_down(&mut self) {
        let page = self.rows.saturating_sub(PAGE_SCROLL_OVERLAP).max(1);
        self.scroll_by(page as isize);
    }

    pub fn scroll_to_top(&mut self) {
        self.offset = 0;
        self.follow = self.max_offset() == 0;
    }

    pub fn scroll_to_bottom(&mut self) {
        self.offset = self.max_offset();
        self.follow = true;
    }

    /// Jump so the previous (`forward == false`) or next user block starts at the top; `false`
    /// when there is none.
    pub fn jump_prompt(&mut self, forward: bool) -> bool {
        let cur = self.pos();
        let user_starts = self
            .user_blocks
            .iter()
            .filter_map(|&i| self.block_starts.get(i).copied());
        let target = if forward {
            user_starts.filter(|&s| s > cur).min()
        } else {
            user_starts.filter(|&s| s < cur).max()
        };
        match target {
            None => false,
            Some(t) => {
                let max = self.max_offset();
                self.offset = t.min(max);
                self.follow = self.offset >= max;
                true
            }
        }
    }

    /// Scroll the smallest amount that makes content line `line` visible (search, selection).
    pub fn reveal(&mut self, line: usize) {
        if self.rows == 0 {
            return;
        }
        let cur = self.pos();
        let next = if line < cur {
            line
        } else if line >= cur + self.rows {
            line + 1 - self.rows
        } else {
            cur
        };
        let max = self.max_offset();
        self.offset = next.min(max);
        self.follow = self.offset >= max;
    }

    /// Handle wheel events and scrollbar drags; `true` = consumed. `m.row` is relative to the
    /// viewport's first row.
    pub fn mouse(&mut self, m: &Mouse) -> bool {
        match m.kind {
            MouseKind::WheelUp => {
                self.scroll_by(-self.wheel_step());
                true
            }
            MouseKind::WheelDown => {
                self.scroll_by(self.wheel_step());
                true
            }
            MouseKind::Press | MouseKind::Drag
                if self.scrollbar_visible() && self.width > 0 && m.col == self.width - 1 =>
            {
                let max = self.max_offset();
                let off = scrollbar_offset_for_row(self.total, self.rows, m.row as usize).min(max);
                self.offset = off;
                self.follow = off >= max;
                true
            }
            _ => false,
        }
    }

    /// `true` when the scrollbar is drawn for the current content.
    pub fn scrollbar_visible(&self) -> bool {
        match self.scrollbar {
            ScrollbarMode::Always => true,
            ScrollbarMode::Auto => self.total > self.rows && self.auto_bar_visible,
            ScrollbarMode::Never => false,
        }
    }

    /// Exactly `rows` lines of exactly `width` columns: the visible slice of `content` (each
    /// line fitted to `content_width()`, blank lines below the end) plus the scrollbar column.
    pub fn render(&self, content: &[String], theme: &Theme) -> Vec<String> {
        let cols = self.content_width() as usize;
        let pos = self.pos();
        let bar = match self.scrollbar {
            ScrollbarMode::Never => None,
            // pi `always` with content that fits: the thumb fills the track.
            ScrollbarMode::Always if self.total <= self.rows => {
                Some(vec![theme.fg("scrollbarThumb", "█"); self.rows])
            }
            ScrollbarMode::Auto if !self.auto_bar_visible => Some(vec![" ".to_string(); self.rows]),
            _ => Some(scrollbar_cells(self.total, self.rows, pos, theme)),
        };
        (0..self.rows)
            .map(|r| {
                let body = match content.get(pos + r) {
                    Some(line) => fit(line, cols),
                    None => " ".repeat(cols),
                };
                match &bar {
                    Some(cells) => format!("{body}{}", cells.get(r).map(String::as_str).unwrap_or(" ")),
                    None => body,
                }
            })
            .collect()
    }

    /// The content line index shown at viewport row `row`, if any (mouse hit testing).
    pub fn line_at(&self, row: usize) -> Option<usize> {
        if row >= self.rows {
            return None;
        }
        let i = self.pos() + row;
        (i < self.total).then_some(i)
    }
}

/// Compose the fullscreen frame: `viewport` rows on top, then `bottom` (loader, widgets,
/// editor, footer), then pad or cut to exactly `rows` lines. `bottom` wins when the screen is
/// too small (the viewport shrinks to what is left, at least 0 rows). Returns the frame lines
/// and the number of rows given to the viewport.
///
/// The viewport part is padded with empty lines when `viewport` is short. When `bottom` does not
/// fit, its last lines are kept (the editor and footer stay visible).
pub fn compose_fullscreen(viewport: &[String], bottom: &[String], rows: usize) -> (Vec<String>, usize) {
    let bottom_rows = bottom.len().min(rows);
    let vp_rows = rows - bottom_rows;
    let mut frame: Vec<String> = (0..vp_rows)
        .map(|i| viewport.get(i).cloned().unwrap_or_default())
        .collect();
    frame.extend(bottom[bottom.len() - bottom_rows..].iter().cloned());
    (frame, vp_rows)
}

/// pi's `WheelScrollAccelerator` (packages/tui/src/wheel-scroll.ts): lines per wheel event.
/// A fixed count (`fullscreenWheelScrollLines` 1..100) is used as is; `auto` accelerates
/// with the event velocity on terminals that do not accelerate the wheel themselves: an
/// isolated notch moves one line, notches 100 ms apart 1 line each, 50 ms apart 2, 20 ms apart
/// 5, at most 6. Events closer than 5 ms (one physical notch, high-resolution sources) move
/// one line each; a pause over 200 ms or a direction change ends the gesture.
#[derive(Debug, Clone, PartialEq)]
pub struct WheelAccel {
    /// `None` = `auto`.
    fixed: Option<u16>,
    accelerate: bool,
    last_ms: Option<u64>,
    last_dir: i8,
    avg_gap: Option<f64>,
    carry: f64,
}

impl WheelAccel {
    /// `fixed` = `None` for `auto`; `accelerate` = the terminal does not accelerate the wheel
    /// (pi: everything but a local macOS terminal).
    pub fn new(fixed: Option<u16>, accelerate: bool) -> WheelAccel {
        WheelAccel {
            fixed,
            accelerate,
            last_ms: None,
            last_dir: 0,
            avg_gap: None,
            carry: 0.0,
        }
    }

    /// The positive line count for a wheel event in `dir` (-1 up, 1 down) at `now_ms`.
    pub fn next(&mut self, dir: i8, now_ms: u64) -> u16 {
        if let Some(n) = self.fixed {
            return n.max(1);
        }
        if !self.accelerate {
            return 1;
        }
        let gap = self.last_ms.map(|t| now_ms.saturating_sub(t));
        let same = dir == self.last_dir && gap.is_some_and(|g| g <= 200);
        self.last_ms = Some(now_ms);
        self.last_dir = dir;
        let Some(gap) = gap.filter(|_| same) else {
            self.avg_gap = None;
            self.carry = 0.0;
            return 1;
        };
        if gap < 5 {
            return 1;
        }
        let avg = match self.avg_gap {
            None => gap as f64,
            Some(a) => (a + gap as f64) / 2.0,
        };
        self.avg_gap = Some(avg);
        let lines = (100.0 / avg).clamp(1.0, 6.0) + self.carry;
        let whole = lines.floor();
        self.carry = lines - whole;
        whole as u16
    }
}
