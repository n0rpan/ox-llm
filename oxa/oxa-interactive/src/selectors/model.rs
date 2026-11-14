//! D7: model selector and scoped-models editor. Owner: task D7. Signatures below are frozen; bodies are yours.
// ported from pi packages/coding-agent/src/modes/interactive/components/{model-selector.ts,scoped-models-selector.ts} and model-search.ts
use crate::Outcome;
use oxa_config::Keymap;
use oxa_tui::{
    fuzzy_filter, truncate_to_width, Component, Key, KeyKind, SelectAction, SelectItem, SelectList, Theme,
};

/// Rows shown at once by the model lists.
const MAX_VISIBLE: usize = 10;

/// One selectable model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelChoice {
    /// The id sent to the engine.
    pub id: String,
    /// Endpoint name from models.json (`laptop`, `lan`), shown dimmed.
    pub endpoint: String,
    pub n_ctx: Option<u32>,
    /// `true` when the endpoint has it loaded now (others need a load = slow first reply).
    pub loaded: bool,
}

/// `/model`: a search field over a list; typing filters (fuzzy over id and endpoint), Enter
/// selects, Escape cancels, Tab toggles between all models and the scoped list (when one is
/// set). The current model is marked. Result: the model id. A model switch is a new prefix
/// epoch; the selector's footer line says so ("switching model re-reads the context").
pub struct ModelSelector {
    models: Vec<ModelChoice>,
    current: String,
    scoped: Vec<String>,
    /// `true` while Tab has selected the scoped list.
    scope_on: bool,
    search: String,
    list: SelectList,
    /// The result was chosen with `app.models.save` (also save it as the default model).
    save: bool,
}

/// `"ctx 32768 · loaded"`-style description of a row.
fn describe(m: &ModelChoice) -> String {
    let mut parts = vec![m.endpoint.clone()];
    if let Some(n) = m.n_ctx {
        parts.push(format!("ctx {n}"));
    }
    parts.push(if m.loaded {
        "loaded".to_string()
    } else {
        "not loaded".to_string()
    });
    parts.join(" · ")
}

impl ModelSelector {
    pub fn new(models: Vec<ModelChoice>, current: &str, scoped: &[String]) -> ModelSelector {
        let mut selector = ModelSelector {
            models,
            current: current.to_string(),
            scoped: scoped.to_vec(),
            scope_on: false,
            search: String::new(),
            list: SelectList::new(Vec::new(), MAX_VISIBLE),
            save: false,
        };
        selector.rebuild();
        selector.select_current();
        selector
    }

    /// `true` when the model was chosen with `app.models.save`: the caller also writes it as
    /// `defaultModel` (pi model-selector.ts `onSelectAsDefault`).
    pub fn saves_default(&self) -> bool {
        self.save
    }

    /// The models the list is drawn from: all of them, or the scoped ones after Tab.
    fn pool(&self) -> Vec<&ModelChoice> {
        self.models
            .iter()
            .filter(|m| !self.scope_on || self.scoped.contains(&m.id))
            .collect()
    }

    /// Recompute the rows from the pool and the search text; the selection goes to the top.
    fn rebuild(&mut self) {
        let items: Vec<SelectItem> = {
            let pool = self.pool();
            let texts: Vec<String> = pool.iter().map(|m| format!("{} {}", m.id, m.endpoint)).collect();
            fuzzy_filter(&texts, &self.search)
                .into_iter()
                .map(|i| {
                    let m = pool[i];
                    let mark = if m.id == self.current { "✓ " } else { "  " };
                    SelectItem {
                        value: m.id.clone(),
                        label: format!("{mark}{}", m.id),
                        description: Some(describe(m)),
                    }
                })
                .collect()
        };
        self.list.set_items(items);
        self.list.set_selected_index(0);
    }

    /// Move the selection onto the current model when it is shown; the top row otherwise.
    fn select_current(&mut self) {
        for i in 0..self.list.len() {
            self.list.set_selected_index(i);
            if self
                .list
                .selected()
                .is_some_and(|item| item.value == self.current)
            {
                return;
            }
        }
        self.list.set_selected_index(0);
    }

    pub fn key(&mut self, key: &Key, keymap: &Keymap) -> Outcome<String> {
        if key.kind == KeyKind::Release {
            return Outcome::Ignored;
        }
        if keymap.matches(&key.id, "tui.select.cancel") {
            return Outcome::Cancelled;
        }
        if keymap.matches(&key.id, "app.models.save") {
            return match self.list.selected() {
                Some(item) => {
                    self.save = true;
                    Outcome::Done(item.value.clone())
                }
                None => Outcome::Ignored,
            };
        }
        if key.is("tab") {
            if self.scoped.is_empty() {
                return Outcome::Ignored;
            }
            self.scope_on = !self.scope_on;
            self.rebuild();
            self.select_current();
            return Outcome::Pending;
        }
        match self.list.key(key, keymap) {
            SelectAction::Selected(id) => return Outcome::Done(id),
            SelectAction::Cancelled => return Outcome::Cancelled,
            SelectAction::Moved => return Outcome::Pending,
            SelectAction::Ignored => {}
        }
        if key.is("backspace") {
            return if self.search.pop().is_some() {
                self.rebuild();
                Outcome::Pending
            } else {
                Outcome::Ignored
            };
        }
        if let Some(text) = printable_text(key.text.as_deref()) {
            self.search.push_str(text);
            self.rebuild();
            return Outcome::Pending;
        }
        Outcome::Ignored
    }

    pub fn paste(&mut self, text: &str) {
        let clean: String = text.chars().filter(|c| !c.is_control()).collect();
        self.search.push_str(&clean);
        self.rebuild();
    }

    pub fn render(&mut self, width: u16, theme: &Theme) -> Vec<String> {
        let w = width as usize;
        let title = if self.scope_on {
            "Select model (scoped)"
        } else {
            "Select model"
        };
        let mut lines = vec![theme.fg("accent", &truncate_to_width(title, w, ""))];
        lines.push(theme.fg(
            "muted",
            &truncate_to_width(&format!("search: {}", self.search), w, ""),
        ));
        lines.extend(self.list.render(width, theme));
        let mut footer = "switching model re-reads the context".to_string();
        if !self.scoped.is_empty() {
            footer.push_str(if self.scope_on {
                " · tab: all models"
            } else {
                " · tab: scoped models"
            });
        }
        lines.push(theme.fg("muted", &truncate_to_width(&footer, w, "")));
        lines
    }
}

/// One row of the scoped-models editor: the model id, its endpoint (`""` when the pattern names
/// a model that is not available now) and whether Ctrl+P cycles through it.
struct ScopedRow {
    id: String,
    endpoint: String,
    enabled: bool,
}

/// `/scoped-models`: toggle which models Ctrl+P cycles through. pi's editor keys: space/Enter
/// toggles, the `app.models.*` actions of keybindings.md enable all / clear all / reorder /
/// save. Result: the ordered list of enabled model patterns and whether to persist it
/// (`true` = save to settings, `false` = this session only).
pub struct ScopedModelsSelector {
    rows: Vec<ScopedRow>,
    list: SelectList,
    dirty: bool,
    hint: String,
}

impl ScopedModelsSelector {
    pub fn new(models: Vec<ModelChoice>, enabled: &[String]) -> ScopedModelsSelector {
        let mut rows: Vec<ScopedRow> = Vec::new();
        // Enabled patterns first, in their order (a pattern naming no available model is kept
        // and shown as unavailable); then every other model, disabled.
        for id in enabled {
            if rows.iter().any(|r| &r.id == id) {
                continue;
            }
            let endpoint = models
                .iter()
                .find(|m| &m.id == id)
                .map(|m| m.endpoint.clone())
                .unwrap_or_default();
            rows.push(ScopedRow {
                id: id.clone(),
                endpoint,
                enabled: true,
            });
        }
        for m in &models {
            if rows.iter().any(|r| r.id == m.id) {
                continue;
            }
            rows.push(ScopedRow {
                id: m.id.clone(),
                endpoint: m.endpoint.clone(),
                enabled: false,
            });
        }
        let mut selector = ScopedModelsSelector {
            rows,
            list: SelectList::new(Vec::new(), MAX_VISIBLE),
            dirty: false,
            hint: hint_text(&Keymap::defaults()),
        };
        selector.rebuild(0);
        selector
    }

    /// Rows from `rows`, the cursor moved to `select`.
    fn rebuild(&mut self, select: usize) {
        let items: Vec<SelectItem> = self
            .rows
            .iter()
            .map(|r| SelectItem {
                value: r.id.clone(),
                label: format!("{} {}", if r.enabled { "[x]" } else { "[ ]" }, r.id),
                description: Some(if r.endpoint.is_empty() {
                    "unavailable".to_string()
                } else {
                    r.endpoint.clone()
                }),
            })
            .collect();
        self.list.set_items(items);
        self.list.set_selected_index(select);
    }

    /// The enabled patterns in list order.
    fn result(&self) -> Vec<String> {
        self.rows
            .iter()
            .filter(|r| r.enabled)
            .map(|r| r.id.clone())
            .collect()
    }

    fn toggle(&mut self, index: usize) {
        if let Some(row) = self.rows.get_mut(index) {
            row.enabled = !row.enabled;
            self.dirty = true;
            self.rebuild(index);
        }
    }

    pub fn key(&mut self, key: &Key, keymap: &Keymap) -> Outcome<(Vec<String>, bool)> {
        if key.kind == KeyKind::Release {
            return Outcome::Ignored;
        }
        self.hint = hint_text(keymap);
        if keymap.matches(&key.id, "app.models.save") {
            return Outcome::Done((self.result(), true));
        }
        if keymap.matches(&key.id, "tui.select.cancel") {
            // Changes made in this session are not lost on Escape: they apply for this session.
            return if self.dirty {
                Outcome::Done((self.result(), false))
            } else {
                Outcome::Cancelled
            };
        }
        let cursor = self.list.selected_index();
        if keymap.matches(&key.id, "app.models.enableAll") || keymap.matches(&key.id, "app.models.clearAll") {
            let on = keymap.matches(&key.id, "app.models.enableAll");
            for row in &mut self.rows {
                row.enabled = on;
            }
            self.dirty = true;
            self.rebuild(cursor);
            return Outcome::Pending;
        }
        if keymap.matches(&key.id, "app.models.toggleProvider") {
            let Some(endpoint) = self.rows.get(cursor).map(|r| r.endpoint.clone()) else {
                return Outcome::Ignored;
            };
            let group: Vec<usize> = (0..self.rows.len())
                .filter(|&i| self.rows[i].endpoint == endpoint)
                .collect();
            let all_on = group.iter().all(|&i| self.rows[i].enabled);
            for i in group {
                self.rows[i].enabled = !all_on;
            }
            self.dirty = true;
            self.rebuild(cursor);
            return Outcome::Pending;
        }
        let reorder_up = keymap.matches(&key.id, "app.models.reorderUp");
        if reorder_up || keymap.matches(&key.id, "app.models.reorderDown") {
            if cursor >= self.rows.len() {
                return Outcome::Ignored;
            }
            let target = if reorder_up {
                cursor.checked_sub(1)
            } else {
                Some(cursor + 1)
            };
            return match target.filter(|&t| t < self.rows.len()) {
                Some(t) => {
                    self.rows.swap(cursor, t);
                    self.dirty = true;
                    self.rebuild(t);
                    Outcome::Pending
                }
                None => Outcome::Ignored,
            };
        }
        if key.is("space") {
            self.toggle(cursor);
            return Outcome::Pending;
        }
        match self.list.key(key, keymap) {
            SelectAction::Selected(_) => {
                self.toggle(cursor);
                Outcome::Pending
            }
            SelectAction::Moved => Outcome::Pending,
            SelectAction::Cancelled => Outcome::Cancelled,
            SelectAction::Ignored => Outcome::Ignored,
        }
    }

    pub fn paste(&mut self, text: &str) {
        // No search field in this editor: pasted text is ignored.
        let _ = text;
    }

    pub fn render(&mut self, width: u16, theme: &Theme) -> Vec<String> {
        let w = width as usize;
        let enabled = self.rows.iter().filter(|r| r.enabled).count();
        let mut lines = vec![theme.fg(
            "accent",
            &truncate_to_width("Model configuration (session only until saved)", w, ""),
        )];
        lines.extend(self.list.render(width, theme));
        let status = format!(
            "{enabled}/{} enabled{}",
            self.rows.len(),
            if self.dirty { " (unsaved)" } else { "" }
        );
        // The status gets its own line: the key hint is cut at narrow widths.
        lines.push(theme.fg("muted", &truncate_to_width(&status, w, "")));
        lines.push(theme.fg("muted", &truncate_to_width(&self.hint, w, "")));
        lines
    }
}

/// The cycle order of the model keys: the neighbour of `current` in `scoped` (or in `all` when
/// `scoped` is empty), wrapping. `None` when there is at most one candidate.
pub fn cycle_model(all: &[String], scoped: &[String], current: &str, forward: bool) -> Option<String> {
    let list = if scoped.is_empty() { all } else { scoped };
    if list.len() < 2 {
        return None;
    }
    let n = list.len();
    let next = match list.iter().position(|m| m == current) {
        Some(i) if forward => (i + 1) % n,
        Some(i) => (i + n - 1) % n,
        None if forward => 0,
        None => n - 1,
    };
    Some(list[next].clone())
}

/// The text a printable key inserts: `None` for non-printing keys and control characters.
pub(crate) fn printable_text(text: Option<&str>) -> Option<&str> {
    text.filter(|t| !t.is_empty() && !t.chars().any(char::is_control))
}

/// The labels of the keys bound to `action`, `/`-separated; `unbound` when there are none.
pub(crate) fn key_label(keymap: &Keymap, action: &str) -> String {
    let keys: Vec<String> = keymap.keys_for(action).iter().map(|k| k.to_string()).collect();
    if keys.is_empty() {
        "unbound".to_string()
    } else {
        keys.join("/")
    }
}

/// The footer hint of the scoped-models editor, in the keys of `keymap`.
fn hint_text(keymap: &Keymap) -> String {
    let parts = [
        format!("{} toggle", key_label(keymap, "tui.select.confirm")),
        format!("{} all", key_label(keymap, "app.models.enableAll")),
        format!("{} clear", key_label(keymap, "app.models.clearAll")),
        format!("{} provider", key_label(keymap, "app.models.toggleProvider")),
        format!(
            "{}/{} reorder",
            key_label(keymap, "app.models.reorderUp"),
            key_label(keymap, "app.models.reorderDown")
        ),
        format!("{} save", key_label(keymap, "app.models.save")),
    ];
    parts.join(" · ")
}
