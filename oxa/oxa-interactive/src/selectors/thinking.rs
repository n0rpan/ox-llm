//! D7: thinking level selector. Owner: task D7. Signatures below are frozen; bodies are yours.
// ported from pi packages/coding-agent/src/modes/interactive/components/thinking-selector.ts
use crate::Outcome;
use oxa_config::Keymap;
use oxa_proto::ThinkingLevel;
use oxa_tui::{truncate_to_width, Component, Key, KeyKind, SelectAction, SelectItem, SelectList, Theme};

/// Rows shown at once.
const MAX_VISIBLE: usize = 8;

/// pi's description of each level.
fn description(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Off => "No reasoning",
        ThinkingLevel::Minimal => "Very brief reasoning (~1k tokens)",
        ThinkingLevel::Low => "Light reasoning (~2k tokens)",
        ThinkingLevel::Medium => "Moderate reasoning (~8k tokens)",
        ThinkingLevel::High => "Deep reasoning (~16k tokens)",
        ThinkingLevel::Xhigh => "Extra-high reasoning (~32k tokens)",
        ThinkingLevel::Max => "Maximum reasoning",
    }
}

/// `/thinking`: the levels the model accepts, each with pi's description; the current one is
/// preselected. When the model's template cannot keep old thinking (W6) only `off` is
/// offered and `note` is shown as the reason.
pub struct ThinkingSelector {
    levels: Vec<ThinkingLevel>,
    note: Option<String>,
    list: SelectList,
    /// The result was chosen with `app.thinking.save` (also save it as the default).
    save: bool,
}

impl ThinkingSelector {
    pub fn new(available: &[ThinkingLevel], current: ThinkingLevel, note: Option<&str>) -> ThinkingSelector {
        let levels: Vec<ThinkingLevel> = available.to_vec();
        let items: Vec<SelectItem> = levels
            .iter()
            .map(|&level| SelectItem {
                value: level.as_str().to_string(),
                label: format!("{}{}", if level == current { "✓ " } else { "  " }, level.as_str()),
                description: Some(description(level).to_string()),
            })
            .collect();
        let mut list = SelectList::new(items, MAX_VISIBLE);
        let position = levels.iter().position(|&l| l == current).unwrap_or(0);
        list.set_selected_index(position);
        ThinkingSelector {
            levels,
            note: note.map(str::to_string),
            list,
            save: false,
        }
    }

    pub fn key(&mut self, key: &Key, keymap: &Keymap) -> Outcome<ThinkingLevel> {
        if key.kind == KeyKind::Release {
            return Outcome::Ignored;
        }
        if keymap.matches(&key.id, "app.thinking.save") {
            // pi thinking-selector.ts: select the highlighted level and set it as the default.
            let chosen = self.list.selected().map(|item| item.value.clone());
            if let Some(level) = chosen.and_then(|v| self.levels.iter().copied().find(|l| l.as_str() == v)) {
                self.save = true;
                return Outcome::Done(level);
            }
            return Outcome::Ignored;
        }
        match self.list.key(key, keymap) {
            SelectAction::Selected(value) => self
                .levels
                .iter()
                .copied()
                .find(|l| l.as_str() == value)
                .map_or(Outcome::Ignored, Outcome::Done),
            SelectAction::Cancelled => Outcome::Cancelled,
            SelectAction::Moved => Outcome::Pending,
            SelectAction::Ignored => Outcome::Ignored,
        }
    }

    /// `true` when the level was chosen with `app.thinking.save`: the caller also writes it as
    /// `defaultThinkingLevel`.
    pub fn saves_default(&self) -> bool {
        self.save
    }

    pub fn render(&mut self, width: u16, theme: &Theme) -> Vec<String> {
        let w = width as usize;
        let mut lines = vec![theme.fg("accent", &truncate_to_width("Thinking level", w, ""))];
        if let Some(note) = &self.note {
            lines.push(theme.fg("muted", &truncate_to_width(note, w, "")));
        }
        lines.extend(self.list.render(width, theme));
        lines
    }
}

/// The next level for the cycle key (Shift+Tab): the following entry of `available`, wrapping.
/// `current` not in `available` gives the first entry; an empty list gives `Off`.
pub fn cycle_thinking(available: &[ThinkingLevel], current: ThinkingLevel) -> ThinkingLevel {
    if available.is_empty() {
        return ThinkingLevel::Off;
    }
    match available.iter().position(|&l| l == current) {
        Some(i) => available[(i + 1) % available.len()],
        None => available[0],
    }
}
