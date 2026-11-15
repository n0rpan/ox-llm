//! B12: Input, a single-line text field.
// ported from pi packages/tui/src/components/input.ts
use crate::{
    pad_to_width, slice_by_column, truncate_to_width, visible_width, Component, CursorPos, Interactive, Key,
    KeyKind, Theme,
};
use oxa_config::Keymap;

/// What a key did to an input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputAction {
    Ignored,
    /// The value or the cursor moved.
    Changed,
    /// Enter; carries the value.
    Submit(String),
    Cancelled,
}

/// Prompt shown before the text.
const PROMPT: &str = "> ";
/// Displayed for every character when masked.
const MASK: &str = "•";
/// Undo history depth.
const UNDO_LIMIT: usize = 100;

/// A one-line field with horizontal scrolling, a `> ` prompt and the `tui.input.*` /
/// `tui.editor.*` single-line keys (move, word move, delete, kill, yank, undo). Pasted text
/// has newlines replaced by spaces.
pub struct Input {
    value: String,
    /// Byte offset of the cursor in `value`, always on a character boundary.
    cursor: usize,
    placeholder: String,
    masked: bool,
    keymap: Option<Keymap>,
    action: InputAction,
    /// Snapshots `(value, cursor)` taken before each edit.
    undo: Vec<(String, usize)>,
    /// Killed text, most recent last.
    kill_ring: Vec<String>,
    /// For yank-pop: the byte range of the last yank and the ring index it shows.
    last_yank: Option<(usize, usize)>,
    yank_index: usize,
    /// First visible column of the value.
    scroll: usize,
    /// Hardware cursor column as of the last render.
    cursor_col: u16,
}

impl Input {
    pub fn new(placeholder: &str) -> Input {
        Input {
            value: String::new(),
            cursor: 0,
            placeholder: placeholder.to_string(),
            masked: false,
            keymap: None,
            action: InputAction::Ignored,
            undo: Vec::new(),
            kill_ring: Vec::new(),
            last_yank: None,
            yank_index: 0,
            scroll: 0,
            cursor_col: PROMPT.len() as u16,
        }
    }
    pub fn value(&self) -> &str {
        &self.value
    }
    /// Replace the value. The cursor stays where it was, clamped into the new value.
    pub fn set_value(&mut self, value: &str) {
        self.value = value.to_string();
        self.cursor = self.cursor.min(self.value.len());
        while !self.value.is_char_boundary(self.cursor) {
            self.cursor -= 1;
        }
    }
    /// Show `•` for every character (secrets).
    pub fn set_masked(&mut self, masked: bool) {
        self.masked = masked;
    }
    pub fn key(&mut self, key: &Key, keymap: &Keymap) -> InputAction {
        let action = self.apply(key, keymap);
        self.action = action.clone();
        action
    }
    pub fn set_keymap(&mut self, keymap: Keymap) {
        self.keymap = Some(keymap);
    }
    /// The action of the last `Component::input` call (reset to `Ignored` once taken).
    pub fn take_action(&mut self) -> InputAction {
        std::mem::replace(&mut self.action, InputAction::Ignored)
    }

    // ---- editing primitives (byte offsets, always on character boundaries) -----------------

    fn snapshot(&mut self) {
        self.undo.push((self.value.clone(), self.cursor));
        if self.undo.len() > UNDO_LIMIT {
            self.undo.remove(0);
        }
    }

    fn prev_boundary(&self, i: usize) -> usize {
        self.value[..i]
            .char_indices()
            .next_back()
            .map(|(j, _)| j)
            .unwrap_or(0)
    }

    fn next_boundary(&self, i: usize) -> usize {
        self.value[i..]
            .chars()
            .next()
            .map(|c| i + c.len_utf8())
            .unwrap_or(self.value.len())
    }

    /// Start of the word before `i`: skip whitespace, then the word.
    fn word_back(&self, i: usize) -> usize {
        let head = self.value[..i].trim_end_matches(char::is_whitespace);
        head.trim_end_matches(|c: char| !c.is_whitespace()).len()
    }

    /// End of the word after `i`: skip whitespace, then the word.
    fn word_forward(&self, i: usize) -> usize {
        let rest = self.value[i..].trim_start_matches(char::is_whitespace);
        let rest = rest.trim_start_matches(|c: char| !c.is_whitespace());
        self.value.len() - rest.len()
    }

    /// Remove `start..end`, keep it in the kill ring and move the cursor to `start`.
    fn kill(&mut self, start: usize, end: usize) {
        self.snapshot();
        let removed: String = self.value.drain(start..end).collect();
        self.kill_ring.push(removed);
        self.cursor = start;
    }

    fn insert(&mut self, text: &str) {
        self.value.insert_str(self.cursor, text);
        self.cursor += text.len();
    }

    /// `Changed` when a movement actually moved the cursor.
    fn move_cursor(&mut self, to: usize) -> InputAction {
        if to == self.cursor {
            InputAction::Ignored
        } else {
            self.cursor = to;
            InputAction::Changed
        }
    }

    fn apply(&mut self, key: &Key, keymap: &Keymap) -> InputAction {
        if key.kind == KeyKind::Release {
            return InputAction::Ignored;
        }
        let id = &key.id;
        let is = |action: &str| keymap.matches(id, action);
        let yank_pop = is("tui.editor.yankPop");
        if !yank_pop {
            self.last_yank = None;
        }

        if is("tui.select.cancel") {
            return InputAction::Cancelled;
        }
        if is("tui.editor.undo") {
            return match self.undo.pop() {
                Some((value, cursor)) => {
                    self.value = value;
                    self.cursor = cursor.min(self.value.len());
                    InputAction::Changed
                }
                None => InputAction::Ignored,
            };
        }
        if is("tui.input.submit") {
            return InputAction::Submit(self.value.clone());
        }

        // Deletion and kill.
        if is("tui.editor.deleteCharBackward") {
            if self.cursor == 0 {
                return InputAction::Ignored;
            }
            let start = self.prev_boundary(self.cursor);
            self.snapshot();
            self.value.replace_range(start..self.cursor, "");
            self.cursor = start;
            return InputAction::Changed;
        }
        if is("tui.editor.deleteCharForward") {
            if self.cursor >= self.value.len() {
                return InputAction::Ignored;
            }
            let end = self.next_boundary(self.cursor);
            self.snapshot();
            self.value.replace_range(self.cursor..end, "");
            return InputAction::Changed;
        }
        if is("tui.editor.deleteWordBackward") {
            let start = self.word_back(self.cursor);
            if start == self.cursor {
                return InputAction::Ignored;
            }
            self.kill(start, self.cursor);
            return InputAction::Changed;
        }
        if is("tui.editor.deleteWordForward") {
            let end = self.word_forward(self.cursor);
            if end == self.cursor {
                return InputAction::Ignored;
            }
            self.kill(self.cursor, end);
            return InputAction::Changed;
        }
        if is("tui.editor.deleteToLineStart") {
            if self.cursor == 0 {
                return InputAction::Ignored;
            }
            self.kill(0, self.cursor);
            return InputAction::Changed;
        }
        if is("tui.editor.deleteToLineEnd") {
            if self.cursor >= self.value.len() {
                return InputAction::Ignored;
            }
            self.kill(self.cursor, self.value.len());
            return InputAction::Changed;
        }

        // Yank and yank-pop.
        if is("tui.editor.yank") {
            let Some(text) = self.kill_ring.last().cloned() else {
                return InputAction::Ignored;
            };
            self.snapshot();
            let start = self.cursor;
            self.insert(&text);
            self.last_yank = Some((start, text.len()));
            self.yank_index = self.kill_ring.len() - 1;
            return InputAction::Changed;
        }
        if yank_pop {
            let Some((start, len)) = self.last_yank else {
                return InputAction::Ignored;
            };
            if self.kill_ring.len() < 2 {
                return InputAction::Ignored;
            }
            self.yank_index = (self.yank_index + self.kill_ring.len() - 1) % self.kill_ring.len();
            let text = self.kill_ring[self.yank_index].clone();
            self.value.replace_range(start..start + len, &text);
            self.cursor = start + text.len();
            self.last_yank = Some((start, text.len()));
            return InputAction::Changed;
        }

        // Cursor movement.
        if is("tui.editor.cursorLeft") {
            let to = self.prev_boundary(self.cursor);
            return self.move_cursor(to);
        }
        if is("tui.editor.cursorRight") {
            let to = self.next_boundary(self.cursor);
            return self.move_cursor(to);
        }
        if is("tui.editor.cursorWordLeft") {
            let to = self.word_back(self.cursor);
            return self.move_cursor(to);
        }
        if is("tui.editor.cursorWordRight") {
            let to = self.word_forward(self.cursor);
            return self.move_cursor(to);
        }
        if is("tui.editor.cursorLineStart") {
            return self.move_cursor(0);
        }
        if is("tui.editor.cursorLineEnd") {
            let to = self.value.len();
            return self.move_cursor(to);
        }

        // Printable text.
        match &key.text {
            Some(text) if !text.is_empty() => {
                let clean: String = text
                    .chars()
                    .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
                    .collect();
                self.snapshot();
                self.insert(&clean);
                InputAction::Changed
            }
            _ => InputAction::Ignored,
        }
    }

    /// The text as displayed: the value, or one `•` per character when masked.
    fn displayed(&self) -> String {
        if self.masked {
            MASK.repeat(self.value.chars().count())
        } else {
            self.value.clone()
        }
    }

    /// The cursor column inside the displayed text.
    fn cursor_column(&self) -> usize {
        if self.masked {
            self.value[..self.cursor].chars().count()
        } else {
            visible_width(&self.value[..self.cursor])
        }
    }
}

impl Component for Input {
    /// Exactly one line.
    fn render(&mut self, width: u16, theme: &Theme) -> Vec<String> {
        let w = width as usize;
        // One column is kept free so the cursor at the end of the text stays on the line.
        let avail = w.saturating_sub(PROMPT.len() + 1).max(1);
        if self.value.is_empty() {
            self.scroll = 0;
            self.cursor_col = PROMPT.len() as u16;
            let hint = truncate_to_width(&self.placeholder, avail, "…");
            let line = format!("{PROMPT}{}", theme.dim(&hint));
            return vec![pad_to_width(&truncate_to_width(&line, w, ""), w, false)];
        }

        let cursor = self.cursor_column();
        if cursor < self.scroll {
            self.scroll = cursor;
        }
        if cursor >= self.scroll + avail {
            self.scroll = cursor + 1 - avail;
        }
        let visible = slice_by_column(&self.displayed(), self.scroll, avail);
        self.cursor_col = (PROMPT.len() + cursor - self.scroll) as u16;
        let line = format!("{PROMPT}{visible}");
        vec![pad_to_width(&truncate_to_width(&line, w, ""), w, false)]
    }

    fn input(&mut self, key: &Key) -> bool {
        if self.keymap.is_none() {
            self.keymap = Some(Keymap::defaults());
        }
        let keymap = self.keymap.take().unwrap_or_default();
        let action = self.key(key, &keymap);
        self.keymap = Some(keymap);
        !matches!(action, InputAction::Ignored)
    }
}

impl Interactive for Input {
    fn cursor(&self) -> Option<CursorPos> {
        Some(CursorPos {
            row: 0,
            col: self.cursor_col,
        })
    }
    /// Newlines in pasted text become spaces; the paste is always consumed.
    fn paste(&mut self, text: &str) -> bool {
        let clean = text.replace("\r\n", " ").replace(['\r', '\n'], " ");
        if !clean.is_empty() {
            self.snapshot();
            self.insert(&clean);
        }
        true
    }
}
