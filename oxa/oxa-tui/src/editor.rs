//! B10: the multi-line prompt editor.
// ported from pi packages/tui/src/components/editor.ts, kill-ring.ts, undo-stack.ts, word-navigation.ts
use crate::{
    cluster_boundaries, pad_to_width, truncate_to_width, visible_width, AutocompleteItem,
    AutocompleteProvider, Component, CursorPos, Interactive, Key, KeyKind, Mouse, PopupStyle, Theme,
};
use oxa_config::Keymap;
use std::collections::BTreeMap;

/// Editor options (from settings).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorOpts {
    /// Horizontal padding inside the border, in columns.
    pub padding_x: u16,
    /// Maximum number of visible suggestion rows (`autocompleteMaxVisible`).
    pub autocomplete_max_visible: usize,
    /// Maximum number of visible text rows before the editor scrolls.
    pub max_visible_rows: usize,
    /// Theme token of the border (`border`, a thinking-level token, `bashMode`).
    pub border_token: String,
    /// A paste longer than this many lines or 1000 characters is shown as a paste marker
    /// (`[paste #1 +12 lines]`) instead of its text.
    pub paste_marker_min_lines: usize,
    /// How the suggestions look: pi's bare list (the default) or OXA's panel.
    pub popup: PopupStyle,
    /// `true`: [`Component::render`] leaves the suggestions out and the owner draws
    /// [`Editor::popup`] itself (OXA's fullscreen layout floats it over the transcript, so an
    /// opening list moves nothing).
    pub popup_detached: bool,
}

impl Default for EditorOpts {
    fn default() -> Self {
        EditorOpts {
            padding_x: 0,
            autocomplete_max_visible: 5,
            max_visible_rows: 20,
            border_token: "border".to_string(),
            paste_marker_min_lines: 10,
            popup: PopupStyle::List,
            popup_detached: false,
        }
    }
}

/// What a key did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorAction {
    /// Not an editor key; the caller may use it.
    Ignored,
    /// Consumed; text or cursor may have changed.
    Changed,
    /// The submit key was pressed; carries [`Editor::expanded_text`]. The editor is cleared and
    /// the text is added to the history.
    Submit(String),
}

/// Byte positions `(line, byte offset)`.
type Pos = (usize, usize);

/// Maximum number of history entries kept.
const HISTORY_MAX: usize = 100;
/// Pastes longer than this many characters become markers (pi: 1000).
const PASTE_MARKER_MIN_CHARS: usize = 1000;
/// Column width assumed before the first render.
const DEFAULT_WIDTH: usize = 80;

/// The document: logical lines and a cursor. Snapshots of this are the undo stack.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Text {
    lines: Vec<String>,
    line: usize,
    /// Byte offset in `lines[line]`.
    col: usize,
}

impl Text {
    fn from_text(s: &str) -> Text {
        let lines: Vec<String> = s.split('\n').map(str::to_string).collect();
        let line = lines.len() - 1;
        let col = lines[line].len();
        Text { lines, line, col }
    }
}

/// What the previous key did; kills accumulate and typed words coalesce only in a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Last {
    Other,
    Kill,
    Yank,
    TypeWord,
}

/// Character class used by word movement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Ws,
    Word,
    Punct,
    /// A paste marker: one unit.
    Atomic,
}

/// One visual row: byte range `start..end` of logical line `line`.
#[derive(Debug, Clone, Copy)]
struct Row {
    line: usize,
    start: usize,
    end: usize,
}

/// An open suggestion list.
struct Suggest {
    items: Vec<AutocompleteItem>,
    prefix: String,
    selected: usize,
    /// The user moved the selection (up / down / page). Until then Enter sends the text as
    /// typed; only a moved selection (or Tab) is applied.
    touched: bool,
}

/// The prompt editor.
///
/// Keys are resolved **only** through the `Keymap` (`tui.editor.*`, `tui.input.*` and
/// `tui.select.*` action ids of keybindings.md); the editor hard-codes no key.
///
/// Contract (acceptance, plan B10):
/// * key tables: cursor movement by character / word / line / document, delete char / word /
///   to line start / to line end, newline vs submit, tab;
/// * **kill ring**: kills accumulate while consecutive, `yank` inserts the newest, `yankPop`
///   cycles; a non-kill action breaks accumulation;
/// * **undo**: one step per word-sized typing burst, per paste, per kill; undo restores text
///   and cursor;
/// * **history**: up/down at the first/last line walk the history; the draft is kept;
/// * **paste markers** survive deletion of neighbouring text, are deleted as one unit by
///   backspace, and [`Editor::expanded_text`] replaces each by its full content;
/// * **IME cursor**: [`Interactive::cursor`] reports the exact cell of the cursor so the
///   terminal places the IME window there;
/// * cursor movement and deletion work on grapheme clusters ([`crate::cluster_boundaries`]).
pub struct Editor {
    opts: EditorOpts,
    st: Text,
    /// Paste id -> full content, for the markers present in the text.
    pastes: BTreeMap<u32, String>,
    paste_counter: u32,
    /// Kill ring, newest last.
    kill: Vec<String>,
    last: Last,
    /// Where the last yank was inserted (for yank-pop).
    yank_span: Option<(Pos, Pos)>,
    undo: Vec<Text>,
    /// Submitted entries, newest last, at most [`HISTORY_MAX`].
    history: Vec<String>,
    /// Index into `history` while browsing; `None` = editing the draft.
    hist_pos: Option<usize>,
    hist_draft: Option<Text>,
    provider: Option<Box<dyn AutocompleteProvider>>,
    suggest: Option<Suggest>,
    keymap: Keymap,
    action: EditorAction,
    /// `Some(true)` after the forward jump key, `Some(false)` after the backward one.
    jump: Option<bool>,
    /// Visual column kept across up/down moves.
    preferred_col: Option<usize>,
    /// First visible visual row.
    scroll: usize,
    last_width: usize,
    focused: bool,
    /// Hardware cursor cell as of the last render.
    cursor: Option<CursorPos>,
}

impl Editor {
    pub fn new(opts: EditorOpts) -> Editor {
        Editor {
            opts,
            st: Text::from_text(""),
            pastes: BTreeMap::new(),
            paste_counter: 0,
            kill: Vec::new(),
            last: Last::Other,
            yank_span: None,
            undo: Vec::new(),
            history: Vec::new(),
            hist_pos: None,
            hist_draft: None,
            provider: None,
            suggest: None,
            keymap: Keymap::default(),
            action: EditorAction::Ignored,
            jump: None,
            preferred_col: None,
            scroll: 0,
            last_width: DEFAULT_WIDTH,
            focused: false,
            cursor: None,
        }
    }
    pub fn set_opts(&mut self, opts: EditorOpts) {
        self.opts = opts;
    }
    /// The text as shown (paste markers as markers), lines joined by `\n`.
    pub fn text(&self) -> String {
        self.st.lines.join("\n")
    }
    /// The text with every paste marker replaced by its content.
    pub fn expanded_text(&self) -> String {
        expand(&self.text(), &self.pastes)
    }
    /// Put `prefix` in front of the current content, keeping paste contents.
    pub fn prepend_text(&mut self, prefix: &str) {
        let current = self.text();
        if current.is_empty() {
            self.set_text(prefix);
            return;
        }
        let (pastes, counter) = (self.pastes.clone(), self.paste_counter);
        self.set_text(&format!("{prefix}\n\n{current}"));
        self.pastes = pastes;
        self.paste_counter = counter;
    }
    /// Replace the content (one undo step); cursor to the end.
    pub fn set_text(&mut self, text: &str) {
        self.suggest = None;
        self.last = Last::Other;
        self.exit_history();
        let norm = normalize(text);
        if self.text() != norm {
            self.push_undo();
        }
        self.pastes.clear();
        self.paste_counter = 0;
        self.st = Text::from_text(&norm);
    }
    /// Insert at the cursor as typed text (one undo step, no paste marker).
    pub fn insert(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.suggest = None;
        self.exit_history();
        self.push_undo();
        self.last = Last::Other;
        let norm = normalize(text);
        self.insert_str(&norm);
    }
    pub fn is_empty(&self) -> bool {
        self.st.lines.len() == 1 && self.st.lines[0].is_empty()
    }
    /// `(line index, byte offset in that line)`.
    pub fn cursor_pos(&self) -> (usize, usize) {
        (self.st.line, self.st.col)
    }
    /// Handle a key through `keymap`.
    pub fn key(&mut self, key: &Key, keymap: &Keymap) -> EditorAction {
        if key.kind == KeyKind::Release {
            return EditorAction::Ignored;
        }
        self.dispatch(key, keymap)
    }
    /// A bracketed paste: short pastes insert text, long ones insert a paste marker.
    pub fn paste_text(&mut self, text: &str) {
        let clean = normalize(text);
        if clean.is_empty() {
            return;
        }
        self.suggest = None;
        self.exit_history();
        self.last = Last::Other;
        self.push_undo();

        let mut filtered = clean;
        // A path pasted right after a word character gets a separating space (pi).
        if filtered.starts_with(['/', '~', '.']) {
            let (l, c) = (self.st.line, self.st.col);
            let prev = self.st.lines[l][..c].chars().last();
            if prev.is_some_and(|ch| ch.is_alphanumeric() || ch == '_') {
                filtered = format!(" {filtered}");
            }
        }
        let lines = filtered.matches('\n').count() + 1;
        let chars = filtered.chars().count();
        let to_insert = if lines > self.opts.paste_marker_min_lines || chars > PASTE_MARKER_MIN_CHARS {
            self.paste_counter += 1;
            let id = self.paste_counter;
            let marker = if lines > self.opts.paste_marker_min_lines {
                format!("[paste #{id} +{lines} lines]")
            } else {
                format!("[paste #{id} {chars} chars]")
            };
            self.pastes.insert(id, filtered);
            marker
        } else {
            filtered
        };
        self.insert_str(&to_insert);
    }
    /// Add an entry to the history (newest last; consecutive duplicates are dropped; at most
    /// 100 entries).
    pub fn add_history(&mut self, text: &str) {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        if self.history.last().map(String::as_str) == Some(trimmed) {
            return;
        }
        self.history.push(trimmed.to_string());
        if self.history.len() > HISTORY_MAX {
            self.history.remove(0);
        }
    }
    /// Install (or remove) the completion source. While suggestions are open, up/down/tab/enter
    /// and escape go to the list (rendered below the text with `SelectList`).
    pub fn set_autocomplete(&mut self, provider: Option<Box<dyn AutocompleteProvider>>) {
        self.provider = provider;
        self.suggest = None;
    }
    pub fn autocomplete_open(&self) -> bool {
        self.suggest.is_some()
    }
    /// The open suggestions as lines of `width` columns (empty when none are open): what
    /// `render` appends below the editor unless `EditorOpts::popup_detached`.
    pub fn popup(&self, width: u16, theme: &Theme) -> Vec<String> {
        self.suggest_popup((width as usize).max(1), theme)
    }
    /// The keymap used by `Component::input` (which has no keymap parameter). Must be called
    /// once before the editor is used as a plain `Component`.
    pub fn set_keymap(&mut self, keymap: Keymap) {
        self.keymap = keymap;
    }
    /// The action of the last `Component::input` call (so `Submit` is not lost there).
    pub fn take_action(&mut self) -> EditorAction {
        std::mem::replace(&mut self.action, EditorAction::Ignored)
    }

    // ---- key dispatch -------------------------------------------------------------------

    fn dispatch(&mut self, key: &Key, km: &Keymap) -> EditorAction {
        let is = |a: &str| km.matches(&key.id, a);
        use EditorAction::{Changed, Ignored};

        if let Some(forward) = self.jump.take() {
            if is("tui.editor.jumpForward") || is("tui.editor.jumpBackward") {
                return Changed; // pressing the jump key again cancels
            }
            if let Some(t) = key
                .text
                .clone()
                .filter(|t| !t.is_empty() && !t.chars().any(|c| c < ' '))
            {
                self.jump_to(&t, forward);
                return Changed;
            }
            // A control key cancels jump mode and is handled normally below.
        }

        if is("tui.input.copy") {
            return Ignored; // the parent handles copy / exit
        }
        if is("tui.editor.undo") {
            self.do_undo();
            return Changed;
        }

        if self.suggest.is_some() {
            if is("tui.select.cancel") {
                self.suggest = None;
                return Changed;
            }
            if is("tui.select.up") {
                self.suggest_move(-1, true);
                return Changed;
            }
            if is("tui.select.down") {
                self.suggest_move(1, true);
                return Changed;
            }
            let page = self.opts.autocomplete_max_visible.max(1) as isize;
            if is("tui.select.pageUp") {
                self.suggest_move(-page, false);
                return Changed;
            }
            if is("tui.select.pageDown") {
                self.suggest_move(page, false);
                return Changed;
            }
            if is("tui.input.tab") {
                self.accept_suggestion();
                return Changed;
            }
            if is("tui.select.confirm") {
                let (slash, touched) = self
                    .suggest
                    .as_ref()
                    .map_or((false, false), |s| (s.prefix.starts_with('/'), s.touched));
                // A slash command (its name or an argument) keeps pi's Enter. A file list (`@`,
                // a path, Tab) never replaces what was typed unless the user moved the
                // selection: Enter sends the text as typed.
                if !self.in_slash_command() && !touched {
                    self.suggest = None;
                    return self.submit();
                }
                if self.accept_suggestion() && !slash {
                    return Changed;
                }
                // A slash command is submitted as completed (pi).
                return self.submit();
            }
        }

        if is("tui.input.tab") {
            if self.provider.is_none() {
                return Ignored;
            }
            self.refresh_suggestions(true);
            // pi: a Tab with exactly one candidate applies it directly.
            if self.suggest.as_ref().is_some_and(|s| s.items.len() == 1) {
                self.accept_suggestion();
            }
            return Changed;
        }

        if is("tui.editor.deleteToLineEnd") {
            return if self.kill_to_line_end() { Changed } else { Ignored };
        }
        if is("tui.editor.deleteToLineStart") {
            return if self.kill_to_line_start() {
                Changed
            } else {
                Ignored
            };
        }
        if is("tui.editor.deleteWordBackward") {
            return if self.kill_word_backward() {
                Changed
            } else {
                Ignored
            };
        }
        if is("tui.editor.deleteWordForward") {
            return if self.kill_word_forward() {
                Changed
            } else {
                Ignored
            };
        }
        if is("tui.editor.deleteCharBackward") {
            return if self.backspace() { Changed } else { Ignored };
        }
        if is("tui.editor.deleteCharForward") {
            return if self.forward_delete() { Changed } else { Ignored };
        }
        if is("tui.editor.yank") {
            return if self.yank() { Changed } else { Ignored };
        }
        if is("tui.editor.yankPop") {
            return if self.yank_pop() { Changed } else { Ignored };
        }

        if is("tui.editor.historyPrevious") {
            self.history_nav(-1);
            return Changed;
        }
        if is("tui.editor.historyNext") {
            self.history_nav(1);
            return Changed;
        }

        if is("tui.editor.cursorLineStart") {
            self.cursor_line_start();
            return Changed;
        }
        if is("tui.editor.cursorLineEnd") {
            self.cursor_line_end();
            return Changed;
        }
        if is("tui.editor.cursorWordLeft") {
            self.cursor_word_left();
            return Changed;
        }
        if is("tui.editor.cursorWordRight") {
            self.cursor_word_right();
            return Changed;
        }

        if is("tui.input.newLine") {
            self.newline();
            return Changed;
        }
        if is("tui.input.submit") {
            // Terminals without shift+enter: a trailing backslash becomes a newline (pi).
            if self.st.col > 0 && self.cur().as_bytes()[self.st.col - 1] == b'\\' {
                self.backspace();
                self.newline();
                return Changed;
            }
            return self.submit();
        }

        if is("tui.editor.cursorUp") {
            let rows = self.layout(self.content_width());
            let cur = self.cursor_row(&rows).unwrap_or(0);
            if cur == 0 && (self.is_empty() || self.hist_pos.is_some() || self.st.col == 0) {
                self.history_nav(-1);
            } else if cur == 0 {
                self.cursor_line_start();
            } else {
                self.move_vertical(-1);
            }
            return Changed;
        }
        if is("tui.editor.cursorDown") {
            let rows = self.layout(self.content_width());
            let cur = self.cursor_row(&rows).unwrap_or(0);
            let last = rows.len().saturating_sub(1);
            if self.hist_pos.is_some() && cur == last {
                self.history_nav(1);
            } else if cur == last {
                self.cursor_line_end();
            } else {
                self.move_vertical(1);
            }
            return Changed;
        }
        if is("tui.editor.cursorRight") {
            self.cursor_right();
            return Changed;
        }
        if is("tui.editor.cursorLeft") {
            self.cursor_left();
            return Changed;
        }
        if is("tui.editor.pageUp") || is("tui.editor.pageDown") {
            let down = is("tui.editor.pageDown");
            let page = self.opts.max_visible_rows.max(1);
            for _ in 0..page {
                if !self.move_vertical(if down { 1 } else { -1 }) {
                    break;
                }
            }
            return Changed;
        }
        if is("tui.editor.jumpForward") {
            self.jump = Some(true);
            return Changed;
        }
        if is("tui.editor.jumpBackward") {
            self.jump = Some(false);
            return Changed;
        }

        if let Some(t) = key.text.clone() {
            if !t.is_empty() && !t.chars().any(|c| c < ' ') {
                self.type_text(&t);
                return Changed;
            }
        }
        Ignored
    }

    // ---- helpers over the document -------------------------------------------------------

    fn cur(&self) -> &str {
        &self.st.lines[self.st.line]
    }

    fn push_undo(&mut self) {
        self.undo.push(self.st.clone());
    }

    fn do_undo(&mut self) {
        if let Some(s) = self.undo.pop() {
            self.st = s;
            self.last = Last::Other;
            self.preferred_col = None;
        }
    }

    fn exit_history(&mut self) {
        self.hist_pos = None;
        self.hist_draft = None;
    }

    /// Valid paste markers in `line` as `(start, end)` byte spans.
    fn markers(&self, line: &str) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        if !line.contains("[paste #") {
            return out;
        }
        for (i, _) in line.match_indices("[paste #") {
            if let Some((end, id)) = parse_marker(line, i) {
                if self.pastes.contains_key(&id) {
                    out.push((i, end));
                }
            }
        }
        out
    }

    /// Grapheme boundaries of `line` with paste markers merged into single units. Ascending,
    /// starts with 0 and ends with `line.len()`, no duplicates.
    fn bounds(&self, line: &str) -> Vec<usize> {
        let mut b = cluster_boundaries(line);
        for (s, e) in self.markers(line) {
            b.retain(|&x| x <= s || x >= e);
            b.push(s);
            b.push(e);
        }
        b.sort_unstable();
        b.dedup();
        b
    }

    fn class_of(line: &str, b: &[usize], mk: &[(usize, usize)], k: usize) -> Class {
        if mk.iter().any(|&(s, _)| s == b[k]) {
            return Class::Atomic;
        }
        match line[b[k]..].chars().next() {
            Some(c) if c.is_whitespace() => Class::Ws,
            Some(c) if c.is_alphanumeric() || c == '_' => Class::Word,
            _ => Class::Punct,
        }
    }

    /// Start of the unit before byte `col` (grapheme or marker).
    fn prev_boundary(&self, line: &str, col: usize) -> usize {
        self.bounds(line)
            .iter()
            .rev()
            .find(|&&x| x < col)
            .copied()
            .unwrap_or(0)
    }

    /// End of the unit after byte `col`.
    fn next_boundary(&self, line: &str, col: usize) -> usize {
        self.bounds(line)
            .into_iter()
            .find(|&x| x > col)
            .unwrap_or(line.len())
    }

    /// Byte offset one word left of `col` (pi's word rules: skip whitespace, then a run of
    /// word characters or of punctuation; a paste marker is one unit).
    fn word_back(&self, line: &str, col: usize) -> usize {
        let b = self.bounds(line);
        let mk = self.markers(line);
        let Some(mut i) = b.iter().position(|&x| x == col) else {
            return col;
        };
        while i > 0 && Self::class_of(line, &b, &mk, i - 1) == Class::Ws {
            i -= 1;
        }
        if i == 0 {
            return 0;
        }
        match Self::class_of(line, &b, &mk, i - 1) {
            Class::Atomic => i -= 1,
            c => {
                while i > 0 && Self::class_of(line, &b, &mk, i - 1) == c {
                    i -= 1;
                }
            }
        }
        b[i]
    }

    /// Byte offset one word right of `col`.
    fn word_fwd(&self, line: &str, col: usize) -> usize {
        let b = self.bounds(line);
        let mk = self.markers(line);
        let Some(mut i) = b.iter().position(|&x| x == col) else {
            return col;
        };
        let n = b.len() - 1;
        while i < n && Self::class_of(line, &b, &mk, i) == Class::Ws {
            i += 1;
        }
        if i >= n {
            return b[n];
        }
        match Self::class_of(line, &b, &mk, i) {
            Class::Atomic => i += 1,
            c => {
                while i < n && Self::class_of(line, &b, &mk, i) == c {
                    i += 1;
                }
            }
        }
        b[i]
    }

    /// Insert `text` at the cursor (newlines split lines); no undo step. Returns the end.
    fn insert_str(&mut self, text: &str) -> Pos {
        let (l, c) = (self.st.line, self.st.col);
        let line = self.st.lines[l].clone();
        let (before, after) = (&line[..c], &line[c..]);
        let parts: Vec<&str> = text.split('\n').collect();
        if parts.len() == 1 {
            self.st.lines[l] = format!("{before}{text}{after}");
            self.st.col = c + text.len();
            return (l, self.st.col);
        }
        let last_part = parts[parts.len() - 1];
        self.st.lines[l] = format!("{before}{}", parts[0]);
        let mut at = l + 1;
        for mid in &parts[1..parts.len() - 1] {
            self.st.lines.insert(at, (*mid).to_string());
            at += 1;
        }
        self.st.lines.insert(at, format!("{last_part}{after}"));
        self.st.line = at;
        self.st.col = last_part.len();
        (self.st.line, self.st.col)
    }

    /// Delete `a..b` (a <= b); the cursor goes to `a`.
    fn delete_range(&mut self, a: Pos, b: Pos) {
        if a.0 == b.0 {
            self.st.lines[a.0].replace_range(a.1..b.1, "");
        } else {
            let tail = self.st.lines[b.0][b.1..].to_string();
            self.st.lines[a.0].truncate(a.1);
            self.st.lines[a.0].push_str(&tail);
            self.st.lines.drain(a.0 + 1..=b.0);
        }
        self.st.line = a.0;
        self.st.col = a.1;
    }

    /// Join line `l` onto the end of line `l - 1`; the cursor goes to the join point.
    fn merge_into_prev(&mut self, l: usize) {
        let cur = self.st.lines.remove(l);
        let prev_len = self.st.lines[l - 1].len();
        self.st.lines[l - 1].push_str(&cur);
        self.st.line = l - 1;
        self.st.col = prev_len;
    }

    /// Join line `l + 1` onto the end of line `l`.
    fn merge_next(&mut self, l: usize) {
        let next = self.st.lines.remove(l + 1);
        self.st.lines[l].push_str(&next);
    }

    fn kill_push(&mut self, text: String, prepend: bool, accumulate: bool) {
        if text.is_empty() {
            return;
        }
        if accumulate {
            if let Some(last) = self.kill.pop() {
                self.kill.push(if prepend { text + &last } else { last + &text });
                return;
            }
        }
        self.kill.push(text);
    }

    fn set_cursor(&mut self, line: usize, col: usize) {
        self.st.line = line;
        self.st.col = col;
        self.last = Last::Other;
        self.preferred_col = None;
    }

    fn suggest_refresh_if_open(&mut self) {
        if self.suggest.is_some() {
            self.refresh_suggestions(false);
        }
    }

    // ---- editing -------------------------------------------------------------------------

    /// A typed piece of text (one key): undo coalescing as pi.
    fn type_text(&mut self, t: &str) {
        self.exit_history();
        let ws = t.chars().all(char::is_whitespace);
        if ws || self.last != Last::TypeWord {
            self.push_undo();
        }
        self.last = Last::TypeWord;
        self.preferred_col = None;
        self.insert_str(t);
        if self.provider.is_some() {
            self.refresh_suggestions(false);
        }
    }

    fn newline(&mut self) {
        self.exit_history();
        self.suggest = None;
        self.push_undo();
        self.last = Last::Other;
        self.preferred_col = None;
        self.insert_str("\n");
    }

    /// Delete the unit before the cursor. `false` when there was nothing to delete.
    fn backspace(&mut self) -> bool {
        self.exit_history();
        let (l, c) = (self.st.line, self.st.col);
        if c > 0 {
            self.push_undo();
            let line = self.st.lines[l].clone();
            let p = self.prev_boundary(&line, c);
            self.delete_range((l, p), (l, c));
        } else if l > 0 {
            self.push_undo();
            self.merge_into_prev(l);
        } else {
            return false;
        }
        self.last = Last::Other;
        self.preferred_col = None;
        self.suggest_refresh_if_open();
        true
    }

    /// Delete the unit after the cursor. `false` when there was nothing to delete.
    fn forward_delete(&mut self) -> bool {
        self.exit_history();
        let (l, c) = (self.st.line, self.st.col);
        let line = self.st.lines[l].clone();
        if c < line.len() {
            self.push_undo();
            let n = self.next_boundary(&line, c);
            self.delete_range((l, c), (l, n));
        } else if l + 1 < self.st.lines.len() {
            self.push_undo();
            self.merge_next(l);
        } else {
            return false;
        }
        self.last = Last::Other;
        self.preferred_col = None;
        self.suggest_refresh_if_open();
        true
    }

    fn kill_to_line_start(&mut self) -> bool {
        self.exit_history();
        let (l, c) = (self.st.line, self.st.col);
        let acc = self.last == Last::Kill;
        if c > 0 {
            self.push_undo();
            let deleted = self.st.lines[l][..c].to_string();
            self.kill_push(deleted, true, acc);
            self.st.lines[l].replace_range(..c, "");
            self.st.col = 0;
        } else if l > 0 {
            self.push_undo();
            self.kill_push("\n".to_string(), true, acc);
            self.merge_into_prev(l);
        } else {
            return false;
        }
        self.last = Last::Kill;
        self.preferred_col = None;
        true
    }

    fn kill_to_line_end(&mut self) -> bool {
        self.exit_history();
        let (l, c) = (self.st.line, self.st.col);
        let acc = self.last == Last::Kill;
        let len = self.st.lines[l].len();
        if c < len {
            self.push_undo();
            let deleted = self.st.lines[l][c..].to_string();
            self.kill_push(deleted, false, acc);
            self.st.lines[l].truncate(c);
        } else if l + 1 < self.st.lines.len() {
            self.push_undo();
            self.kill_push("\n".to_string(), false, acc);
            self.merge_next(l);
        } else {
            return false;
        }
        self.last = Last::Kill;
        self.preferred_col = None;
        true
    }

    fn kill_word_backward(&mut self) -> bool {
        self.exit_history();
        let (l, c) = (self.st.line, self.st.col);
        let acc = self.last == Last::Kill;
        if c == 0 {
            if l == 0 {
                return false;
            }
            self.push_undo();
            self.kill_push("\n".to_string(), true, acc);
            self.merge_into_prev(l);
        } else {
            self.push_undo();
            let line = self.st.lines[l].clone();
            let p = self.word_back(&line, c);
            self.kill_push(line[p..c].to_string(), true, acc);
            self.delete_range((l, p), (l, c));
        }
        self.last = Last::Kill;
        self.preferred_col = None;
        true
    }

    fn kill_word_forward(&mut self) -> bool {
        self.exit_history();
        let (l, c) = (self.st.line, self.st.col);
        let acc = self.last == Last::Kill;
        let line = self.st.lines[l].clone();
        if c >= line.len() {
            if l + 1 >= self.st.lines.len() {
                return false;
            }
            self.push_undo();
            self.kill_push("\n".to_string(), false, acc);
            self.merge_next(l);
        } else {
            self.push_undo();
            let n = self.word_fwd(&line, c);
            self.kill_push(line[c..n].to_string(), false, acc);
            self.delete_range((l, c), (l, n));
        }
        self.last = Last::Kill;
        self.preferred_col = None;
        true
    }

    fn yank(&mut self) -> bool {
        let Some(text) = self.kill.last().cloned() else {
            return false;
        };
        self.exit_history();
        self.push_undo();
        let start = (self.st.line, self.st.col);
        let end = self.insert_str(&text);
        self.yank_span = Some((start, end));
        self.last = Last::Yank;
        self.preferred_col = None;
        true
    }

    /// Replace the text of the last yank by the next older kill-ring entry.
    fn yank_pop(&mut self) -> bool {
        if self.last != Last::Yank || self.kill.len() <= 1 {
            return false;
        }
        self.exit_history();
        self.push_undo();
        if let Some((a, b)) = self.yank_span.take() {
            self.delete_range(a, b);
        }
        let newest = self.kill.pop().unwrap_or_default();
        self.kill.insert(0, newest);
        let text = self.kill.last().cloned().unwrap_or_default();
        let start = (self.st.line, self.st.col);
        let end = self.insert_str(&text);
        self.yank_span = Some((start, end));
        self.last = Last::Yank;
        self.preferred_col = None;
        true
    }

    // ---- cursor --------------------------------------------------------------------------

    fn cursor_left(&mut self) {
        self.suggest = None;
        let (l, c) = (self.st.line, self.st.col);
        if c > 0 {
            let line = self.st.lines[l].clone();
            let p = self.prev_boundary(&line, c);
            self.set_cursor(l, p);
        } else if l > 0 {
            let len = self.st.lines[l - 1].len();
            self.set_cursor(l - 1, len);
        }
    }

    fn cursor_right(&mut self) {
        self.suggest = None;
        let (l, c) = (self.st.line, self.st.col);
        let line = self.st.lines[l].clone();
        if c < line.len() {
            let n = self.next_boundary(&line, c);
            self.set_cursor(l, n);
        } else if l + 1 < self.st.lines.len() {
            self.set_cursor(l + 1, 0);
        }
    }

    fn cursor_line_start(&mut self) {
        self.suggest = None;
        let l = self.st.line;
        self.set_cursor(l, 0);
    }

    fn cursor_line_end(&mut self) {
        self.suggest = None;
        let l = self.st.line;
        let len = self.st.lines[l].len();
        self.set_cursor(l, len);
    }

    fn cursor_word_left(&mut self) {
        self.suggest = None;
        let (l, c) = (self.st.line, self.st.col);
        if c == 0 {
            if l > 0 {
                let len = self.st.lines[l - 1].len();
                self.set_cursor(l - 1, len);
            }
        } else {
            let line = self.st.lines[l].clone();
            let p = self.word_back(&line, c);
            self.set_cursor(l, p);
        }
    }

    fn cursor_word_right(&mut self) {
        self.suggest = None;
        let (l, c) = (self.st.line, self.st.col);
        let line = self.st.lines[l].clone();
        if c >= line.len() {
            if l + 1 < self.st.lines.len() {
                self.set_cursor(l + 1, 0);
            }
        } else {
            let n = self.word_fwd(&line, c);
            self.set_cursor(l, n);
        }
    }

    /// Move one visual row (wrapped rows count), keeping the preferred column.
    /// `false` when there is no row in that direction.
    fn move_vertical(&mut self, delta: isize) -> bool {
        let rows = self.layout(self.content_width());
        let Some(cur) = self.cursor_row(&rows) else {
            return false;
        };
        let next = cur as isize + delta;
        if next < 0 || next as usize >= rows.len() {
            return false;
        }
        let next = next as usize;
        let cur_row = rows[cur];
        let cur_line = self.st.lines[cur_row.line].clone();
        let want = self
            .preferred_col
            .unwrap_or_else(|| visible_width(&cur_line[cur_row.start..self.st.col.max(cur_row.start)]));
        let target = rows[next];
        let tline = self.st.lines[target.line].clone();
        let is_last_chunk = target.end == tline.len();
        let mut best = target.start;
        for b in self.bounds(&tline) {
            if b < target.start || b > target.end || (b == target.end && !is_last_chunk) {
                continue;
            }
            if visible_width(&tline[target.start..b]) <= want {
                best = b;
            } else {
                break;
            }
        }
        self.st.line = target.line;
        self.st.col = best;
        self.last = Last::Other;
        self.preferred_col = Some(want);
        true
    }

    fn jump_to(&mut self, t: &str, forward: bool) {
        let (l, c) = (self.st.line, self.st.col);
        let line = self.st.lines[l].clone();
        let found = if forward {
            let from = self.next_boundary(&line, c);
            line[from..].find(t).map(|i| from + i)
        } else {
            line[..c].rfind(t)
        };
        if let Some(p) = found {
            self.set_cursor(l, p);
        }
    }

    // ---- history -------------------------------------------------------------------------

    /// Walk the history: `dir < 0` older, `dir > 0` newer; leaving the newest entry restores
    /// the draft.
    fn history_nav(&mut self, dir: isize) {
        self.last = Last::Other;
        if self.history.is_empty() {
            return;
        }
        let len = self.history.len();
        if dir < 0 {
            match self.hist_pos {
                None => {
                    self.push_undo();
                    self.hist_draft = Some(self.st.clone());
                    self.hist_pos = Some(len - 1);
                    self.show_history(len - 1, true);
                }
                Some(0) => {}
                Some(p) => {
                    self.hist_pos = Some(p - 1);
                    self.show_history(p - 1, true);
                }
            }
        } else if let Some(p) = self.hist_pos {
            if p + 1 < len {
                self.hist_pos = Some(p + 1);
                self.show_history(p + 1, false);
            } else {
                self.hist_pos = None;
                if let Some(draft) = self.hist_draft.take() {
                    self.st = draft;
                }
                self.preferred_col = None;
            }
        }
    }

    fn show_history(&mut self, index: usize, cursor_at_start: bool) {
        let text = self.history[index].clone();
        self.st = Text::from_text(&text);
        if cursor_at_start {
            self.st.line = 0;
            self.st.col = 0;
        }
        self.preferred_col = None;
    }

    // ---- submit --------------------------------------------------------------------------

    fn submit(&mut self) -> EditorAction {
        let text = self.expanded_text();
        if text.trim().is_empty() {
            return EditorAction::Changed;
        }
        self.add_history(&text);
        self.st = Text::from_text("");
        self.pastes.clear();
        self.paste_counter = 0;
        self.undo.clear();
        self.exit_history();
        self.suggest = None;
        self.last = Last::Other;
        self.preferred_col = None;
        self.scroll = 0;
        EditorAction::Submit(text)
    }

    // ---- suggestions ---------------------------------------------------------------------

    fn refresh_suggestions(&mut self, force: bool) {
        let (l, c) = (self.st.line, self.st.col);
        let found = match self.provider.as_mut() {
            Some(p) => p.suggestions(&self.st.lines, l, c, force),
            None => None,
        };
        self.suggest = match found {
            Some(s) if !s.items.is_empty() => Some(Suggest {
                items: s.items,
                prefix: s.prefix,
                selected: 0,
                touched: false,
            }),
            _ => None,
        };
    }

    /// The cursor is in a slash command line (`/name args`, first line), where the provider
    /// offers command names and arguments rather than files.
    fn in_slash_command(&self) -> bool {
        self.st.line == 0
            && self
                .st
                .lines
                .first()
                .is_some_and(|l| l.trim_start().starts_with('/'))
    }

    fn suggest_move(&mut self, delta: isize, wrap: bool) {
        let Some(s) = self.suggest.as_mut() else {
            return;
        };
        let n = s.items.len() as isize;
        if n == 0 {
            return;
        }
        s.touched = true;
        let target = s.selected as isize + delta;
        s.selected = if wrap {
            target.rem_euclid(n) as usize
        } else {
            target.clamp(0, n - 1) as usize
        };
    }

    /// Apply the selected suggestion; `false` when nothing was open or applicable.
    fn accept_suggestion(&mut self) -> bool {
        let Some(s) = self.suggest.take() else {
            return false;
        };
        let Some(item) = s.items.get(s.selected).cloned() else {
            return false;
        };
        let (l, c) = (self.st.line, self.st.col);
        let applied = match self.provider.as_mut() {
            Some(p) => p.apply(&self.st.lines, l, c, &item, &s.prefix),
            None => return false,
        };
        self.push_undo();
        self.st.lines = applied.lines;
        self.st.line = applied.cursor_line;
        self.st.col = applied.cursor_col;
        self.exit_history();
        self.last = Last::Other;
        self.preferred_col = None;
        true
    }

    fn suggest_popup(&self, w: usize, theme: &Theme) -> Vec<String> {
        let Some(s) = self.suggest.as_ref() else {
            return Vec::new();
        };
        let max = self.opts.autocomplete_max_visible.max(1);
        if self.opts.popup != PopupStyle::List {
            return crate::suggest_panel(&s.items, s.selected, &s.prefix, max, w, self.opts.popup, theme);
        }
        let start = if s.selected >= max {
            s.selected + 1 - max
        } else {
            0
        };
        let end = (start + max).min(s.items.len());
        (start..end)
            .map(|i| {
                let item = &s.items[i];
                let label = if item.label.is_empty() {
                    &item.value
                } else {
                    &item.label
                };
                let mut line = if i == s.selected {
                    theme.fg("accent", &format!("→ {label}"))
                } else {
                    format!("  {label}")
                };
                if let Some(d) = item.description.as_deref().filter(|d| !d.is_empty()) {
                    line.push_str(&theme.fg("muted", &format!("  {d}")));
                }
                truncate_to_width(&line, w, "")
            })
            .collect()
    }

    // ---- layout --------------------------------------------------------------------------

    /// `(padding, content width)` for a component of `width` columns.
    fn geometry(&self, width: usize) -> (usize, usize) {
        let max_pad = (width.max(1) - 1) / 2;
        let padx = (self.opts.padding_x as usize).min(max_pad);
        let cw = width.max(1).saturating_sub(2 * padx).max(1);
        (padx, cw)
    }

    fn content_width(&self) -> usize {
        self.geometry(self.last_width).1
    }

    /// Every visual row of the document at content width `cw`.
    fn layout(&self, cw: usize) -> Vec<Row> {
        let mut rows = Vec::new();
        for (i, line) in self.st.lines.iter().enumerate() {
            let b = self.bounds(line);
            for c in wrap_bounds(line, &b, cw) {
                rows.push(Row {
                    line: i,
                    start: c.start,
                    end: c.end,
                });
            }
        }
        rows
    }

    /// Index of the visual row holding the cursor.
    fn cursor_row(&self, rows: &[Row]) -> Option<usize> {
        let (l, c) = (self.st.line, self.st.col);
        rows.iter()
            .enumerate()
            .filter(|(_, r)| r.line == l && r.start <= c)
            .map(|(i, _)| i)
            .next_back()
    }

    fn border(&self, w: usize, hidden: usize, arrow: &str, theme: &Theme) -> String {
        let mut s = if hidden > 0 {
            format!("─── {arrow} {hidden} more ")
        } else {
            String::new()
        };
        s = truncate_to_width(&s, w, "");
        while visible_width(&s) < w {
            s.push('─');
        }
        theme.fg(&self.opts.border_token, &s)
    }
}

// ---- free helpers -------------------------------------------------------------------------

/// Line endings to `\n`, tabs to four spaces, control characters dropped (pi's rules).
fn normalize(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\t', "    ")
        .chars()
        .filter(|&c| c == '\n' || c >= ' ')
        .collect()
}

/// Parse a paste marker at byte `start` of `s`: `[paste #N]`, `[paste #N +M lines]` or
/// `[paste #N K chars]`. Returns the end offset and the id.
fn parse_marker(s: &str, start: usize) -> Option<(usize, u32)> {
    let mut j = start + "[paste #".len();
    let ds = j;
    let digits = s[j..].bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 || digits > 9 {
        return None;
    }
    j += digits;
    let id: u32 = s[ds..j].parse().ok()?;
    let rest = &s[j..];
    let tail = if let Some(r) = rest.strip_prefix(' ') {
        if let Some(plus) = r.strip_prefix('+') {
            let nd = plus.bytes().take_while(u8::is_ascii_digit).count();
            if nd == 0 {
                return None;
            }
            plus[nd..].strip_prefix(" lines")?
        } else {
            let nd = r.bytes().take_while(u8::is_ascii_digit).count();
            if nd == 0 {
                return None;
            }
            r[nd..].strip_prefix(" chars")?
        }
    } else {
        rest
    };
    let after = tail.strip_prefix(']')?;
    Some((s.len() - after.len(), id))
}

/// Replace every valid paste marker in `text` by its content.
fn expand(text: &str, pastes: &BTreeMap<u32, String>) -> String {
    let mut out = String::new();
    let mut last = 0usize;
    for (i, _) in text.match_indices("[paste #") {
        if i < last {
            continue;
        }
        if let Some((end, id)) = parse_marker(text, i) {
            if let Some(content) = pastes.get(&id) {
                out.push_str(&text[last..i]);
                out.push_str(content);
                last = end;
            }
        }
    }
    out.push_str(&text[last..]);
    out
}

/// Soft-wrap cluster boundaries `bounds` of `line` to `max` columns. Breaks fall after
/// whitespace when possible, else at a cluster boundary; a unit wider than `max` stands alone.
fn wrap_bounds(line: &str, bounds: &[usize], max: usize) -> Vec<TextChunk> {
    let max = max.max(1);
    let n = bounds.len().saturating_sub(1);
    if n == 0 {
        return vec![TextChunk {
            start: 0,
            end: line.len(),
        }];
    }
    let widths: Vec<usize> = (0..n)
        .map(|k| visible_width(&line[bounds[k]..bounds[k + 1]]))
        .collect();
    let ws: Vec<bool> = (0..n)
        .map(|k| line[bounds[k]..].chars().next().is_some_and(char::is_whitespace))
        .collect();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    let mut width = 0usize;
    while i < n {
        if width + widths[i] > max && i > start {
            let brk = ((start + 1)..=i).rev().find(|&k| ws[k - 1]).unwrap_or(i);
            out.push(TextChunk {
                start: bounds[start],
                end: bounds[brk],
            });
            start = brk;
            width = widths[start..i].iter().sum();
            continue;
        }
        width += widths[i];
        i += 1;
    }
    out.push(TextChunk {
        start: bounds[start],
        end: line.len(),
    });
    out
}

/// One visual row of a soft-wrapped logical line: byte range `start..end` of the line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextChunk {
    pub start: usize,
    pub end: usize,
}

/// Soft-wrap one logical line (plain text) to `max_width` columns: break after whitespace when
/// possible, else at a cluster boundary; an empty line gives one empty chunk.
pub fn word_wrap_line(line: &str, max_width: usize) -> Vec<TextChunk> {
    let mut b = cluster_boundaries(line);
    b.dedup();
    wrap_bounds(line, &b, max_width)
}

impl Component for Editor {
    /// Top border, the visible text rows (soft-wrapped at cluster boundaries, scrolled so the
    /// cursor is visible), bottom border, then the suggestion list when open.
    fn render(&mut self, width: u16, theme: &Theme) -> Vec<String> {
        let w = (width as usize).max(1);
        self.last_width = w;
        let (padx, cw) = self.geometry(w);
        let rows = self.layout(cw);
        let total = rows.len();
        let visible = total.min(self.opts.max_visible_rows.max(1));
        let cur = self.cursor_row(&rows).unwrap_or(0);
        if cur < self.scroll {
            self.scroll = cur;
        }
        if cur >= self.scroll + visible {
            self.scroll = cur + 1 - visible;
        }
        if self.scroll + visible > total {
            self.scroll = total - visible;
        }
        let above = self.scroll;
        let below = total - self.scroll - visible;

        let mut out = vec![self.border(w, above, "↑", theme)];
        let pad = " ".repeat(padx);
        let mut cursor = None;
        for (k, r) in rows[self.scroll..self.scroll + visible].iter().enumerate() {
            let text = &self.st.lines[r.line][r.start..r.end];
            let line = truncate_to_width(&format!("{pad}{text}"), w, "");
            if self.scroll + k == cur {
                let col = padx + visible_width(&self.st.lines[r.line][r.start..self.st.col]);
                cursor = Some(CursorPos {
                    row: 1 + k,
                    col: col.min(w - 1) as u16,
                });
            }
            out.push(pad_to_width(&line, w, false));
        }
        out.push(self.border(w, below, "↓", theme));
        if !self.opts.popup_detached {
            out.extend(self.suggest_popup(w, theme));
        }
        self.cursor = cursor;
        out
    }

    fn input(&mut self, key: &Key) -> bool {
        let km = std::mem::take(&mut self.keymap);
        let action = self.dispatch(key, &km);
        self.keymap = km;
        let consumed = action != EditorAction::Ignored;
        self.action = action;
        consumed
    }

    fn invalidate(&mut self) {
        // Nothing is cached: the layout is recomputed on every render.
    }
}

impl Interactive for Editor {
    fn cursor(&self) -> Option<CursorPos> {
        if self.focused {
            self.cursor
        } else {
            None
        }
    }

    fn paste(&mut self, text: &str) -> bool {
        self.paste_text(text);
        true
    }

    fn mouse(&mut self, _m: &Mouse) -> bool {
        false
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }
}
