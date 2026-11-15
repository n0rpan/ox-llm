//! D8: the session picker (`-r`, `/resume`).
// ported from pi packages/coding-agent/src/modes/interactive/components/{session-selector.ts,session-selector-search.ts}
use crate::Outcome;
use oxa_config::Keymap;
use oxa_session::{search_sessions, sort_sessions, SessionMeta, SessionRef, SessionSort};
use oxa_tui::{truncate_to_width, visible_width, Key, KeyKind, Theme};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// What the picker asks for.
pub enum SessionPick {
    /// Open this session.
    Open(SessionRef),
    /// Rename (set the display name of) a session; the picker stays open afterwards: the app
    /// performs it and calls [`SessionPicker::set_sessions`].
    Rename { session: SessionRef, name: String },
    /// Delete a session file (already confirmed in the picker; moved to trash by the app).
    Delete(SessionRef),
    /// The user pressed Tab: the app loads the other scope (current directory <-> all
    /// directories) and calls [`SessionPicker::set_sessions`].
    ToggleScope,
}

/// The picker: a search field over the session list.
///
/// Contract (acceptance, plan D8): **search** (pi's query syntax through
/// `oxa_session::search_sessions`: tokens, quoted phrases, `re:` regex), **sort** cycling
/// (`oxa_session::SessionSort`), **named filter** toggle, **paths** toggle (show the cwd of
/// each session), **rename** (inline input), **delete with confirm** (a second key press
/// within the confirm line; Escape cancels the delete, not the picker), **other-directory
/// Tab**. Keys from the `app.session.*` actions of keybindings.md. Each row: name or first
/// message, age, message count; the current session is marked. `Rename`, `Delete` and
/// `ToggleScope` are returned as `Outcome::Done` **without closing** semantics for the app:
/// the app keeps the picker open and feeds it new data.
pub struct SessionPicker {
    metas: Vec<SessionMeta>,
    current_id: Option<String>,
    all_dirs: bool,
    now_ms: u64,
    query: String,
    sort: SessionSort,
    named_only: bool,
    show_path: bool,
    /// The visible rows, in display order (indices into `metas`, plus tree shape).
    rows: Vec<Row>,
    selected: usize,
    mode: Mode,
    /// Path of the session waiting for the delete confirmation.
    confirm_delete: Option<PathBuf>,
    /// `(is_error, text)`: shown on the hint line until the next key.
    status: Option<(bool, String)>,
    /// Key labels for the hint lines, taken from the keymap of the last key.
    hints: Hints,
    max_visible: usize,
}

/// One displayed row.
#[derive(Debug, Clone)]
struct Row {
    /// Index into `SessionPicker::metas`.
    meta: usize,
    depth: usize,
    is_last: bool,
    /// For each ancestor level below the root, whether a sibling follows (pi's connectors).
    ancestors: Vec<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Mode {
    List,
    /// Inline rename of the session at `path`; `text` is the input.
    Rename {
        path: PathBuf,
        text: String,
    },
}

/// Key labels shown in the hints (display only; the keymap decides what a key does).
#[derive(Debug, Clone)]
struct Hints {
    tab: String,
    sort: String,
    named: String,
    path: String,
    delete: String,
    rename: String,
    confirm: String,
    cancel: String,
}

impl Hints {
    fn of(keymap: &Keymap) -> Hints {
        let first = |action: &str| {
            keymap
                .keys_for(action)
                .first()
                .map(|k| k.to_string())
                .unwrap_or_default()
        };
        Hints {
            tab: first("tui.input.tab"),
            sort: first("app.session.toggleSort"),
            named: first("app.session.toggleNamedFilter"),
            path: first("app.session.togglePath"),
            delete: first("app.session.delete"),
            rename: first("app.session.rename"),
            confirm: first("tui.select.confirm"),
            cancel: first("tui.select.cancel"),
        }
    }
}

impl SessionPicker {
    /// `all_dirs` = the list covers every directory (paths shown by default).
    pub fn new(
        sessions: Vec<SessionMeta>,
        current_id: Option<&str>,
        all_dirs: bool,
        now_ms: u64,
    ) -> SessionPicker {
        let mut picker = SessionPicker {
            metas: sessions,
            current_id: current_id.map(str::to_string),
            all_dirs,
            now_ms,
            query: String::new(),
            sort: SessionSort::Threaded,
            named_only: false,
            show_path: false,
            rows: Vec::new(),
            selected: 0,
            mode: Mode::List,
            confirm_delete: None,
            status: None,
            hints: Hints::of(&Keymap::defaults()),
            max_visible: 10,
        };
        picker.refresh(None);
        picker
    }

    /// Replace the list (after rename, delete or scope toggle), keeping query, sort and filters
    /// and, when possible, the selection.
    pub fn set_sessions(&mut self, sessions: Vec<SessionMeta>, all_dirs: bool) {
        let keep = self.selected_path();
        if all_dirs != self.all_dirs {
            // pi: the All scope shows each session's cwd; the path toggle is independent.
            self.all_dirs = all_dirs;
        }
        self.metas = sessions;
        if self.confirm_delete.as_ref().is_some_and(|p| !self.has_path(p)) {
            self.confirm_delete = None;
        }
        let gone = matches!(&self.mode, Mode::Rename { path, .. } if !self.has_path(path));
        if gone {
            self.mode = Mode::List;
        }
        self.refresh(keep);
    }

    pub fn key(&mut self, key: &Key, keymap: &Keymap) -> Outcome<SessionPick> {
        if key.kind == KeyKind::Release {
            return Outcome::Ignored;
        }
        self.hints = Hints::of(keymap);
        self.status = None;
        if matches!(self.mode, Mode::Rename { .. }) {
            return self.key_rename(key, keymap);
        }
        if self.confirm_delete.is_some() {
            return self.key_confirm(key, keymap);
        }
        self.key_list(key, keymap)
    }

    pub fn paste(&mut self, text: &str) {
        let clean = clean_paste(text);
        if clean.is_empty() {
            return;
        }
        if let Mode::Rename { text: input, .. } = &mut self.mode {
            input.push_str(&clean);
            return;
        }
        if self.confirm_delete.is_some() {
            return;
        }
        self.query.push_str(&clean);
        let keep = self.selected_path();
        self.refresh(keep);
    }

    /// pi's `SessionSelectorComponent` layout: a spacer, an accent border, a spacer, the header
    /// (title line + two hint lines) and a spacer, the search line, a blank line and the list,
    /// then a spacer and a closing border. The rename panel replaces header and list.
    pub fn render(&mut self, width: u16, theme: &Theme) -> Vec<String> {
        let w = width as usize;
        let border = theme.fg("accent", &"─".repeat(w));
        let mut lines = vec![String::new(), border.clone(), String::new()];
        if let Mode::Rename { text, .. } = &self.mode {
            lines.extend(self.rename_lines(text, w, theme));
        } else {
            lines.push(self.header_line(w, theme));
            lines.extend(self.hint_lines(w, theme));
            lines.push(String::new());
            lines.push(format!("> {}▏", self.query));
            lines.push(String::new());
            lines.extend(self.list_lines(w, theme));
        }
        lines.push(String::new());
        lines.push(border);
        lines
    }

    /// A status message in place of the hint lines (pi `setStatusMessage`: delete results,
    /// errors), shown until the next key.
    pub fn set_status(&mut self, is_error: bool, message: &str) {
        self.status = Some((is_error, message.to_string()));
    }

    // ----- keys -------------------------------------------------------------------------

    fn key_list(&mut self, key: &Key, keymap: &Keymap) -> Outcome<SessionPick> {
        let id = &key.id;
        if keymap.matches(id, "tui.input.tab") {
            return Outcome::Done(SessionPick::ToggleScope);
        }
        if keymap.matches(id, "app.session.toggleSort") {
            self.sort = match self.sort {
                SessionSort::Threaded => SessionSort::Recent,
                SessionSort::Recent => SessionSort::Relevance,
                SessionSort::Relevance => SessionSort::Threaded,
            };
            let keep = self.selected_path();
            self.refresh(keep);
            return Outcome::Pending;
        }
        if keymap.matches(id, "app.session.toggleNamedFilter") {
            self.named_only = !self.named_only;
            let keep = self.selected_path();
            self.refresh(keep);
            return Outcome::Pending;
        }
        if keymap.matches(id, "app.session.togglePath") {
            self.show_path = !self.show_path;
            return Outcome::Pending;
        }
        if keymap.matches(id, "app.session.delete") {
            self.start_delete();
            return Outcome::Pending;
        }
        if keymap.matches(id, "app.session.rename") {
            if let Some(meta) = self.selected_meta() {
                let path = meta.sref.path.clone();
                let text = meta.name.clone().unwrap_or_default();
                self.mode = Mode::Rename { path, text };
            }
            return Outcome::Pending;
        }
        if keymap.matches(id, "app.session.deleteNoninvasive") {
            if self.query.is_empty() {
                self.start_delete();
            } else if delete_word(&mut self.query) {
                let keep = self.selected_path();
                self.refresh(keep);
            }
            return Outcome::Pending;
        }
        if keymap.matches(id, "tui.select.up") {
            self.selected = self.selected.saturating_sub(1);
            return Outcome::Pending;
        }
        if keymap.matches(id, "tui.select.down") {
            self.selected = (self.selected + 1).min(self.rows.len().saturating_sub(1));
            return Outcome::Pending;
        }
        if keymap.matches(id, "tui.select.pageUp") {
            self.selected = self.selected.saturating_sub(self.max_visible);
            return Outcome::Pending;
        }
        if keymap.matches(id, "tui.select.pageDown") {
            self.selected = (self.selected + self.max_visible).min(self.rows.len().saturating_sub(1));
            return Outcome::Pending;
        }
        if keymap.matches(id, "tui.select.confirm") {
            return match self.selected_sref() {
                Some(session) => Outcome::Done(SessionPick::Open(session)),
                None => Outcome::Pending,
            };
        }
        if keymap.matches(id, "tui.select.cancel") {
            return Outcome::Cancelled;
        }
        if edit_text(&mut self.query, key) {
            let keep = self.selected_path();
            self.refresh(keep);
            return Outcome::Pending;
        }
        Outcome::Ignored
    }

    /// While the delete confirmation is open every key is intercepted: Enter deletes, Escape
    /// (or the cancel binding) only cancels the delete, other keys are ignored.
    fn key_confirm(&mut self, key: &Key, keymap: &Keymap) -> Outcome<SessionPick> {
        if keymap.matches(&key.id, "tui.select.confirm") {
            let path = self.confirm_delete.take();
            return match path.and_then(|p| self.sref_of(&p)) {
                Some(session) => Outcome::Done(SessionPick::Delete(session)),
                None => Outcome::Pending,
            };
        }
        if keymap.matches(&key.id, "tui.select.cancel") {
            self.confirm_delete = None;
        }
        Outcome::Pending
    }

    fn key_rename(&mut self, key: &Key, keymap: &Keymap) -> Outcome<SessionPick> {
        if keymap.matches(&key.id, "tui.select.cancel") {
            self.mode = Mode::List;
            return Outcome::Pending;
        }
        if keymap.matches(&key.id, "tui.select.confirm") {
            let name = match &self.mode {
                Mode::Rename { text, .. } => text.trim().to_string(),
                Mode::List => String::new(),
            };
            if name.is_empty() {
                return Outcome::Pending;
            }
            let previous = std::mem::replace(&mut self.mode, Mode::List);
            return match previous {
                Mode::Rename { path, .. } => match self.sref_of(&path) {
                    Some(session) => Outcome::Done(SessionPick::Rename { session, name }),
                    None => Outcome::Pending,
                },
                Mode::List => Outcome::Pending,
            };
        }
        if let Mode::Rename { text, .. } = &mut self.mode {
            edit_text(text, key);
        }
        Outcome::Pending
    }

    fn start_delete(&mut self) {
        let Some(session) = self.selected_sref() else {
            return;
        };
        if self.is_current(&session) {
            self.status = Some((true, "Cannot delete the currently active session".to_string()));
            return;
        }
        self.confirm_delete = Some(session.path);
    }

    // ----- state ------------------------------------------------------------------------

    /// Rebuild the rows from the data and the filters, keeping `keep` selected when it is
    /// still listed, otherwise the same position (clamped).
    fn refresh(&mut self, keep: Option<PathBuf>) {
        self.rows = self.compute_rows();
        let position = keep.and_then(|path| {
            self.rows
                .iter()
                .position(|row| self.metas[row.meta].sref.path == path)
        });
        self.selected = position.unwrap_or(self.selected.min(self.rows.len().saturating_sub(1)));
    }

    fn compute_rows(&self) -> Vec<Row> {
        let mut indices = search_sessions(&self.metas, &self.query, self.named_only);
        let empty_query = self.query.trim().is_empty();
        match self.sort {
            // pi: threaded without a search shows the tree; with a search it is a ranked list.
            SessionSort::Threaded if empty_query => return tree_rows(&self.metas, &indices),
            SessionSort::Recent => sort_sessions(&self.metas, &mut indices, SessionSort::Recent),
            SessionSort::Threaded | SessionSort::Relevance => {}
        }
        indices
            .into_iter()
            .map(|meta| Row {
                meta,
                depth: 0,
                is_last: true,
                ancestors: Vec::new(),
            })
            .collect()
    }

    fn selected_path(&self) -> Option<PathBuf> {
        self.selected_meta().map(|m| m.sref.path.clone())
    }

    fn selected_meta(&self) -> Option<&SessionMeta> {
        self.rows.get(self.selected).map(|row| &self.metas[row.meta])
    }

    fn selected_sref(&self) -> Option<SessionRef> {
        self.selected_meta().map(|m| m.sref.clone())
    }

    fn sref_of(&self, path: &Path) -> Option<SessionRef> {
        self.metas
            .iter()
            .find(|m| m.sref.path.as_path() == path)
            .map(|m| m.sref.clone())
    }

    fn has_path(&self, path: &Path) -> bool {
        self.metas.iter().any(|m| m.sref.path.as_path() == path)
    }

    fn is_current(&self, session: &SessionRef) -> bool {
        self.current_id.as_deref() == Some(session.id.as_str())
    }

    // ----- rendering --------------------------------------------------------------------

    fn rename_lines(&self, text: &str, w: usize, theme: &Theme) -> Vec<String> {
        let hint = format!("{} to save · {} to cancel", self.hints.confirm, self.hints.cancel);
        vec![
            theme.bold("Rename Session"),
            String::new(),
            format!("> {text}▏"),
            String::new(),
            theme.fg("muted", &truncate_to_width(&hint, w, "…")),
        ]
        .into_iter()
        .map(|line| truncate_to_width(&line, w, ""))
        .collect()
    }

    fn header_line(&self, w: usize, theme: &Theme) -> String {
        let title = if self.all_dirs {
            "Resume Session (All)"
        } else {
            "Resume Session (Current Folder)"
        };
        let left = theme.bold(title);
        let scope = if self.all_dirs {
            format!(
                "{}{}",
                theme.fg("muted", "○ Current Folder | "),
                theme.fg("accent", "◉ All")
            )
        } else {
            format!(
                "{}{}",
                theme.fg("accent", "◉ Current Folder"),
                theme.fg("muted", " | ○ All")
            )
        };
        let name = format!(
            "{}{}",
            theme.fg("muted", "Name: "),
            theme.fg("accent", if self.named_only { "Named" } else { "All" })
        );
        let sort_label = match self.sort {
            SessionSort::Threaded => "Threaded",
            SessionSort::Recent => "Recent",
            SessionSort::Relevance => "Fuzzy",
        };
        let sort = format!(
            "{}{}",
            theme.fg("muted", "Sort: "),
            theme.fg("accent", sort_label)
        );
        let right = truncate_to_width(&format!("{scope}  {name}  {sort}"), w, "");
        let left = truncate_to_width(&left, w.saturating_sub(visible_width(&right) + 1), "");
        let spacing = w.saturating_sub(visible_width(&left) + visible_width(&right));
        format!("{left}{}{right}", " ".repeat(spacing))
    }

    /// Two hint lines, or the delete confirmation / a status message in their place.
    fn hint_lines(&self, w: usize, theme: &Theme) -> Vec<String> {
        let h = &self.hints;
        if self.confirm_delete.is_some() {
            let msg = format!("Delete session? {} confirm · {} cancel", h.confirm, h.cancel);
            return vec![theme.fg("error", &truncate_to_width(&msg, w, "…")), String::new()];
        }
        if let Some((is_error, msg)) = &self.status {
            let color = if *is_error { "error" } else { "accent" };
            return vec![theme.fg(color, &truncate_to_width(msg, w, "…")), String::new()];
        }
        let path_state = if self.show_path { "(on)" } else { "(off)" };
        let line1 = format!("{} scope · re:<pattern> regex · \"phrase\" exact", h.tab);
        let line2 = [
            format!("{} sort", h.sort),
            format!("{} named", h.named),
            format!("{} delete", h.delete),
            format!("{} path {path_state}", h.path),
            format!("{} rename", h.rename),
        ]
        .join(" · ");
        vec![
            theme.fg("muted", &truncate_to_width(&line1, w, "…")),
            theme.fg("muted", &truncate_to_width(&line2, w, "…")),
        ]
    }

    fn list_lines(&self, w: usize, theme: &Theme) -> Vec<String> {
        if self.rows.is_empty() {
            let msg = if self.named_only {
                if self.all_dirs {
                    format!(
                        "  No named sessions found. Press {} to show all.",
                        self.hints.named
                    )
                } else {
                    format!(
                        "  No named sessions in current folder. Press {} to show all, or Tab to view all.",
                        self.hints.named
                    )
                }
            } else if self.all_dirs {
                "  No sessions found".to_string()
            } else {
                "  No sessions in current folder. Press Tab to view all.".to_string()
            };
            return vec![theme.fg("muted", &truncate_to_width(&msg, w, "…"))];
        }
        let len = self.rows.len();
        let max = self.max_visible;
        let start = self.selected.saturating_sub(max / 2).min(len.saturating_sub(max));
        let end = (start + max).min(len);
        let mut out: Vec<String> = (start..end).map(|pos| self.row_line(pos, w, theme)).collect();
        if start > 0 || end < len {
            let info = format!("  ({}/{})", self.selected + 1, len);
            out.push(theme.fg("muted", &truncate_to_width(&info, w, "")));
        }
        out
    }

    fn row_line(&self, pos: usize, w: usize, theme: &Theme) -> String {
        let row = &self.rows[pos];
        let meta = &self.metas[row.meta];
        let selected = pos == self.selected;
        let confirming = self.confirm_delete.as_deref() == Some(meta.sref.path.as_path());
        let current = self.is_current(&meta.sref);

        let prefix = tree_prefix(row);
        let message = clean_line(meta.name.as_deref().unwrap_or(&meta.first_message));
        // pi: `<count> <age>`, the cwd in front in the All scope, the file path with Ctrl+P.
        let mut right = format!(
            "{} {}",
            meta.message_count,
            relative_age(meta.modified_ms, self.now_ms)
        );
        if self.all_dirs {
            right = format!("{} {right}", meta.cwd.display());
        }
        if self.show_path {
            right = format!("{} {right}", meta.sref.path.display());
        }

        let cursor = if selected {
            theme.fg("accent", "› ")
        } else {
            "  ".to_string()
        };
        let right_w = visible_width(&right) + 2;
        let avail = w.saturating_sub(2 + visible_width(&prefix) + right_w).max(10);
        let truncated = truncate_to_width(&message, avail, "…");
        let mut styled = if confirming {
            theme.fg("error", &truncated)
        } else if current {
            theme.fg("accent", &truncated)
        } else if meta.name.is_some() {
            theme.fg("warning", &truncated)
        } else {
            truncated
        };
        if selected {
            styled = theme.bold(&styled);
        }
        let left = format!("{cursor}{}{styled}", theme.fg("dim", &prefix));
        let spacing = w
            .saturating_sub(visible_width(&left) + visible_width(&right))
            .max(1);
        let right_styled = theme.fg(if confirming { "error" } else { "dim" }, &right);
        let mut line = format!("{left}{}{right_styled}", " ".repeat(spacing));
        if selected {
            line = theme.bg("selectedBg", &line);
        }
        truncate_to_width(&line, w, "")
    }
}

/// `3m ago`, `2h ago`, `5d ago`, or a date for more than 30 days (pi's format).
///
/// Under a minute (including a timestamp in the future) reads `now`; a date is `YYYY-MM-DD` in UTC.
pub fn relative_age(ts_ms: u64, now_ms: u64) -> String {
    // pi session-selector.ts `formatSessionDate`.
    let diff = now_ms.saturating_sub(ts_ms);
    let mins = diff / 60_000;
    let hours = diff / 3_600_000;
    let days = diff / 86_400_000;
    if mins < 1 {
        "now".to_string()
    } else if mins < 60 {
        format!("{mins}m")
    } else if hours < 24 {
        format!("{hours}h")
    } else if days < 7 {
        format!("{days}d")
    } else if days < 30 {
        format!("{}w", days / 7)
    } else if days < 365 {
        format!("{}mo", days / 30)
    } else {
        format!("{}y", days / 365)
    }
}

/// Edit a text field from a key: Backspace deletes one character, printable text is inserted.
/// Returns `true` when the text changed.
fn edit_text(text: &mut String, key: &Key) -> bool {
    if key.is("backspace") {
        return text.pop().is_some();
    }
    if key.is("ctrl+backspace") {
        return delete_word(text);
    }
    match &key.text {
        Some(t) if !t.is_empty() => {
            text.push_str(t);
            true
        }
        _ => false,
    }
}

/// Remove the word before the end of `text`; `true` when something was removed.
fn delete_word(text: &mut String) -> bool {
    let before = text.len();
    while text.ends_with(char::is_whitespace) {
        text.pop();
    }
    while text.chars().last().is_some_and(|c| !c.is_whitespace()) {
        text.pop();
    }
    text.len() != before
}

/// Pasted text as one line: line breaks and tabs become spaces, other control characters go.
fn clean_paste(text: &str) -> String {
    text.chars()
        .filter_map(|c| match c {
            '\r' | '\n' | '\t' => Some(' '),
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect()
}

/// A session message as one trimmed line (control characters become spaces).
fn clean_line(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_string()
}

fn tree_prefix(row: &Row) -> String {
    if row.depth == 0 {
        return String::new();
    }
    let mut prefix: String = row
        .ancestors
        .iter()
        .map(|more| if *more { "│  " } else { "   " })
        .collect();
    prefix.push_str(if row.is_last { "└─ " } else { "├─ " });
    prefix
}

/// pi's threaded view: a session is placed directly after its parent (when the parent is in
/// the list); roots and children are ordered by the newest activity in their subtree.
fn tree_rows(metas: &[SessionMeta], indices: &[usize]) -> Vec<Row> {
    let by_path: HashMap<String, usize> = indices
        .iter()
        .map(|&i| (metas[i].sref.path.to_string_lossy().into_owned(), i))
        .collect();
    let mut children: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut roots: Vec<usize> = Vec::new();
    for &i in indices {
        let parent = metas[i]
            .parent_session
            .as_ref()
            .and_then(|p| by_path.get(p).copied())
            .filter(|&p| p != i);
        match parent {
            Some(p) => children.entry(p).or_default().push(i),
            None => roots.push(i),
        }
    }
    let mut latest: HashMap<usize, u64> = HashMap::new();
    for &i in indices {
        subtree_latest(i, metas, &children, &mut latest);
    }
    let newest_first = |a: &usize, b: &usize| latest[b].cmp(&latest[a]);
    roots.sort_by(newest_first);
    for kids in children.values_mut() {
        kids.sort_by(newest_first);
    }

    let mut walk = TreeWalk {
        children: &children,
        seen: HashSet::new(),
        out: Vec::new(),
    };
    let count = roots.len();
    for (k, &root) in roots.iter().enumerate() {
        walk.visit(root, 0, Vec::new(), k + 1 == count);
    }
    // Sessions that only point at each other (a parent cycle) are not reachable from a root.
    for &i in indices {
        if !walk.seen.contains(&i) {
            walk.visit(i, 0, Vec::new(), true);
        }
    }
    walk.out
}

fn subtree_latest(
    i: usize,
    metas: &[SessionMeta],
    children: &HashMap<usize, Vec<usize>>,
    memo: &mut HashMap<usize, u64>,
) -> u64 {
    if let Some(&known) = memo.get(&i) {
        return known;
    }
    let own = metas[i].modified_ms;
    // Guard against parent cycles: the node is recorded before its children are visited.
    memo.insert(i, own);
    let mut best = own;
    if let Some(kids) = children.get(&i) {
        for &child in kids {
            best = best.max(subtree_latest(child, metas, children, memo));
        }
    }
    memo.insert(i, best);
    best
}

struct TreeWalk<'a> {
    children: &'a HashMap<usize, Vec<usize>>,
    seen: HashSet<usize>,
    out: Vec<Row>,
}

impl TreeWalk<'_> {
    fn visit(&mut self, i: usize, depth: usize, ancestors: Vec<bool>, is_last: bool) {
        if !self.seen.insert(i) {
            return;
        }
        // pi: a child's connector continues when its parent is not the last of its siblings
        // (never for the roots).
        let continues = depth > 0 && !is_last;
        self.out.push(Row {
            meta: i,
            depth,
            is_last,
            ancestors: ancestors.clone(),
        });
        let kids: Vec<usize> = self.children.get(&i).cloned().unwrap_or_default();
        let count = kids.len();
        for (k, child) in kids.into_iter().enumerate() {
            let mut child_ancestors = ancestors.clone();
            child_ancestors.push(continues);
            self.visit(child, depth + 1, child_ancestors, k + 1 == count);
        }
    }
}
