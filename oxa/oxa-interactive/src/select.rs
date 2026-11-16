//! D3: mouse text selection and copy in fullscreen mode.
// ported from pi packages/coding-agent/src/modes/interactive/chat-viewport.ts (selection)
use oxa_tui::{
    cluster_boundaries, slice_by_column, strip_ansi, visible_width, Mouse, MouseButton, MouseKind, Theme,
};

/// A cell of the content: line index and 0-based column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Cell {
    pub line: usize,
    pub col: u16,
}

/// A selection between two cells, in content coordinates (not screen rows), so it survives
/// scrolling. `anchor` is where the drag began; either may come first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: Cell,
    pub head: Cell,
}

impl Selection {
    /// `(start, end)` in reading order; `end` is inclusive.
    pub fn ordered(&self) -> (Cell, Cell) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }
}

/// How line breaks inside a selection are judged when the text is copied (D-INT).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SoftWrap {
    /// Every line break is copied as a newline: the text is exactly what the screen shows.
    Off,
    /// Join soft wraps, taking the widest content line as the wrap width (no width known).
    #[default]
    Guess,
    /// Join soft wraps at this content width (`Viewport::content_width`): a break is a wrap
    /// when the next line's first word would not have fitted on the previous line.
    Width(u16),
}

/// The last mouse press, for double and triple clicks (D-INT).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Click {
    pub cell: Cell,
    pub at_ms: u64,
    /// 1 = single, 2 = double (word), 3 = triple (line).
    pub count: u8,
}

/// Two presses on the same cell within this time are one multi-click (pi's
/// `DOUBLE_CLICK_INTERVAL_MS`).
pub const DOUBLE_CLICK_INTERVAL_MS: u64 = 500;

/// Tracks a selection from mouse events.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Selector {
    pub selection: Option<Selection>,
    /// `true` between press and release.
    pub dragging: bool,
    /// The previous press (D-INT: click state for double and triple clicks).
    pub last_click: Option<Click>,
    /// How the copied text treats soft wraps (D-INT); the app sets `SoftWrap::Width`.
    pub wrap: SoftWrap,
}

/// What a mouse event did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectEvent {
    Ignored,
    /// The selection changed; redraw.
    Changed,
    /// The button was released over a non-empty selection; carries the selected text (the app
    /// copies it when `copy_on_select`).
    Finished(String),
    /// A click without movement cleared the selection.
    Cleared,
}

impl Selector {
    pub fn new() -> Selector {
        Selector::default()
    }
    /// Feed a mouse event. `line_at` maps a viewport row to a content line
    /// (`Viewport::line_at`); `content` are the content lines. Left press starts, drag extends
    /// (the caller auto-scrolls when the drag leaves the viewport), release finishes. A double
    /// click selects the word, a triple click the line (a fourth press starts over).
    pub fn mouse(
        &mut self,
        m: &Mouse,
        line_at: &dyn Fn(usize) -> Option<usize>,
        content: &[String],
        now_ms: u64,
    ) -> SelectEvent {
        match m.kind {
            MouseKind::Press if m.button == MouseButton::Left => {
                let Some(cell) = cell_at(m, line_at, content) else {
                    return SelectEvent::Ignored;
                };
                let count = match self.last_click {
                    Some(prev)
                        if prev.cell == cell
                            && prev.count < 3
                            && now_ms.saturating_sub(prev.at_ms) <= DOUBLE_CLICK_INTERVAL_MS =>
                    {
                        prev.count + 1
                    }
                    _ => 1,
                };
                self.last_click = Some(Click {
                    cell,
                    at_ms: now_ms,
                    count,
                });
                self.selection = Some(match count {
                    2 => word_at(&content[cell.line], cell),
                    3 => line_of(&content[cell.line], cell.line),
                    _ => Selection {
                        anchor: cell,
                        head: cell,
                    },
                });
                self.dragging = true;
                SelectEvent::Changed
            }
            MouseKind::Drag if self.dragging => {
                let Some(cell) = cell_at(m, line_at, content) else {
                    return SelectEvent::Ignored;
                };
                match self.selection {
                    Some(sel) if sel.head != cell => {
                        self.selection = Some(Selection {
                            anchor: sel.anchor,
                            head: cell,
                        });
                        SelectEvent::Changed
                    }
                    _ => SelectEvent::Ignored,
                }
            }
            MouseKind::Release if self.dragging => {
                self.dragging = false;
                let Some(mut sel) = self.selection else {
                    return SelectEvent::Ignored;
                };
                let multi = self.last_click.is_some_and(|c| c.count > 1);
                // A word or line selected by a multi-click stays as it is unless the pointer moved.
                let moved = cell_at(m, line_at, content)
                    .is_some_and(|cell| self.last_click.is_none_or(|c| c.cell != cell));
                if !multi || moved {
                    if let Some(cell) = cell_at(m, line_at, content) {
                        sel.head = cell;
                        self.selection = Some(sel);
                    }
                }
                if sel.anchor == sel.head && !multi {
                    self.selection = None;
                    return SelectEvent::Cleared;
                }
                let text = selection_text_with(content, &sel, self.wrap);
                if text.is_empty() {
                    self.selection = None;
                    SelectEvent::Cleared
                } else {
                    SelectEvent::Finished(text)
                }
            }
            _ => SelectEvent::Ignored,
        }
    }
    pub fn clear(&mut self) {
        self.selection = None;
        self.dragging = false;
    }
    /// The text of the current selection (empty without one), per [`Selector::wrap`].
    pub fn text(&self, content: &[String]) -> String {
        match &self.selection {
            Some(sel) => selection_text_with(content, sel, self.wrap),
            None => String::new(),
        }
    }
}

/// The word under `cell`: the run of word characters (letters, digits, `_`) around it, or the
/// single cell when it is on anything else.
fn word_at(text: &str, cell: Cell) -> Selection {
    let plain = strip_ansi(text);
    let bounds = cluster_boundaries(&plain);
    // (start column, end column, is word) per cluster.
    let mut spans: Vec<(usize, usize, bool)> = Vec::new();
    let mut col = 0usize;
    for w in bounds.windows(2) {
        let cluster = &plain[w[0]..w[1]];
        let end = col + visible_width(cluster);
        let wordy = cluster.chars().all(|c| c.is_alphanumeric() || c == '_');
        if end > col {
            spans.push((col, end, wordy));
        }
        col = end;
    }
    let at = cell.col as usize;
    let single = Selection {
        anchor: cell,
        head: cell,
    };
    let Some(i) = spans.iter().position(|&(a, b, _)| at >= a && at < b) else {
        return single;
    };
    if !spans[i].2 {
        return single;
    }
    let mut first = i;
    while first > 0 && spans[first - 1].2 {
        first -= 1;
    }
    let mut last = i;
    while last + 1 < spans.len() && spans[last + 1].2 {
        last += 1;
    }
    let col_of = |c: usize| c.min(u16::MAX as usize) as u16;
    Selection {
        anchor: Cell {
            line: cell.line,
            col: col_of(spans[first].0),
        },
        head: Cell {
            line: cell.line,
            col: col_of(spans[last].1.saturating_sub(1)),
        },
    }
}

/// The whole content line `line`.
fn line_of(text: &str, line: usize) -> Selection {
    let width = visible_width(&strip_ansi(text));
    Selection {
        anchor: Cell { line, col: 0 },
        head: Cell {
            line,
            col: width.saturating_sub(1).min(u16::MAX as usize) as u16,
        },
    }
}

/// The content cell under a mouse report, if the row shows a content line.
fn cell_at(m: &Mouse, line_at: &dyn Fn(usize) -> Option<usize>, content: &[String]) -> Option<Cell> {
    let line = line_at(m.row as usize)?;
    if line >= content.len() {
        return None;
    }
    Some(Cell { line, col: m.col })
}

/// Column range `[start, end)` of content line `line` covered by `sel`, or `None` when the line
/// is outside the selection. `end` is `usize::MAX` for lines that run to their end.
fn span_on_line(sel: &Selection, line: usize) -> Option<(usize, usize)> {
    let (s, e) = sel.ordered();
    if line < s.line || line > e.line {
        return None;
    }
    let start = if line == s.line { s.col as usize } else { 0 };
    let end = if line == e.line {
        e.col as usize + 1
    } else {
        usize::MAX
    };
    Some((start, end))
}

/// Length of the word at the start of `s` (after leading spaces), in columns.
fn first_word_width(s: &str) -> usize {
    visible_width(s.split(' ').find(|w| !w.is_empty()).unwrap_or(""))
}

/// `true` when the break between content lines `prev` and `next` looks like a soft wrap: the
/// next line's first word would not have fitted on the previous line at the widest content
/// width. The frozen signatures carry no wrap width or wrap flags, so this is a heuristic.
fn is_soft_break(prev: &str, next: &str, wrap_width: usize) -> bool {
    let p = strip_ansi(prev);
    let n = strip_ansi(next);
    let p = p.trim_end_matches(' ');
    // An indented next line is a new paragraph or block, not a continuation.
    if p.is_empty() || n.is_empty() || n.starts_with(' ') {
        return false;
    }
    visible_width(p) + 1 + first_word_width(&n) > wrap_width
}

/// The selected text: escape sequences removed, lines joined by `\n`, trailing spaces of each
/// line trimmed, columns cut at cluster boundaries (a half-selected wide character is
/// included).
///
/// Soft wraps are joined with a space instead of a newline (see [`is_soft_break`]); the wrap
/// width is guessed ([`SoftWrap::Guess`]). Use [`selection_text_with`] when the width is known.
pub fn selection_text(content: &[String], sel: &Selection) -> String {
    selection_text_with(content, sel, SoftWrap::Guess)
}

/// [`selection_text`] with an explicit soft-wrap rule (D-INT).
pub fn selection_text_with(content: &[String], sel: &Selection, wrap: SoftWrap) -> String {
    let (s, e) = sel.ordered();
    if s.line >= content.len() {
        return String::new();
    }
    let last = e.line.min(content.len() - 1);
    let wrap_width = match wrap {
        SoftWrap::Off => None,
        SoftWrap::Width(w) => Some(w as usize),
        SoftWrap::Guess => Some(
            content
                .iter()
                .map(|l| visible_width(&strip_ansi(l)))
                .max()
                .unwrap_or(0),
        ),
    };
    let mut out = String::new();
    for line in s.line..=last {
        let (start, end) = span_on_line(sel, line).unwrap_or((0, 0));
        let plain = strip_ansi(&content[line]);
        let piece = text_in_cols(&plain, start, end);
        let piece = piece.trim_end_matches(' ');
        if line > s.line {
            let soft = wrap_width.is_some_and(|w| is_soft_break(&content[line - 1], &content[line], w));
            out.push(if soft { ' ' } else { '\n' });
        }
        out.push_str(piece);
    }
    out
}

/// The clusters of plain `plain` that overlap columns `[start, end)`, whole. A wide cluster cut
/// by either edge is included in full.
fn text_in_cols(plain: &str, start: usize, end: usize) -> String {
    let bounds = cluster_boundaries(plain);
    let mut col = 0usize;
    let mut out = String::new();
    for w in bounds.windows(2) {
        let cluster = &plain[w[0]..w[1]];
        let (a, b) = (col, col + visible_width(cluster));
        col = b;
        if b > start && a < end {
            out.push_str(cluster);
        }
    }
    out
}

/// The visible part of `text` from column `from` to its end, styles kept. Empty when `from` is at
/// or past the end (no trailing style state is emitted).
pub(crate) fn tail(text: &str, from: usize) -> String {
    let w = visible_width(text);
    if from >= w {
        String::new()
    } else {
        slice_by_column(text, from, w - from)
    }
}

/// Wrap `text` in `open` and `close`, re-applying `open` after every SGR reset inside it so
/// the highlight survives the existing styles of the line.
pub(crate) fn paint_span(text: &str, open: &str, close: &str) -> String {
    let inner = text.replace("\x1b[0m", &format!("\x1b[0m{open}"));
    format!("{open}{inner}{close}")
}

/// `content[first..first + count]` with the selected cells drawn on `selectedBg` (existing
/// styles kept). Lines outside the selection are returned unchanged.
pub fn highlight_selection(
    content: &[String],
    first: usize,
    count: usize,
    sel: &Selection,
    theme: &Theme,
) -> Vec<String> {
    let (open, close) = {
        let bg = theme.bg_code("selectedBg");
        if bg.is_empty() {
            ("\x1b[7m".to_string(), "\x1b[27m".to_string())
        } else {
            (bg, "\x1b[49m".to_string())
        }
    };
    let end = first.saturating_add(count).min(content.len());
    (first..end)
        .map(|line| {
            let text = &content[line];
            let Some((start, span_end)) = span_on_line(sel, line) else {
                return text.clone();
            };
            let len = span_end.saturating_sub(start);
            let mid = slice_by_column(text, start, len);
            if mid.is_empty() {
                return text.clone();
            }
            let pre = slice_by_column(text, 0, start);
            let post = tail(text, span_end);
            format!("{pre}{}{post}", paint_span(&mid, &open, &close))
        })
        .collect()
}
