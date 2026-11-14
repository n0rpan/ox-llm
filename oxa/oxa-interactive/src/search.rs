//! D3: transcript search in fullscreen mode. Owner: task D3. Signatures below are frozen; bodies are yours.
// ported from pi packages/tui/src/alt-screen-search.ts
use oxa_tui::{slice_by_column, strip_ansi, visible_width, Theme};

use crate::select::{paint_span, tail};

/// One match: content line and the column range `[col_start, col_end)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchMatch {
    pub line: usize,
    pub col_start: u16,
    pub col_end: u16,
}

/// Incremental, case-insensitive (smart case: an uppercase letter in the query makes it
/// case-sensitive) plain-text search over the content lines, escape sequences ignored.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TranscriptSearch {
    pub query: String,
    pub matches: Vec<SearchMatch>,
    /// Index into `matches` of the current match.
    pub current: Option<usize>,
}

/// Lower-case form of a character for the comparison (the first char of its lower case).
fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// Smart case: the query is case-sensitive when it contains an uppercase letter.
fn is_case_sensitive(query: &str) -> bool {
    query.chars().any(|c| c.is_uppercase())
}

/// All non-overlapping matches of `query` in `content`, in reading order.
fn find_matches(query: &str, content: &[String]) -> Vec<SearchMatch> {
    if query.is_empty() {
        return Vec::new();
    }
    let sensitive = is_case_sensitive(query);
    let norm = |c: char| if sensitive { c } else { fold(c) };
    let q: Vec<char> = query.chars().map(norm).collect();
    let mut out = Vec::new();
    for (line, text) in content.iter().enumerate() {
        let plain = strip_ansi(text);
        // `cols[i]` is the column where character `i` starts; the last entry is the line width.
        let mut chars = Vec::new();
        let mut cols = Vec::new();
        let mut col = 0usize;
        for c in plain.chars() {
            chars.push(norm(c));
            cols.push(col);
            col += visible_width(c.encode_utf8(&mut [0u8; 4]));
        }
        cols.push(col);
        if chars.len() < q.len() {
            continue;
        }
        let mut i = 0;
        while i + q.len() <= chars.len() {
            if chars[i..i + q.len()] == q[..] {
                out.push(SearchMatch {
                    line,
                    col_start: cols[i].min(u16::MAX as usize) as u16,
                    col_end: cols[i + q.len()].min(u16::MAX as usize) as u16,
                });
                i += q.len();
            } else {
                i += 1;
            }
        }
    }
    out
}

impl TranscriptSearch {
    pub fn new() -> TranscriptSearch {
        TranscriptSearch::default()
    }
    /// Set the query and recompute matches over `content`. The current match becomes the last
    /// match at or before `from_line` (search starts from what the user sees), else the first.
    /// An empty query clears everything.
    pub fn set_query(&mut self, query: &str, content: &[String], from_line: usize) {
        self.query = query.to_string();
        self.matches = find_matches(query, content);
        self.current = if self.matches.is_empty() {
            None
        } else {
            Some(
                self.matches
                    .iter()
                    .rposition(|m| m.line <= from_line)
                    .unwrap_or(0),
            )
        };
    }
    /// Content changed (streaming): recompute, keeping the current match when it still exists.
    /// When it is gone, the first match at or after its old line is current, else the last.
    pub fn refresh(&mut self, content: &[String]) {
        let old = self.current.and_then(|i| self.matches.get(i).copied());
        self.matches = find_matches(&self.query, content);
        self.current = match (self.matches.is_empty(), old) {
            (true, _) => None,
            (false, None) => None,
            (false, Some(old)) => {
                let same = self
                    .matches
                    .iter()
                    .position(|m| m.line == old.line && m.col_start == old.col_start);
                same.or_else(|| self.matches.iter().position(|m| m.line >= old.line))
                    .or(Some(self.matches.len() - 1))
            }
        };
    }
    /// Move to the next match (wraps); returns its line for `Viewport::reveal`.
    pub fn next_match(&mut self) -> Option<usize> {
        let len = self.matches.len();
        if len == 0 {
            return None;
        }
        let next = match self.current {
            None => 0,
            Some(c) => (c + 1) % len,
        };
        self.current = Some(next);
        Some(self.matches[next].line)
    }
    /// Move to the previous match (wraps).
    pub fn prev_match(&mut self) -> Option<usize> {
        let len = self.matches.len();
        if len == 0 {
            return None;
        }
        let prev = match self.current {
            None => len - 1,
            Some(c) => (c + len - 1) % len,
        };
        self.current = Some(prev);
        Some(self.matches[prev].line)
    }
    /// `"3/17"`, or `"0/0"` without matches.
    pub fn position_text(&self) -> String {
        let n = self.matches.len();
        if n == 0 {
            return "0/0".to_string();
        }
        format!("{}/{}", self.current.map(|c| c + 1).unwrap_or(0), n)
    }
    pub fn clear(&mut self) {
        *self = TranscriptSearch::default();
    }
}

/// `content[first..first + count]` with matches drawn on `searchMatchBg` in `searchMatchText`
/// and the current match with the two swapped.
pub fn highlight_matches(
    content: &[String],
    first: usize,
    count: usize,
    search: &TranscriptSearch,
    theme: &Theme,
) -> Vec<String> {
    let token_pair = |fg: &str, bg: &str| {
        let open = format!("{}{}", theme.fg_code(fg), theme.bg_code(bg));
        if open.is_empty() {
            ("\x1b[7m".to_string(), "\x1b[27m".to_string())
        } else {
            (open, "\x1b[39m\x1b[49m".to_string())
        }
    };
    let normal = token_pair("searchMatchText", "searchMatchBg");
    let current = token_pair("searchMatchBg", "searchMatchText");
    let end = first.saturating_add(count).min(content.len());
    (first..end)
        .map(|line| {
            let text = &content[line];
            let on_line: Vec<(usize, SearchMatch)> = search
                .matches
                .iter()
                .enumerate()
                .filter(|(_, m)| m.line == line)
                .map(|(i, m)| (i, *m))
                .collect();
            if on_line.is_empty() {
                return text.clone();
            }
            let mut out = String::new();
            let mut cursor = 0usize;
            for (i, m) in on_line {
                let start = m.col_start as usize;
                let len = (m.col_end as usize).saturating_sub(start);
                if start < cursor || len == 0 {
                    continue;
                }
                out.push_str(&slice_by_column(text, cursor, start - cursor));
                let (open, close) = if search.current == Some(i) {
                    &current
                } else {
                    &normal
                };
                out.push_str(&paint_span(&slice_by_column(text, start, len), open, close));
                cursor = start + len;
            }
            out.push_str(&tail(text, cursor));
            out
        })
        .collect()
}
