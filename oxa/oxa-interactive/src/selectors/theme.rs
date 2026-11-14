//! D7: theme selector with live preview. Owner: task D7. Signatures below are frozen; bodies are yours.
// ported from pi packages/coding-agent/src/modes/interactive/components/theme-selector.ts
use crate::Outcome;
use oxa_config::Keymap;
use oxa_tui::{
    truncate_to_width, Component, Key, KeyKind, SelectAction, SelectItem, SelectList, Theme, ThemeInfo,
};

/// Rows shown at once.
const MAX_VISIBLE: usize = 10;

/// `/theme`: lists the themes; moving the selection previews it ([`ThemeSelector::preview`]),
/// Enter confirms (the app persists it), Escape restores the theme that was active
/// ([`ThemeSelector::original`]).
pub struct ThemeSelector {
    list: SelectList,
    original: String,
    /// The name last reported by [`ThemeSelector::preview`].
    reported: Option<String>,
}

impl ThemeSelector {
    pub fn new(themes: Vec<ThemeInfo>, current: &str) -> ThemeSelector {
        let items: Vec<SelectItem> = themes
            .iter()
            .map(|t| SelectItem {
                value: t.name.clone(),
                label: t.name.clone(),
                description: (t.name == current).then(|| "(current)".to_string()),
            })
            .collect();
        let mut list = SelectList::new(items, MAX_VISIBLE);
        let position = themes.iter().position(|t| t.name == current).unwrap_or(0);
        list.set_selected_index(position);
        // The theme that is already active is not a change: nothing to preview until the cursor moves.
        let reported = list.selected().map(|item| item.value.clone());
        ThemeSelector {
            list,
            original: current.to_string(),
            reported,
        }
    }

    /// Result: the chosen theme name.
    pub fn key(&mut self, key: &Key, keymap: &Keymap) -> Outcome<String> {
        if key.kind == KeyKind::Release {
            return Outcome::Ignored;
        }
        match self.list.key(key, keymap) {
            SelectAction::Selected(name) => Outcome::Done(name),
            SelectAction::Cancelled => Outcome::Cancelled,
            SelectAction::Moved => Outcome::Pending,
            SelectAction::Ignored => Outcome::Ignored,
        }
    }

    /// The theme name under the cursor when it changed since the last call (the app loads and
    /// applies it for preview); `None` when unchanged.
    pub fn preview(&mut self) -> Option<String> {
        let now = self.list.selected().map(|item| item.value.clone());
        if now == self.reported {
            None
        } else {
            self.reported = now.clone();
            now
        }
    }

    /// The theme that was active when the selector opened.
    pub fn original(&self) -> &str {
        &self.original
    }

    pub fn render(&mut self, width: u16, theme: &Theme) -> Vec<String> {
        let w = width as usize;
        let mut lines = vec![theme.fg("accent", &truncate_to_width("Theme", w, ""))];
        lines.extend(self.list.render(width, theme));
        lines
    }
}
