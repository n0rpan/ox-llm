//! B11: Markdown to terminal lines.
// ported from pi packages/tui/src/components/markdown.ts
//
// The parser is a small line-based block parser plus a delimiter-run inline scanner. It is
// total: every prefix of a document parses, unclosed constructs fall back to text, and every
// rendered line is closed (its own styles end on it) and fits the requested width.
use crate::{
    highlight_code, pad_to_width, strip_ansi, truncate_to_width, visible_width, wrap_ansi, Component, Theme,
};

/// Rendering options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownOpts {
    /// Columns of indentation for code block content (`markdown.codeBlockIndent`, default 2).
    pub code_block_indent: u16,
    /// pi's `markdown.codeBlockIndent` as given (any prefix string, e.g. `"│ "`); replaces
    /// `code_block_indent` spaces when set.
    pub code_block_prefix: Option<String>,
    /// Emit OSC 8 hyperlinks for links (only when the terminal supports them).
    pub hyperlinks: bool,
    /// Horizontal padding in columns on both sides.
    pub padding_x: u16,
    /// Theme token for plain text (`text`, `thinkingText`, `userMessageText`, ...).
    pub text_token: String,
    /// Background token for every line (padded to the full width), if any.
    pub bg_token: Option<String>,
    /// Render all text in italics (thinking blocks).
    pub italic: bool,
}

impl Default for MarkdownOpts {
    fn default() -> Self {
        MarkdownOpts {
            code_block_indent: 2,
            code_block_prefix: None,
            hyperlinks: false,
            padding_x: 0,
            text_token: "text".to_string(),
            bg_token: None,
            italic: false,
        }
    }
}

/// Render Markdown `src` to lines of at most `width` columns.
///
/// Supported (CommonMark subset + GFM tables, as pi): headings, paragraphs, emphasis, strong,
/// strikethrough, inline code, links (`text (url)` or OSC 8), ordered / unordered / nested
/// lists, task items, blockquotes (nested), fenced and indented code blocks (highlighted
/// through [`crate::highlight_code`], frames drawn in `mdCodeBlockBorder`), horizontal rules,
/// tables (column widths fitted to `width`, cells wrapped), hard breaks, HTML shown as text.
/// Mermaid fences are shown as highlighted code blocks (plan sec. 1).
///
/// * **Streaming-safe**: any prefix of a valid document (an unclosed fence, half a table, a
///   lone `*`) renders without panicking, and an unclosed fence renders as an open code block.
/// * Deterministic; every line is self-contained and fits `width`.
///
/// Acceptance (plan B11): snapshots at 80/120 cols: lists, tables, nested code, links (OSC 8),
/// `codeBlockIndent`; streaming partial input never panics.
pub fn render_markdown(src: &str, width: u16, theme: &Theme, opts: &MarkdownOpts) -> Vec<String> {
    if src.trim().is_empty() {
        return Vec::new();
    }
    let width = width as usize;
    let pad = opts.padding_x as usize;
    let avail = width.saturating_sub(2 * pad).max(1);
    let normalized = src
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\t', "    ");
    // Untrusted model output is text, not control: a raw ESC/BEL/C1 byte would otherwise reach
    // the terminal (cursor movement, title, clipboard) both as literal text and inside an
    // autolink URL. Keep the line break the block parser needs; SGR/OSC is added by the theme.
    let normalized: String = normalized
        .chars()
        .filter(|c| *c == '\n' || !c.is_control())
        .collect();
    let lines: Vec<&str> = normalized.split('\n').collect();
    let blocks = parse_blocks(&lines, 0);
    let cx = Cx { theme, opts };
    let base = St {
        token: opts.text_token.clone(),
        italic: opts.italic,
        ..St::default()
    };
    let body = render_blocks(&cx, &blocks, avail, &base);
    let lpad = " ".repeat(pad);
    body.into_iter()
        .map(|l| {
            let fitted = truncate_to_width(&format!("{lpad}{l}"), width, "");
            match &opts.bg_token {
                Some(bg) => theme.bg(bg, &pad_to_width(&fitted, width, false)),
                None => fitted,
            }
        })
        .collect()
}

/// A component around [`render_markdown`] that caches by `(text, width, theme name)`.
pub struct Markdown {
    text: String,
    opts: MarkdownOpts,
    cache: Option<(u16, String, Vec<String>)>,
}

impl Markdown {
    pub fn new(text: &str, opts: MarkdownOpts) -> Markdown {
        Markdown {
            text: text.to_string(),
            opts,
            cache: None,
        }
    }
    /// Replace the text (streaming updates call this for every delta batch).
    pub fn set_text(&mut self, text: &str) {
        if self.text != text {
            self.text = text.to_string();
            self.cache = None;
        }
    }
    pub fn text(&self) -> &str {
        &self.text
    }
}

impl Component for Markdown {
    fn render(&mut self, width: u16, theme: &Theme) -> Vec<String> {
        if let Some((w, name, lines)) = &self.cache {
            if *w == width && *name == theme.name {
                return lines.clone();
            }
        }
        let lines = render_markdown(&self.text, width, theme, &self.opts);
        self.cache = Some((width, theme.name.clone(), lines.clone()));
        lines
    }
    fn invalidate(&mut self) {
        self.cache = None;
    }
}

// ---- block model and parser ------------------------------------------------------------------

/// Nesting limit for quotes and lists; deeper markers are plain paragraph text.
const MAX_DEPTH: usize = 24;
/// Nesting limit for emphasis and link text; deeper spans are plain text.
const MAX_INLINE_DEPTH: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Block {
    Heading {
        level: usize,
        text: String,
    },
    /// Inline source; soft line breaks are spaces, hard breaks are `\n`.
    Paragraph(String),
    Code {
        lang: String,
        code: String,
    },
    Quote(Vec<Block>),
    List {
        ordered: bool,
        start: u64,
        loose: bool,
        items: Vec<Item>,
    },
    Table {
        header: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    Rule,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Item {
    /// `Some(checked)` for a GFM task item.
    task: Option<bool>,
    blocks: Vec<Block>,
}

struct ListStart {
    ordered: bool,
    number: u64,
    /// Column where the item's content starts (continuation lines are indented to it).
    content_indent: usize,
    content: String,
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

fn is_blank(line: &str) -> bool {
    line.trim().is_empty()
}

/// An opening code fence: (fence char, run length, info string).
fn fence_open(line: &str) -> Option<(char, usize, String)> {
    if indent_of(line) > 3 {
        return None;
    }
    let t = line.trim_start();
    let c = t.chars().next()?;
    if c != '`' && c != '~' {
        return None;
    }
    let run = t.chars().take_while(|&x| x == c).count();
    if run < 3 {
        return None;
    }
    let info = t[run..].trim();
    if c == '`' && info.contains('`') {
        return None;
    }
    Some((c, run, info.to_string()))
}

/// A closing fence: only the fence char, at least as long as the opening run.
fn is_fence_close(line: &str, c: char, n: usize) -> bool {
    if indent_of(line) > 3 {
        return false;
    }
    let t = line.trim();
    let run = t.chars().take_while(|&x| x == c).count();
    run >= n && run == t.len()
}

fn atx_heading(line: &str) -> Option<(usize, String)> {
    if indent_of(line) > 3 {
        return None;
    }
    let t = line.trim_start();
    let level = t.bytes().take_while(|&b| b == b'#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let after = &t[level..];
    if !(after.is_empty() || after.starts_with(' ') || after.starts_with('\t')) {
        return None;
    }
    let text = after.trim();
    // A closing run of `#` is dropped when it is separated by a space.
    let no_hash = text.trim_end_matches('#');
    let text = if no_hash.is_empty() {
        ""
    } else if no_hash.ends_with(' ') {
        no_hash.trim_end()
    } else {
        text
    };
    Some((level, text.to_string()))
}

fn is_rule(line: &str) -> bool {
    if indent_of(line) > 3 {
        return false;
    }
    let t = line.trim();
    let Some(c) = t.chars().next() else {
        return false;
    };
    if !matches!(c, '-' | '*' | '_') {
        return false;
    }
    let mut n = 0;
    for ch in t.chars() {
        if ch == c {
            n += 1;
        } else if ch != ' ' && ch != '\t' {
            return false;
        }
    }
    n >= 3
}

fn quote_content(line: &str) -> Option<String> {
    if indent_of(line) > 3 {
        return None;
    }
    let rest = line.trim_start().strip_prefix('>')?;
    Some(rest.strip_prefix(' ').unwrap_or(rest).to_string())
}

fn list_start(line: &str) -> Option<ListStart> {
    let ind = indent_of(line);
    if ind > 3 {
        return None;
    }
    let t = &line[ind..];
    let b = t.as_bytes();
    let (ordered, number, marker_len) = if matches!(b.first(), Some(b'-' | b'*' | b'+')) {
        (false, 0, 1)
    } else {
        let digits = b.iter().take_while(|c| c.is_ascii_digit()).count();
        if digits == 0 || digits > 9 || !matches!(b.get(digits), Some(b'.' | b')')) {
            return None;
        }
        (true, t[..digits].parse::<u64>().ok()?, digits + 1)
    };
    let after = &t[marker_len..];
    if !(after.is_empty() || after.starts_with(' ') || after.starts_with('\t')) {
        return None;
    }
    let spaces = after.len() - after.trim_start_matches(' ').len();
    // An empty item, or five or more spaces (an indented code block), puts the content one
    // column after the marker.
    let (pad, content) = if after.trim().is_empty() {
        (1, "")
    } else if spaces >= 5 {
        (1, &after[1..])
    } else {
        (spaces, &after[spaces..])
    };
    Some(ListStart {
        ordered,
        number,
        content_indent: ind + marker_len + pad,
        content: content.to_string(),
    })
}

/// `true` when `line` starts a block that may end a paragraph.
fn interrupts_paragraph(line: &str) -> bool {
    fence_open(line).is_some()
        || atx_heading(line).is_some()
        || is_rule(line)
        || quote_content(line).is_some()
        || list_start(line).is_some_and(|s| !s.content.trim().is_empty() && (!s.ordered || s.number == 1))
}

/// `true` when `line` starts any block, a list item of either kind included. Lazy
/// continuation lines of list items and quotes never start a block.
fn starts_block(line: &str) -> bool {
    fence_open(line).is_some()
        || atx_heading(line).is_some()
        || is_rule(line)
        || quote_content(line).is_some()
        || list_start(line).is_some()
}

/// Cells of a table row: pipes split cells, `\|` is a literal pipe, outer pipes are optional.
fn split_row(line: &str) -> Vec<String> {
    let t = line.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = if t.ends_with('|') && !t.ends_with("\\|") {
        &t[..t.len() - 1]
    } else {
        t
    };
    let mut cells = Vec::new();
    let mut cur = String::new();
    let mut chars = t.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && chars.peek() == Some(&'|') {
            cur.push('|');
            chars.next();
        } else if c == '|' {
            cells.push(cur.trim().to_string());
            cur.clear();
        } else {
            cur.push(c);
        }
    }
    cells.push(cur.trim().to_string());
    cells
}

/// Column count of a delimiter row (`---`, `:--`, `--:`, `:-:` cells), `None` otherwise.
fn delimiter_cols(line: &str) -> Option<usize> {
    let cells = split_row(line);
    let ok = cells.iter().all(|c| {
        let inner = c.trim_matches(':');
        !inner.is_empty() && inner.chars().all(|x| x == '-')
    });
    ok.then_some(cells.len())
}

/// The column count of a table whose header is `lines[i]`, when `lines[i + 1]` is its delimiter.
fn table_cols(lines: &[&str], i: usize) -> Option<usize> {
    let head = lines.get(i)?;
    let delim = lines.get(i + 1)?;
    if !head.contains('|') || !delim.contains('|') {
        return None;
    }
    let cols = delimiter_cols(delim)?;
    (split_row(head).len() == cols).then_some(cols)
}

fn fit_cells(mut cells: Vec<String>, cols: usize) -> Vec<String> {
    cells.resize(cols, String::new());
    cells
}

/// A GFM task marker at the start of an item: `[ ]`, `[x]` or `[X]`.
fn strip_task(s: &str) -> Option<(bool, &str)> {
    for (pat, checked) in [("[ ]", false), ("[x]", true), ("[X]", true)] {
        if let Some(rest) = s.strip_prefix(pat) {
            if rest.is_empty() {
                return Some((checked, rest));
            }
            if rest.starts_with(' ') {
                return Some((checked, rest.trim_start()));
            }
        }
    }
    None
}

/// One list item starting at `lines[start]`. Returns the item and the index after its last
/// content line (trailing blank lines are not consumed).
fn list_item(lines: &[&str], start: usize, s: &ListStart, depth: usize) -> (Item, usize) {
    let n = lines.len();
    let mut content: Vec<String> = vec![s.content.clone()];
    let mut end = start + 1;
    let mut j = start + 1;
    while j < n {
        let l = lines[j];
        if is_blank(l) {
            content.push(String::new());
            j += 1;
            continue;
        }
        if indent_of(l) >= s.content_indent {
            content.push(l[s.content_indent..].to_string());
        } else if content.last().is_some_and(|c| !is_blank(c)) && !starts_block(l) {
            // Lazy continuation of the paragraph in the item.
            content.push(l.trim_start().to_string());
        } else {
            break;
        }
        j += 1;
        end = j;
    }
    while content.last().is_some_and(|c| is_blank(c)) {
        content.pop();
    }
    let mut task = None;
    if let Some((checked, rest)) = content
        .first()
        .and_then(|f| strip_task(f))
        .map(|(c, r)| (c, r.to_string()))
    {
        task = Some(checked);
        content[0] = rest;
    }
    let refs: Vec<&str> = content.iter().map(String::as_str).collect();
    (
        Item {
            task,
            blocks: parse_blocks(&refs, depth + 1),
        },
        end,
    )
}

/// Parse block structure from (already de-tabbed) lines.
fn parse_blocks(lines: &[&str], depth: usize) -> Vec<Block> {
    let n = lines.len();
    let nested = depth < MAX_DEPTH;
    let mut out = Vec::new();
    let mut i = 0;
    while i < n {
        let line = lines[i];
        if is_blank(line) {
            i += 1;
            continue;
        }
        if let Some((fence, run, info)) = fence_open(line) {
            let ind = indent_of(line);
            let mut code: Vec<&str> = Vec::new();
            let mut j = i + 1;
            while j < n && !is_fence_close(lines[j], fence, run) {
                let l = lines[j];
                code.push(&l[indent_of(l).min(ind)..]);
                j += 1;
            }
            out.push(Block::Code {
                lang: info,
                code: code.join("\n"),
            });
            // Skip the closing fence; an unclosed fence runs to the end.
            i = j + 1;
            continue;
        }
        if let Some((level, text)) = atx_heading(line) {
            out.push(Block::Heading { level, text });
            i += 1;
            continue;
        }
        if is_rule(line) {
            out.push(Block::Rule);
            i += 1;
            continue;
        }
        if indent_of(line) >= 4 {
            let mut code: Vec<&str> = Vec::new();
            let mut j = i;
            let mut end = i;
            while j < n && (is_blank(lines[j]) || indent_of(lines[j]) >= 4) {
                let l = lines[j];
                code.push(if is_blank(l) { "" } else { &l[4..] });
                j += 1;
                if !is_blank(l) {
                    end = j;
                }
            }
            code.truncate(end - i);
            out.push(Block::Code {
                lang: String::new(),
                code: code.join("\n"),
            });
            i = end;
            continue;
        }
        if nested && quote_content(line).is_some() {
            let mut inner: Vec<String> = Vec::new();
            let mut j = i;
            while j < n {
                if let Some(c) = quote_content(lines[j]) {
                    inner.push(c);
                } else if !is_blank(lines[j])
                    && inner.last().is_some_and(|s| !is_blank(s))
                    && !starts_block(lines[j])
                {
                    inner.push(lines[j].trim_start().to_string());
                } else {
                    break;
                }
                j += 1;
            }
            let refs: Vec<&str> = inner.iter().map(String::as_str).collect();
            out.push(Block::Quote(parse_blocks(&refs, depth + 1)));
            i = j;
            continue;
        }
        if nested {
            if let Some(first) = list_start(line) {
                let ordered = first.ordered;
                let start = first.number;
                let mut items = Vec::new();
                let mut loose = false;
                let mut j = i;
                let mut next_i = i + 1;
                let mut cur = Some(first);
                while let Some(s) = cur.take() {
                    let (item, end) = list_item(lines, j, &s, depth);
                    items.push(item);
                    next_i = end;
                    let mut k = end;
                    while k < n && is_blank(lines[k]) {
                        k += 1;
                    }
                    let more = k < n && list_start(lines[k]).is_some_and(|t| t.ordered == ordered);
                    if !more {
                        break;
                    }
                    if k > end {
                        loose = true;
                    }
                    j = k;
                    cur = list_start(lines[k]);
                }
                out.push(Block::List {
                    ordered,
                    start,
                    loose,
                    items,
                });
                i = next_i;
                continue;
            }
        }
        if let Some(cols) = table_cols(lines, i) {
            let header = fit_cells(split_row(line), cols);
            let mut rows = Vec::new();
            let mut j = i + 2;
            while j < n && !is_blank(lines[j]) && lines[j].contains('|') {
                rows.push(fit_cells(split_row(lines[j]), cols));
                j += 1;
            }
            out.push(Block::Table { header, rows });
            i = j;
            continue;
        }
        // Paragraph: until a blank line or a block that interrupts it.
        let mut parts: Vec<&str> = Vec::new();
        let mut j = i;
        while j < n
            && !is_blank(lines[j])
            && (j == i || (!interrupts_paragraph(lines[j]) && table_cols(lines, j).is_none()))
        {
            parts.push(lines[j]);
            j += 1;
        }
        let mut text = String::new();
        let last = parts.len() - 1;
        for (k, p) in parts.iter().enumerate() {
            let t = p.trim_start();
            let t_end = t.trim_end();
            if k == last {
                text.push_str(t_end);
            } else if let Some(stripped) = t_end.strip_suffix('\\') {
                text.push_str(stripped);
                text.push('\n');
            } else if t.ends_with("  ") {
                text.push_str(t_end);
                text.push('\n');
            } else {
                text.push_str(t_end);
                text.push(' ');
            }
        }
        out.push(Block::Paragraph(text));
        i = j;
    }
    out
}

// ---- rendering -------------------------------------------------------------------------------

/// Rendering context: the theme and the options.
struct Cx<'a> {
    theme: &'a Theme,
    opts: &'a MarkdownOpts,
}

/// The style of a run of text: a foreground token and the attributes on top of it.
#[derive(Debug, Clone, Default)]
struct St {
    token: String,
    bold: bool,
    italic: bool,
    strike: bool,
    underline: bool,
}

impl St {
    fn tok(token: &str) -> St {
        St {
            token: token.to_string(),
            ..St::default()
        }
    }
}

/// `text` in the style `st`, closed on the same text. Newlines stay unstyled.
fn paint(cx: &Cx, st: &St, text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    if text.contains('\n') {
        return text
            .split('\n')
            .map(|p| paint(cx, st, p))
            .collect::<Vec<_>>()
            .join("\n");
    }
    let t = cx.theme;
    let mut s = text.to_string();
    if st.underline {
        s = t.underline(&s);
    }
    if st.strike {
        s = t.strikethrough(&s);
    }
    if st.italic {
        s = t.italic(&s);
    }
    if st.bold {
        s = t.bold(&s);
    }
    t.fg(&st.token, &s)
}

/// Number of consecutive `c` starting at `i`.
fn run_len(chars: &[char], i: usize, c: char) -> usize {
    chars[i..].iter().take_while(|&&x| x == c).count()
}

/// The index of the closing backtick run of exactly `run` at or after `from`.
fn find_code_close(chars: &[char], from: usize, run: usize) -> Option<usize> {
    let mut p = from;
    while p < chars.len() {
        if chars[p] == '`' {
            let r = run_len(chars, p, '`');
            if r == run {
                return Some(p);
            }
            p += r;
        } else {
            p += 1;
        }
    }
    None
}

/// The index of the closing delimiter run of `k` chars `c` for an opener at `i`.
fn emphasis_end(chars: &[char], i: usize, c: char, k: usize) -> Option<usize> {
    let open = i + k;
    match chars.get(open) {
        None => return None,
        Some(x) if x.is_whitespace() => return None,
        _ => {}
    }
    // An underscore inside a word does not open emphasis.
    if c == '_' && i > 0 && chars[i - 1].is_alphanumeric() {
        return None;
    }
    let mut p = open;
    while p < chars.len() {
        if chars[p] == c {
            let r = run_len(chars, p, c);
            let closes = r == k
                && p > open
                && !chars[p - 1].is_whitespace()
                && !(c == '_' && chars.get(p + k).is_some_and(|x| x.is_alphanumeric()));
            if closes {
                return Some(p);
            }
            p += r;
        } else {
            p += 1;
        }
    }
    None
}

/// The index of the bracket matching the one at `open_at` (`open` / `close`), skipping escapes.
fn matching(chars: &[char], open_at: usize, open: char, close: char) -> Option<usize> {
    let mut depth = 0usize;
    let mut p = open_at;
    while p < chars.len() {
        let c = chars[p];
        if c == '\\' {
            p += 2;
            continue;
        }
        if c == open {
            depth += 1;
        } else if c == close {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                return Some(p);
            }
        }
        p += 1;
    }
    None
}

/// `[text](dest)` starting at `i`: the index of `]` and of the closing `)`.
fn link_at(chars: &[char], i: usize) -> Option<(usize, usize)> {
    let close = matching(chars, i, '[', ']')?;
    if chars.get(close + 1) != Some(&'(') {
        return None;
    }
    let end = matching(chars, close + 1, '(', ')')?;
    Some((close, end))
}

/// A `<scheme://...>` autolink body, without spaces.
fn is_autolink(inner: &str) -> bool {
    !inner.is_empty() && inner.contains("://") && !inner.chars().any(|c| c.is_whitespace() || c == '<')
}

fn flush(cx: &Cx, st: &St, lit: &mut String, out: &mut String) {
    if !lit.is_empty() {
        out.push_str(&paint(cx, st, lit));
        lit.clear();
    }
}

/// A link around already rendered `text`: OSC 8 when enabled, else `text (url)`.
fn push_link(cx: &Cx, st: &St, text: &str, url: &str, out: &mut String) {
    if url.is_empty() {
        out.push_str(text);
        return;
    }
    if cx.opts.hyperlinks {
        out.push_str(&format!("\x1b]8;;{url}\x1b\\{text}\x1b]8;;\x1b\\"));
    } else {
        out.push_str(text);
        if strip_ansi(text) != url {
            let url_st = St {
                token: "mdLinkUrl".to_string(),
                ..st.clone()
            };
            out.push_str(&paint(cx, &url_st, &format!(" ({url})")));
        }
    }
}

/// Inline Markdown of `chars` in style `st`, appended to `out`.
fn inline(cx: &Cx, chars: &[char], st: &St, depth: usize, out: &mut String) {
    let n = chars.len();
    let mut lit = String::new();
    let mut i = 0;
    while i < n {
        let c = chars[i];
        match c {
            '\\' if chars.get(i + 1).is_some_and(|d| d.is_ascii_punctuation()) => {
                lit.push(chars[i + 1]);
                i += 2;
            }
            '\n' => {
                flush(cx, st, &mut lit, out);
                out.push('\n');
                i += 1;
            }
            '`' => {
                let run = run_len(chars, i, '`');
                match find_code_close(chars, i + run, run) {
                    Some(end) => {
                        flush(cx, st, &mut lit, out);
                        let mut body: Vec<char> = chars[i + run..end]
                            .iter()
                            .map(|&x| if x == '\n' { ' ' } else { x })
                            .collect();
                        if body.len() >= 2
                            && body[0] == ' '
                            && body[body.len() - 1] == ' '
                            && body.iter().any(|&x| x != ' ')
                        {
                            body = body[1..body.len() - 1].to_vec();
                        }
                        let code_st = St {
                            token: "mdCode".to_string(),
                            ..st.clone()
                        };
                        out.push_str(&paint(cx, &code_st, &body.iter().collect::<String>()));
                        i = end + run;
                    }
                    None => {
                        lit.extend(std::iter::repeat_n('`', run));
                        i += run;
                    }
                }
            }
            '*' | '_' | '~' => {
                let run = run_len(chars, i, c);
                let k = match (c, run) {
                    ('~', 2) => 2,
                    ('*' | '_', 1..=3) => run,
                    _ => 0,
                };
                let end = if k > 0 && depth < MAX_INLINE_DEPTH {
                    emphasis_end(chars, i, c, k)
                } else {
                    None
                };
                match end {
                    Some(end) => {
                        flush(cx, st, &mut lit, out);
                        let mut inner_st = st.clone();
                        match (c, k) {
                            ('~', _) => inner_st.strike = true,
                            (_, 1) => inner_st.italic = true,
                            (_, 2) => inner_st.bold = true,
                            _ => {
                                inner_st.bold = true;
                                inner_st.italic = true;
                            }
                        }
                        inline(cx, &chars[i + k..end], &inner_st, depth + 1, out);
                        i = end + k;
                    }
                    None => {
                        lit.extend(std::iter::repeat_n(c, run));
                        i += run;
                    }
                }
            }
            '[' => match link_at(chars, i) {
                Some((close, end)) if depth < MAX_INLINE_DEPTH => {
                    flush(cx, st, &mut lit, out);
                    let dest: String = chars[close + 2..end].iter().collect();
                    let url: String = dest
                        .split_whitespace()
                        .next()
                        .unwrap_or("")
                        .trim_matches(|c| c == '<' || c == '>')
                        .chars()
                        .filter(|c| !c.is_control())
                        .collect();
                    let link_st = St {
                        token: "mdLink".to_string(),
                        underline: true,
                        ..st.clone()
                    };
                    let text_chars: Vec<char> = chars[i + 1..close]
                        .iter()
                        .map(|&x| if x == '\n' { ' ' } else { x })
                        .collect();
                    let mut text = String::new();
                    inline(cx, &text_chars, &link_st, depth + 1, &mut text);
                    push_link(cx, st, &text, &url, out);
                    i = end + 1;
                }
                _ => {
                    lit.push('[');
                    i += 1;
                }
            },
            '<' => {
                let found = chars[i + 1..].iter().position(|&x| x == '>');
                match found {
                    Some(off) if is_autolink(&chars[i + 1..i + 1 + off].iter().collect::<String>()) => {
                        flush(cx, st, &mut lit, out);
                        let url: String = chars[i + 1..i + 1 + off].iter().collect();
                        let link_st = St {
                            token: "mdLink".to_string(),
                            underline: true,
                            ..st.clone()
                        };
                        let text = paint(cx, &link_st, &url);
                        push_link(cx, st, &text, &url, out);
                        i = i + 1 + off + 1;
                    }
                    _ => {
                        lit.push('<');
                        i += 1;
                    }
                }
            }
            _ => {
                lit.push(c);
                i += 1;
            }
        }
    }
    flush(cx, st, &mut lit, out);
}

fn render_inline(cx: &Cx, text: &str, st: &St) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    inline(cx, &chars, st, 0, &mut out);
    out
}

/// Lines of `blocks` at `width` columns, in style `base`. Blocks are separated by one blank
/// line, except that a list directly after a paragraph follows it without one (as pi does).
fn render_blocks(cx: &Cx, blocks: &[Block], width: usize, base: &St) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (idx, block) in blocks.iter().enumerate() {
        let after_paragraph = idx > 0 && matches!(blocks[idx - 1], Block::Paragraph(_));
        if idx > 0 && !(matches!(block, Block::List { .. }) && after_paragraph) {
            out.push(String::new());
        }
        match block {
            Block::Heading { level, text } => {
                let hst = St {
                    token: "mdHeading".to_string(),
                    bold: true,
                    underline: *level == 1,
                    ..St::default()
                };
                let mut styled = String::new();
                if *level >= 3 {
                    styled.push_str(&paint(cx, &hst, &format!("{} ", "#".repeat(*level))));
                }
                styled.push_str(&render_inline(cx, text, &hst));
                out.extend(wrap_ansi(&styled, width));
            }
            Block::Paragraph(text) => {
                out.extend(wrap_ansi(&render_inline(cx, text, base), width));
            }
            Block::Code { lang, code } => {
                let border = St::tok("mdCodeBlockBorder");
                // Leave at least one content column; tiny terminals get horizontal rules
                // without side borders. The outer renderer still fits zero-width output.
                let framed = width >= 3;
                let body_w = if framed { width - 2 } else { width };
                let indent = match &cx.opts.code_block_prefix {
                    Some(prefix) => prefix.clone(),
                    None => " ".repeat(cx.opts.code_block_indent as usize),
                };
                let indent = truncate_to_width(&indent, body_w.saturating_sub(1), "");
                let inner_w = body_w.saturating_sub(visible_width(&indent)).max(1);
                let language = lang.split_whitespace().next();
                let label = language.map(|l| format!("─ {l} ")).unwrap_or_default();
                let label = truncate_to_width(&label, body_w, "");
                let top = format!("{label}{}", "─".repeat(body_w - visible_width(&label)));
                let bottom = "─".repeat(body_w);
                out.push(paint(cx, &border, &if framed { format!("┌{top}┐") } else { top }));
                let side = paint(cx, &border, "│");
                for hl in highlight_code(code, language, cx.theme) {
                    for piece in wrap_ansi(&hl, inner_w) {
                        let line = format!("{indent}{piece}");
                        out.push(if framed {
                            format!("{side}{}{side}", pad_to_width(&line, body_w, false))
                        } else {
                            line
                        });
                    }
                }
                out.push(paint(
                    cx,
                    &border,
                    &if framed { format!("└{bottom}┘") } else { bottom },
                ));
            }
            Block::Quote(inner) => {
                let qst = St {
                    token: "mdQuote".to_string(),
                    italic: true,
                    ..base.clone()
                };
                let body = render_blocks(cx, inner, width.saturating_sub(2).max(1), &qst);
                let bar = paint(cx, &St::tok("mdQuoteBorder"), "│ ");
                for l in body {
                    out.push(format!("{bar}{l}"));
                }
            }
            Block::Rule => {
                out.push(paint(cx, &St::tok("mdHr"), &"─".repeat(width.min(80))));
            }
            Block::List {
                ordered,
                start,
                loose,
                items,
            } => {
                for (k, item) in items.iter().enumerate() {
                    if k > 0 && *loose {
                        out.push(String::new());
                    }
                    let bullet = if *ordered {
                        format!("{}. ", start.saturating_add(k as u64))
                    } else {
                        "- ".to_string()
                    };
                    let task = match item.task {
                        Some(true) => "[x] ",
                        Some(false) => "[ ] ",
                        None => "",
                    };
                    let marker = format!("{bullet}{task}");
                    let mw = visible_width(&marker);
                    let first = paint(cx, &St::tok("mdListBullet"), &marker);
                    let body = render_blocks(cx, &item.blocks, width.saturating_sub(mw).max(1), base);
                    if body.is_empty() {
                        out.push(first);
                        continue;
                    }
                    let cont = " ".repeat(mw);
                    for (li, l) in body.into_iter().enumerate() {
                        if li == 0 {
                            out.push(format!("{first}{l}"));
                        } else if l.is_empty() {
                            out.push(String::new());
                        } else {
                            out.push(format!("{cont}{l}"));
                        }
                    }
                }
            }
            Block::Table { header, rows } => {
                out.extend(render_table(cx, header, rows, width, base));
            }
        }
    }
    out
}

/// A GFM table fitted to `width`: natural column widths when they fit, otherwise the longest
/// words plus a share of the remaining columns; cells wrap inside their column.
fn render_table(cx: &Cx, header: &[String], rows: &[Vec<String>], width: usize, base: &St) -> Vec<String> {
    let cols = header.len();
    if cols == 0 {
        return Vec::new();
    }
    let head_st = St {
        bold: true,
        ..base.clone()
    };
    let mut grid: Vec<Vec<String>> = Vec::with_capacity(rows.len() + 1);
    grid.push(header.iter().map(|c| render_inline(cx, c, &head_st)).collect());
    for r in rows {
        grid.push(r.iter().map(|c| render_inline(cx, c, base)).collect());
    }

    let mut min_w = vec![1usize; cols];
    let mut nat = vec![0usize; cols];
    for row in &grid {
        for (c, cell) in row.iter().enumerate() {
            nat[c] = nat[c].max(visible_width(cell));
            let longest = strip_ansi(cell)
                .split_whitespace()
                .map(visible_width)
                .max()
                .unwrap_or(0);
            min_w[c] = min_w[c].max(longest);
        }
    }
    for c in 0..cols {
        nat[c] = nat[c].max(min_w[c]);
    }
    // "│ " + cells joined by " │ " + " │": three columns of chrome per separator, plus one.
    let overhead = 3 * cols + 1;
    let sum_nat: usize = nat.iter().sum();
    let sum_min: usize = min_w.iter().sum();
    let widths: Vec<usize> = if overhead + sum_nat <= width {
        nat.clone()
    } else if overhead + sum_min >= width {
        min_w.clone()
    } else {
        let extra = width - overhead - sum_min;
        let total = sum_nat - sum_min;
        let mut w = min_w.clone();
        let mut given = 0;
        for c in 0..cols {
            let add = extra * (nat[c] - min_w[c]) / total;
            w[c] += add;
            given += add;
        }
        let mut left = extra - given;
        for c in 0..cols {
            if left == 0 {
                break;
            }
            if w[c] < nat[c] {
                w[c] += 1;
                left -= 1;
            }
        }
        w
    };
    let widths: Vec<usize> = widths.into_iter().map(|w| w.max(1)).collect();

    let border = St::tok("borderMuted");
    let bar = paint(cx, &border, "│");
    let rule = |left: &str, mid: &str, right: &str| -> String {
        let body = widths
            .iter()
            .map(|&w| "─".repeat(w))
            .collect::<Vec<_>>()
            .join(mid);
        paint(cx, &border, &format!("{left}─{body}─{right}"))
    };
    let row = |cells: &[String]| -> Vec<String> {
        let wrapped: Vec<Vec<String>> = cells
            .iter()
            .zip(&widths)
            .map(|(cell, &w)| wrap_ansi(cell, w))
            .collect();
        let height = wrapped.iter().map(Vec::len).max().unwrap_or(1).max(1);
        (0..height)
            .map(|li| {
                let parts: Vec<String> = wrapped
                    .iter()
                    .zip(&widths)
                    .map(|(lines, &w)| pad_to_width(lines.get(li).map_or("", String::as_str), w, false))
                    .collect();
                format!("{bar} {} {bar}", parts.join(&format!(" {bar} ")))
            })
            .collect()
    };

    let mut lines = vec![rule("┌", "─┬─", "┐")];
    lines.extend(row(&grid[0]));
    lines.push(rule("├", "─┼─", "┤"));
    for r in &grid[1..] {
        lines.extend(row(r));
    }
    lines.push(rule("└", "─┴─", "┘"));
    lines
}
