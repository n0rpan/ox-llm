//! A13: column width, ANSI-aware wrapping, truncation and slicing.
// ported from pi packages/tui/src/utils.ts
//
// Common rules: escape sequences (CSI .. final byte, OSC .. BEL/ST, APC .. ST) have width 0 and
// are never split; grapheme clusters (base + combining marks, ZWJ emoji sequences, flags,
// variation selectors) are never split; a wide cluster has width 2; a tab counts as 3 columns
// (pi's rule); control characters have width 0. Every returned line is self-contained: the SGR
// state active at a cut is closed with `ESC[0m` and re-opened on the continuation.

use unicode_width::UnicodeWidthChar;

const RESET: &str = "\x1b[0m";
const LINK_CLOSE: &str = "\x1b]8;;\x1b\\";
const ZWJ: char = '\u{200d}';
const VS16: char = '\u{fe0f}';

// ---- clusters -------------------------------------------------------------------------------

fn is_regional(c: char) -> bool {
    ('\u{1f1e6}'..='\u{1f1ff}').contains(&c)
}

fn is_modifier(c: char) -> bool {
    ('\u{1f3fb}'..='\u{1f3ff}').contains(&c)
}

/// A non-control character with column width 0 (combining marks, joiners, selectors).
fn is_extend(c: char) -> bool {
    !c.is_control() && c.width() == Some(0)
}

/// Byte offset where the grapheme cluster starting at `start` ends. `start` must be a char
/// boundary and `start < s.len()` (otherwise `start` is returned).
fn cluster_end(s: &str, start: usize) -> usize {
    let mut chars = s[start..].chars();
    let Some(first) = chars.next() else {
        return start;
    };
    let mut end = start + first.len_utf8();
    if first.is_control() {
        return end;
    }
    let mut prev = first;
    for (idx, c) in chars.enumerate() {
        if c.is_control() {
            break;
        }
        let joins = prev == ZWJ
            || c == ZWJ
            || is_extend(c)
            || is_modifier(c)
            || (idx == 0 && is_regional(first) && is_regional(c));
        if !joins {
            break;
        }
        end += c.len_utf8();
        prev = c;
    }
    end
}

/// Columns of one grapheme cluster (text only, no escapes). Never more than 2.
fn cluster_width(cl: &str) -> usize {
    if cl == "\t" {
        return 3;
    }
    let mut chars = cl.chars();
    let Some(first) = chars.next() else {
        return 0;
    };
    if first.is_control() {
        return 0;
    }
    let rest: Vec<char> = chars.collect();
    if rest.is_empty() {
        return first.width().unwrap_or(0).min(2);
    }
    if rest.len() == 1 && is_regional(first) && is_regional(rest[0]) {
        return 2; // flag
    }
    let mut w = first.width().unwrap_or(0);
    for c in &rest {
        w = w.max(c.width().unwrap_or(0));
    }
    if rest.contains(&VS16) {
        w = w.max(2); // emoji presentation
    }
    w.min(2)
}

// ---- tokens ---------------------------------------------------------------------------------

/// A lexical token of a line: an escape sequence (zero width) or one grapheme cluster of text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tok<'a> {
    Esc(&'a str),
    Text(&'a str),
}

/// Byte length of the escape sequence at the start of `s` (which starts with ESC). At least 1.
fn escape_len(s: &str) -> usize {
    let b = s.as_bytes();
    if b.len() < 2 {
        return b.len().max(1);
    }
    match b[1] {
        b'[' => {
            // CSI: parameters 0x30..=0x3F, intermediates 0x20..=0x2F, final 0x40..=0x7E.
            for (i, &c) in b.iter().enumerate().skip(2) {
                if (0x40..=0x7e).contains(&c) {
                    return i + 1;
                }
            }
            b.len()
        }
        b']' | b'_' => {
            // OSC / APC: terminated by BEL or ST (ESC \).
            let mut i = 2;
            while i < b.len() {
                if b[i] == 0x07 {
                    return i + 1;
                }
                if b[i] == 0x1b && i + 1 < b.len() && b[i + 1] == b'\\' {
                    return i + 2;
                }
                i += 1;
            }
            b.len()
        }
        _ => 1 + s[1..].chars().next().map_or(0, |c| c.len_utf8()),
    }
}

struct Tokens<'a> {
    s: &'a str,
    i: usize,
}

impl<'a> Iterator for Tokens<'a> {
    type Item = Tok<'a>;
    fn next(&mut self) -> Option<Tok<'a>> {
        let s: &'a str = self.s;
        if self.i >= s.len() {
            return None;
        }
        let start = self.i;
        if s.as_bytes()[start] == 0x1b {
            let end = start + escape_len(&s[start..]);
            self.i = end;
            Some(Tok::Esc(&s[start..end]))
        } else {
            let end = cluster_end(s, start);
            self.i = end;
            Some(Tok::Text(&s[start..end]))
        }
    }
}

fn tokens(s: &str) -> Tokens<'_> {
    Tokens { s, i: 0 }
}

// ---- SGR and hyperlink state ----------------------------------------------------------------

/// The style active at a position: the SGR sequences since the last reset, and the open OSC 8
/// hyperlink (its opening sequence).
#[derive(Debug, Default, Clone)]
struct Style {
    sgr: Vec<String>,
    link: Option<String>,
}

impl Style {
    fn feed(&mut self, esc: &str) {
        if let Some(body) = esc.strip_prefix("\x1b[").and_then(|b| b.strip_suffix('m')) {
            let parts: Vec<&str> = body.split(';').collect();
            let is_reset = |p: &str| p.is_empty() || p == "0";
            match parts.iter().rposition(|p| is_reset(p)) {
                None => self.sgr.push(esc.to_string()),
                Some(last) => {
                    self.sgr.clear();
                    let rest = &parts[last + 1..];
                    if !rest.is_empty() {
                        self.sgr.push(format!("\x1b[{}m", rest.join(";")));
                    }
                }
            }
        } else if let Some(rest) = esc.strip_prefix("\x1b]8;") {
            let uri = rest.split_once(';').map_or("", |(_, u)| u);
            let uri = uri.trim_end_matches('\x07').trim_end_matches("\x1b\\");
            self.link = if uri.is_empty() {
                None
            } else {
                Some(esc.to_string())
            };
        }
    }

    fn is_active(&self) -> bool {
        !self.sgr.is_empty() || self.link.is_some()
    }

    /// Sequences that re-open this state.
    fn prefix(&self) -> String {
        let mut s = self.sgr.concat();
        if let Some(l) = &self.link {
            s.push_str(l);
        }
        s
    }

    /// Sequences that close this state.
    fn closer(&self) -> String {
        let mut s = String::new();
        if !self.sgr.is_empty() {
            s.push_str(RESET);
        }
        if self.link.is_some() {
            s.push_str(LINK_CLOSE);
        }
        s
    }
}

// ---- public functions -----------------------------------------------------------------------

/// Visible width of `s` in terminal columns.
pub fn visible_width(s: &str) -> usize {
    tokens(s)
        .map(|t| match t {
            Tok::Text(c) => cluster_width(c),
            Tok::Esc(_) => 0,
        })
        .sum()
}

/// Remove all escape sequences (SGR, OSC 8, cursor, APC).
pub fn strip_ansi(s: &str) -> String {
    tokens(s)
        .filter_map(|t| match t {
            Tok::Text(c) => Some(c),
            Tok::Esc(_) => None,
        })
        .collect()
}

/// One piece of a paragraph while wrapping.
#[derive(Debug, Clone)]
enum Piece {
    Esc(String),
    /// Text cluster, its width, and whether it is a space or tab.
    Text(String, usize, bool),
}

/// A run of spaces or of non-space pieces (escapes attach to the run they follow).
struct Seg {
    pieces: Vec<Piece>,
    w: usize,
    space: bool,
}

fn segments(para: &str) -> Vec<Seg> {
    let mut segs: Vec<Seg> = Vec::new();
    for tok in tokens(para) {
        match tok {
            Tok::Esc(e) => {
                if segs.is_empty() {
                    segs.push(Seg {
                        pieces: Vec::new(),
                        w: 0,
                        space: false,
                    });
                }
                if let Some(s) = segs.last_mut() {
                    s.pieces.push(Piece::Esc(e.to_string()));
                }
            }
            Tok::Text(cl) => {
                let space = cl == " " || cl == "\t";
                let cw = cluster_width(cl);
                if segs.last().is_none_or(|s| s.space != space) {
                    segs.push(Seg {
                        pieces: Vec::new(),
                        w: 0,
                        space,
                    });
                }
                if let Some(s) = segs.last_mut() {
                    s.w += cw;
                    s.pieces.push(Piece::Text(cl.to_string(), cw, space));
                }
            }
        }
    }
    segs
}

/// Builds the lines of one paragraph.
struct Wrapper {
    out: Vec<String>,
    style: Style,
    cur: String,
    /// Visible width of `cur`.
    w: usize,
    /// Start of the trailing run of spaces in `cur` (byte offset, width before the run).
    trail: Option<(usize, usize)>,
}

impl Wrapper {
    fn new() -> Wrapper {
        Wrapper {
            out: Vec::new(),
            style: Style::default(),
            cur: String::new(),
            w: 0,
            trail: None,
        }
    }

    fn esc(&mut self, e: &str) {
        self.style.feed(e);
        self.cur.push_str(e);
    }

    fn text(&mut self, cl: &str, cw: usize, space: bool) {
        if space {
            if self.trail.is_none() {
                self.trail = Some((self.cur.len(), self.w));
            }
        } else {
            self.trail = None;
        }
        self.cur.push_str(cl);
        self.w += cw;
    }

    fn emit(&mut self, pieces: &[Piece]) {
        for p in pieces {
            match p {
                Piece::Esc(e) => self.esc(e),
                Piece::Text(cl, cw, space) => self.text(cl, *cw, *space),
            }
        }
    }

    /// The current line, closed. Does not start a new one.
    fn finish_line(&mut self) -> String {
        let mut s = std::mem::take(&mut self.cur);
        s.push_str(&self.style.closer());
        s
    }

    /// End the current line at a soft break: trailing spaces go, escapes stay, and the style
    /// is re-opened at the start of the next line.
    fn break_line(&mut self) {
        if let Some((pos, w)) = self.trail.take() {
            let tail = self.cur.split_off(pos);
            for t in tokens(&tail) {
                if let Tok::Esc(e) = t {
                    self.cur.push_str(e);
                }
            }
            self.w = w;
        }
        let line = self.finish_line();
        self.out.push(line);
        self.cur = self.style.prefix();
        self.w = 0;
        self.trail = None;
    }
}

fn wrap_paragraph(para: &str, width: usize) -> Vec<String> {
    let segs = segments(para);
    let mut wr = Wrapper::new();
    for seg in &segs {
        if wr.w + seg.w <= width {
            wr.emit(&seg.pieces);
        } else if seg.space {
            // The spaces do not fit: break here and drop them; keep the escapes.
            wr.break_line();
            for p in &seg.pieces {
                if let Piece::Esc(e) = p {
                    wr.esc(e);
                }
            }
        } else if seg.w <= width {
            // A whole word that fits on a line of its own moves down.
            wr.break_line();
            wr.emit(&seg.pieces);
        } else {
            // A word longer than the width is broken at cluster boundaries.
            for p in &seg.pieces {
                match p {
                    Piece::Esc(e) => wr.esc(e),
                    Piece::Text(cl, cw, _) => {
                        if wr.w + cw > width && wr.w > 0 {
                            wr.break_line();
                        }
                        if *cw > width {
                            // A cluster wider than the whole line cannot be shown: use spaces.
                            let sp = " ".repeat(width);
                            wr.text(&sp, width, false);
                        } else {
                            wr.text(cl, *cw, false);
                        }
                    }
                }
            }
        }
    }
    let last = wr.finish_line();
    wr.out.push(last);
    wr.out
}

/// Word-wrap `text` to `width` columns, keeping SGR state and OSC 8 links across breaks.
///
/// Input newlines split paragraphs; an empty paragraph gives an empty line. Breaks happen at
/// whitespace when possible; a word longer than `width` is broken at cluster boundaries.
/// Trailing whitespace at a soft break is dropped. `width == 0` returns the input lines
/// unwrapped. Every result line has `visible_width <= width`.
pub fn wrap_ansi(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return text.split('\n').map(str::to_string).collect();
    }
    text.split('\n')
        .flat_map(|para| wrap_paragraph(para, width))
        .collect()
}

/// Cut `line` to at most `width` columns; when something was cut, the last columns are
/// replaced by `ellipsis` (which may be empty) so the result still fits. Styles are closed.
pub fn truncate_to_width(line: &str, width: usize, ellipsis: &str) -> String {
    if visible_width(line) <= width {
        return line.to_string();
    }
    let ew = visible_width(ellipsis);
    let (ell, budget) = if ew <= width {
        (ellipsis, width - ew)
    } else {
        ("", width)
    };
    let mut style = Style::default();
    let mut out = String::new();
    let mut used = 0usize;
    for tok in tokens(line) {
        match tok {
            Tok::Esc(e) => {
                style.feed(e);
                out.push_str(e);
            }
            Tok::Text(cl) => {
                let cw = cluster_width(cl);
                if used + cw > budget {
                    break;
                }
                used += cw;
                out.push_str(cl);
            }
        }
    }
    out.push_str(ell);
    if style.is_active() {
        out.push_str(&style.closer());
    }
    out
}

/// The columns `start .. start + len` of `line`, with the SGR state that was active at `start`
/// re-applied and closed at the end. A wide cluster cut by either edge becomes spaces.
pub fn slice_by_column(line: &str, start: usize, len: usize) -> String {
    let end = start.saturating_add(len);
    // `before` is the state at `start`; `style` is the state at the current column.
    let mut before = Style::default();
    let mut style = Style::default();
    let mut body = String::new();
    let mut col = 0usize;
    for tok in tokens(line) {
        if col >= end {
            break;
        }
        match tok {
            Tok::Esc(e) => {
                style.feed(e);
                if col < start {
                    before.feed(e);
                } else {
                    body.push_str(e);
                }
            }
            Tok::Text(cl) => {
                let cw = cluster_width(cl);
                let (a, b) = (col, col + cw);
                col = b;
                let lo = a.max(start);
                let hi = b.min(end);
                if lo < hi {
                    if a >= start && b <= end {
                        body.push_str(cl);
                    } else {
                        body.push_str(&" ".repeat(hi - lo));
                    }
                }
            }
        }
    }
    let mut out = before.prefix();
    out.push_str(&body);
    if style.is_active() {
        out.push_str(&style.closer());
    }
    out
}

/// Pad with spaces on the right to exactly `width` columns (no change when already wider).
/// The padding is placed before a trailing reset only if `inside_style` (background fills).
pub fn pad_to_width(line: &str, width: usize, inside_style: bool) -> String {
    let w = visible_width(line);
    if w >= width {
        return line.to_string();
    }
    let pad = " ".repeat(width - w);
    if inside_style && line.ends_with(RESET) {
        let head = &line[..line.len() - RESET.len()];
        format!("{head}{pad}{RESET}")
    } else {
        format!("{line}{pad}")
    }
}

/// Byte offsets of grapheme cluster starts in `s` (plain text, no escapes), plus `s.len()`.
/// Shared with the editor so cursor movement and width agree.
pub fn cluster_boundaries(s: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < s.len() {
        out.push(i);
        i = cluster_end(s, i);
    }
    out.push(s.len());
    out
}

/// Where an overlay is placed on the frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayAnchor {
    Center,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
    TopCenter,
    BottomCenter,
    /// Explicit 0-based `(row, col)` of the overlay's top-left corner.
    At(usize, u16),
}

/// 0-based `(row, col)` of the overlay's top-left corner.
fn anchor_pos(anchor: OverlayAnchor, width: usize, ow: usize, rows: usize, oh: usize) -> (usize, usize) {
    let center_col = width.saturating_sub(ow) / 2;
    let center_row = rows.saturating_sub(oh) / 2;
    let right_col = width.saturating_sub(ow);
    let bottom_row = rows.saturating_sub(oh);
    match anchor {
        OverlayAnchor::Center => (center_row, center_col),
        OverlayAnchor::TopLeft => (0, 0),
        OverlayAnchor::TopRight => (0, right_col),
        OverlayAnchor::BottomLeft => (bottom_row, 0),
        OverlayAnchor::BottomRight => (bottom_row, right_col),
        OverlayAnchor::TopCenter => (0, center_col),
        OverlayAnchor::BottomCenter => (bottom_row, center_col),
        OverlayAnchor::At(r, c) => (r, (c as usize).min(width)),
    }
}

/// Draw `overlay` lines over `base` lines (both already fitted to their widths) and return the
/// composed lines. `base` is padded with empty lines up to `rows` when shorter. Cells of the
/// base that the overlay covers are replaced; styles on both sides of the cut are preserved.
/// The overlay is clipped to the `width` x `rows` area.
pub fn compose_overlay(
    base: &[String],
    overlay: &[String],
    overlay_width: u16,
    anchor: OverlayAnchor,
    width: u16,
    rows: usize,
) -> Vec<String> {
    let mut out: Vec<String> = base.iter().take(rows).cloned().collect();
    out.resize(rows, String::new());
    let width = width as usize;
    let ow = (overlay_width as usize).min(width);
    if ow == 0 {
        return out;
    }
    let (top, left) = anchor_pos(anchor, width, ow, rows, overlay.len());
    for (k, ol) in overlay.iter().enumerate() {
        let r = top + k;
        if r >= rows || left >= width {
            break;
        }
        let span = ow.min(width - left);
        let fitted = pad_to_width(&truncate_to_width(ol, span, ""), span, false);
        let base_line = pad_to_width(&out[r], width, false);
        let head = slice_by_column(&base_line, 0, left);
        let tail = slice_by_column(&base_line, left + span, width - left - span);
        out[r] = format!("{head}{fitted}{RESET}{tail}");
    }
    out
}
