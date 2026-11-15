//! F7: the `/mcp` selector.
// ported from pi packages/coding-agent/src/modes/interactive/components/config-selector.ts (MCP section) and docs/mcp.md
use crate::Outcome;
use oxa_config::Keymap;
use oxa_mcp::{ConfigScope, McpManager, ServerState};
use oxa_proto::{codes, Exposure, OxaError};
use oxa_tui::{truncate_to_width, Component, Key, KeyKind, SelectAction, SelectItem, SelectList, Theme};
use std::path::PathBuf;

/// Rows shown at once.
const MAX_VISIBLE: usize = 10;
/// Lines of a connection error shown under the list.
const MAX_ERROR_LINES: usize = 5;
/// Keys of the selector (besides the list keys of the keymap).
const HINT: &str = "enter reconnect or enable · e enable/disable · x exposure · esc close";

/// Connection state of a server as shown in the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpRowState {
    Disabled,
    Connecting,
    Connected,
    /// Needs `oxa mcp login <name>`.
    NeedsAuth,
    Failed(String),
}

/// One row of `/mcp`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerRow {
    pub name: String,
    pub state: McpRowState,
    pub exposure: Exposure,
    pub tool_count: usize,
    /// `stdio` or `http`.
    pub transport: String,
    /// The mcp.json that defines the server (changes are saved there).
    pub defined_in: PathBuf,
    /// `true` when defined in the project's mcp.json.
    pub project: bool,
}

/// What the selector asks the app to do; the selector stays open and is refreshed with
/// [`McpSelector::set_rows`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpRequest {
    Reconnect(String),
    SetEnabled {
        server: String,
        enabled: bool,
    },
    /// Saved to the defining file. A change to or from `Direct` takes effect at the next
    /// prefix epoch (W3: the tool list is frozen); the selector says so.
    SetExposure {
        server: String,
        exposure: Exposure,
    },
}

/// `/mcp`: the server list in pi's order (servers that need attention first, then the others;
/// each group by name), with reconnect, enable/disable and exposure cycling (`codemode` ->
/// `deferred` -> `direct` -> `hidden`). Escape closes (`Cancelled`); each request is an
/// `Outcome::Done` that the app carries out without closing the selector.
pub struct McpSelector {
    list: SelectList,
    /// The rows in list order; the list's items are the same rows in the same order.
    rows: Vec<McpServerRow>,
    /// What the last request did, shown under the list (`saved to ...`).
    notice: Option<String>,
}

impl McpSelector {
    pub fn new(rows: Vec<McpServerRow>) -> McpSelector {
        let mut rows = rows;
        sort_rows(&mut rows);
        let mut list = SelectList::new(items_of(&rows), MAX_VISIBLE);
        list.set_empty_text("No MCP servers configured.");
        McpSelector {
            list,
            rows,
            notice: None,
        }
    }

    /// Replace the rows (after the app carried out a request). The cursor stays on the same
    /// server when it is still listed.
    pub fn set_rows(&mut self, rows: Vec<McpServerRow>) {
        let mut rows = rows;
        sort_rows(&mut rows);
        let current = self.selected_row().map(|row| row.name.clone());
        self.list.set_items(items_of(&rows));
        if let Some(pos) = current.and_then(|name| rows.iter().position(|row| row.name == name)) {
            self.list.set_selected_index(pos);
        }
        self.rows = rows;
    }

    pub fn key(&mut self, key: &Key, keymap: &Keymap) -> Outcome<McpRequest> {
        if key.kind == KeyKind::Release {
            return Outcome::Ignored;
        }
        match self.list.key(key, keymap) {
            SelectAction::Cancelled => return Outcome::Cancelled,
            SelectAction::Moved => return Outcome::Pending,
            SelectAction::Selected(name) => {
                let Some(row) = self.row(&name) else {
                    return Outcome::Ignored;
                };
                // Enter reconnects a server that is on, and turns on one that is off.
                return if row.state == McpRowState::Disabled {
                    self.toggle(&row)
                } else {
                    self.reconnect(&row)
                };
            }
            SelectAction::Ignored => {}
        }
        let Some(row) = self.selected_row() else {
            return Outcome::Ignored;
        };
        if key.is("r") {
            self.reconnect(&row)
        } else if key.is("e") || key.is("space") {
            self.toggle(&row)
        } else if key.is("x") {
            self.cycle(&row)
        } else {
            Outcome::Ignored
        }
    }

    pub fn render(&mut self, width: u16, theme: &Theme) -> Vec<String> {
        let w = width as usize;
        let mut lines = vec![theme.fg("accent", &truncate_to_width("MCP servers", w, ""))];
        lines.extend(self.list.render(width, theme));
        if let Some(row) = self.selected_row() {
            let source = if row.project { "project" } else { "user" };
            let details = [
                format!("State: {}", state_text(&row.state)),
                format!(
                    "{} · exposure {} · {} · {}",
                    tools_text(row.tool_count),
                    row.exposure.as_str(),
                    row.transport,
                    source
                ),
                format!("Defined in {}", row.defined_in.display()),
            ];
            for detail in details {
                lines.push(theme.fg("muted", &truncate_to_width(&detail, w, "")));
            }
            match &row.state {
                McpRowState::Failed(message) => {
                    for line in message
                        .lines()
                        .filter(|l| !l.trim().is_empty())
                        .take(MAX_ERROR_LINES)
                    {
                        lines.push(theme.fg("error", &truncate_to_width(line, w, "")));
                    }
                }
                McpRowState::NeedsAuth => {
                    let text = format!("Needs sign-in: oxa mcp login {}", row.name);
                    lines.push(theme.fg("warning", &truncate_to_width(&text, w, "")));
                }
                _ => {}
            }
        }
        if let Some(notice) = &self.notice {
            for line in notice.lines() {
                lines.push(theme.fg("accent", &truncate_to_width(line, w, "")));
            }
        }
        lines.push(theme.fg("dim", &truncate_to_width(HINT, w, "")));
        lines
    }

    fn row(&self, name: &str) -> Option<McpServerRow> {
        self.rows.iter().find(|row| row.name == name).cloned()
    }

    fn selected_row(&self) -> Option<McpServerRow> {
        self.rows.get(self.list.selected_index()).cloned()
    }

    fn reconnect(&mut self, row: &McpServerRow) -> Outcome<McpRequest> {
        if row.state == McpRowState::Disabled {
            self.notice = Some(format!("{} is disabled; press e to enable it", row.name));
            return Outcome::Pending;
        }
        self.notice = Some(format!("reconnecting {}", row.name));
        Outcome::Done(McpRequest::Reconnect(row.name.clone()))
    }

    fn toggle(&mut self, row: &McpServerRow) -> Outcome<McpRequest> {
        let enabled = row.state == McpRowState::Disabled;
        let verb = if enabled { "enabled" } else { "disabled" };
        self.notice = Some(format!(
            "{} {}: saved to {}",
            verb,
            row.name,
            row.defined_in.display()
        ));
        Outcome::Done(McpRequest::SetEnabled {
            server: row.name.clone(),
            enabled,
        })
    }

    fn cycle(&mut self, row: &McpServerRow) -> Outcome<McpRequest> {
        let exposure = next_exposure(row.exposure);
        let mut notice = format!(
            "exposure of {} is {}: saved to {}",
            row.name,
            exposure.as_str(),
            row.defined_in.display()
        );
        if row.exposure == Exposure::Direct || exposure == Exposure::Direct {
            notice.push_str("\ndirect tools take effect at the next prefix epoch (/reload)");
        }
        self.notice = Some(notice);
        Outcome::Done(McpRequest::SetExposure {
            server: row.name.clone(),
            exposure,
        })
    }
}

/// The next exposure in the `/mcp` cycle.
fn next_exposure(exposure: Exposure) -> Exposure {
    match exposure {
        Exposure::Codemode => Exposure::Deferred,
        Exposure::Deferred => Exposure::Direct,
        Exposure::Direct => Exposure::Hidden,
        Exposure::Hidden => Exposure::Codemode,
    }
}

/// Position of a row in the list: servers that need attention first (sign-in, failure),
/// then starting ones, connected ones and disabled ones (pi's `attentionRank`).
fn attention_rank(state: &McpRowState) -> u8 {
    match state {
        McpRowState::NeedsAuth => 0,
        McpRowState::Failed(_) => 1,
        McpRowState::Connecting => 3,
        McpRowState::Connected => 4,
        McpRowState::Disabled => 5,
    }
}

/// Sort rows into list order: by attention rank, then by name.
fn sort_rows(rows: &mut [McpServerRow]) {
    rows.sort_by(|a, b| {
        attention_rank(&a.state)
            .cmp(&attention_rank(&b.state))
            .then_with(|| a.name.cmp(&b.name))
    });
}

fn state_text(state: &McpRowState) -> String {
    match state {
        McpRowState::Disabled => "disabled".to_string(),
        McpRowState::Connecting => "connecting".to_string(),
        McpRowState::Connected => "connected".to_string(),
        McpRowState::NeedsAuth => "needs sign-in".to_string(),
        McpRowState::Failed(message) => {
            let first = message
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .unwrap_or("unknown error");
            format!("failed: {first}")
        }
    }
}

fn tools_text(count: usize) -> String {
    if count == 1 {
        "1 tool".to_string()
    } else {
        format!("{count} tools")
    }
}

/// One list item per row: the name, and the state, exposure, tool count and source.
fn items_of(rows: &[McpServerRow]) -> Vec<SelectItem> {
    rows.iter()
        .map(|row| {
            let source = if row.project { "project" } else { "user" };
            SelectItem {
                value: row.name.clone(),
                label: row.name.clone(),
                description: Some(format!(
                    "{} · {} · {} · {}",
                    state_text(&row.state),
                    row.exposure.as_str(),
                    tools_text(row.tool_count),
                    source
                )),
            }
        })
        .collect()
}

/// The rows for the manager's current state, in list order.
pub fn mcp_rows(manager: &McpManager) -> Vec<McpServerRow> {
    let mut rows: Vec<McpServerRow> = manager
        .servers()
        .into_iter()
        .map(|status| {
            let state = match status.state {
                ServerState::Disabled => McpRowState::Disabled,
                ServerState::Pending | ServerState::Connecting => McpRowState::Connecting,
                ServerState::Connected => McpRowState::Connected,
                ServerState::NeedsAuth => McpRowState::NeedsAuth,
                ServerState::Denied => McpRowState::Failed("permission denied: not started".to_string()),
                ServerState::Failed(message) => McpRowState::Failed(message),
            };
            McpServerRow {
                name: status.config.name.clone(),
                state,
                exposure: status.config.exposure,
                tool_count: status.tools.len(),
                transport: match status.config.kind {
                    oxa_mcp::ServerKind::Stdio(_) => "stdio".to_string(),
                    oxa_mcp::ServerKind::Http(_) => "http".to_string(),
                },
                defined_in: status.saves_to.clone(),
                project: status.config.defined_in == ConfigScope::Project,
            }
        })
        .collect();
    sort_rows(&mut rows);
    rows
}

/// Carry out a request on the manager (reconnect in the background, persist enable/exposure
/// to the defining file). Errors: `mcp`, `io`, `not_found`.
pub fn apply_mcp_request(manager: &McpManager, request: &McpRequest) -> Result<(), OxaError> {
    match request {
        McpRequest::Reconnect(server) => manager.reconnect(server),
        McpRequest::SetEnabled { server, enabled } => {
            // The file that receives the change: the defining file, or the project override
            // of a user-level server once one exists.
            let status = manager
                .server(server)
                .ok_or_else(|| OxaError::new(codes::NOT_FOUND, format!("unknown MCP server: {server}")))?;
            let scope = if status.config.defined_in == ConfigScope::Project || status.config.project_override
            {
                ConfigScope::Project
            } else {
                ConfigScope::User
            };
            manager.set_enabled(server, *enabled, scope)
        }
        McpRequest::SetExposure { server, exposure } => manager.set_exposure(server, *exposure),
    }
}
