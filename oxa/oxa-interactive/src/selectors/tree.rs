//! D9: the session tree navigator (`/tree`) and the fork-point picker (`/fork`). Owner: task D9. Signatures below are frozen; bodies are yours.
// ported from pi packages/coding-agent/src/modes/interactive/components/{tree-selector.ts,user-message-selector.ts}
//
// Design notes (D9):
// * The tree is flattened once per `set_tree` in pi's order: the branch of the active leaf first,
//   then the other children in file order. Every refresh (filter, search, fold) only filters that
//   list and recomputes pi's visual structure (connectors, gutters) on the visible tree.
// * Selection and folds are kept by entry id, so they survive `set_tree` after a label change.
// * Timestamps (`[+label time]`) are formatted in UTC so that the output is the same on every
//   machine; pi uses the local time zone.
use std::collections::{HashMap, HashSet};

use crate::Outcome;
use oxa_config::Keymap;
use oxa_proto::{Entry, EntryBody, Role, WireMsg};
use oxa_session::Tree;
use oxa_tui::{truncate_to_width, Key, KeyKind, Theme};

/// The five filters of pi's tree view, cycled with the `app.tree.filter*` actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TreeFilter {
    /// Hide settings-type entries (model / thinking / mode changes, labels, usage, custom).
    Default,
    /// `Default` minus tool results.
    NoTools,
    UserOnly,
    LabeledOnly,
    All,
}

impl TreeFilter {
    pub const ALL: [TreeFilter; 5] = [
        TreeFilter::Default,
        TreeFilter::NoTools,
        TreeFilter::UserOnly,
        TreeFilter::LabeledOnly,
        TreeFilter::All,
    ];
    /// pi's names: `default`, `no-tools`, `user-only`, `labeled-only`, `all`.
    pub fn as_str(self) -> &'static str {
        match self {
            TreeFilter::Default => "default",
            TreeFilter::NoTools => "no-tools",
            TreeFilter::UserOnly => "user-only",
            TreeFilter::LabeledOnly => "labeled-only",
            TreeFilter::All => "all",
        }
    }
}

/// What the navigator asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreePick {
    /// Move the leaf to `target` (`Session::navigate`); `summarize` = write a branch summary of
    /// the branch being left (asked in the navigator, pi's three-way choice: no summary /
    /// summarize / summarize with instructions).
    Navigate {
        target: String,
        summarize: bool,
        instructions: Option<String>,
        /// When the selected entry is a user message, the leaf moves to its parent and this
        /// text refills the editor (pi: "user-message select refills the editor").
        refill_editor: Option<String>,
    },
    /// Set or clear a label on an entry; the navigator stays open (the app performs it and
    /// calls [`TreeSelector::set_tree`]).
    SetLabel { target: String, label: Option<String> },
    /// `app.message.copy` on the selected entry: its full text (`None` = it has none). The
    /// navigator stays open.
    Copy { text: Option<String> },
}

/// The tree navigator.
///
/// Contract (acceptance, plan D9): the tree drawn with pi's connectors (`├─`, `└─`, `│`),
/// the active branch marked, **fold / unfold** of subtrees, the **five filters**, **labels +
/// timestamps** toggles, search by typing, and the user-message refill rule above. Rows show a
/// one-line preview per entry type (user text, assistant text or tool names, tool result
/// summary, compaction, branch summary, custom). The selection starts at the current leaf.
pub struct TreeSelector {
    /// Every entry, in pi's flatten order (parents before children).
    nodes: Vec<Node>,
    index: HashMap<String, usize>,
    filter: TreeFilter,
    query: String,
    /// Folded entries by id (their descendants are hidden).
    folded: HashSet<String>,
    show_label_ts: bool,
    /// The selected entry id; the cursor follows it through refreshes.
    selected_id: Option<String>,
    max_rows: usize,
    now_ms: u64,
    /// Label editor: entry id and the text being typed.
    edit: Option<LabelEdit>,
    // ---- derived by `refresh` ----
    /// Visible node indices, in flatten order.
    visible: Vec<usize>,
    /// Position of each node in `visible` (`usize::MAX` when hidden).
    pos: Vec<usize>,
    cursor: usize,
    /// Nearest visible parent / visible children (pi's visibleParentMap / visibleChildrenMap).
    vparent: Vec<Option<usize>>,
    vchildren: Vec<Vec<usize>>,
    /// Pi's visual structure of each visible node.
    visual: Vec<Visual>,
    multiple_roots: bool,
}

/// The fork-point picker (`/fork`): the user messages of the active branch, oldest first.
pub struct ForkPointSelector {
    /// `(entry id, text)` in branch order.
    items: Vec<(String, String)>,
    cursor: usize,
    max_rows: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    User,
    /// Assistant message; `text` = it has non-empty text (pi hides tool-call-only replies).
    Assistant {
        text: bool,
    },
    ToolResult,
    /// Bookkeeping hidden by the default view (labels, model / thinking / mode changes, ...).
    Settings,
    /// Usage records: never shown.
    Usage,
    Other,
}

#[derive(Debug, Clone)]
struct Node {
    id: String,
    parent: Option<usize>,
    class: Class,
    is_leaf: bool,
    on_active_path: bool,
    label: Option<String>,
    label_ts: Option<u64>,
    /// Styled preview segments: `(token, text)`; an empty token is the default colour.
    preview: Vec<(&'static str, String)>,
    /// Lower-cased searchable text.
    search: String,
    /// The text of a user message (refills the editor on select).
    user_text: Option<String>,
    /// What `app.message.copy` copies (pi `getEntryCopyText`).
    copy_text: Option<String>,
}

#[derive(Debug, Clone, Copy)]
struct Gutter {
    position: usize,
    show: bool,
}

#[derive(Debug, Clone, Default)]
struct Visual {
    indent: usize,
    show_connector: bool,
    is_last: bool,
    virtual_root_child: bool,
    gutters: Vec<Gutter>,
}

struct LabelEdit {
    target: String,
    buf: String,
}

struct Frame {
    node: usize,
    indent: usize,
    just_branched: bool,
    show_connector: bool,
    is_last: bool,
    gutters: Vec<Gutter>,
    virtual_root_child: bool,
}

impl TreeSelector {
    pub fn new(tree: &Tree, initial_filter: TreeFilter, max_rows: usize, now_ms: u64) -> TreeSelector {
        let mut s = TreeSelector {
            nodes: Vec::new(),
            index: HashMap::new(),
            filter: initial_filter,
            query: String::new(),
            folded: HashSet::new(),
            show_label_ts: false,
            selected_id: None,
            max_rows: max_rows.max(1),
            now_ms,
            edit: None,
            visible: Vec::new(),
            pos: Vec::new(),
            cursor: 0,
            vparent: Vec::new(),
            vchildren: Vec::new(),
            visual: Vec::new(),
            multiple_roots: false,
        };
        // The selection starts at the current leaf (pi: initialSelectedId ?? currentLeafId).
        if !tree.leaf().is_empty() {
            s.selected_id = Some(tree.leaf().to_string());
        }
        s.set_tree(tree);
        s
    }

    /// Replace the data after a label change, keeping selection, folds and filter.
    pub fn set_tree(&mut self, tree: &Tree) {
        self.nodes = build_nodes(tree);
        self.index = self
            .nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (n.id.clone(), i))
            .collect();
        self.refresh();
    }

    pub fn filter(&self) -> TreeFilter {
        self.filter
    }

    pub fn key(&mut self, key: &Key, keymap: &Keymap) -> Outcome<TreePick> {
        if key.kind == KeyKind::Release {
            return Outcome::Ignored;
        }
        if self.edit.is_some() {
            return self.edit_key(key, keymap);
        }
        // Shift+letter bindings (`app.tree.editLabel`, `app.tree.toggleLabelTimestamp`) act
        // while the search is empty; a non-empty search takes the capital as text.
        let typing = !self.query.is_empty() && printable(key).is_some() && key.id.has_mod("shift");
        let bound = |action: &str| !typing && bound_key(key, keymap, action);

        if bound("tui.select.up") {
            self.step(-1);
        } else if bound("tui.select.down") {
            self.step(1);
        } else if bound("app.tree.foldOrUp") {
            self.fold_or_up();
        } else if bound("app.tree.unfoldOrDown") {
            self.unfold_or_down();
        } else if bound("tui.editor.cursorLeft") || bound("tui.select.pageUp") {
            self.page(-1);
        } else if bound("tui.editor.cursorRight") || bound("tui.select.pageDown") {
            self.page(1);
        } else if bound("tui.select.confirm") {
            return match self.visible.get(self.cursor) {
                Some(&i) => Outcome::Done(self.pick(i)),
                None => Outcome::Pending,
            };
        } else if bound("tui.select.cancel") {
            if self.query.is_empty() {
                return Outcome::Cancelled;
            }
            self.query.clear();
            self.folded.clear();
            self.refresh();
        } else if bound("app.tree.filter.default") {
            self.set_filter(TreeFilter::Default);
        } else if bound("app.tree.filter.noTools") {
            self.toggle_filter(TreeFilter::NoTools);
        } else if bound("app.tree.filter.userOnly") {
            self.toggle_filter(TreeFilter::UserOnly);
        } else if bound("app.tree.filter.labeledOnly") {
            self.toggle_filter(TreeFilter::LabeledOnly);
        } else if bound("app.tree.filter.all") {
            self.toggle_filter(TreeFilter::All);
        } else if bound("app.tree.filter.cycleForward") {
            self.cycle_filter(1);
        } else if bound("app.tree.filter.cycleBackward") {
            self.cycle_filter(-1);
        } else if bound("tui.editor.deleteCharBackward") {
            if self.query.pop().is_some() {
                self.folded.clear();
                self.refresh();
            }
        } else if bound("app.tree.editLabel") {
            if let Some(&i) = self.visible.get(self.cursor) {
                let node = &self.nodes[i];
                self.edit = Some(LabelEdit {
                    target: node.id.clone(),
                    buf: node.label.clone().unwrap_or_default(),
                });
            }
        } else if bound("app.message.copy") {
            let text = self
                .visible
                .get(self.cursor)
                .and_then(|&i| self.nodes[i].copy_text.clone());
            return Outcome::Done(TreePick::Copy { text });
        } else if bound("app.tree.toggleLabelTimestamp") {
            self.show_label_ts = !self.show_label_ts;
        } else if let Some(text) = printable(key) {
            self.query.push_str(text);
            self.folded.clear();
            self.refresh();
        } else {
            return Outcome::Ignored;
        }
        Outcome::Pending
    }

    /// pi's `TreeSelectorComponent` around [`TreeSelector::render`]: a spacer, a border, the
    /// `Session Tree` title, the key help, the search (or label) line, a border, a spacer, the
    /// rows and the position line, a spacer and a closing border.
    pub fn render_framed(&mut self, width: u16, theme: &Theme, keymap: &Keymap) -> Vec<String> {
        let w = width as usize;
        let border = theme.fg("border", &"─".repeat(w));
        let body = self.render(width, theme);
        let (search, rows) = body
            .split_first()
            .map(|(a, b)| (a.clone(), b.to_vec()))
            .unwrap_or_default();
        let mut lines = vec![
            String::new(),
            border.clone(),
            truncate_to_width(&format!(" {}", theme.bold("  Session Tree")), w, ""),
        ];
        for line in tree_help(keymap, w) {
            lines.push(theme.fg("muted", &line));
        }
        lines.push(search);
        lines.push(border.clone());
        lines.push(String::new());
        lines.extend(rows);
        lines.push(String::new());
        lines.push(border);
        lines
    }

    pub fn paste(&mut self, text: &str) {
        let clean: String = text
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        if let Some(edit) = self.edit.as_mut() {
            edit.buf.push_str(&clean);
        } else {
            self.query.push_str(&clean);
            self.folded.clear();
            self.refresh();
        }
    }

    pub fn render(&mut self, width: u16, theme: &Theme) -> Vec<String> {
        let width = width as usize;
        let mut lines = Vec::new();
        let filter_labels = self.status_labels();
        match &self.edit {
            Some(edit) => {
                let line = format!(
                    "{}{}{}",
                    theme.fg("muted", "  Label (empty to remove): "),
                    theme.fg("accent", &edit.buf),
                    theme.inverse(" ")
                );
                lines.push(truncate_to_width(&line, width, ""));
            }
            None => {
                let line = format!(
                    "{}{}",
                    theme.fg("muted", "  Type to search: "),
                    theme.fg("accent", &self.query)
                );
                lines.push(truncate_to_width(&line, width, ""));
            }
        }
        let n = self.visible.len();
        if n == 0 {
            lines.push(truncate_to_width(
                &theme.fg("muted", "  No entries found"),
                width,
                "",
            ));
            lines.push(truncate_to_width(
                &theme.fg("muted", &format!("  (0/0){filter_labels}")),
                width,
                "",
            ));
            return lines;
        }
        let (start, end) = window(self.cursor, n, self.max_rows);
        for k in start..end {
            let i = self.visible[k];
            lines.push(self.row(i, k == self.cursor, width, theme));
        }
        lines.push(truncate_to_width(
            &theme.fg(
                "muted",
                &format!("  ({}/{}){}", self.cursor + 1, n, filter_labels),
            ),
            width,
            "",
        ));
        lines
    }

    // ---- key actions -------------------------------------------------------------------

    fn edit_key(&mut self, key: &Key, keymap: &Keymap) -> Outcome<TreePick> {
        if bound_key(key, keymap, "tui.select.confirm") {
            let edit = self.edit.take().expect("edit mode");
            let text = edit.buf.trim();
            let label = if text.is_empty() {
                None
            } else {
                Some(text.to_string())
            };
            return Outcome::Done(TreePick::SetLabel {
                target: edit.target,
                label,
            });
        }
        if bound_key(key, keymap, "tui.select.cancel") {
            self.edit = None;
            return Outcome::Pending;
        }
        if bound_key(key, keymap, "tui.editor.deleteCharBackward") {
            if let Some(edit) = self.edit.as_mut() {
                edit.buf.pop();
            }
            return Outcome::Pending;
        }
        if let (Some(text), Some(edit)) = (printable(key), self.edit.as_mut()) {
            edit.buf.push_str(text);
        }
        Outcome::Pending
    }

    fn pick(&self, i: usize) -> TreePick {
        let node = &self.nodes[i];
        match &node.user_text {
            // The leaf moves to the parent of the user message; "" = before the first entry.
            Some(text) => TreePick::Navigate {
                target: node.parent.map(|p| self.nodes[p].id.clone()).unwrap_or_default(),
                summarize: false,
                instructions: None,
                refill_editor: Some(text.clone()),
            },
            None => TreePick::Navigate {
                target: node.id.clone(),
                summarize: false,
                instructions: None,
                refill_editor: None,
            },
        }
    }

    fn set_filter(&mut self, filter: TreeFilter) {
        self.filter = filter;
        self.folded.clear();
        self.refresh();
    }

    fn toggle_filter(&mut self, filter: TreeFilter) {
        let next = if self.filter == filter {
            TreeFilter::Default
        } else {
            filter
        };
        self.set_filter(next);
    }

    fn cycle_filter(&mut self, delta: isize) {
        let all = TreeFilter::ALL;
        let at = all.iter().position(|f| *f == self.filter).unwrap_or(0) as isize;
        let next = (at + delta).rem_euclid(all.len() as isize) as usize;
        self.set_filter(all[next]);
    }

    fn step(&mut self, delta: isize) {
        let n = self.visible.len();
        if n == 0 {
            return;
        }
        let next = if delta < 0 {
            if self.cursor == 0 {
                n - 1
            } else {
                self.cursor - 1
            }
        } else if self.cursor + 1 >= n {
            0
        } else {
            self.cursor + 1
        };
        self.go(next);
    }

    fn page(&mut self, delta: isize) {
        let n = self.visible.len();
        if n == 0 {
            return;
        }
        let m = self.max_rows;
        let next = if delta < 0 {
            self.cursor.saturating_sub(m)
        } else {
            (self.cursor + m).min(n - 1)
        };
        self.go(next);
    }

    fn go(&mut self, pos: usize) {
        if self.visible.is_empty() {
            return;
        }
        self.cursor = pos.min(self.visible.len() - 1);
        self.selected_id = Some(self.nodes[self.visible[self.cursor]].id.clone());
    }

    fn fold_or_up(&mut self) {
        let Some(&cur) = self.visible.get(self.cursor) else {
            return;
        };
        let id = self.nodes[cur].id.clone();
        if self.foldable(cur) && !self.folded.contains(&id) {
            self.folded.insert(id);
            self.refresh();
        } else {
            let pos = self.segment_start(true);
            self.go(pos);
        }
    }

    fn unfold_or_down(&mut self) {
        let Some(&cur) = self.visible.get(self.cursor) else {
            return;
        };
        let id = self.nodes[cur].id.clone();
        if self.folded.remove(&id) {
            self.refresh();
        } else {
            let pos = self.segment_start(false);
            self.go(pos);
        }
    }

    /// pi's `isFoldable`: a visible node with visible children that is a root or a segment start.
    fn foldable(&self, i: usize) -> bool {
        if self.vchildren[i].is_empty() {
            return false;
        }
        match self.vparent[i] {
            None => true,
            Some(p) => self.vchildren[p].len() > 1,
        }
    }

    /// pi's `findBranchSegmentStart`, in visible positions.
    fn segment_start(&self, up: bool) -> usize {
        let mut cur = self.visible[self.cursor];
        if up {
            loop {
                match self.vparent[cur] {
                    None => return self.pos[cur],
                    Some(p) => {
                        if self.vchildren[p].len() > 1 && self.pos[cur] < self.cursor {
                            return self.pos[cur];
                        }
                        cur = p;
                    }
                }
            }
        } else {
            loop {
                let children = &self.vchildren[cur];
                if children.is_empty() {
                    return self.pos[cur];
                }
                if children.len() > 1 {
                    return self.pos[children[0]];
                }
                cur = children[0];
            }
        }
    }

    // ---- derived state -----------------------------------------------------------------

    /// Filter, search and folds, then pi's visual structure of the visible tree, then the
    /// cursor (nearest visible ancestor of the selected entry, else the last visible entry).
    fn refresh(&mut self) {
        let n = self.nodes.len();
        let tokens: Vec<String> = self.query.split_whitespace().map(|t| t.to_lowercase()).collect();
        let folded_flag: Vec<bool> = self.nodes.iter().map(|x| self.folded.contains(&x.id)).collect();
        let mut under_fold = vec![false; n];
        for i in 0..n {
            if let Some(p) = self.nodes[i].parent {
                under_fold[i] = under_fold[p] || folded_flag[p];
            }
        }
        let mut vis = vec![false; n];
        for i in 0..n {
            let node = &self.nodes[i];
            if under_fold[i] || !passes_filter(node, self.filter) {
                continue;
            }
            if !tokens.is_empty() && !tokens.iter().all(|t| node.search.contains(t.as_str())) {
                continue;
            }
            vis[i] = true;
        }

        self.visible = (0..n).filter(|&i| vis[i]).collect();
        self.pos = vec![usize::MAX; n];
        for (k, &i) in self.visible.iter().enumerate() {
            self.pos[i] = k;
        }

        // Nearest visible ancestor-or-self, then the visible tree.
        let mut anc: Vec<Option<usize>> = vec![None; n];
        for i in 0..n {
            anc[i] = if vis[i] {
                Some(i)
            } else {
                self.nodes[i].parent.and_then(|p| anc[p])
            };
        }
        self.vparent = vec![None; n];
        self.vchildren = vec![Vec::new(); n];
        let mut vroots = Vec::new();
        for &i in &self.visible {
            let p = self.nodes[i].parent.and_then(|p| anc[p]);
            self.vparent[i] = p;
            match p {
                Some(p) => self.vchildren[p].push(i),
                None => vroots.push(i),
            }
        }
        self.multiple_roots = vroots.len() > 1;
        self.visual = vec![Visual::default(); n];
        self.layout(&vroots);

        // Cursor: the selected entry, or its nearest visible ancestor, else the last one.
        let mut found = None;
        if let Some(sel) = self
            .selected_id
            .as_deref()
            .and_then(|id| self.index.get(id))
            .copied()
        {
            let mut cur = Some(sel);
            while let Some(c) = cur {
                if vis[c] {
                    found = Some(c);
                    break;
                }
                cur = self.nodes[c].parent;
            }
        }
        let target = found.or_else(|| self.visible.last().copied());
        self.cursor = target.map(|t| self.pos[t]).unwrap_or(0);
        self.selected_id = self.visible.get(self.cursor).map(|&i| self.nodes[i].id.clone());
    }

    /// pi's `recalculateVisualStructure`: indents, connectors and gutters of the visible tree.
    fn layout(&mut self, roots: &[usize]) {
        let mr = self.multiple_roots;
        let mut stack: Vec<Frame> = Vec::new();
        for (k, &r) in roots.iter().enumerate().rev() {
            stack.push(Frame {
                node: r,
                indent: usize::from(mr),
                just_branched: mr,
                show_connector: mr,
                is_last: k == roots.len() - 1,
                gutters: Vec::new(),
                virtual_root_child: mr,
            });
        }
        while let Some(f) = stack.pop() {
            self.visual[f.node] = Visual {
                indent: f.indent,
                show_connector: f.show_connector,
                is_last: f.is_last,
                virtual_root_child: f.virtual_root_child,
                gutters: f.gutters.clone(),
            };
            let children = self.vchildren[f.node].clone();
            let multiple = children.len() > 1;
            let child_indent = if multiple || (f.just_branched && f.indent > 0) {
                f.indent + 1
            } else {
                f.indent
            };
            let connector_displayed = f.show_connector && !f.virtual_root_child;
            let display = if mr { f.indent.saturating_sub(1) } else { f.indent };
            let connector_pos = display.saturating_sub(1);
            let mut child_gutters = f.gutters.clone();
            if connector_displayed {
                child_gutters.push(Gutter {
                    position: connector_pos,
                    show: !f.is_last,
                });
            }
            for (k, &c) in children.iter().enumerate().rev() {
                stack.push(Frame {
                    node: c,
                    indent: child_indent,
                    just_branched: multiple,
                    show_connector: multiple,
                    is_last: k == children.len() - 1,
                    gutters: child_gutters.clone(),
                    virtual_root_child: false,
                });
            }
        }
    }

    fn status_labels(&self) -> String {
        let mut s = String::new();
        match self.filter {
            TreeFilter::Default => {}
            TreeFilter::NoTools => s.push_str(" [no-tools]"),
            TreeFilter::UserOnly => s.push_str(" [user]"),
            TreeFilter::LabeledOnly => s.push_str(" [labeled]"),
            TreeFilter::All => s.push_str(" [all]"),
        }
        if self.show_label_ts {
            s.push_str(" [+label time]");
        }
        s
    }

    fn row(&self, i: usize, selected: bool, width: usize, theme: &Theme) -> String {
        let node = &self.nodes[i];
        let v = &self.visual[i];
        let display = if self.multiple_roots {
            v.indent.saturating_sub(1)
        } else {
            v.indent
        };
        let connector = v.show_connector && !v.virtual_root_child;
        let connector_pos = if connector { display.checked_sub(1) } else { None };
        let folded = self.folded.contains(&node.id);
        let foldable = self.foldable(i);
        let mut prefix = String::new();
        for ch in 0..display * 3 {
            let level = ch / 3;
            let pos = ch % 3;
            if let Some(g) = v.gutters.iter().find(|g| g.position == level) {
                prefix.push(if pos == 0 && g.show { '│' } else { ' ' });
            } else if connector_pos == Some(level) {
                prefix.push(match pos {
                    0 => {
                        if v.is_last {
                            '└'
                        } else {
                            '├'
                        }
                    }
                    1 => {
                        if folded {
                            '⊞'
                        } else if foldable {
                            '⊟'
                        } else {
                            '─'
                        }
                    }
                    _ => ' ',
                });
            } else {
                prefix.push(' ');
            }
        }
        let fold_marker = if folded && !connector {
            theme.fg("accent", "⊞ ")
        } else {
            String::new()
        };
        let path_marker = if node.on_active_path {
            theme.fg("accent", "• ")
        } else {
            String::new()
        };
        let label = match &node.label {
            Some(l) => theme.fg("warning", &format!("[{l}] ")),
            None => String::new(),
        };
        let label_ts = match (self.show_label_ts, node.label.is_some(), node.label_ts) {
            (true, true, Some(ts)) => theme.fg("muted", &format!("{} ", format_label_ts(ts, self.now_ms))),
            _ => String::new(),
        };
        let mut content = String::new();
        for (tok, text) in &node.preview {
            content.push_str(&if tok.is_empty() {
                text.clone()
            } else {
                theme.fg(tok, text)
            });
        }
        if selected {
            content = theme.bold(&content);
        }
        let body_prefix = format!("{}{}{}", theme.fg("dim", &prefix), fold_marker, path_marker);
        let mut gutter = if selected {
            theme.fg("accent", "› ")
        } else {
            "  ".to_string()
        };
        let mut body = format!("{body_prefix}{label}{label_ts}{content}");
        if selected {
            gutter = theme.bg("selectedBg", &gutter);
            body = theme.bg("selectedBg", &body);
        }
        truncate_to_width(&format!("{gutter}{body}"), width, "")
    }
}

impl ForkPointSelector {
    pub fn new(tree: &Tree, max_rows: usize) -> ForkPointSelector {
        let items: Vec<(String, String)> = tree
            .branch(tree.leaf())
            .into_iter()
            .filter_map(|e| match &e.body {
                EntryBody::Msg { wire, .. } if wire.role == Role::User => {
                    Some((e.id.clone(), wire.content.clone().unwrap_or_default()))
                }
                _ => None,
            })
            .collect();
        // Pi: the most recent message is selected first.
        let cursor = items.len().saturating_sub(1);
        ForkPointSelector {
            items,
            cursor,
            max_rows: max_rows.max(1),
        }
    }

    pub fn key(&mut self, key: &Key, keymap: &Keymap) -> Outcome<(String, String)> {
        if key.kind == KeyKind::Release {
            return Outcome::Ignored;
        }
        let n = self.items.len();
        if bound_key(key, keymap, "tui.select.up") {
            if n > 0 {
                self.cursor = if self.cursor == 0 { n - 1 } else { self.cursor - 1 };
            }
        } else if bound_key(key, keymap, "tui.select.down") {
            if n > 0 {
                self.cursor = if self.cursor + 1 >= n { 0 } else { self.cursor + 1 };
            }
        } else if bound_key(key, keymap, "tui.select.pageUp") {
            self.cursor = self.cursor.saturating_sub(self.max_rows);
        } else if bound_key(key, keymap, "tui.select.pageDown") {
            if n > 0 {
                self.cursor = (self.cursor + self.max_rows).min(n - 1);
            }
        } else if bound_key(key, keymap, "tui.select.confirm") {
            return match self.items.get(self.cursor) {
                Some((id, text)) => Outcome::Done((id.clone(), text.clone())),
                None => Outcome::Ignored,
            };
        } else if bound_key(key, keymap, "tui.select.cancel") {
            return Outcome::Cancelled;
        } else {
            return Outcome::Ignored;
        }
        Outcome::Pending
    }

    pub fn render(&mut self, width: u16, theme: &Theme) -> Vec<String> {
        let width = width as usize;
        let mut lines = vec![
            String::new(),
            theme.bold("Fork from Message"),
            theme.fg(
                "muted",
                "Select a user message to copy the active path up to that point into a new session",
            ),
            String::new(),
        ];
        let n = self.items.len();
        if n == 0 {
            lines.push(theme.fg("muted", "  No user messages found"));
            return lines
                .into_iter()
                .map(|l| truncate_to_width(&l, width, ""))
                .collect();
        }
        let (start, end) = window(self.cursor, n, self.max_rows);
        for k in start..end {
            let text: String = self.items[k]
                .1
                .chars()
                .map(|c| if c.is_control() { ' ' } else { c })
                .collect();
            let text = text.trim().to_string();
            let selected = k == self.cursor;
            let cursor = if selected {
                theme.fg("accent", "› ")
            } else {
                "  ".to_string()
            };
            let shown = if selected { theme.bold(&text) } else { text };
            lines.push(truncate_to_width(&format!("{cursor}{shown}"), width, ""));
            lines.push(theme.fg("muted", &format!("  Message {} of {}", k + 1, n)));
            lines.push(String::new());
        }
        if start > 0 || end < n {
            lines.push(theme.fg("muted", &format!("  ({}/{})", self.cursor + 1, n)));
        }
        lines
            .into_iter()
            .map(|l| truncate_to_width(&l, width, ""))
            .collect()
    }
}

// ---- helpers ---------------------------------------------------------------------------

/// pi's scroll window: the selected row sits in the middle when possible.
fn window(cursor: usize, n: usize, max: usize) -> (usize, usize) {
    let start = (cursor as isize - (max / 2) as isize)
        .min(n as isize - max as isize)
        .max(0) as usize;
    (start, (start + max).min(n))
}

/// `true` when `key` is bound to `action`.
fn bound_key(key: &Key, keymap: &Keymap, action: &str) -> bool {
    keymap.matches(&key.id, action)
}

fn printable(key: &Key) -> Option<&str> {
    let text = key.text.as_deref()?;
    if text.is_empty() || text.chars().any(|c| c.is_control()) {
        None
    } else {
        Some(text)
    }
}

/// Pi's filter modes over one node. The assistant rule (hide replies without text unless they
/// are the leaf) applies before the filter, as in pi.
fn passes_filter(node: &Node, filter: TreeFilter) -> bool {
    match node.class {
        Class::Usage => return false,
        Class::Assistant { text: false } if !node.is_leaf => return false,
        _ => {}
    }
    let settings = node.class == Class::Settings;
    match filter {
        TreeFilter::All => true,
        TreeFilter::UserOnly => node.class == Class::User,
        TreeFilter::LabeledOnly => node.label.is_some(),
        TreeFilter::Default => !settings,
        TreeFilter::NoTools => !settings && node.class != Class::ToolResult,
    }
}

/// Flatten a tree in pi's order: roots and children with the active branch first, then file
/// order. Parents always come before their children in the result.
fn build_nodes(tree: &Tree) -> Vec<Node> {
    let entries = tree.entries();
    let n = entries.len();
    let mut file_index: HashMap<&str, usize> = HashMap::new();
    for (i, e) in entries.iter().enumerate() {
        file_index.entry(e.id.as_str()).or_insert(i);
    }
    let parent_of: Vec<Option<usize>> = entries
        .iter()
        .map(|e| e.parent.as_deref().and_then(|p| file_index.get(p).copied()))
        .collect();
    let mut children_of: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, p) in parent_of.iter().enumerate() {
        if let Some(p) = p {
            if *p != i {
                children_of[*p].push(i);
            }
        }
    }

    // The active path: leaf up to its root (guarded against malformed cycles).
    let mut on_path = vec![false; n];
    if let Some(&leaf) = file_index.get(tree.leaf()) {
        let mut cur = Some(leaf);
        let mut steps = 0;
        while let Some(c) = cur {
            if on_path[c] || steps > n {
                break;
            }
            on_path[c] = true;
            cur = parent_of[c];
            steps += 1;
        }
    }
    let by_path = |a: &usize, b: &usize| on_path[*b].cmp(&on_path[*a]);
    for kids in children_of.iter_mut() {
        kids.sort_by(by_path);
    }

    // Labels: the last Label entry for a target wins; `None` clears it.
    let mut labels: HashMap<&str, (Option<String>, u64)> = HashMap::new();
    for e in entries {
        if let EntryBody::Label { target, label } = &e.body {
            labels.insert(target.as_str(), (label.clone(), e.ts_ms));
        }
    }
    // Tool calls by id, for the names of tool results.
    let mut calls: HashMap<&str, (&str, &str)> = HashMap::new();
    for e in entries {
        if let EntryBody::Msg { wire, .. } = &e.body {
            for c in &wire.tool_calls {
                calls.insert(c.id.as_str(), (c.name.as_str(), c.args_raw.as_str()));
            }
        }
    }

    // Preorder from the roots (in pi's order), then from any entry a malformed file left
    // unreached (a parent cycle): that entry is shown as a root. Visiting is cycle-safe.
    let mut ordered: Vec<usize> = Vec::with_capacity(n);
    let mut visited = vec![false; n];
    let mut treated_root = vec![false; n];
    let roots: Vec<usize> = {
        let mut r: Vec<usize> = (0..n).filter(|&i| parent_of[i].is_none()).collect();
        r.sort_by(by_path);
        r
    };
    for start in roots.into_iter().chain(0..n) {
        if visited[start] {
            continue;
        }
        if parent_of[start].is_some() {
            treated_root[start] = true;
        }
        let mut stack = vec![start];
        while let Some(c) = stack.pop() {
            if visited[c] {
                continue;
            }
            visited[c] = true;
            ordered.push(c);
            for &k in children_of[c].iter().rev() {
                if !visited[k] {
                    stack.push(k);
                }
            }
        }
    }
    let mut pos = vec![usize::MAX; n];
    for (k, &i) in ordered.iter().enumerate() {
        pos[i] = k;
    }
    let mut nodes: Vec<Node> = ordered
        .iter()
        .map(|&i| {
            let e: &Entry = &entries[i];
            let (class, preview, user_text) = describe(e, &calls);
            let plain: String = preview.iter().map(|(_, t)| t.as_str()).collect();
            let (label, label_ts) = match labels.get(e.id.as_str()) {
                Some((Some(l), ts)) => (Some(l.clone()), Some(*ts)),
                _ => (None, None),
            };
            let mut search = String::new();
            if let Some(l) = &label {
                search.push_str(l);
                search.push(' ');
            }
            search.push_str(&plain);
            Node {
                id: e.id.clone(),
                parent: None,
                class,
                is_leaf: e.id == tree.leaf(),
                on_active_path: on_path[i],
                label,
                label_ts,
                preview,
                search: search.to_lowercase(),
                user_text,
                copy_text: copy_text(e),
            }
        })
        .collect();
    for (k, &i) in ordered.iter().enumerate() {
        if treated_root[i] {
            nodes[k].parent = None;
        } else if let Some(p) = parent_of[i] {
            // Parents come before children in preorder, so `pos[p] < k` for every real link.
            nodes[k].parent = Some(pos[p]);
        }
    }
    nodes
}

/// The full text of an entry for `app.message.copy` (pi `getEntryCopyText`): message text
/// (an assistant reply without text: none), the command of a `!cmd`, summaries, custom
/// message content. Bookkeeping entries have none.
fn copy_text(e: &Entry) -> Option<String> {
    let text = match &e.body {
        EntryBody::Msg { wire, .. } => wire.content.clone(),
        EntryBody::Shown { text, .. } => Some(text.clone()),
        EntryBody::BashExecution { command, .. } => Some(command.clone()),
        EntryBody::Compaction { summary, .. } | EntryBody::BranchSummary { summary, .. } => {
            Some(summary.clone())
        }
        EntryBody::CustomMessage { content, .. } => Some(content.clone()),
        _ => None,
    };
    text.filter(|t| !t.trim().is_empty())
}

/// Class, styled preview and user text of one entry (pi's one-line previews).
fn describe(
    e: &Entry,
    calls: &HashMap<&str, (&str, &str)>,
) -> (Class, Vec<(&'static str, String)>, Option<String>) {
    match &e.body {
        EntryBody::Msg { wire, .. } => describe_msg(wire, calls),
        EntryBody::Shown { text, why } => {
            let preview = match *why {
                "aborted" => vec![
                    ("success", "assistant: ".to_string()),
                    ("muted", "(aborted)".to_string()),
                ],
                "failed" => vec![
                    ("success", "assistant: ".to_string()),
                    ("error", format!("(failed) {}", cut(&norm(text), 120))),
                ],
                other => vec![("warning", format!("[{other}]: ")), ("", cut(&norm(text), 160))],
            };
            (Class::Other, preview, None)
        }
        EntryBody::BashExecution {
            command, excluded, ..
        } => {
            let tail = if *excluded { " (excluded)" } else { "" };
            (
                Class::Other,
                vec![("dim", format!("[bash]: {}{tail}", cut(&norm(command), 120)))],
                None,
            )
        }
        EntryBody::ModelChange(model) => (Class::Settings, vec![("dim", format!("[model: {model}]"))], None),
        EntryBody::ThinkingLevelChange(level) => (
            Class::Settings,
            vec![("dim", format!("[thinking: {}]", level.as_str()))],
            None,
        ),
        EntryBody::ModeChange(mode) => (
            Class::Settings,
            vec![("dim", format!("[mode: {}]", mode.as_str()))],
            None,
        ),
        EntryBody::Prefix(_) => (
            Class::Settings,
            vec![("dim", "[system prompt]".to_string())],
            None,
        ),
        EntryBody::Usage { .. } => (Class::Usage, vec![("dim", "[usage]".to_string())], None),
        EntryBody::Compaction { tokens_before, .. } => (
            Class::Other,
            vec![(
                "borderAccent",
                format!("[compaction: {}k tokens]", (tokens_before + 500) / 1000),
            )],
            None,
        ),
        EntryBody::ContextEdit { target, replacement } => {
            let how = if replacement.is_none() { "omit" } else { "replace" };
            (
                Class::Settings,
                vec![("dim", format!("[context {how}: {target}]"))],
                None,
            )
        }
        EntryBody::BranchSummary { summary, .. } => (
            Class::Other,
            vec![
                ("warning", "[branch summary]: ".to_string()),
                ("", cut(&norm(summary), 200)),
            ],
            None,
        ),
        EntryBody::Custom { custom_type, .. } => (
            Class::Settings,
            vec![("dim", format!("[custom: {custom_type}]"))],
            None,
        ),
        EntryBody::CustomMessage {
            custom_type, content, ..
        } => (
            Class::Other,
            vec![
                ("accent", format!("[{custom_type}]: ")),
                ("", cut(&norm(content), 200)),
            ],
            None,
        ),
        EntryBody::Label { label, .. } => (
            Class::Settings,
            vec![(
                "dim",
                format!("[label: {}]", label.as_deref().unwrap_or("(cleared)")),
            )],
            None,
        ),
        EntryBody::SessionInfo { name } => {
            let text = if name.is_empty() {
                "[title: empty]".to_string()
            } else {
                format!("[title: {name}]")
            };
            (Class::Settings, vec![("dim", text)], None)
        }
    }
}

fn describe_msg(
    wire: &WireMsg,
    calls: &HashMap<&str, (&str, &str)>,
) -> (Class, Vec<(&'static str, String)>, Option<String>) {
    let text = wire.content.clone().unwrap_or_default();
    match wire.role {
        Role::User => (
            Class::User,
            vec![("accent", "user: ".to_string()), ("", cut(&norm(&text), 200))],
            Some(text),
        ),
        Role::Assistant => {
            let has_text = !text.trim().is_empty();
            let preview = if has_text {
                vec![
                    ("success", "assistant: ".to_string()),
                    ("", cut(&norm(&text), 200)),
                ]
            } else if !wire.tool_calls.is_empty() {
                let names: Vec<String> = wire
                    .tool_calls
                    .iter()
                    .map(|c| format_call(&c.name, &c.args_raw))
                    .collect();
                vec![("success", "assistant: ".to_string()), ("muted", names.join(" "))]
            } else {
                vec![
                    ("success", "assistant: ".to_string()),
                    ("muted", "(no content)".to_string()),
                ]
            };
            (Class::Assistant { text: has_text }, preview, None)
        }
        Role::Tool => {
            let shown = match wire.tool_call_id.as_deref().and_then(|id| calls.get(id)) {
                Some((name, args)) => format_call(name, args),
                None => "[tool]".to_string(),
            };
            (Class::ToolResult, vec![("muted", shown)], None)
        }
        Role::System => (Class::Other, vec![("dim", "[system]".to_string())], None),
    }
}

/// pi's `formatToolCall`: a short bracketed form of a tool call.
fn format_call(name: &str, args_raw: &str) -> String {
    let args: serde_json::Value = serde_json::from_str(args_raw).unwrap_or(serde_json::Value::Null);
    let text = |key: &str| args.get(key).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let path = |default: &str| {
        let p = [text("path"), text("file_path")]
            .into_iter()
            .find(|s| !s.is_empty())
            .unwrap_or_else(|| default.to_string());
        norm(&p)
    };
    match name {
        "read" | "write" | "edit" => format!("[{name}: {}]", path("")),
        "bash" => {
            let raw = norm(&text("command"));
            let more = if raw.chars().count() > 50 { "..." } else { "" };
            format!("[bash: {}{more}]", cut(&raw, 50))
        }
        "grep" => {
            let pattern = text("pattern");
            format!("[grep: /{pattern}/ in {}]", path("."))
        }
        "find" => format!("[find: {} in {}]", text("pattern"), path(".")),
        "ls" => format!("[ls: {}]", path(".")),
        other => {
            let json = args.to_string();
            let more = if json.chars().count() > 40 { "..." } else { "" };
            format!("[{other}: {}{more}]", cut(&json, 40))
        }
    }
}

/// One line: control characters become spaces, the ends are trimmed.
fn norm(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_string()
}

/// The first `n` characters.
fn cut(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// `HH:MM` when the label was set today, `M/D HH:MM` in the same year, else `YY/M/D HH:MM`.
/// UTC, so the text does not depend on the machine's zone.
fn format_label_ts(ts_ms: u64, now_ms: u64) -> String {
    let (y, m, d) = civil_from_days((ts_ms / 86_400_000) as i64);
    let (ny, nm, nd) = civil_from_days((now_ms / 86_400_000) as i64);
    let sod = (ts_ms / 1000) % 86_400;
    let time = format!("{:02}:{:02}", sod / 3600, (sod % 3600) / 60);
    if (y, m, d) == (ny, nm, nd) {
        time
    } else if y == ny {
        format!("{m}/{d} {time}")
    } else {
        format!("{:02}/{m}/{d} {time}", y.rem_euclid(100))
    }
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// pi's `TREE_HELP_ITEMS`, each with the first key of its actions (`formatHelpKeys`), joined
/// with ` · ` and wrapped at `width` (indent 2).
fn tree_help(keymap: &Keymap, width: usize) -> Vec<String> {
    let items: [(&[&str], &str, bool); 8] = [
        (&["tui.select.up", "tui.select.down"], "move", false),
        (
            &["tui.editor.cursorLeft", "tui.editor.cursorRight"],
            "page",
            false,
        ),
        (&["app.tree.foldOrUp", "app.tree.unfoldOrDown"], "branch", false),
        (&["app.message.copy"], "copy", false),
        (&["app.tree.editLabel"], "label", false),
        (&["app.tree.toggleLabelTimestamp"], "label time", false),
        (
            &[
                "app.tree.filter.default",
                "app.tree.filter.noTools",
                "app.tree.filter.userOnly",
                "app.tree.filter.labeledOnly",
                "app.tree.filter.all",
            ],
            "filters",
            true,
        ),
        (
            &["app.tree.filter.cycleForward", "app.tree.filter.cycleBackward"],
            "cycle",
            true,
        ),
    ];
    let texts: Vec<String> = items
        .iter()
        .map(|(actions, label, label_first)| {
            let keys: Vec<String> = actions
                .iter()
                .filter_map(|a| keymap.keys_for(a).first().map(|k| k.to_string()))
                .collect();
            let keys = help_keys(&keys);
            match (keys.is_empty(), label_first) {
                (true, _) => label.to_string(),
                (false, true) => format!("{label} {keys}"),
                (false, false) => format!("{keys} {label}"),
            }
        })
        .collect();
    let mut lines = Vec::new();
    let mut cur = String::new();
    for item in texts {
        let candidate = if cur.is_empty() {
            format!("  {item}")
        } else {
            format!("{cur} · {item}")
        };
        if cur.is_empty() || oxa_tui::visible_width(&candidate) <= width {
            cur = candidate;
        } else {
            lines.push(std::mem::take(&mut cur));
            cur = format!("  {item}");
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
        .into_iter()
        .map(|l| truncate_to_width(&l, width, ""))
        .collect()
}

/// pi `formatHelpKeys` + `compactRawKeys`: keys with one shared modifier prefix are joined
/// after it (`ctrl+d/t/u`), arrows become symbols.
fn help_keys(keys: &[String]) -> String {
    if keys.is_empty() {
        return String::new();
    }
    let split = |k: &str| match k.rfind('+') {
        Some(i) if i + 1 < k.len() => (k[..=i].to_string(), k[i + 1..].to_string()),
        _ => (String::new(), k.to_string()),
    };
    let joined = if keys.len() == 1 {
        keys[0].clone()
    } else {
        let parts: Vec<(String, String)> = keys.iter().map(|k| split(k)).collect();
        let prefix = parts[0].0.clone();
        if !prefix.is_empty() && parts.iter().all(|(p, _)| *p == prefix) {
            let suffixes: Vec<&str> = parts.iter().map(|(_, s)| s.as_str()).collect();
            format!("{prefix}{}", suffixes.join("/"))
        } else {
            keys.join("/")
        }
    };
    let mut out = String::new();
    // Replace whole words only (pi's `\bup\b` ...).
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        let w = match word.as_str() {
            "pageUp" => "pgup",
            "pageDown" => "pgdn",
            "up" => "↑",
            "down" => "↓",
            "left" => "←",
            "right" => "→",
            other => other,
        }
        .to_string();
        out.push_str(&w);
        word.clear();
    };
    for c in joined.chars() {
        if c.is_alphanumeric() {
            word.push(c);
        } else {
            flush(&mut word, &mut out);
            out.push(c);
        }
    }
    flush(&mut word, &mut out);
    out
}
