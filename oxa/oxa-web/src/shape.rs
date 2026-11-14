//! The result text (W4: deterministic, capped). Both Brave endpoints produce the same shape:
//!
//! ```text
//! Web search results for: <query>
//!
//! [1] <title>
//! <url>
//! Published: <provider's date>      (when the provider gave one)
//! <snippet>
//! <snippet>
//!
//! [2] ...
//!
//! (<n> more sources omitted)        (when the budget ran out)
//! ```
//!
//! Sources keep the provider's order; snippets are trimmed, whitespace-normalised,
//! deduplicated and capped. Nothing from the local clock appears.
use oxa_tools::descriptions::web_search as text;

/// One source as a provider returned it (before shaping).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Found {
    pub title: String,
    pub url: String,
    /// The provider's date of the page (content, not the local clock).
    pub age: Option<String>,
    pub snippets: Vec<String>,
}

/// Byte budgets of the result text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budgets {
    /// The whole text. Below the default tool-output cap (16384), so the cap does not cut it.
    pub total: usize,
    /// One source block (title, URL, date, snippets).
    pub per_source: usize,
    /// One snippet before it is cut with [`text::CUT`].
    pub per_snippet: usize,
    /// The longest title in characters.
    pub title_chars: usize,
}

impl Default for Budgets {
    fn default() -> Budgets {
        Budgets {
            total: 14 * 1024,
            per_source: 2600,
            per_snippet: 1000,
            title_chars: 200,
        }
    }
}

/// Collapse every whitespace run to one space and trim.
pub(crate) fn normalise(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// At most `max` bytes of `s`, cut at a character boundary; `marker` is appended when cut
/// (and counts toward `max` when it fits).
fn cut_bytes(s: &str, max: usize, marker: &str) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max.saturating_sub(marker.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{marker}", s[..end].trim_end())
}

fn cut_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}{}", s[..i].trim_end(), text::CUT),
        None => s.to_string(),
    }
}

/// Remove HTML tags and decode the common entities (the web search endpoint marks matches
/// with `<strong>` in `description`).
pub fn strip_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    decode_entities(&out)
}

fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let end = tail.find(';').filter(|&e| e <= 10);
        let decoded = end.and_then(|e| entity(&tail[1..e]));
        match (end, decoded) {
            (Some(e), Some(c)) => {
                out.push(c);
                rest = &tail[e + 1..];
            }
            _ => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn entity(name: &str) -> Option<char> {
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => ' ',
        _ => {
            let num = name.strip_prefix('#')?;
            let code = match num.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => num.parse::<u32>().ok()?,
            };
            return char::from_u32(code);
        }
    })
}

/// One source block (no trailing newline), within `b.per_source` bytes. The title, URL and
/// date lines always stay; snippets are added in order while they fit.
fn block(n: usize, f: &Found, b: &Budgets) -> String {
    let title = normalise(&f.title);
    let title = if title.is_empty() {
        text::UNTITLED.to_string()
    } else {
        cut_chars(&title, b.title_chars)
    };
    let mut out = text::SOURCE
        .replace("{n}", &n.to_string())
        .replace("{title}", &title);
    out.push('\n');
    out.push_str(&normalise(&f.url));
    if let Some(age) = f.age.as_deref().map(normalise).filter(|a| !a.is_empty()) {
        out.push('\n');
        out.push_str(&text::AGE.replace("{age}", &age));
    }
    let mut seen: Vec<String> = Vec::new();
    for s in &f.snippets {
        let s = normalise(s);
        if s.is_empty() || seen.contains(&s) {
            continue;
        }
        let capped = cut_bytes(&s, b.per_snippet, text::CUT);
        seen.push(s);
        let room = b.per_source.saturating_sub(out.len() + 1);
        if capped.len() <= room {
            out.push('\n');
            out.push_str(&capped);
        } else {
            // The first snippet that does not fit is cut to the room left (when a useful part
            // fits), and the block ends.
            if room > 80 {
                out.push('\n');
                out.push_str(&cut_bytes(&capped, room, text::CUT));
            }
            break;
        }
    }
    out
}

/// The result text of `found` for `query`: at most `max_sources` sources, within `b.total`
/// bytes, with the omitted line when sources did not fit.
pub fn shape(query: &str, found: &[Found], max_sources: Option<usize>, b: &Budgets) -> String {
    let mut out = text::HEADER.replace("{query}", &normalise(query));
    let found: Vec<&Found> = found
        .iter()
        .filter(|f| !normalise(&f.url).is_empty())
        .take(max_sources.unwrap_or(usize::MAX))
        .collect();
    if found.is_empty() {
        out.push_str("\n\n");
        out.push_str(text::NO_RESULTS);
        return out;
    }
    // Room kept for the omitted line.
    let reserve = text::OMITTED.len() + 8;
    let mut shown = 0;
    for (i, f) in found.iter().enumerate() {
        let blk = block(i + 1, f, b);
        if out.len() + 2 + blk.len() + reserve > b.total {
            break;
        }
        out.push_str("\n\n");
        out.push_str(&blk);
        shown += 1;
    }
    let omitted = found.len() - shown;
    if omitted > 0 {
        out.push_str("\n\n");
        out.push_str(&text::OMITTED.replace("{n}", &omitted.to_string()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_and_entities() {
        assert_eq!(
            strip_html("a <strong>b</strong> &amp; c&#39;s &lt;x&gt; &#x41; &bogus; &"),
            "a b & c's <x> A &bogus; &"
        );
    }

    #[test]
    fn cut_respects_char_boundaries() {
        let s = "é".repeat(100);
        let c = cut_bytes(&s, 51, text::CUT);
        assert!(c.len() <= 51 && c.ends_with(text::CUT));
    }
}
