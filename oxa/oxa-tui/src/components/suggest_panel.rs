//! The suggestion panel of OXA's look: the `/` command menu and the file suggestions of the
//! prompt editor as one bordered, themed panel (POPUPS.md). pi's look keeps its bare list
//! (`Editor` draws that itself); nothing here is used by it.
//!
//! ```text
//! ┌─ commands ──────────────────────────────── 2/14 ─┐
//! │   /mcp       MCP servers and their tools         ┃
//! │ ▌ /model     switch the model                    ┃
//! │   /mode      ask, workspace or full access       │
//! └─ ↑↓ select · tab complete · enter run · esc close ┘
//! ```
//!
//! Pure: lines from the arguments. Every line is exactly `width` columns and self-contained.
//! Colour only through theme tokens; with a plain theme there is no escape sequence and the
//! selected row is marked `>`.
use crate::{pad_to_width, scrollbar_thumb, truncate_to_width, visible_width, AutocompleteItem, Theme};

/// How the editor shows its suggestions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PopupStyle {
    /// pi's bare list (`→ item`).
    #[default]
    List,
    /// The bordered panel, box-drawing glyphs.
    Panel,
    /// The bordered panel, ASCII only.
    PanelAscii,
}

/// What a suggestion list completes (the panel's header and footer differ).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuggestKind {
    Commands,
    Files,
}

impl SuggestKind {
    /// From the text being completed: a leading `/` is the command menu (as the editor's own
    /// Enter rule), everything else is a file or path list.
    pub fn of_prefix(prefix: &str) -> SuggestKind {
        if prefix.starts_with('/') {
            SuggestKind::Commands
        } else {
            SuggestKind::Files
        }
    }
}

struct Set {
    h: &'static str,
    v: &'static str,
    tl: &'static str,
    tr: &'static str,
    bl: &'static str,
    br: &'static str,
    thumb: &'static str,
    mark: &'static str,
    mid: &'static str,
    arrows: &'static str,
    ell: &'static str,
}

const UNICODE: Set = Set {
    h: "─",
    v: "│",
    tl: "┌",
    tr: "┐",
    bl: "└",
    br: "┘",
    thumb: "┃",
    mark: "▌",
    mid: "·",
    arrows: "↑↓",
    ell: "…",
};

const ASCII: Set = Set {
    h: "-",
    v: "|",
    tl: "+",
    tr: "+",
    bl: "+",
    br: "+",
    thumb: "#",
    mark: ">",
    mid: "-",
    arrows: "up/down",
    ell: "...",
};

/// Indices (in `char`s of `label`) of the characters that `query` matched: the query's
/// characters in order, case-insensitively, as early as possible. Empty when the query does
/// not occur as a subsequence.
fn match_positions(label: &str, query: &str) -> Vec<usize> {
    let want: Vec<char> = query
        .chars()
        .filter(|c| !c.is_whitespace())
        .map(|c| c.to_lowercase().next().unwrap_or(c))
        .collect();
    if want.is_empty() {
        return Vec::new();
    }
    let mut found = Vec::with_capacity(want.len());
    let mut next = 0;
    for (i, c) in label.chars().enumerate() {
        if next < want.len() && c.to_lowercase().next().unwrap_or(c) == want[next] {
            found.push(i);
            next += 1;
        }
    }
    if next == want.len() {
        found
    } else {
        Vec::new()
    }
}

/// The matched characters of `label` for the text being completed: the sigils (`/`, `@`, an
/// opening quote) are not part of the query; a path query that does not match as a whole is
/// tried by its last segment.
pub fn suggest_match_positions(label: &str, prefix: &str) -> Vec<usize> {
    let query = prefix.trim_start_matches(['/', '@', '"']);
    let found = match_positions(label, query);
    if !found.is_empty() {
        return found;
    }
    match query.rsplit('/').next() {
        Some(last) if last.len() < query.len() => match_positions(label, last),
        _ => Vec::new(),
    }
}

/// `label` with the characters at `hits` emphasised (bold, `accent`) and the rest in `token`
/// (bold too when `strong`: the selected row).
fn emphasise(label: &str, hits: &[usize], token: &str, strong: bool, theme: &Theme) -> String {
    let mut out = String::new();
    let mut run = String::new();
    let mut run_hit = false;
    let flush = |run: &mut String, hit: bool, out: &mut String| {
        if run.is_empty() {
            return;
        }
        if hit {
            out.push_str(&theme.fg("accent", &theme.bold(run)));
        } else if strong {
            out.push_str(&theme.fg(token, &theme.bold(run)));
        } else {
            out.push_str(&theme.fg(token, run));
        }
        run.clear();
    };
    for (i, c) in label.chars().enumerate() {
        let hit = hits.binary_search(&i).is_ok();
        if hit != run_hit {
            flush(&mut run, run_hit, &mut out);
            run_hit = hit;
        }
        run.push(c);
    }
    flush(&mut run, run_hit, &mut out);
    out
}

/// A border line: `left`, `lead` after one rule, rules, `trail` before one rule, `right`;
/// exactly `width` columns. `lead` and `trail` are dropped (trail first) when they do not fit.
fn rule(left: &str, right: &str, lead: &str, trail: &str, width: usize, set: &Set, theme: &Theme) -> String {
    let token = "borderMuted";
    let inner = width.saturating_sub(2);
    let (lw, tw) = (visible_width(lead), visible_width(trail));
    let lead_fits = lw > 0 && lw + 4 <= inner;
    let lead_cols = if lead_fits { lw + 3 } else { 0 };
    let trail_fits = tw > 0 && lead_cols + tw + 4 <= inner;
    let mut used = 0;
    let mut out = theme.fg(token, left);
    if lead_fits {
        out.push_str(&theme.fg(token, &format!("{} ", set.h)));
        out.push_str(lead);
        out.push(' ');
        used += lw + 3;
    }
    let tail = if trail_fits { tw + 3 } else { 0 };
    out.push_str(&theme.fg(token, &set.h.repeat(inner.saturating_sub(used + tail))));
    if trail_fits {
        out.push(' ');
        out.push_str(trail);
        out.push_str(&theme.fg(token, &format!(" {}", set.h)));
    }
    out.push_str(&theme.fg(token, right));
    out
}

/// The footer hints, longest first; the first that fits is shown.
fn hints(kind: SuggestKind, set: &Set) -> [String; 3] {
    let m = set.mid;
    let a = set.arrows;
    match kind {
        SuggestKind::Commands => [
            format!("{a} select {m} tab complete {m} enter run {m} esc close"),
            format!("{a} {m} tab {m} enter run {m} esc"),
            "esc close".to_string(),
        ],
        SuggestKind::Files => [
            format!("{a} select {m} tab/enter accept {m} esc close"),
            format!("{a} {m} tab accept {m} esc"),
            "esc close".to_string(),
        ],
    }
}

/// The suggestion panel: a header (`commands` / `files` and the position), at most
/// `max_visible` item rows scrolled so that the selected one shows, and a footer of key
/// hints. The selected row is highlighted (`selectedBg`), the characters `prefix` matched are
/// emphasised, descriptions are dimmed, and when the list is longer than the panel the right
/// border carries a scroll thumb. Empty for no items or a width under 12.
pub fn suggest_panel(
    items: &[AutocompleteItem],
    selected: usize,
    prefix: &str,
    max_visible: usize,
    width: usize,
    style: PopupStyle,
    theme: &Theme,
) -> Vec<String> {
    if items.is_empty() || width < 12 {
        return Vec::new();
    }
    let set = if style == PopupStyle::PanelAscii {
        &ASCII
    } else {
        &UNICODE
    };
    let kind = SuggestKind::of_prefix(prefix);
    let selected = selected.min(items.len() - 1);
    let rows = max_visible.max(1).min(items.len());
    let start = if selected >= rows { selected + 1 - rows } else { 0 };
    let label_of = |it: &AutocompleteItem| -> String {
        let l = if it.label.is_empty() { &it.value } else { &it.label };
        l.chars().filter(|c| !c.is_control()).collect()
    };
    let inner = width - 2;
    // Row: one space, the mark, one space, the label column, two spaces, the description.
    let text_w = inner.saturating_sub(4);
    let has_desc = items
        .iter()
        .any(|it| it.description.as_deref().is_some_and(|d| !d.is_empty()));
    let label_col = if has_desc {
        let widest = items
            .iter()
            .map(|it| visible_width(&label_of(it)))
            .max()
            .unwrap_or(0);
        widest.min(28).min(text_w / 2)
    } else {
        text_w
    };

    let title = match kind {
        SuggestKind::Commands => "commands",
        SuggestKind::Files => "files",
    };
    let position = theme.fg("muted", &format!("{}/{}", selected + 1, items.len()));
    let mut out = vec![rule(
        set.tl,
        set.tr,
        &theme.fg("muted", &theme.bold(title)),
        &position,
        width,
        set,
        theme,
    )];

    let thumb = scrollbar_thumb(items.len(), rows, start);
    for (row, item) in items.iter().enumerate().skip(start).take(rows) {
        let is_sel = row == selected;
        let label = truncate_to_width(&label_of(item), label_col, set.ell);
        let hits = suggest_match_positions(&label, prefix);
        let mut text = emphasise(&label, &hits, "text", is_sel, theme);
        if let Some(d) = item.description.as_deref().filter(|d| !d.is_empty()) {
            let room = text_w.saturating_sub(label_col + 2);
            if room >= 4 {
                let d: String = d.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
                let d = truncate_to_width(&d, room, set.ell);
                text = pad_to_width(&text, label_col + 2, false);
                text.push_str(&theme.fg(if is_sel { "text" } else { "muted" }, &d));
            }
        }
        let mark = if is_sel {
            theme.fg("accent", set.mark)
        } else {
            " ".to_string()
        };
        let body = pad_to_width(&format!(" {mark} {text}"), inner, false);
        let body = if is_sel {
            theme.bg("selectedBg", &body)
        } else {
            body
        };
        let on_thumb = thumb.is_some_and(|(top, len)| (top..top + len).contains(&(row - start)));
        let right = if on_thumb {
            theme.fg("scrollbarThumb", set.thumb)
        } else {
            theme.fg("borderMuted", set.v)
        };
        out.push(format!("{}{body}{right}", theme.fg("borderMuted", set.v)));
    }

    let hint = hints(kind, set)
        .into_iter()
        .find(|h| visible_width(h) + 4 <= inner)
        .unwrap_or_default();
    out.push(rule(
        set.bl,
        set.br,
        &theme.fg("muted", &hint),
        "",
        width,
        set,
        theme,
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_are_found_in_order_without_the_sigil() {
        assert_eq!(suggest_match_positions("/model", "/mo"), vec![1, 2]);
        assert_eq!(suggest_match_positions("model", "/MD"), vec![0, 2]);
        assert_eq!(suggest_match_positions("@src/main.rs", "@sm"), vec![1, 5]);
        assert_eq!(suggest_match_positions("main.rs", "src/ma"), vec![0, 1]);
        assert!(suggest_match_positions("model", "/xyz").is_empty());
        assert!(suggest_match_positions("model", "/").is_empty());
    }
}
