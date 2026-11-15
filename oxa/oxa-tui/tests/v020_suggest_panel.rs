//! The suggestion panel of OXA's look (POPUPS.md): the `/` command menu and the file
//! suggestions as one themed panel; pi's bare list stays as it was. Public API only.
use oxa_config::Keymap;
use oxa_tui::{
    strip_ansi, suggest_match_positions, suggest_panel, visible_width, Applied, AutocompleteItem,
    AutocompleteProvider, ColorMode, Component, Editor, EditorOpts, Key, PopupStyle, Suggestions, Theme,
};

fn colour() -> Theme {
    oxa_tui::builtin_theme("dark", ColorMode::TrueColor, None).expect("builtin dark theme")
}

fn item(label: &str, description: &str) -> AutocompleteItem {
    AutocompleteItem {
        value: label.to_string(),
        label: label.to_string(),
        description: (!description.is_empty()).then(|| description.to_string()),
    }
}

fn commands() -> Vec<AutocompleteItem> {
    vec![
        item("model", "Select the model"),
        item("mode", "Permission mode: ask, workspace or full"),
        item("mcp", "MCP servers and their tools"),
        item("compact", "Summarise the conversation to free context"),
        item("new", "Start a new session"),
        item("resume", "Pick an earlier session"),
        item("theme", "Choose a colour theme"),
    ]
}

fn files() -> Vec<AutocompleteItem> {
    [
        "@src/main.rs",
        "@src/model.rs",
        "@docs/pages/settings.md",
        "@Cargo.toml",
    ]
    .iter()
    .map(|l| item(l, ""))
    .collect()
}

fn panel(
    items: &[AutocompleteItem],
    selected: usize,
    prefix: &str,
    width: usize,
    style: PopupStyle,
) -> Vec<String> {
    suggest_panel(items, selected, prefix, 5, width, style, &Theme::plain())
}

// ---------------------------------------------------------------------------------------------
// Golden panels

#[test]
fn golden_command_menu_at_sixty_columns() {
    assert_eq!(
        panel(&commands(), 1, "/mo", 60, PopupStyle::Panel),
        [
            "┌─ commands ───────────────────────────────────────── 2/7 ─┐",
            "│   model    Select the model                              ┃",
            "│ ▌ mode     Permission mode: ask, workspace or full       ┃",
            "│   mcp      MCP servers and their tools                   ┃",
            "│   compact  Summarise the conversation to free context    │",
            "│   new      Start a new session                           │",
            "└─ ↑↓ select · tab complete · enter run · esc close ───────┘",
        ]
    );
    // Scrolled to the end: the thumb is at the bottom, the position says so.
    assert_eq!(
        panel(&commands(), 6, "/", 60, PopupStyle::Panel),
        [
            "┌─ commands ───────────────────────────────────────── 7/7 ─┐",
            "│   mcp      MCP servers and their tools                   │",
            "│   compact  Summarise the conversation to free context    │",
            "│   new      Start a new session                           ┃",
            "│   resume   Pick an earlier session                       ┃",
            "│ ▌ theme    Choose a colour theme                         ┃",
            "└─ ↑↓ select · tab complete · enter run · esc close ───────┘",
        ]
    );
}

#[test]
fn golden_file_suggestions() {
    assert_eq!(
        panel(&files(), 0, "@sm", 60, PopupStyle::Panel),
        [
            "┌─ files ──────────────────────────────────────────── 1/4 ─┐",
            "│ ▌ @src/main.rs                                           │",
            "│   @src/model.rs                                          │",
            "│   @docs/pages/settings.md                                │",
            "│   @Cargo.toml                                            │",
            "└─ ↑↓ select · tab/enter accept · esc close ───────────────┘",
        ]
    );
}

#[test]
fn golden_ascii_and_narrow() {
    assert_eq!(
        panel(&commands(), 1, "/mo", 60, PopupStyle::PanelAscii),
        [
            "+- commands ----------------------------------------- 2/7 -+",
            "|   model    Select the model                              #",
            "| > mode     Permission mode: ask, workspace or full       #",
            "|   mcp      MCP servers and their tools                   #",
            "|   compact  Summarise the conversation to free context    |",
            "|   new      Start a new session                           |",
            "+- up/down select - tab complete - enter run - esc close --+",
        ]
    );
    // Narrow: descriptions are cut, the hint shrinks, the frame holds.
    assert_eq!(
        panel(&commands(), 1, "/mo", 30, PopupStyle::Panel),
        [
            "┌─ commands ─────────── 2/7 ─┐",
            "│   model    Select the mod… ┃",
            "│ ▌ mode     Permission mod… ┃",
            "│   mcp      MCP servers an… ┃",
            "│   compact  Summarise the … │",
            "│   new      Start a new se… │",
            "└─ esc close ────────────────┘",
        ]
    );
    for style in [PopupStyle::Panel, PopupStyle::PanelAscii] {
        let lines = panel(&commands(), 3, "/c", 44, style);
        assert_eq!(
            lines.iter().all(|l| l.is_ascii()),
            style == PopupStyle::PanelAscii
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Properties

#[test]
fn every_line_is_exactly_the_width_and_the_height_is_bounded() {
    let long = vec![
        item(&"x".repeat(90), &"long description ".repeat(20)),
        item("b\u{1b}[31mad\ttab", "con\ntrol"),
        item("日本語のファイル.md", "wide characters"),
    ];
    for theme in [Theme::plain(), colour()] {
        for style in [PopupStyle::Panel, PopupStyle::PanelAscii] {
            for width in 12..=140usize {
                for (items, prefix) in [(commands(), "/m"), (files(), "@s"), (long.clone(), "/x")] {
                    for selected in 0..items.len() {
                        let lines = suggest_panel(&items, selected, prefix, 5, width, style, &theme);
                        assert_eq!(lines.len(), items.len().min(5) + 2, "rows + header + footer");
                        for l in &lines {
                            assert_eq!(visible_width(l), width, "{width} {selected}: {:?}", strip_ansi(l));
                            assert!(!l.contains('\n') && !l.contains('\t'));
                        }
                        // The selected row is always on screen.
                        let mark = if style == PopupStyle::Panel { "▌" } else { ">" };
                        assert_eq!(
                            lines
                                .iter()
                                .filter(|l| strip_ansi(l).contains(&format!(" {mark} ")))
                                .count(),
                            1,
                            "{width} {selected}"
                        );
                    }
                }
            }
        }
    }
    // Nothing to show, or no room for a frame: no lines.
    assert!(suggest_panel(&[], 0, "/", 5, 80, PopupStyle::Panel, &colour()).is_empty());
    assert!(suggest_panel(&commands(), 0, "/", 5, 11, PopupStyle::Panel, &colour()).is_empty());
    // The height follows `autocompleteMaxVisible`.
    assert_eq!(
        suggest_panel(&commands(), 0, "/", 3, 80, PopupStyle::Panel, &colour()).len(),
        5
    );
}

#[test]
fn plain_has_no_escape_and_colour_comes_from_the_theme() {
    for l in panel(&commands(), 1, "/mo", 80, PopupStyle::Panel) {
        assert!(!l.contains('\x1b'), "{l:?}");
    }
    let theme = colour();
    let lines = suggest_panel(&commands(), 1, "/mo", 5, 80, PopupStyle::Panel, &theme);
    // The frame is subtle, the selected row is highlighted, the others are not.
    assert!(lines[0].starts_with(&theme.fg_code("borderMuted")));
    assert!(lines[2].contains(&theme.bg_code("selectedBg")));
    assert!(!lines[1].contains(&theme.bg_code("selectedBg")));
    // The matched characters are emphasised: bold in `accent`, on every row that matches.
    let hit = format!("{}\x1b[1mmo\x1b[22m", theme.fg_code("accent"));
    assert!(
        lines[1].contains(&hit) && lines[2].contains(&hit),
        "{:?}",
        lines[1]
    );
    assert!(
        !lines[3].contains("\x1b[1m"),
        "`mcp` does not match `mo`: {:?}",
        lines[3]
    );
    // Descriptions are dimmed; the selected one is readable.
    assert!(lines[1].contains(&format!("{}Select the model", theme.fg_code("muted"))));
    assert!(lines[2].contains(&format!("{}Permission mode", theme.fg_code("text"))));
    // The scroll thumb only when the list is longer than the panel.
    assert!(lines.iter().any(|l| l.contains('┃')));
    let short = suggest_panel(&files(), 0, "@", 5, 80, PopupStyle::Panel, &theme);
    assert!(short.iter().all(|l| !l.contains('┃')));
    // Every line closes what it opens.
    for l in &lines {
        assert!(l.ends_with("\x1b[39m"), "{l:?}");
        assert_eq!(l.matches("\x1b[48;").count(), l.matches("\x1b[49m").count());
    }
}

#[test]
fn matched_characters() {
    assert_eq!(suggest_match_positions("model", "/mo"), [0, 1]);
    assert_eq!(suggest_match_positions("@src/model.rs", "@sm"), [1, 5]);
    assert_eq!(suggest_match_positions("settings.md", "docs/pages/se"), [0, 1]);
    assert!(suggest_match_positions("model", "/zz").is_empty());
}

// ---------------------------------------------------------------------------------------------
// In the editor

struct Cmds;

impl AutocompleteProvider for Cmds {
    fn suggestions(
        &mut self,
        lines: &[String],
        line: usize,
        col: usize,
        _force: bool,
    ) -> Option<Suggestions> {
        let text = &lines[line][..col];
        text.starts_with('/').then(|| Suggestions {
            items: commands(),
            prefix: text.to_string(),
        })
    }
    fn apply(
        &mut self,
        lines: &[String],
        line: usize,
        _col: usize,
        item: &AutocompleteItem,
        _prefix: &str,
    ) -> Applied {
        let mut out = lines.to_vec();
        out[line] = format!("/{} ", item.value);
        Applied {
            cursor_col: out[line].len(),
            lines: out,
            cursor_line: line,
        }
    }
}

fn editor(opts: EditorOpts) -> Editor {
    let mut e = Editor::new(opts);
    e.set_autocomplete(Some(Box::new(Cmds)));
    let km = Keymap::defaults();
    for c in "/mo".chars() {
        e.key(&Key::char(c), &km);
    }
    assert!(e.autocomplete_open());
    e
}

#[test]
fn the_editor_keeps_pis_list_unless_asked_for_the_panel() {
    let plain = Theme::plain();
    // The default (pi's look): the bare list, byte for byte as before.
    let mut pi = editor(EditorOpts::default());
    let lines = pi.render(60, &plain);
    assert_eq!(lines[3], "→ model  Select the model");
    assert!(lines.iter().all(|l| !l.contains('┌')));
    assert_eq!(pi.popup(60, &plain)[0], "→ model  Select the model");

    // OXA's look, inline (the regular layout): the panel follows the editor.
    let mut oxa = editor(EditorOpts {
        popup: PopupStyle::Panel,
        ..EditorOpts::default()
    });
    let inline = oxa.render(60, &plain);
    assert_eq!(inline.len(), 3 + 7);
    assert_eq!(
        inline[3],
        "┌─ commands ───────────────────────────────────────── 1/7 ─┐"
    );
    assert_eq!(
        inline[4],
        "│ ▌ model    Select the model                              ┃"
    );

    // Detached (the fullscreen layout): the editor is as high as without suggestions, so an
    // opening list moves nothing; the owner floats `popup()` where it wants.
    let mut floating = editor(EditorOpts {
        popup: PopupStyle::Panel,
        popup_detached: true,
        ..EditorOpts::default()
    });
    assert_eq!(floating.render(60, &plain).len(), 3);
    assert_eq!(floating.popup(60, &plain), inline[3..]);
    // Moving the selection moves the mark and the position.
    floating.key(&Key::press("down"), &Keymap::defaults());
    let moved = floating.popup(60, &plain);
    assert_eq!(
        moved[0],
        "┌─ commands ───────────────────────────────────────── 2/7 ─┐"
    );
    assert_eq!(
        moved[2],
        "│ ▌ mode     Permission mode: ask, workspace or full       ┃"
    );
    // Closed: no lines.
    floating.key(&Key::press("escape"), &Keymap::defaults());
    assert!(floating.popup(60, &plain).is_empty());
}
