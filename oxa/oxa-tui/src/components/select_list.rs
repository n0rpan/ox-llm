//! B12: SelectList, a scrolling single-choice list. Owner: task B12. Signatures below are frozen; bodies are yours.
// ported from pi packages/tui/src/components/select-list.ts
use crate::{truncate_to_width, visible_width, Component, Key, KeyKind, Theme};
use oxa_config::Keymap;

/// One row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectItem {
    /// Returned on selection.
    pub value: String,
    /// Shown; falls back to `value` when empty.
    pub label: String,
    /// Shown dimmed after the label when the width allows.
    pub description: Option<String>,
}

/// What a key did to a list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectAction {
    Ignored,
    /// Selection moved.
    Moved,
    /// Confirmed; carries the item's `value`.
    Selected(String),
    Cancelled,
}

/// Widest primary column pi allows before descriptions are cut to make room.
const MAX_PRIMARY_COLUMN: usize = 32;
/// Rows narrower than this drop their description (pi).
const DESCRIPTION_MIN_WIDTH: usize = 40;
/// Least description width worth showing (pi).
const DESCRIPTION_MIN_CELLS: usize = 10;

/// A list showing at most `max_visible` rows with a scroll window that keeps the selection
/// visible, and a `(3/12)` position line when it scrolls.
///
/// Keys through the keymap: `tui.select.up`, `tui.select.down`, `tui.select.pageUp`,
/// `tui.select.pageDown`, `tui.select.confirm`, `tui.select.cancel`. Up on the first row
/// wraps to the last and vice versa (pi). The selected row is prefixed `→ ` in `accent`.
/// An empty (or fully filtered) list renders one `No matching items`-style line in `muted`.
pub struct SelectList {
    items: Vec<SelectItem>,
    /// Indices into `items` of the rows that pass the filter, best match first.
    shown: Vec<usize>,
    selected: usize,
    max_visible: usize,
    filter: String,
    empty_text: String,
    keymap: Option<Keymap>,
    action: SelectAction,
}

/// The text a row is matched and shown by: `label`, or `value` when the label is empty.
fn display_text(item: &SelectItem) -> &str {
    if item.label.is_empty() {
        &item.value
    } else {
        &item.label
    }
}

/// Case-insensitive subsequence score of `query` in `text` (lower is better), `None` when
/// the characters of `query` do not occur in order. Gaps between matched characters and a
/// late first match both cost.
fn fuzzy_score(text: &str, query: &str) -> Option<usize> {
    let chars: Vec<char> = text.to_lowercase().chars().collect();
    let mut score = 0usize;
    let mut pos = 0usize;
    let mut last: Option<usize> = None;
    for qc in query.to_lowercase().chars() {
        if qc.is_whitespace() {
            continue;
        }
        let found = chars[pos..].iter().position(|&c| c == qc)?;
        let idx = pos + found;
        score += match last {
            Some(l) => idx - l - 1,
            None => idx,
        };
        last = Some(idx);
        pos = idx + 1;
    }
    Some(score)
}

/// First line of a description with runs of line breaks turned into single spaces.
fn single_line(text: &str) -> String {
    text.split(['\r', '\n'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

impl SelectList {
    pub fn new(items: Vec<SelectItem>, max_visible: usize) -> SelectList {
        let mut list = SelectList {
            items,
            shown: Vec::new(),
            selected: 0,
            max_visible: max_visible.max(1),
            filter: String::new(),
            empty_text: "No matching items".to_string(),
            keymap: None,
            action: SelectAction::Ignored,
        };
        list.refilter();
        list
    }
    pub fn set_items(&mut self, items: Vec<SelectItem>) {
        self.items = items;
        self.refilter();
    }
    /// Keep only items whose label fuzzy-matches `filter`, best first; selection resets to the
    /// first row. An empty filter restores all items in their original order.
    pub fn set_filter(&mut self, filter: &str) {
        self.filter = filter.to_string();
        self.refilter();
        self.selected = 0;
    }
    pub fn selected(&self) -> Option<&SelectItem> {
        self.shown.get(self.selected).map(|&i| &self.items[i])
    }
    pub fn selected_index(&self) -> usize {
        self.selected
    }
    pub fn set_selected_index(&mut self, index: usize) {
        self.selected = index.min(self.shown.len().saturating_sub(1));
    }
    /// The number of rows currently shown (after the filter).
    pub fn len(&self) -> usize {
        self.shown.len()
    }
    pub fn is_empty(&self) -> bool {
        self.shown.is_empty()
    }
    /// Text of the line shown when there is nothing to list.
    pub fn set_empty_text(&mut self, text: &str) {
        self.empty_text = text.to_string();
    }
    pub fn key(&mut self, key: &Key, keymap: &Keymap) -> SelectAction {
        if key.kind == KeyKind::Release {
            return SelectAction::Ignored;
        }
        let n = self.shown.len();
        let page = self.max_visible;
        let action = if keymap.matches(&key.id, "tui.select.up") {
            if n == 0 {
                SelectAction::Ignored
            } else {
                self.selected = if self.selected == 0 {
                    n - 1
                } else {
                    self.selected - 1
                };
                SelectAction::Moved
            }
        } else if keymap.matches(&key.id, "tui.select.down") {
            if n == 0 {
                SelectAction::Ignored
            } else {
                self.selected = if self.selected + 1 >= n {
                    0
                } else {
                    self.selected + 1
                };
                SelectAction::Moved
            }
        } else if keymap.matches(&key.id, "tui.select.pageUp") {
            let target = self.selected.saturating_sub(page);
            self.move_to(target)
        } else if keymap.matches(&key.id, "tui.select.pageDown") {
            let target = (self.selected + page).min(n.saturating_sub(1));
            self.move_to(target)
        } else if keymap.matches(&key.id, "tui.select.confirm") {
            match self.selected() {
                Some(item) => SelectAction::Selected(item.value.clone()),
                None => SelectAction::Ignored,
            }
        } else if keymap.matches(&key.id, "tui.select.cancel") {
            SelectAction::Cancelled
        } else {
            SelectAction::Ignored
        };
        self.action = action.clone();
        action
    }
    /// Keymap for `Component::input`; see `Editor::set_keymap`.
    pub fn set_keymap(&mut self, keymap: Keymap) {
        self.keymap = Some(keymap);
    }
    /// The action of the last `Component::input` call.
    pub fn take_action(&mut self) -> SelectAction {
        std::mem::replace(&mut self.action, SelectAction::Ignored)
    }

    /// Move the selection for a page key; `Moved` only when the row changed.
    fn move_to(&mut self, target: usize) -> SelectAction {
        if self.shown.is_empty() || target == self.selected {
            return SelectAction::Ignored;
        }
        self.selected = target;
        SelectAction::Moved
    }

    /// Recompute `shown` from the current filter; the selection is clamped, not reset.
    fn refilter(&mut self) {
        if self.filter.is_empty() {
            self.shown = (0..self.items.len()).collect();
        } else {
            let mut scored: Vec<(usize, usize)> = self
                .items
                .iter()
                .enumerate()
                .filter_map(|(i, item)| fuzzy_score(display_text(item), &self.filter).map(|s| (s, i)))
                .collect();
            scored.sort();
            self.shown = scored.into_iter().map(|(_, i)| i).collect();
        }
        self.selected = self.selected.min(self.shown.len().saturating_sub(1));
    }

    /// First visible row: keeps the selection in the middle of the window where possible.
    fn window_start(&self) -> usize {
        let max_start = self.shown.len().saturating_sub(self.max_visible);
        self.selected.saturating_sub(self.max_visible / 2).min(max_start)
    }

    fn render_row(
        &self,
        item: &SelectItem,
        selected: bool,
        width: usize,
        column: usize,
        theme: &Theme,
    ) -> String {
        let prefix = if selected { "→ " } else { "  " };
        let label = display_text(item);
        let description = item
            .description
            .as_deref()
            .map(single_line)
            .filter(|d| !d.is_empty());

        let line = match description {
            Some(desc) if width > DESCRIPTION_MIN_WIDTH => {
                let effective = column.min(width - 2 - 4).max(1);
                let value = truncate_to_width(label, effective.saturating_sub(2).max(1), "");
                let value_w = visible_width(&value);
                let spacing = " ".repeat(effective.saturating_sub(value_w).max(1));
                let desc_start = 2 + value_w + spacing.len();
                let remaining = width.saturating_sub(desc_start + 2);
                if remaining > DESCRIPTION_MIN_CELLS {
                    let desc = truncate_to_width(&desc, remaining, "");
                    if selected {
                        theme.fg("accent", &format!("{prefix}{value}{spacing}{desc}"))
                    } else {
                        format!(
                            "{prefix}{value}{}",
                            theme.fg("muted", &format!("{spacing}{desc}"))
                        )
                    }
                } else {
                    self.plain_row(label, prefix, selected, width, theme)
                }
            }
            _ => self.plain_row(label, prefix, selected, width, theme),
        };
        truncate_to_width(&line, width, "")
    }

    fn plain_row(&self, label: &str, prefix: &str, selected: bool, width: usize, theme: &Theme) -> String {
        let value = truncate_to_width(label, width.saturating_sub(4), "");
        let line = format!("{prefix}{value}");
        if selected {
            theme.fg("accent", &line)
        } else {
            line
        }
    }
}

impl Component for SelectList {
    fn render(&mut self, width: u16, theme: &Theme) -> Vec<String> {
        let w = width as usize;
        if self.shown.is_empty() {
            return vec![theme.fg("muted", &truncate_to_width(&self.empty_text, w, ""))];
        }
        let n = self.shown.len();
        let start = self.window_start();
        let end = (start + self.max_visible).min(n);

        // The primary column is as wide as the widest label of the shown rows, bounded (pi).
        let widest = self
            .shown
            .iter()
            .map(|&i| visible_width(display_text(&self.items[i])) + 2)
            .max()
            .unwrap_or(0);
        let column = widest.clamp(1, MAX_PRIMARY_COLUMN);

        let mut lines = Vec::with_capacity(end - start + 1);
        for pos in start..end {
            let item = &self.items[self.shown[pos]];
            lines.push(self.render_row(item, pos == self.selected, w, column, theme));
        }
        if start > 0 || end < n {
            let info = format!("  ({}/{})", self.selected + 1, n);
            lines.push(theme.fg("muted", &truncate_to_width(&info, w.saturating_sub(2), "")));
        }
        lines
    }
    fn input(&mut self, key: &Key) -> bool {
        if self.keymap.is_none() {
            self.keymap = Some(Keymap::defaults());
        }
        let keymap = self.keymap.take().unwrap_or_default();
        let action = self.key(key, &keymap);
        self.keymap = Some(keymap);
        !matches!(action, SelectAction::Ignored)
    }
    fn invalidate(&mut self) {
        // Nothing is cached: every render is computed from the rows.
    }
}
