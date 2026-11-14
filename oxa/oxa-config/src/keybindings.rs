//! B5: keybinding ids, defaults and keybindings.json. Owner: task B5. Signatures below are frozen; bodies are yours.
// ported from pi packages/tui/src/keybindings.ts and packages/coding-agent/docs/keybindings.md
use crate::{Diagnostic, Dirs};
use oxa_proto::{KeyId, Level, Value};

/// One bindable action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionDef {
    /// pi's id (`tui.editor.cursorUp`, `app.model.cycle`, ..) or oxa's `app.mode.cycle`.
    pub id: &'static str,
    /// Default keys in pi's notation.
    pub default_keys: &'static [&'static str],
    pub description: &'static str,
}

/// The active key map: action id -> keys. The same key may be bound to actions of different
/// contexts (editor vs selector); consumers ask [`Keymap::matches`] for the action they handle.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Keymap {
    bindings: Vec<(String, Vec<KeyId>)>,
}

/// Every action of keybindings.md on Linux (not Windows/WSL), in the document's order, then
/// oxa's `app.mode.cycle`. Defaults are pi's notation; `KeyId::parse` normalises them.
const ACTIONS: &[ActionDef] = &[
    // Terminal UI: cursor movement
    ActionDef {
        id: "tui.editor.cursorUp",
        default_keys: &["up"],
        description: "Move cursor up, browsing older history at the top",
    },
    ActionDef {
        id: "tui.editor.cursorDown",
        default_keys: &["down"],
        description: "Move cursor down, browsing newer history at the bottom",
    },
    ActionDef {
        id: "tui.editor.historyPrevious",
        default_keys: &[],
        description: "Select the previous prompt history entry",
    },
    ActionDef {
        id: "tui.editor.historyNext",
        default_keys: &[],
        description: "Select the next prompt history entry",
    },
    ActionDef {
        id: "tui.editor.cursorLeft",
        default_keys: &["left", "ctrl+b"],
        description: "Move cursor left",
    },
    ActionDef {
        id: "tui.editor.cursorRight",
        default_keys: &["right", "ctrl+f"],
        description: "Move cursor right",
    },
    ActionDef {
        id: "tui.editor.cursorWordLeft",
        default_keys: &["alt+left", "ctrl+left", "alt+b"],
        description: "Move cursor word left",
    },
    ActionDef {
        id: "tui.editor.cursorWordRight",
        default_keys: &["alt+right", "ctrl+right", "alt+f"],
        description: "Move cursor word right",
    },
    ActionDef {
        id: "tui.editor.cursorLineStart",
        default_keys: &["home", "ctrl+a"],
        description: "Move to line start",
    },
    ActionDef {
        id: "tui.editor.cursorLineEnd",
        default_keys: &["end", "ctrl+e"],
        description: "Move to line end",
    },
    ActionDef {
        id: "tui.editor.jumpForward",
        default_keys: &["ctrl+]"],
        description: "Jump forward to character",
    },
    ActionDef {
        id: "tui.editor.jumpBackward",
        default_keys: &["ctrl+alt+]"],
        description: "Jump backward to character",
    },
    ActionDef {
        id: "tui.editor.pageUp",
        default_keys: &["pageUp", "ctrl+pageUp"],
        description: "Scroll up by page",
    },
    ActionDef {
        id: "tui.editor.pageDown",
        default_keys: &["pageDown", "ctrl+pageDown"],
        description: "Scroll down by page",
    },
    // Terminal UI: text editing
    ActionDef {
        id: "tui.editor.deleteCharBackward",
        default_keys: &["backspace"],
        description: "Delete character backward",
    },
    ActionDef {
        id: "tui.editor.deleteCharForward",
        default_keys: &["delete", "ctrl+d"],
        description: "Delete character forward",
    },
    ActionDef {
        id: "tui.editor.deleteWordBackward",
        default_keys: &["ctrl+w", "alt+backspace"],
        description: "Delete word backward",
    },
    ActionDef {
        id: "tui.editor.deleteWordForward",
        default_keys: &["alt+d", "alt+delete"],
        description: "Delete word forward",
    },
    ActionDef {
        id: "tui.editor.deleteToLineStart",
        default_keys: &["ctrl+u"],
        description: "Delete to line start",
    },
    ActionDef {
        id: "tui.editor.deleteToLineEnd",
        default_keys: &["ctrl+k"],
        description: "Delete to line end",
    },
    ActionDef {
        id: "tui.editor.yank",
        default_keys: &["ctrl+y"],
        description: "Paste most recently deleted text",
    },
    ActionDef {
        id: "tui.editor.yankPop",
        default_keys: &["alt+y"],
        description: "Cycle through deleted text after yank",
    },
    ActionDef {
        id: "tui.editor.undo",
        default_keys: &["ctrl+-"],
        description: "Undo last edit",
    },
    // Terminal UI: input and selection
    ActionDef {
        id: "tui.input.newLine",
        default_keys: &["shift+enter", "ctrl+j"],
        description: "Insert new line",
    },
    ActionDef {
        id: "tui.input.submit",
        default_keys: &["enter"],
        description: "Submit input",
    },
    ActionDef {
        id: "tui.input.tab",
        default_keys: &["tab"],
        description: "Tab or autocomplete",
    },
    ActionDef {
        id: "tui.input.copy",
        default_keys: &["ctrl+c"],
        description: "Copy selection",
    },
    ActionDef {
        id: "tui.select.up",
        default_keys: &["up"],
        description: "Move selection up",
    },
    ActionDef {
        id: "tui.select.down",
        default_keys: &["down"],
        description: "Move selection down",
    },
    ActionDef {
        id: "tui.select.pageUp",
        default_keys: &["pageUp"],
        description: "Page up in list",
    },
    ActionDef {
        id: "tui.select.pageDown",
        default_keys: &["pageDown"],
        description: "Page down in list",
    },
    ActionDef {
        id: "tui.select.confirm",
        default_keys: &["enter"],
        description: "Confirm selection",
    },
    ActionDef {
        id: "tui.select.cancel",
        default_keys: &["escape", "ctrl+c"],
        description: "Cancel selection",
    },
    // Terminal UI: fullscreen
    ActionDef {
        id: "tui.altScreen.pageUp",
        default_keys: &["pageUp"],
        description: "Scroll the transcript up by one page",
    },
    ActionDef {
        id: "tui.altScreen.pageDown",
        default_keys: &["pageDown"],
        description: "Scroll the transcript down by one page",
    },
    ActionDef {
        id: "tui.altScreen.halfPageUp",
        default_keys: &[],
        description: "Scroll the transcript up by half a page",
    },
    ActionDef {
        id: "tui.altScreen.halfPageDown",
        default_keys: &[],
        description: "Scroll the transcript down by half a page",
    },
    ActionDef {
        id: "tui.altScreen.lineUp",
        default_keys: &[],
        description: "Scroll the transcript up by one line",
    },
    ActionDef {
        id: "tui.altScreen.lineDown",
        default_keys: &[],
        description: "Scroll the transcript down by one line",
    },
    ActionDef {
        id: "tui.altScreen.previousPrompt",
        default_keys: &["ctrl+shift+up", "ctrl+up"],
        description: "Jump to the previous marked message",
    },
    ActionDef {
        id: "tui.altScreen.nextPrompt",
        default_keys: &["ctrl+shift+down", "ctrl+down"],
        description: "Jump to the next marked message",
    },
    ActionDef {
        id: "tui.altScreen.search",
        // oxa: `alt+s` as well, which a terminal without the kitty keyboard protocol can send
        // (it sends ctrl+shift+f as ctrl+f, the cursor key).
        default_keys: &["ctrl+shift+f", "alt+s"],
        description: "Search the rendered transcript",
    },
    ActionDef {
        id: "tui.altScreen.searchNext",
        default_keys: &["enter", "ctrl+g"],
        description: "Select the next search match while searching",
    },
    ActionDef {
        id: "tui.altScreen.searchPrevious",
        default_keys: &["shift+enter", "ctrl+shift+g"],
        description: "Select the previous search match while searching",
    },
    ActionDef {
        id: "tui.altScreen.searchClose",
        default_keys: &["escape"],
        description: "Close transcript search",
    },
    ActionDef {
        id: "tui.altScreen.top",
        default_keys: &["ctrl+home"],
        description: "Scroll to the beginning of the transcript",
    },
    ActionDef {
        id: "tui.altScreen.bottom",
        default_keys: &["ctrl+end"],
        description: "Scroll to the transcript end and follow new output",
    },
    // Application
    ActionDef {
        id: "app.interrupt",
        default_keys: &["escape"],
        description: "Cancel / abort",
    },
    ActionDef {
        id: "app.clear",
        default_keys: &["ctrl+c"],
        description: "Clear editor (first) / exit (second)",
    },
    ActionDef {
        id: "app.exit",
        default_keys: &["ctrl+d"],
        description: "Exit (when editor empty)",
    },
    ActionDef {
        id: "app.suspend",
        default_keys: &["ctrl+z"],
        description: "Suspend to background",
    },
    ActionDef {
        id: "app.editor.external",
        default_keys: &["ctrl+g"],
        description: "Open in external editor",
    },
    ActionDef {
        id: "app.clipboard.pasteImage",
        default_keys: &["ctrl+v"],
        description: "Paste files on macOS, images, or text from clipboard",
    },
    // Sessions
    ActionDef {
        id: "app.session.new",
        default_keys: &[],
        description: "Start a new session (/new)",
    },
    ActionDef {
        id: "app.session.tree",
        default_keys: &[],
        description: "Open session tree navigator (/tree)",
    },
    ActionDef {
        id: "app.session.fork",
        default_keys: &[],
        description: "Fork current session (/fork)",
    },
    ActionDef {
        id: "app.session.resume",
        default_keys: &[],
        description: "Open session resume picker (/resume)",
    },
    ActionDef {
        id: "app.session.togglePath",
        default_keys: &["ctrl+p"],
        description: "Toggle path display",
    },
    ActionDef {
        id: "app.session.toggleSort",
        default_keys: &["ctrl+s"],
        description: "Toggle sort mode",
    },
    ActionDef {
        id: "app.session.toggleNamedFilter",
        default_keys: &["ctrl+n"],
        description: "Toggle named-only filter",
    },
    ActionDef {
        id: "app.session.rename",
        default_keys: &["ctrl+r"],
        description: "Rename session",
    },
    ActionDef {
        id: "app.session.delete",
        default_keys: &["ctrl+d"],
        description: "Delete session",
    },
    ActionDef {
        id: "app.session.deleteNoninvasive",
        default_keys: &["ctrl+backspace"],
        description: "Delete session when query is empty",
    },
    // Models and thinking
    ActionDef {
        id: "app.model.select",
        default_keys: &["ctrl+l"],
        description: "Open model selector",
    },
    ActionDef {
        id: "app.model.cycleForward",
        default_keys: &["ctrl+p"],
        description: "Cycle to next model",
    },
    ActionDef {
        id: "app.model.cycleBackward",
        default_keys: &["shift+ctrl+p"],
        description: "Cycle to previous model",
    },
    ActionDef {
        id: "app.models.save",
        default_keys: &["ctrl+s"],
        description: "Save the selected default model or scoped model configuration to settings",
    },
    ActionDef {
        id: "app.thinking.cycle",
        default_keys: &["shift+tab"],
        description: "Cycle thinking level",
    },
    ActionDef {
        id: "app.thinking.save",
        default_keys: &["ctrl+s"],
        description: "Save current thinking level to settings",
    },
    ActionDef {
        id: "app.thinking.toggle",
        default_keys: &["ctrl+t"],
        description: "Collapse or expand thinking blocks",
    },
    // Display and message queue
    ActionDef {
        id: "app.tools.expand",
        default_keys: &["ctrl+o"],
        description: "Collapse or expand tool output",
    },
    ActionDef {
        id: "app.message.copy",
        default_keys: &["ctrl+x"],
        description: "Copy the selected message, the active selection, or the last assistant message",
    },
    ActionDef {
        id: "app.message.followUp",
        default_keys: &["alt+enter"],
        description: "Queue follow-up message",
    },
    ActionDef {
        id: "app.message.dequeue",
        default_keys: &["alt+up"],
        description: "Restore queued messages to editor",
    },
    // Tree navigation
    ActionDef {
        id: "app.tree.foldOrUp",
        default_keys: &["ctrl+left", "alt+left"],
        description: "Fold current branch segment, or jump to the previous segment start",
    },
    ActionDef {
        id: "app.tree.unfoldOrDown",
        default_keys: &["ctrl+right", "alt+right"],
        description: "Unfold current branch segment, or jump to the next segment start or branch end",
    },
    ActionDef {
        id: "app.tree.editLabel",
        default_keys: &["shift+l"],
        description: "Edit the label on the selected tree node",
    },
    ActionDef {
        id: "app.tree.toggleLabelTimestamp",
        default_keys: &["shift+t"],
        description: "Toggle label timestamps in the tree",
    },
    ActionDef {
        id: "app.tree.filter.default",
        default_keys: &["ctrl+d"],
        description: "Set tree filter to default view",
    },
    ActionDef {
        id: "app.tree.filter.noTools",
        default_keys: &["ctrl+t"],
        description: "Toggle tree filter that hides tool results",
    },
    ActionDef {
        id: "app.tree.filter.userOnly",
        default_keys: &["ctrl+u"],
        description: "Toggle tree filter that shows only user messages",
    },
    ActionDef {
        id: "app.tree.filter.labeledOnly",
        default_keys: &["ctrl+l"],
        description: "Toggle tree filter that shows only labeled entries",
    },
    ActionDef {
        id: "app.tree.filter.all",
        default_keys: &["ctrl+a"],
        description: "Toggle tree filter that shows all entries",
    },
    ActionDef {
        id: "app.tree.filter.cycleForward",
        default_keys: &["ctrl+o"],
        description: "Cycle tree filter forward",
    },
    ActionDef {
        id: "app.tree.filter.cycleBackward",
        default_keys: &["shift+ctrl+o"],
        description: "Cycle tree filter backward",
    },
    // Scoped models selector
    ActionDef {
        id: "app.models.enableAll",
        default_keys: &["ctrl+a"],
        description: "Enable all models (or all matching the current search)",
    },
    ActionDef {
        id: "app.models.clearAll",
        default_keys: &["ctrl+x"],
        description: "Clear all models (or all matching the current search)",
    },
    ActionDef {
        id: "app.models.toggleProvider",
        default_keys: &["ctrl+p"],
        description: "Toggle all models for the current provider",
    },
    ActionDef {
        id: "app.models.reorderUp",
        default_keys: &["alt+up"],
        description: "Move the selected model up in the cycle order",
    },
    ActionDef {
        id: "app.models.reorderDown",
        default_keys: &["alt+down"],
        description: "Move the selected model down in the cycle order",
    },
    // oxa
    ActionDef {
        id: "app.mode.cycle",
        default_keys: &["alt+m"],
        description: "Cycle the permission mode",
    },
    ActionDef {
        id: "app.yolo.toggle",
        default_keys: &["ctrl+alt+o"],
        description: "Toggle YOLO (no permission prompts; the sandbox stays on)",
    },
];

fn warn(source: &str, message: impl Into<String>) -> Diagnostic {
    Diagnostic {
        level: Level::Warning,
        source: source.to_string(),
        message: message.into(),
    }
}

/// The conflict context of an id: `app` is one context; `tui.editor`, `tui.select`, ... are
/// their own two-segment prefixes.
fn context_of(id: &str) -> String {
    let mut parts = id.split('.');
    match (parts.next(), parts.next()) {
        (Some("app"), _) => "app".to_string(),
        (Some(a), Some(b)) => format!("{a}.{b}"),
        _ => id.to_string(),
    }
}

/// A key list from a JSON value: one string, or a list of strings. Any invalid element rejects
/// the whole entry (the default stays). Duplicates are dropped; an empty list unbinds.
fn parse_keys(v: &Value) -> Result<Vec<KeyId>, String> {
    let items: Vec<&Value> = match v {
        Value::String(_) => vec![v],
        Value::Array(a) => a.iter().collect(),
        _ => return Err("expected a key string or a list of key strings".to_string()),
    };
    let mut keys: Vec<KeyId> = Vec::new();
    for item in items {
        let Value::String(s) = item else {
            return Err("expected a key string in the list".to_string());
        };
        let key = KeyId::parse(s).map_err(|e| format!("invalid key {s:?}: {}", e.message))?;
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    Ok(keys)
}

impl Keymap {
    /// Every id and default of keybindings.md, plus `app.mode.cycle` = `alt+m`.
    /// `shift+tab` stays the thinking cycle.
    pub fn actions() -> &'static [ActionDef] {
        ACTIONS
    }
    pub fn defaults() -> Keymap {
        let bindings = ACTIONS
            .iter()
            .map(|a| {
                let keys = a
                    .default_keys
                    .iter()
                    .map(|k| KeyId::parse(k).expect("default key ids parse"))
                    .collect();
                (a.id.to_string(), keys)
            })
            .collect();
        Keymap { bindings }
    }
    /// Defaults overridden by `<agent_dir>/keybindings.json`. Unknown id or unparsable key = a
    /// diagnostic; the default stays.
    pub fn load(dirs: &Dirs) -> (Keymap, Vec<Diagnostic>) {
        let mut keymap = Keymap::defaults();
        let path = dirs.keybindings_file();
        let source = path.display().to_string();
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (keymap, Vec::new()),
            Err(e) => {
                return (
                    keymap,
                    vec![warn(&source, format!("cannot read keybindings: {e}"))],
                )
            }
        };
        let value: Value = match serde_json::from_str(text.trim_start_matches('\u{feff}')) {
            Ok(value) => value,
            Err(e) => {
                return (
                    keymap,
                    vec![warn(&source, format!("invalid JSON, defaults kept: {e}"))],
                )
            }
        };
        let diagnostics = keymap.apply_from(&value, &source);
        (keymap, diagnostics)
    }
    /// Apply a parsed keybindings.json object over `self` (a string or a list of strings per id;
    /// an empty list unbinds).
    pub fn apply(&mut self, overrides: &Value) -> Vec<Diagnostic> {
        self.apply_from(overrides, "keybindings")
    }
    fn apply_from(&mut self, overrides: &Value, source: &str) -> Vec<Diagnostic> {
        let Some(object) = overrides.as_object() else {
            return vec![warn(source, "keybindings must be a JSON object; defaults kept")];
        };
        let mut diagnostics = Vec::new();
        // pi's conflicts: keys claimed by more than one configured id (KeybindingsManager.rebuild).
        let mut claims: Vec<(KeyId, Vec<String>)> = Vec::new();
        for (id, value) in object {
            if id == "$schema" {
                continue;
            }
            let Some(slot) = self.bindings.iter_mut().find(|(name, _)| name == id) else {
                diagnostics.push(warn(source, format!("unknown keybinding id {id:?}; ignored")));
                continue;
            };
            match parse_keys(value) {
                Ok(keys) => {
                    for key in &keys {
                        match claims.iter_mut().find(|(k, _)| k == key) {
                            Some((_, ids)) => ids.push(id.clone()),
                            None => claims.push((key.clone(), vec![id.clone()])),
                        }
                    }
                    slot.1 = keys;
                }
                Err(message) => diagnostics.push(warn(source, format!("{id}: {message}; default kept"))),
            }
        }
        for (key, ids) in claims.into_iter().filter(|(_, ids)| ids.len() > 1) {
            diagnostics.push(warn(
                source,
                format!("key conflict: {} is bound to {}", key.as_str(), ids.join(", ")),
            ));
        }
        diagnostics
    }
    /// `true` when `key` is bound to `action`.
    pub fn matches(&self, key: &KeyId, action: &str) -> bool {
        self.bindings
            .iter()
            .any(|(id, keys)| id.as_str() == action && keys.contains(key))
    }
    /// The first action bound to `key`, in the order of [`Keymap::actions`].
    pub fn action_for(&self, key: &KeyId) -> Option<&str> {
        self.bindings
            .iter()
            .find(|(_, keys)| keys.contains(key))
            .map(|(id, _)| id.as_str())
    }
    pub fn actions_for(&self, key: &KeyId) -> Vec<&str> {
        self.bindings
            .iter()
            .filter(|(_, keys)| keys.contains(key))
            .map(|(id, _)| id.as_str())
            .collect()
    }
    /// This keymap with only the keys `keep` accepts (an action may end up with none). For the
    /// footer: the keys a terminal can actually send.
    pub fn retain_keys(&self, keep: impl Fn(&KeyId) -> bool) -> Keymap {
        Keymap {
            bindings: self
                .bindings
                .iter()
                .map(|(id, keys)| (id.clone(), keys.iter().filter(|k| keep(k)).cloned().collect()))
                .collect(),
        }
    }
    pub fn keys_for(&self, action: &str) -> Vec<KeyId> {
        self.bindings
            .iter()
            .find(|(id, _)| id.as_str() == action)
            .map(|(_, keys)| keys.clone())
            .unwrap_or_default()
    }
    /// Keys bound to more than one action within the same context prefix (`tui.editor.`,
    /// `tui.select.`, `app.`), for the conflict report.
    pub fn conflicts(&self) -> Vec<(KeyId, Vec<String>)> {
        // (context, key, ids) in order of first appearance.
        let mut groups: Vec<(String, KeyId, Vec<String>)> = Vec::new();
        for (id, keys) in &self.bindings {
            let context = context_of(id);
            for key in keys {
                match groups.iter_mut().find(|(c, k, _)| *c == context && k == key) {
                    Some((_, _, ids)) => ids.push(id.clone()),
                    None => groups.push((context.clone(), key.clone(), vec![id.clone()])),
                }
            }
        }
        groups
            .into_iter()
            .filter(|(_, _, ids)| ids.len() > 1)
            .map(|(_, key, ids)| (key, ids))
            .collect()
    }
}
