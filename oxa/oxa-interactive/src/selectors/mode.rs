//! #202: the `/mode` picker. Each permission mode is offered twice: for this session, and
//! "make default" (also written as `permissions.defaultMode`).
use crate::Outcome;
use oxa_config::Keymap;
use oxa_proto::PermissionMode;
use oxa_tui::{truncate_to_width, Component, Key, KeyKind, SelectAction, SelectItem, SelectList, Theme};

const MAX_VISIBLE: usize = 8;

/// The picker's rows: `(mode, make default)`.
fn rows() -> Vec<(PermissionMode, bool)> {
    let mut rows: Vec<(PermissionMode, bool)> = PermissionMode::ALL.iter().map(|&m| (m, false)).collect();
    rows.extend(PermissionMode::ALL.iter().map(|&m| (m, true)));
    rows
}

pub struct ModeSelector {
    rows: Vec<(PermissionMode, bool)>,
    list: SelectList,
}

impl ModeSelector {
    pub fn new(current: PermissionMode, os_sandbox: bool) -> ModeSelector {
        let rows = rows();
        let items = rows
            .iter()
            .enumerate()
            .map(|(i, &(mode, save))| SelectItem {
                value: i.to_string(),
                label: if save {
                    format!("  {} (make default)", mode.as_str())
                } else {
                    format!("{}{}", if mode == current { "✓ " } else { "  " }, mode.as_str())
                },
                description: Some(mode.describe_with(os_sandbox).to_string()),
            })
            .collect();
        let mut list = SelectList::new(items, MAX_VISIBLE);
        list.set_selected_index(rows.iter().position(|&(m, _)| m == current).unwrap_or(0));
        ModeSelector { rows, list }
    }

    /// `Done((mode, save))`: `save` asks to write the mode as the default.
    pub fn key(&mut self, key: &Key, keymap: &Keymap) -> Outcome<(PermissionMode, bool)> {
        if key.kind == KeyKind::Release {
            return Outcome::Ignored;
        }
        match self.list.key(key, keymap) {
            SelectAction::Selected(value) => value
                .parse::<usize>()
                .ok()
                .and_then(|i| self.rows.get(i).copied())
                .map_or(Outcome::Ignored, Outcome::Done),
            SelectAction::Cancelled => Outcome::Cancelled,
            SelectAction::Moved => Outcome::Pending,
            SelectAction::Ignored => Outcome::Ignored,
        }
    }

    pub fn render(&mut self, width: u16, theme: &Theme) -> Vec<String> {
        let w = width as usize;
        let mut lines = vec![theme.fg("accent", &truncate_to_width("Permission mode", w, ""))];
        lines.extend(self.list.render(width, theme));
        lines
    }
}
