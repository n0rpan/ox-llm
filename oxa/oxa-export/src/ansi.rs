//! E12: ANSI (SGR) text to HTML. Owner: task E12. Signatures below are frozen; bodies are yours.
// ported from pi packages/coding-agent/src/core/export-html/ansi-to-html.ts

/// Standard ANSI palette, indices 0-15 (pi's `ANSI_COLORS`).
const ANSI_COLORS: [&str; 16] = [
    "#000000", "#800000", "#008000", "#808000", "#000080", "#800080", "#008080", "#c0c0c0", "#808080",
    "#ff0000", "#00ff00", "#ffff00", "#0000ff", "#ff00ff", "#00ffff", "#ffffff",
];

/// What one escape sequence is, as far as the converter cares.
enum Esc<'a> {
    /// `ESC [ <digits and ;> m`, with the parameter text.
    Sgr(&'a str),
    /// `ESC ] <payload> (BEL | ESC \)`, with the payload.
    Osc(&'a str),
    /// Any other CSI or APC sequence: matched and dropped.
    Other,
}

/// The escape sequence starting at byte `i` (which must be an ESC), following pi's `ANSI_REGEX`
/// alternatives in order. Returns the byte index just after the sequence.
fn match_at(text: &str, i: usize) -> Option<(usize, Esc<'_>)> {
    let b = text.as_bytes();
    if b.get(i) != Some(&0x1b) {
        return None;
    }
    match *b.get(i + 1)? {
        b'[' => {
            // SGR: ESC [ [\d;]* m
            let mut j = i + 2;
            while j < b.len() && (b[j].is_ascii_digit() || b[j] == b';') {
                j += 1;
            }
            if b.get(j) == Some(&b'm') {
                return Some((j + 1, Esc::Sgr(&text[i + 2..j])));
            }
            // Other CSI: ESC [ [0-?]* [ -/]* [@-~]
            let mut j = i + 2;
            while j < b.len() && (0x30..=0x3f).contains(&b[j]) {
                j += 1;
            }
            while j < b.len() && (0x20..=0x2f).contains(&b[j]) {
                j += 1;
            }
            if j < b.len() && (0x40..=0x7e).contains(&b[j]) {
                return Some((j + 1, Esc::Other));
            }
            None
        }
        b']' | b'_' => {
            // OSC (ESC ]) or APC (ESC _): payload up to BEL or ESC, then BEL or ESC \.
            let start = i + 2;
            let mut j = start;
            while j < b.len() && b[j] != 0x07 && b[j] != 0x1b {
                j += 1;
            }
            if j >= b.len() {
                return None;
            }
            let payload = &text[start..j];
            let esc = if b[i + 1] == b']' {
                Esc::Osc(payload)
            } else {
                Esc::Other
            };
            if b[j] == 0x07 {
                return Some((j + 1, esc));
            }
            if b.get(j + 1) == Some(&b'\\') {
                return Some((j + 2, esc));
            }
            None
        }
        _ => None,
    }
}

/// Remove every escape sequence (CSI, OSC incl. OSC 8 hyperlinks, single-character escapes).
pub fn strip_ansi_escapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    let mut i = 0;
    while let Some(off) = text[i..].find('\x1b') {
        let p = i + off;
        match match_at(text, p) {
            Some((end, _)) => {
                out.push_str(&text[last..p]);
                last = end;
                i = end;
            }
            None => i = p + 1,
        }
    }
    out.push_str(&text[last..]);
    out.replace('\x1b', "")
}

/// Convert text with SGR sequences to HTML `<span style="...">` runs.
///
/// Supports: reset, bold, dim, italic, underline, strikethrough, the 16 standard colours,
/// 256-colour (`38;5;n` / `48;5;n`) and true colour (`38;2;r;g;b`), default-colour resets.
/// Text is HTML-escaped (`& < > " '`). Unknown sequences are dropped. Output for the same input
/// is byte-identical to pi's `ansiToHtml` (fixtures in the task's tests).
pub fn ansi_to_html(text: &str) -> String {
    let mut style = Style::default();
    let mut out = String::with_capacity(text.len());
    let mut in_span = false;
    let mut in_link = false;
    let mut last = 0;
    let mut i = 0;

    while let Some(off) = text[i..].find('\x1b') {
        let p = i + off;
        let Some((end, kind)) = match_at(text, p) else {
            i = p + 1;
            continue;
        };
        push_text(&mut out, &text[last..p]);
        match kind {
            Esc::Sgr(params) => {
                close_span(&mut out, &mut in_span);
                apply_sgr(&parse_params(params), &mut style);
                open_span(&mut out, &mut in_span, &style);
            }
            Esc::Osc(payload) => {
                if let Some(raw_url) = osc8_url(payload) {
                    // Strip C0 controls: browsers drop them from URLs anyway.
                    let url: String = raw_url
                        .trim()
                        .chars()
                        .filter(|c| !(*c <= '\u{1f}' || *c == '\u{7f}'))
                        .collect();
                    close_span(&mut out, &mut in_span);
                    if in_link {
                        out.push_str("</a>");
                        in_link = false;
                    }
                    if !url.is_empty() && is_safe_link(&url) {
                        out.push_str("<a href=\"");
                        out.push_str(&escape_html(&url));
                        out.push_str("\">");
                        in_link = true;
                    }
                    open_span(&mut out, &mut in_span, &style);
                }
            }
            Esc::Other => {}
        }
        last = end;
        i = end;
    }
    push_text(&mut out, &text[last..]);
    close_span(&mut out, &mut in_span);
    if in_link {
        out.push_str("</a>");
    }
    out
}

/// Each line through [`ansi_to_html`], wrapped as `<div class="ansi-line">..</div>`; an empty
/// line becomes `<div class="ansi-line">&nbsp;</div>` (pi's rule).
pub fn ansi_lines_to_html(lines: &[String]) -> String {
    let mut out = String::new();
    for line in lines {
        let html = ansi_to_html(line);
        out.push_str("<div class=\"ansi-line\">");
        if html.is_empty() {
            out.push_str("&nbsp;");
        } else {
            out.push_str(&html);
        }
        out.push_str("</div>");
    }
    out
}

/// Escape `& < > " '` for HTML text and attribute values.
pub fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#039;"),
            _ => out.push(c),
        }
    }
    out
}

/// Plain text between escape sequences: stray ESC characters are dropped, the rest escaped.
fn push_text(out: &mut String, text: &str) {
    if !text.is_empty() {
        out.push_str(&escape_html(&text.replace('\x1b', "")));
    }
}

/// The URL of an OSC 8 hyperlink payload (`8;params;url`), `None` for other OSC payloads.
fn osc8_url(payload: &str) -> Option<&str> {
    let rest = payload.strip_prefix("8;")?;
    let sep = rest.find(';')?;
    Some(&rest[sep + 1..])
}

/// pi's `SAFE_LINK_REGEX` (`/^(?:https?:|mailto:)/i`).
fn is_safe_link(url: &str) -> bool {
    let b = url.as_bytes();
    [&b"http:"[..], &b"https:"[..], &b"mailto:"[..]]
        .iter()
        .any(|p| b.len() >= p.len() && b[..p.len()].eq_ignore_ascii_case(p))
}

#[derive(Default)]
struct Style {
    fg: Option<String>,
    bg: Option<String>,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
}

impl Style {
    fn has_style(&self) -> bool {
        self.fg.is_some() || self.bg.is_some() || self.bold || self.dim || self.italic || self.underline
    }

    fn css(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(fg) = &self.fg {
            parts.push(format!("color:{fg}"));
        }
        if let Some(bg) = &self.bg {
            parts.push(format!("background-color:{bg}"));
        }
        if self.bold {
            parts.push("font-weight:bold".to_string());
        }
        if self.dim {
            parts.push("opacity:0.6".to_string());
        }
        if self.italic {
            parts.push("font-style:italic".to_string());
        }
        if self.underline {
            parts.push("text-decoration:underline".to_string());
        }
        parts.join(";")
    }
}

fn close_span(out: &mut String, in_span: &mut bool) {
    if *in_span {
        out.push_str("</span>");
        *in_span = false;
    }
}

fn open_span(out: &mut String, in_span: &mut bool, style: &Style) {
    if style.has_style() {
        out.push_str("<span style=\"");
        out.push_str(&style.css());
        out.push_str("\">");
        *in_span = true;
    }
}

/// SGR parameters as pi reads them: `parseInt(p) || 0` per `;`-separated field; empty = `[0]`.
fn parse_params(params: &str) -> Vec<u64> {
    if params.is_empty() {
        return vec![0];
    }
    params
        .split(';')
        .map(|p| {
            p.bytes().take_while(u8::is_ascii_digit).fold(0u64, |n, d| {
                n.saturating_mul(10).saturating_add(u64::from(d - b'0'))
            })
        })
        .collect()
}

/// The xterm 256-colour palette entry `n`, as pi's `color256ToHex` writes it.
fn color256_to_hex(n: u64) -> String {
    if n < 16 {
        return ANSI_COLORS[n as usize].to_string();
    }
    if n < 232 {
        let cube = n - 16;
        let comp = |x: u64| if x == 0 { 0 } else { 55 + x * 40 };
        return format!(
            "#{:02x}{:02x}{:02x}",
            comp(cube / 36),
            comp((cube % 36) / 6),
            comp(cube % 6)
        );
    }
    let gray = 8u64.saturating_add(n.saturating_sub(232).saturating_mul(10));
    let g = format!("{gray:02x}");
    format!("#{g}{g}{g}")
}

/// pi's `applySgrCode`: update `style` for one SGR parameter list. Unknown codes are ignored.
fn apply_sgr(params: &[u64], style: &mut Style) {
    let mut i = 0;
    while i < params.len() {
        let code = params[i];
        match code {
            0 => *style = Style::default(),
            1 => style.bold = true,
            2 => style.dim = true,
            3 => style.italic = true,
            4 => style.underline = true,
            22 => {
                style.bold = false;
                style.dim = false;
            }
            23 => style.italic = false,
            24 => style.underline = false,
            30..=37 => style.fg = Some(ANSI_COLORS[(code - 30) as usize].to_string()),
            38 => {
                if params.get(i + 1) == Some(&5) && params.len() > i + 2 {
                    style.fg = Some(color256_to_hex(params[i + 2]));
                    i += 2;
                } else if params.get(i + 1) == Some(&2) && params.len() > i + 4 {
                    style.fg = Some(format!(
                        "rgb({},{},{})",
                        params[i + 2],
                        params[i + 3],
                        params[i + 4]
                    ));
                    i += 4;
                }
            }
            39 => style.fg = None,
            40..=47 => style.bg = Some(ANSI_COLORS[(code - 40) as usize].to_string()),
            48 => {
                if params.get(i + 1) == Some(&5) && params.len() > i + 2 {
                    style.bg = Some(color256_to_hex(params[i + 2]));
                    i += 2;
                } else if params.get(i + 1) == Some(&2) && params.len() > i + 4 {
                    style.bg = Some(format!(
                        "rgb({},{},{})",
                        params[i + 2],
                        params[i + 3],
                        params[i + 4]
                    ));
                    i += 4;
                }
            }
            49 => style.bg = None,
            90..=97 => style.fg = Some(ANSI_COLORS[(code - 90 + 8) as usize].to_string()),
            100..=107 => style.bg = Some(ANSI_COLORS[(code - 100 + 8) as usize].to_string()),
            _ => {}
        }
        i += 1;
    }
}
