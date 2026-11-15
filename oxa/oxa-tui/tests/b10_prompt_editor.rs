//! B10 acceptance tests: Prompt editor. Owner: task B10.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_config::Keymap;
use oxa_tui::{
    visible_width, word_wrap_line, Applied, AutocompleteItem, AutocompleteProvider, Component, Editor,
    EditorAction, EditorOpts, Interactive, Key, Suggestions, Theme,
};

// ---- helpers ------------------------------------------------------------------------------

/// An editor plus the default keymap, driven by key ids.
struct Ed {
    e: Editor,
    k: Keymap,
}

impl Ed {
    fn new() -> Ed {
        Ed::with(EditorOpts::default())
    }
    fn with(opts: EditorOpts) -> Ed {
        Ed {
            e: Editor::new(opts),
            k: Keymap::defaults(),
        }
    }
    /// Type each character as its own key press.
    fn type_str(&mut self, s: &str) {
        for c in s.chars() {
            let action = self.e.key(&Key::char(c), &self.k);
            assert_eq!(action, EditorAction::Changed, "typing {c:?}");
        }
    }
    fn press(&mut self, id: &str) -> EditorAction {
        self.e.key(&Key::press(id), &self.k)
    }
    fn text(&self) -> String {
        self.e.text()
    }
}

fn plain() -> Theme {
    Theme::plain()
}

/// Ten lines of 11 characters, a paste that becomes a marker (pi: more than 10 lines).
fn big_paste() -> String {
    (1..=12)
        .map(|i| format!("line {i:02}"))
        .collect::<Vec<_>>()
        .join("\n")
}

// ---- acceptance ---------------------------------------------------------------------------

/// cursor, word and line movement, deletion
#[test]
fn key_tables() {
    let mut t = Ed::new();
    t.type_str("hello world");
    assert_eq!(t.e.cursor_pos(), (0, 11));
    t.press("alt+left");
    assert_eq!(t.e.cursor_pos(), (0, 6), "word left");
    t.press("ctrl+a");
    assert_eq!(t.e.cursor_pos(), (0, 0), "line start");
    t.press("ctrl+e");
    assert_eq!(t.e.cursor_pos(), (0, 11), "line end");
    t.press("ctrl+b");
    assert_eq!(t.e.cursor_pos(), (0, 10), "char left");
    t.press("ctrl+f");
    assert_eq!(t.e.cursor_pos(), (0, 11), "char right");
    t.press("backspace");
    assert_eq!(t.text(), "hello worl");
    t.press("ctrl+a");
    t.press("delete");
    assert_eq!(t.text(), "ello worl", "forward delete");
    t.press("alt+d");
    assert_eq!(t.text(), " worl", "delete word forward");
    t.press("ctrl+e");
    t.press("ctrl+w");
    assert_eq!(t.text(), " ", "delete word backward");
    assert_eq!(
        t.press("tab"),
        EditorAction::Ignored,
        "tab without a completion source"
    );
    assert_eq!(
        t.press("ctrl+c"),
        EditorAction::Ignored,
        "ctrl+c belongs to the parent"
    );
}

/// ctrl+k / ctrl+y / alt+y
#[test]
fn kill_ring_and_yank_pop() {
    // Consecutive kills accumulate; ctrl+y yanks the newest entry back.
    let mut t = Ed::new();
    t.type_str("alpha beta");
    t.press("ctrl+w");
    assert_eq!(t.text(), "alpha ");
    t.press("ctrl+w");
    assert_eq!(t.text(), "");
    t.press("ctrl+y");
    assert_eq!(t.text(), "alpha beta", "accumulated kill yanks back whole");

    // Separate kills form separate entries; alt+y cycles through them.
    let mut t = Ed::new();
    t.type_str("two");
    t.press("ctrl+w");
    t.type_str("zzz");
    t.press("ctrl+w");
    assert_eq!(t.text(), "");
    t.press("ctrl+y");
    assert_eq!(t.text(), "zzz", "newest first");
    t.press("alt+y");
    assert_eq!(t.text(), "two", "yank-pop replaces with the older entry");
    t.press("alt+y");
    assert_eq!(t.text(), "zzz", "yank-pop cycles");

    // A non-kill action breaks accumulation.
    let mut t = Ed::new();
    t.type_str("one two");
    t.press("ctrl+w");
    t.press("ctrl+a");
    t.press("ctrl+k");
    assert_eq!(t.text(), "", "ctrl+k kills to the line end");
}

/// grouping as pi
#[test]
fn undo_redo() {
    // A word-sized typing burst is one undo step; a space starts a new one.
    let mut t = Ed::new();
    t.type_str("hello world");
    t.press("ctrl+-");
    assert_eq!(t.text(), "hello", "undo removes the space and the word after it");
    assert_eq!(t.e.cursor_pos(), (0, 5), "undo restores the cursor");
    t.press("ctrl+-");
    assert_eq!(t.text(), "", "undo removes the first word");

    // A paste is one step.
    let mut t = Ed::new();
    t.type_str("x");
    t.e.paste_text("abc");
    assert_eq!(t.text(), "xabc");
    t.press("ctrl+-");
    assert_eq!(t.text(), "x", "undo restores the state before the paste");

    // A kill is one step.
    let mut t = Ed::new();
    t.type_str("one two");
    t.press("ctrl+w");
    assert_eq!(t.text(), "one ");
    t.press("ctrl+-");
    assert_eq!(t.text(), "one two");

    // set_text is one step; undo returns to the previous content.
    let mut t = Ed::new();
    t.e.set_text("a\nb");
    assert_eq!(t.e.cursor_pos(), (1, 1), "set_text puts the cursor at the end");
    t.press("ctrl+-");
    assert_eq!(t.text(), "");
}

/// up/down at buffer edges
#[test]
fn history_navigation() {
    let mut t = Ed::new();
    t.e.add_history("first");
    t.e.add_history("second");
    t.e.add_history("second"); // consecutive duplicate dropped
    t.type_str("draft");
    // Not at column 0: up moves to the line start first.
    t.press("up");
    assert_eq!(t.e.cursor_pos(), (0, 0));
    t.press("up");
    assert_eq!(t.text(), "second", "up at the top walks back in history");
    t.press("up");
    assert_eq!(t.text(), "first");
    t.press("up");
    assert_eq!(t.text(), "first", "up at the oldest entry stays there");
    t.press("down");
    assert_eq!(t.text(), "second");
    t.press("down");
    assert_eq!(t.text(), "draft", "down past the newest entry restores the draft");
}

/// large paste collapses to a marker and expands on submit
#[test]
fn paste_markers_survive_delete() {
    let big = big_paste();
    let mut t = Ed::new();
    t.e.paste_text(&big);
    assert_eq!(t.text(), "[paste #1 +12 lines]", "large paste is a marker");
    assert_eq!(t.e.expanded_text(), big, "marker expands to the full content");

    // Edits around the marker leave it intact.
    t.press("ctrl+a");
    t.type_str("x");
    t.press("ctrl+e");
    t.type_str("!");
    assert_eq!(t.text(), "x[paste #1 +12 lines]!");
    t.press("backspace");
    assert_eq!(
        t.text(),
        "x[paste #1 +12 lines]",
        "the neighbouring character went"
    );
    assert_eq!(t.e.expanded_text(), format!("x{big}"));

    // Backspace at the marker deletes the whole marker as one unit.
    t.press("backspace");
    assert_eq!(t.text(), "x");
    assert_eq!(t.e.expanded_text(), "x");
}

/// reported cursor cell with wide chars
#[test]
fn ime_cursor_position() {
    let mut t = Ed::new();
    let theme = plain();
    t.e.set_focused(true);
    t.type_str("漢字a");
    t.e.render(40, &theme);
    let c = t.e.cursor().expect("cursor reported when focused");
    assert_eq!(c.row, 1, "first text row is below the top border");
    assert_eq!(c.col, 5, "two wide characters take two cells each");

    t.press("left");
    t.e.render(40, &theme);
    assert_eq!(t.e.cursor().unwrap().col, 4, "cursor before the last character");

    // Padding shifts the cell; a second line moves the row.
    let mut p = Ed::with(EditorOpts {
        padding_x: 1,
        ..EditorOpts::default()
    });
    p.e.set_focused(true);
    p.type_str("漢字");
    p.press("shift+enter");
    p.type_str("x");
    p.e.render(40, &theme);
    let c = p.e.cursor().unwrap();
    assert_eq!((c.row, c.col), (2, 2), "padding 1, second row");

    // No cursor while unfocused.
    let mut u = Ed::new();
    u.type_str("a");
    u.e.render(40, &theme);
    assert!(u.e.cursor().is_none());
}

/// render snapshots at 40 and 80 cols
#[test]
fn multiline_wrap_and_scroll() {
    let text = "The quick brown fox jumps over the lazy dog. ".repeat(3);
    for width in [40u16, 80] {
        let mut t = Ed::with(EditorOpts {
            max_visible_rows: 50,
            ..EditorOpts::default()
        });
        t.e.set_text(text.trim_end());
        let lines = t.e.render(width, &plain());
        let rows = word_wrap_line(text.trim_end(), width as usize).len();
        assert_eq!(lines.len(), rows + 2, "border, rows, border at width {width}");
        for l in &lines {
            assert!(
                visible_width(l) <= width as usize,
                "line too wide at {width}: {l:?}"
            );
        }
        assert!(lines[1].starts_with("The quick"));
        assert!(lines[lines.len() - 1].contains('─'), "bottom border");
    }

    // Scrolling: only 3 rows visible; the cursor (end of text) stays in view.
    let mut t = Ed::with(EditorOpts {
        max_visible_rows: 3,
        ..EditorOpts::default()
    });
    t.e.set_text(text.trim_end());
    t.e.set_focused(true);
    let lines = t.e.render(40, &plain());
    let total = word_wrap_line(text.trim_end(), 40).len();
    assert_eq!(lines.len(), 5, "top border, 3 rows, bottom border");
    assert!(
        lines[0].contains(&format!("↑ {} more", total - 3)),
        "top border shows hidden rows: {:?}",
        lines[0]
    );
    assert!(!lines[4].contains('↓'), "nothing hidden below the cursor");
    assert_eq!(t.e.cursor().unwrap().row, 3, "cursor on the last visible row");

    // Going to the top scrolls back.
    t.e.set_text(text.trim_end());
    t.press("ctrl+a");
    for _ in 0..(total + 2) {
        t.press("up");
    }
    let lines = t.e.render(40, &plain());
    assert!(!lines[0].contains('↑'), "at the top nothing is hidden above");
    assert!(lines[4].contains('↓'));
}

/// enter vs shift+enter
#[test]
fn submit_and_newline_keys() {
    let mut t = Ed::new();
    assert_eq!(
        t.press("enter"),
        EditorAction::Changed,
        "enter on an empty editor submits nothing"
    );
    t.type_str("a");
    assert_eq!(t.press("shift+enter"), EditorAction::Changed);
    t.type_str("b");
    assert_eq!(t.text(), "a\nb", "shift+enter inserts a newline");
    t.press("ctrl+j");
    t.type_str("c");
    assert_eq!(t.text(), "a\nb\nc", "ctrl+j also inserts a newline");
    assert_eq!(t.press("enter"), EditorAction::Submit("a\nb\nc".to_string()));
    assert!(t.e.is_empty(), "submit clears the editor");
    t.press("up");
    assert_eq!(t.text(), "a\nb\nc", "the submitted text is in the history");

    // A backslash before enter becomes a newline (terminals without shift+enter).
    let mut b = Ed::new();
    b.type_str("x\\");
    assert_eq!(b.press("enter"), EditorAction::Changed);
    assert_eq!(b.text(), "x\n");
}

// ---- further coverage ---------------------------------------------------------------------

#[test]
fn word_wrap_breaks_after_whitespace_then_clusters() {
    let chunks = word_wrap_line("hello world foo", 6);
    let parts: Vec<&str> = chunks
        .iter()
        .map(|c| &"hello world foo"[c.start..c.end])
        .collect();
    assert_eq!(parts, vec!["hello ", "world ", "foo"]);

    let chunks = word_wrap_line("abcdefghij", 4);
    let parts: Vec<&str> = chunks.iter().map(|c| &"abcdefghij"[c.start..c.end]).collect();
    assert_eq!(parts, vec!["abcd", "efgh", "ij"]);

    let chunks = word_wrap_line("", 10);
    assert_eq!(chunks.len(), 1);
    assert_eq!((chunks[0].start, chunks[0].end), (0, 0));
}

#[test]
fn render_never_exceeds_width() {
    let text = "漢字😀 abc def ghijklmnop ".repeat(4);
    for width in 1u16..=20 {
        for padding in [0u16, 1, 3] {
            let mut t = Ed::with(EditorOpts {
                padding_x: padding,
                ..EditorOpts::default()
            });
            t.e.set_text(&text);
            t.e.set_focused(true);
            let lines = t.e.render(width, &plain());
            for l in &lines {
                assert!(
                    visible_width(l) <= width as usize,
                    "width {width} pad {padding}: {l:?}"
                );
            }
            if let Some(c) = t.e.cursor() {
                assert!((c.col as usize) < width as usize, "cursor inside the width");
            }
        }
    }
}

#[test]
fn component_input_path_reports_submit() {
    let mut e = Editor::new(EditorOpts::default());
    e.set_keymap(Keymap::defaults());
    assert!(e.input(&Key::char('h')));
    assert!(e.input(&Key::char('i')));
    assert!(e.input(&Key::press("enter")));
    assert_eq!(e.take_action(), EditorAction::Submit("hi".to_string()));
    assert_eq!(e.take_action(), EditorAction::Ignored, "the action is taken once");
}

/// Completion source: `@src...` tokens complete to two files.
struct Files;

impl AutocompleteProvider for Files {
    fn suggestions(
        &mut self,
        lines: &[String],
        line: usize,
        col: usize,
        _force: bool,
    ) -> Option<Suggestions> {
        let before = &lines[line][..col];
        let token = before.rsplit(' ').next().unwrap_or("");
        if !token.starts_with("@src") {
            return None;
        }
        let items: Vec<AutocompleteItem> = ["@src/main.rs", "@src/lib.rs"]
            .iter()
            .filter(|v| v.starts_with(token))
            .map(|v| AutocompleteItem {
                value: v.to_string(),
                label: v.to_string(),
                description: None,
            })
            .collect();
        if items.is_empty() {
            return None;
        }
        Some(Suggestions {
            items,
            prefix: token.to_string(),
        })
    }

    fn apply(
        &mut self,
        lines: &[String],
        line: usize,
        col: usize,
        item: &AutocompleteItem,
        prefix: &str,
    ) -> Applied {
        let mut out = lines.to_vec();
        let start = col - prefix.len();
        let value = format!("{} ", item.value);
        out[line].replace_range(start..col, &value);
        Applied {
            lines: out,
            cursor_line: line,
            cursor_col: start + value.len(),
        }
    }
}

#[test]
fn autocomplete_list_opens_moves_and_applies() {
    let mut t = Ed::new();
    t.e.set_autocomplete(Some(Box::new(Files)));
    t.type_str("see @src");
    assert!(
        t.e.autocomplete_open(),
        "typing a completable token opens the list"
    );

    let lines = t.e.render(40, &plain());
    assert!(
        lines.iter().any(|l| l.contains("→ @src/main.rs")),
        "selected row first: {lines:?}"
    );

    t.press("down");
    let lines = t.e.render(40, &plain());
    assert!(
        lines.iter().any(|l| l.contains("→ @src/lib.rs")),
        "down moves the selection"
    );

    assert_eq!(t.press("tab"), EditorAction::Changed);
    assert_eq!(t.text(), "see @src/lib.rs ", "tab applies the selection");
    assert!(!t.e.autocomplete_open());

    // Escape closes the list and keeps the text.
    t.type_str("@src");
    assert!(t.e.autocomplete_open());
    t.press("escape");
    assert!(!t.e.autocomplete_open());
    assert_eq!(t.text(), "see @src/lib.rs @src");
}

#[test]
fn set_text_insert_and_is_empty() {
    let mut t = Ed::new();
    assert!(t.e.is_empty());
    t.e.insert("abc");
    assert_eq!(t.text(), "abc");
    assert_eq!(t.e.cursor_pos(), (0, 3));
    t.e.set_text("");
    assert!(t.e.is_empty());
    t.press("ctrl+-");
    assert_eq!(t.text(), "abc", "undo restores the text before set_text");
}

/// First run: Enter on a list the user did not touch sends the text as typed; a moved
/// selection is applied as before.
#[test]
fn enter_on_an_untouched_list_sends_the_text() {
    let mut t = Ed::new();
    t.e.set_autocomplete(Some(Box::new(Files)));
    t.type_str("see @src");
    assert!(t.e.autocomplete_open());
    assert_eq!(t.press("enter"), EditorAction::Submit("see @src".to_string()));
    assert!(!t.e.autocomplete_open());

    t.type_str("see @src");
    t.press("down");
    assert_eq!(
        t.press("enter"),
        EditorAction::Changed,
        "a moved selection is applied"
    );
    assert_eq!(t.text(), "see @src/lib.rs ");
}

/// First run: prose with an apostrophe never opens the file list in the real provider.
#[test]
fn prose_apostrophe_opens_no_list_in_the_editor() {
    let env = oxa_fake::TestEnv::new();
    env.write("work/.cargo/config.toml", "");
    env.write("work/src/main.rs", "");
    let work = env.work();
    let p =
        oxa_tui::CombinedProvider::new(vec![], oxa_tui::FileIndex::build(&work, 1000), &work, &env.home());
    let mut t = Ed::new();
    t.e.set_autocomplete(Some(Box::new(p)));
    t.type_str("what's ");
    assert!(!t.e.autocomplete_open(), "what's  opened the list");
    t.type_str("it's \"fine\" ");
    assert!(!t.e.autocomplete_open());
    assert_eq!(
        t.press("enter"),
        EditorAction::Submit("what's it's \"fine\" ".to_string())
    );
    t.type_str("@src/");
    assert!(t.e.autocomplete_open(), "@src/ opens the list");
}

/// Tab on a word with one candidate applies it (pi); `src/ma` alone opens no list.
#[test]
fn tab_with_one_candidate_applies_it() {
    let env = oxa_fake::TestEnv::new();
    env.write("work/src/main.rs", "");
    env.write("work/src/lib.rs", "");
    let work = env.work();
    let p =
        oxa_tui::CombinedProvider::new(vec![], oxa_tui::FileIndex::build(&work, 1000), &work, &env.home());
    let mut t = Ed::new();
    t.e.set_autocomplete(Some(Box::new(p)));
    t.type_str("src/ma");
    assert!(!t.e.autocomplete_open(), "a word with a slash is no trigger");
    t.press("tab");
    assert_eq!(t.text(), "src/main.rs");
    assert!(!t.e.autocomplete_open());
}

/// restoring queued messages in front of the draft keeps a pasted block's content
#[test]
fn prepend_text_keeps_paste_content() {
    let big = big_paste();
    let mut t = Ed::new();
    t.e.paste_text(&big);
    t.e.prepend_text("queued");
    assert_eq!(t.e.expanded_text(), format!("queued\n\n{big}"));
}
