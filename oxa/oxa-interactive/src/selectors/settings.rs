//! D7: settings selector.
// ported from pi packages/coding-agent/src/modes/interactive/components/{settings-selector.ts,settings-submenu.ts} and packages/tui/src/components/settings-list.ts
use super::model::{key_label, printable_text};
use crate::Outcome;
use oxa_config::{schema, Keymap, Kind, Settings};
use oxa_proto::Value;
use oxa_tui::{truncate_to_width, Component, Key, KeyKind, SelectAction, SelectItem, SelectList, Theme};

/// Rows shown at once.
const MAX_VISIBLE: usize = 10;
/// A choice with more values than this opens a submenu (pi), otherwise Enter cycles.
const SUBMENU_ABOVE: usize = 4;
/// Keys that stay editable only with `/theme`, `/model` or the session picker, not here.
const EXCLUDED: &[&str] = &["$schema", "defaultModel", "theme", "sessionDir"];

/// How a setting is edited.
#[derive(Debug, Clone, PartialEq)]
pub enum SettingKind {
    /// Enter / space toggles.
    Bool,
    /// Enter cycles through the values (or opens a submenu when more than 4).
    Choice(Vec<String>),
    /// Enter opens a one-line input; the text must parse as a non-negative integer.
    Number,
    Text,
}

/// One row of `/settings`.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingItem {
    /// Dotted settings key (`compaction.enabled`, `oxa.toolOutputCap`, `theme`).
    pub key: String,
    pub label: String,
    pub description: String,
    pub kind: SettingKind,
    pub value: Value,
}

/// One change made in the selector, to be saved to the user settings file by the app.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingChange {
    pub key: String,
    pub value: Value,
}

/// The rows of `/settings` with their current values: pi's list (settings-selector.ts) minus
/// provider/OAuth rows, plus oxa's (`oxa.toolOutputCap`, `oxa.compaction.softTokens`, default
/// mode). Order as pi.
pub fn setting_items(settings: &Settings) -> Vec<SettingItem> {
    schema()
        .iter()
        .filter(|def| def.supported && !EXCLUDED.contains(&def.key))
        .filter_map(|def| {
            let kind = match def.kind {
                Kind::Bool => SettingKind::Bool,
                Kind::Number => SettingKind::Number,
                Kind::String => SettingKind::Text,
                Kind::Enum(values) => SettingKind::Choice(values.iter().map(|v| v.to_string()).collect()),
                Kind::StringList | Kind::Object | Kind::Any => return None,
            };
            let value = if def.key == "oxa.local.vCacheThp" {
                Value::Bool(settings.local_v_cache_thp())
            } else { settings
                .get(def.key)
                .cloned()
                .unwrap_or_else(|| serde_json::from_str(def.default).unwrap_or(Value::Null)) };
            Some(SettingItem {
                key: def.key.to_string(),
                label: humanize(def.key),
                description: if def.key == "oxa.local.vCacheThp" {
                    "Faster generation (~1%) at the cost of ~1.2-1.4 GiB more RAM. Applies on next model load; unload/reload a running model.".into()
                } else { format!("{} (default {})", def.key, def.default) },
                kind,
                value,
            })
        })
        .collect()
}

/// `compaction.keepRecentTokens` -> `Compaction: keep recent tokens`.
fn humanize(key: &str) -> String {
    let joined = key
        .split('.')
        .map(|part| {
            let mut out = String::new();
            for (i, c) in part.chars().enumerate() {
                if c.is_uppercase() && i > 0 {
                    out.push(' ');
                    out.extend(c.to_lowercase());
                } else {
                    out.push(c);
                }
            }
            out
        })
        .collect::<Vec<_>>()
        .join(": ");
    let mut chars = joined.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => joined,
    }
}

/// The value as shown in the row.
fn value_text(item: &SettingItem) -> String {
    match (&item.kind, &item.value) {
        (SettingKind::Bool, Value::Bool(true)) => "on".to_string(),
        (SettingKind::Bool, Value::Bool(false)) => "off".to_string(),
        (_, Value::Null) => "unset".to_string(),
        (_, Value::String(s)) => s.clone(),
        (_, other) => other.to_string(),
    }
}

/// `/settings`: a searchable list; each change takes effect at once and is reported through
/// [`SettingsSelector::take_changes`] (the app applies and saves it; search, cycle, save to
/// settings = acceptance of plan D7). Escape closes: `Done(())`.
pub struct SettingsSelector {
    items: Vec<SettingItem>,
    list: SelectList,
    search: String,
    changes: Vec<SettingChange>,
    /// Number or text row being edited: (key, buffer).
    edit: Option<(String, String)>,
    /// Choice row with more than [`SUBMENU_ABOVE`] values: (key, values list).
    submenu: Option<(String, SelectList)>,
    hint: String,
}

impl SettingsSelector {
    pub fn new(items: Vec<SettingItem>) -> SettingsSelector {
        let mut selector = SettingsSelector {
            items,
            list: SelectList::new(Vec::new(), MAX_VISIBLE),
            search: String::new(),
            changes: Vec::new(),
            edit: None,
            submenu: None,
            hint: hint_text(&Keymap::defaults()),
        };
        selector.rebuild(false);
        selector
    }

    /// Rows from `items` filtered by the search text. `keep` keeps the selected key; otherwise
    /// the selection goes to the top.
    fn rebuild(&mut self, keep: bool) {
        let previous = if keep {
            self.list.selected().map(|item| item.value.clone())
        } else {
            None
        };
        let items: Vec<SelectItem> = self
            .items
            .iter()
            .map(|item| SelectItem {
                value: item.key.clone(),
                label: item.label.clone(),
                description: Some(value_text(item)),
            })
            .collect();
        self.list.set_items(items);
        self.list.set_filter(&self.search);
        if let Some(key) = previous {
            self.select_key(&key);
        }
    }

    /// Move the selection onto the row of `key` when it is shown; the top row otherwise.
    fn select_key(&mut self, key: &str) {
        for i in 0..self.list.len() {
            self.list.set_selected_index(i);
            if self.list.selected().is_some_and(|item| item.value == key) {
                return;
            }
        }
        self.list.set_selected_index(0);
    }

    fn set_value(&mut self, key: &str, value: Value) {
        if let Some(item) = self.items.iter_mut().find(|item| item.key == key) {
            item.value = value.clone();
            self.changes.push(SettingChange {
                key: key.to_string(),
                value,
            });
        }
        self.rebuild(true);
    }

    /// Enter / space on the selected row.
    fn activate(&mut self) -> Outcome<()> {
        let Some(key) = self.list.selected().map(|item| item.value.clone()) else {
            return Outcome::Ignored;
        };
        let Some(item) = self.items.iter().find(|item| item.key == key).cloned() else {
            return Outcome::Ignored;
        };
        match &item.kind {
            SettingKind::Bool => {
                let next = !matches!(item.value, Value::Bool(true));
                self.set_value(&key, Value::Bool(next));
            }
            SettingKind::Choice(values) => {
                if values.is_empty() {
                    return Outcome::Ignored;
                }
                let current = value_text(&item);
                if values.len() > SUBMENU_ABOVE {
                    let items: Vec<SelectItem> = values
                        .iter()
                        .map(|v| SelectItem {
                            value: v.clone(),
                            label: v.clone(),
                            description: None,
                        })
                        .collect();
                    let mut sub = SelectList::new(items, MAX_VISIBLE);
                    let position = values.iter().position(|v| *v == current).unwrap_or(0);
                    sub.set_selected_index(position);
                    self.submenu = Some((key, sub));
                } else {
                    let next = match values.iter().position(|v| *v == current) {
                        Some(i) => values[(i + 1) % values.len()].clone(),
                        None => values[0].clone(),
                    };
                    self.set_value(&key, Value::String(next));
                }
            }
            SettingKind::Number | SettingKind::Text => {
                let buffer = match &item.value {
                    Value::Number(n) => n.to_string(),
                    Value::String(s) => s.clone(),
                    _ => String::new(),
                };
                self.edit = Some((key, buffer));
            }
        }
        Outcome::Pending
    }

    /// The value the edit buffer of `key` stands for, `None` when the text is not valid.
    /// Empty text clears a number or text setting (`null`).
    fn parse_edit(&self, key: &str, buffer: &str) -> Option<Value> {
        let item = self.items.iter().find(|item| item.key == key)?;
        match item.kind {
            SettingKind::Number => {
                if buffer.is_empty() {
                    Some(Value::Null)
                } else if buffer.chars().all(|c| c.is_ascii_digit()) {
                    buffer.parse::<u64>().ok().map(Value::from)
                } else {
                    None
                }
            }
            SettingKind::Text => Some(if buffer.is_empty() {
                Value::Null
            } else {
                Value::String(buffer.to_string())
            }),
            _ => None,
        }
    }

    /// Keys while a number or text row is being edited. Escape drops the edit (the selector
    /// stays open); Enter commits when the text is valid.
    fn edit_key(&mut self, key: &Key, keymap: &Keymap) -> Outcome<()> {
        let Some((name, mut buffer)) = self.edit.take() else {
            return Outcome::Ignored;
        };
        if keymap.matches(&key.id, "tui.select.cancel") {
            return Outcome::Pending;
        }
        if keymap.matches(&key.id, "tui.select.confirm") {
            match self.parse_edit(&name, &buffer) {
                Some(value) => self.set_value(&name, value),
                None => self.edit = Some((name, buffer)),
            }
            return Outcome::Pending;
        }
        let number = self
            .items
            .iter()
            .any(|item| item.key == name && item.kind == SettingKind::Number);
        let mut handled = true;
        if key.is("backspace") {
            buffer.pop();
        } else if let Some(text) = printable_text(key.text.as_deref()) {
            if number && !text.chars().all(|c| c.is_ascii_digit()) {
                handled = false;
            } else {
                buffer.push_str(text);
            }
        } else {
            handled = false;
        }
        self.edit = Some((name, buffer));
        if handled {
            Outcome::Pending
        } else {
            Outcome::Ignored
        }
    }

    pub fn key(&mut self, key: &Key, keymap: &Keymap) -> Outcome<()> {
        if key.kind == KeyKind::Release {
            return Outcome::Ignored;
        }
        self.hint = hint_text(keymap);
        if self.edit.is_some() {
            return self.edit_key(key, keymap);
        }
        if let Some((name, mut sub)) = self.submenu.take() {
            return match sub.key(key, keymap) {
                SelectAction::Selected(value) => {
                    self.set_value(&name, Value::String(value));
                    Outcome::Pending
                }
                SelectAction::Cancelled => Outcome::Pending,
                SelectAction::Moved => {
                    self.submenu = Some((name, sub));
                    Outcome::Pending
                }
                SelectAction::Ignored => {
                    self.submenu = Some((name, sub));
                    Outcome::Ignored
                }
            };
        }
        if keymap.matches(&key.id, "tui.select.cancel") {
            return Outcome::Done(());
        }
        if keymap.matches(&key.id, "tui.select.confirm") || key.is("space") {
            return self.activate();
        }
        if let SelectAction::Moved = self.list.key(key, keymap) {
            return Outcome::Pending;
        }
        if key.is("backspace") {
            return if self.search.pop().is_some() {
                self.rebuild(false);
                Outcome::Pending
            } else {
                Outcome::Ignored
            };
        }
        if let Some(text) = printable_text(key.text.as_deref()) {
            self.search.push_str(text);
            self.rebuild(false);
            return Outcome::Pending;
        }
        Outcome::Ignored
    }

    pub fn paste(&mut self, text: &str) {
        let clean: String = text.chars().filter(|c| !c.is_control()).collect();
        if let Some((name, buffer)) = self.edit.as_mut() {
            let number = self
                .items
                .iter()
                .any(|item| item.key == *name && item.kind == SettingKind::Number);
            if !number || clean.chars().all(|c| c.is_ascii_digit()) {
                buffer.push_str(&clean);
            }
            return;
        }
        self.search.push_str(&clean);
        self.rebuild(false);
    }

    /// Changes since the last call, in the order made.
    pub fn take_changes(&mut self) -> Vec<SettingChange> {
        std::mem::take(&mut self.changes)
    }

    pub fn render(&mut self, width: u16, theme: &Theme) -> Vec<String> {
        let w = width as usize;
        let mut lines = vec![theme.fg("accent", &truncate_to_width("Settings", w, ""))];
        lines.push(theme.fg(
            "muted",
            &truncate_to_width(&format!("search: {}", self.search), w, ""),
        ));
        if let Some((name, buffer)) = &self.edit {
            let label = self
                .items
                .iter()
                .find(|item| item.key == *name)
                .map(|item| item.label.clone())
                .unwrap_or_else(|| name.clone());
            lines.push(truncate_to_width(&format!("  {label}: {buffer}_"), w, ""));
        }
        match self.submenu.as_mut() {
            Some((_, sub)) => lines.extend(sub.render(width, theme)),
            None => lines.extend(self.list.render(width, theme)),
        }
        if self
            .list
            .selected()
            .is_some_and(|item| item.value == "oxa.local.vCacheThp")
        {
            for text in [
                "THP on: ~1% faster generation; ~1.2-1.4 GiB more RAM.",
                "Default off. Applies on next model load; unload/reload first.",
            ] {
                lines.push(theme.fg("muted", &truncate_to_width(text, w, "")));
            }
        }
        lines.push(theme.fg("muted", &truncate_to_width(&self.hint, w, "")));
        lines
    }
}

/// The footer hint of `/settings`, in the keys of `keymap`.
fn hint_text(keymap: &Keymap) -> String {
    format!(
        "{} change · {} close",
        key_label(keymap, "tui.select.confirm"),
        key_label(keymap, "tui.select.cancel")
    )
}

#[cfg(test)]
mod v_cache_thp_tests {
    use super::*;

    #[test]
    fn shared_toggle_is_user_only_and_renders_tradeoff_and_reload_notice() {
        let settings = Settings::from_values(
            serde_json::json!({}),
            serde_json::json!({"oxa":{"local":{"vCacheThp":true}}}),
        );
        let items = setting_items(&settings);
        let toggle = items
            .iter()
            .find(|item| item.key == "oxa.local.vCacheThp")
            .unwrap();
        assert_eq!(toggle.value, Value::Bool(false));
        let mut selector = SettingsSelector::new(items);
        selector.select_key("oxa.local.vCacheThp");
        let text = selector.render(120, &Theme::plain()).join("\n");
        assert!(text.contains("1.2-1.4 GiB"));
        assert!(text.contains("next model load"));
    }
}
