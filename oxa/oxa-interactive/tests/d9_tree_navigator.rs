//! D9 acceptance tests: Tree navigator.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.
//! (The tree navigator is pure: no file, no engine and no terminal is touched here.)

use oxa_config::Keymap;
use oxa_interactive::{ForkPointSelector, Outcome, TreeFilter, TreePick, TreeSelector};
use oxa_proto::{Entry, EntryBody, ThinkingLevel, ToolCall, WireMsg};
use oxa_session::Tree;
use oxa_tui::{Key, Theme};

const T0: u64 = 1_700_000_000_000; // 2023-11-14 22:13:20 UTC

fn entry(id: &str, parent: Option<&str>, body: EntryBody) -> Entry {
    Entry {
        id: id.to_string(),
        parent: parent.map(str::to_string),
        ts_ms: T0,
        body,
    }
}

fn user(id: &str, parent: Option<&str>, text: &str) -> Entry {
    entry(
        id,
        parent,
        EntryBody::Msg {
            wire: WireMsg::user(text),
            parts: Vec::new(),
            tool: None,
        },
    )
}

fn assistant(id: &str, parent: Option<&str>, text: &str) -> Entry {
    entry(
        id,
        parent,
        EntryBody::Msg {
            wire: WireMsg::assistant(text),
            parts: Vec::new(),
            tool: None,
        },
    )
}

fn label(id: &str, parent: Option<&str>, target: &str, text: Option<&str>, ts: u64) -> Entry {
    let mut e = entry(
        id,
        parent,
        EntryBody::Label {
            target: target.to_string(),
            label: text.map(str::to_string),
        },
    );
    e.ts_ms = ts;
    e
}

fn keymap() -> Keymap {
    Keymap::defaults()
}

fn press(id: &str) -> Key {
    Key::press(id)
}

/// A typed character, as the terminal parser delivers it (capitals carry their text).
fn typed(c: char) -> Key {
    Key::char(c)
}

fn render(sel: &mut TreeSelector) -> Vec<String> {
    sel.render(100, &Theme::plain())
}

/// The status line `(i/n) [filter]`.
fn status(lines: &[String]) -> String {
    lines.last().expect("status line").trim().to_string()
}

/// The row lines (between the search line and the status line).
fn rows(lines: &[String]) -> Vec<String> {
    lines[1..lines.len() - 1].to_vec()
}

fn has_row(lines: &[String], needle: &str) -> bool {
    rows(lines).iter().any(|l| l.contains(needle))
}

/// A tree: u0 -> a1 -> { u1 -> a2 , u2 (leaf) }.
fn fold_tree() -> Tree {
    Tree::new(vec![
        user("u0", None, "start"),
        assistant("a1", Some("u0"), "one"),
        user("u1", Some("a1"), "left"),
        assistant("a2", Some("u1"), "left reply"),
        user("u2", Some("a1"), "right"),
    ])
}

/// keys and state
#[test]
fn fold_unfold() {
    let tree = fold_tree();
    let mut sel = TreeSelector::new(&tree, TreeFilter::Default, 20, T0);
    let km = keymap();

    // The selection starts at the leaf u2 (3rd of 5 visible rows in branch-first order:
    // u0, a1, u2, u1, a2).
    let lines = render(&mut sel);
    assert_eq!(status(&lines), "(3/5)");
    assert_eq!(rows(&lines).len(), 5);

    // u2 is a leaf: fold does nothing there and jumps to the segment start (u0, the root).
    assert_eq!(sel.key(&press("ctrl+left"), &km), Outcome::Pending);
    assert_eq!(status(&render(&mut sel)), "(1/5)");

    // Folding the root hides its whole subtree.
    assert_eq!(sel.key(&press("ctrl+left"), &km), Outcome::Pending);
    let folded = render(&mut sel);
    assert_eq!(status(&folded), "(1/1)");
    assert!(!has_row(&folded, "one"));
    assert!(has_row(&folded, "start"));

    // Fold again on an already folded node: no change.
    sel.key(&press("ctrl+left"), &km);
    assert_eq!(status(&render(&mut sel)), "(1/1)");

    // Unfold restores the subtree.
    sel.key(&press("ctrl+right"), &km);
    let unfolded = render(&mut sel);
    assert_eq!(status(&unfolded), "(1/5)");

    // Unfold on an unfolded node jumps to the next segment start: a1 has two children, so the
    // first one (u2, the branch of the leaf) is selected.
    sel.key(&press("ctrl+right"), &km);
    assert_eq!(status(&render(&mut sel)), "(3/5)");

    // Move to u1 (a branch segment) and fold it: its child a2 disappears, the fold marker shows.
    sel.key(&press("down"), &km);
    assert_eq!(status(&render(&mut sel)), "(4/5)");
    sel.key(&press("ctrl+left"), &km);
    let folded_u1 = render(&mut sel);
    assert_eq!(status(&folded_u1), "(4/4)");
    assert!(!has_row(&folded_u1, "left reply"));
    assert!(has_row(&folded_u1, "⊞"));
    sel.key(&press("ctrl+right"), &km);
    assert_eq!(status(&render(&mut sel)), "(4/5)");
}

/// pi's filter modes
#[test]
fn five_filters() {
    // u1 -> a1 -> usage ; a1 -> c1 (tool call only) -> t1 (tool result) -> a2 -> label(u1) ->
    // model change -> u2 (leaf).
    let tree = Tree::new(vec![
        user("u1", None, "hello"),
        assistant("a1", Some("u1"), "hi there"),
        entry(
            "usage",
            Some("a1"),
            EntryBody::Usage {
                kind: "chat".to_string(),
                usage: Default::default(),
            },
        ),
        entry(
            "c1",
            Some("a1"),
            EntryBody::Msg {
                wire: WireMsg {
                    images: Vec::new(),
                    tool_calls: vec![ToolCall {
                        id: "call1".to_string(),
                        name: "bash".to_string(),
                        args_raw: r#"{"command":"ls -la"}"#.to_string(),
                    }],
                    ..WireMsg::assistant("")
                },
                parts: Vec::new(),
                tool: None,
            },
        ),
        entry(
            "t1",
            Some("c1"),
            EntryBody::Msg {
                wire: WireMsg::tool_result("call1", "file list"),
                parts: Vec::new(),
                tool: None,
            },
        ),
        assistant("a2", Some("t1"), "done"),
        label("lab", Some("a2"), "u1", Some("start"), T0),
        entry("mc", Some("lab"), EntryBody::ModelChange("m2".to_string())),
        user("u2", Some("mc"), "bye"),
    ]);
    let km = keymap();
    let mut sel = TreeSelector::new(&tree, TreeFilter::Default, 20, T0);

    // Default: user and assistant text, tool results; no labels, model changes, usage, or the
    // tool-call-only reply (the leaf is u2): u1, a1, t1, a2, u2.
    let lines = render(&mut sel);
    assert_eq!(status(&lines), "(5/5)");
    assert!(has_row(&lines, "user: hello"));
    // The tool result shows the call it answers; the tool-call-only reply above it is hidden.
    assert!(has_row(&lines, "[bash: ls -la]"));
    assert!(!has_row(&lines, "[label"));
    assert!(!has_row(&lines, "[model"));

    // No tools: the tool result goes.
    sel.key(&press("ctrl+t"), &km);
    assert_eq!(sel.filter(), TreeFilter::NoTools);
    let lines = render(&mut sel);
    assert_eq!(status(&lines), "(4/4) [no-tools]");
    assert!(!has_row(&lines, "[bash"));
    // Toggling again goes back to the default.
    sel.key(&press("ctrl+t"), &km);
    assert_eq!(sel.filter(), TreeFilter::Default);

    // User only: u1 and u2; the leaf stays selected.
    sel.key(&press("ctrl+u"), &km);
    assert_eq!(sel.filter(), TreeFilter::UserOnly);
    let lines = render(&mut sel);
    assert_eq!(status(&lines), "(2/2) [user]");
    assert_eq!(rows(&lines).len(), 2);

    // Labeled only: u1 is the only labeled entry; the hidden leaf moves to its nearest visible
    // ancestor.
    sel.key(&press("ctrl+l"), &km);
    assert_eq!(sel.filter(), TreeFilter::LabeledOnly);
    let lines = render(&mut sel);
    assert_eq!(status(&lines), "(1/1) [labeled]");
    assert!(has_row(&lines, "[start]"));

    // All: every entry except usage and the tool-call-only reply: u1, a1, t1, a2, lab, mc, u2.
    sel.key(&press("ctrl+a"), &km);
    assert_eq!(sel.filter(), TreeFilter::All);
    // The selection followed the labeled-only view to u1 (pi: the cursor keeps the nearest
    // visible entry), so it is the first of the seven rows.
    let lines = render(&mut sel);
    assert_eq!(status(&lines), "(1/7) [all]");
    assert!(has_row(&lines, "[label: start]"));
    assert!(has_row(&lines, "[model: m2]"));

    // Cycling: all -> default (forward), default -> all (backward), default -> no-tools (forward).
    sel.key(&press("ctrl+o"), &km);
    assert_eq!(sel.filter(), TreeFilter::Default);
    sel.key(&press("shift+ctrl+o"), &km);
    assert_eq!(sel.filter(), TreeFilter::All);
    sel.key(&press("ctrl+d"), &km);
    assert_eq!(sel.filter(), TreeFilter::Default);
    sel.key(&press("ctrl+o"), &km);
    assert_eq!(sel.filter(), TreeFilter::NoTools);
    sel.key(&press("ctrl+o"), &km);
    assert_eq!(sel.filter(), TreeFilter::UserOnly);
    sel.key(&press("ctrl+o"), &km);
    assert_eq!(sel.filter(), TreeFilter::LabeledOnly);
}

/// toggles
#[test]
fn labels_and_timestamps() {
    let tree = Tree::new(vec![
        user("u1", None, "hello"),
        label("lab", Some("u1"), "u1", Some("start"), T0),
        assistant("a1", Some("lab"), "hi"),
    ]);
    let km = keymap();

    // Same day as the label: HH:MM (UTC). Hidden until the toggle.
    let mut sel = TreeSelector::new(&tree, TreeFilter::All, 20, T0 + 3_600_000);
    let before = render(&mut sel);
    assert!(has_row(&before, "[start] user: hello"));
    assert!(!status(&before).contains("[+label time]"));

    // shift+t: the timestamp appears before the content and the status says so.
    sel.key(&typed('T'), &km);
    let on = render(&mut sel);
    assert!(status(&on).contains("[+label time]"), "{:?}", status(&on));
    assert!(has_row(&on, "[start] 22:13 user: hello"), "{on:?}");

    // Toggle back off.
    sel.key(&typed('T'), &km);
    let off = render(&mut sel);
    assert!(!status(&off).contains("[+label time]"));
    assert!(!has_row(&off, "22:13"));

    // Another year: YY/M/D HH:MM. Another day in the same year: M/D HH:MM.
    let mut next_year = TreeSelector::new(&tree, TreeFilter::All, 20, 1_735_689_600_000);
    next_year.key(&typed('T'), &km);
    assert!(has_row(&render(&mut next_year), "23/11/14 22:13"));
    let mut same_year = TreeSelector::new(&tree, TreeFilter::All, 20, 1_701_388_800_000);
    same_year.key(&typed('T'), &km);
    assert!(has_row(&render(&mut same_year), "11/14 22:13"));
}

/// return value
#[test]
fn user_message_select_refills_editor() {
    let tree = Tree::new(vec![
        user("u1", None, "first"),
        assistant("a1", Some("u1"), "one"),
        user("u2", Some("a1"), "second"),
    ]);
    let km = keymap();
    let mut sel = TreeSelector::new(&tree, TreeFilter::Default, 20, T0);

    // The leaf is the user message "second": selecting it moves the leaf to its parent and
    // refills the editor with its text.
    assert_eq!(
        sel.key(&press("enter"), &km),
        Outcome::Done(TreePick::Navigate {
            target: "a1".to_string(),
            summarize: false,
            instructions: None,
            refill_editor: Some("second".to_string()),
        })
    );

    // An assistant entry: the leaf moves to it, nothing to refill.
    sel.key(&press("up"), &km);
    assert_eq!(
        sel.key(&press("enter"), &km),
        Outcome::Done(TreePick::Navigate {
            target: "a1".to_string(),
            summarize: false,
            instructions: None,
            refill_editor: None,
        })
    );

    // A root user message: the leaf moves before the first entry (empty target).
    sel.key(&press("up"), &km);
    assert_eq!(
        sel.key(&press("enter"), &km),
        Outcome::Done(TreePick::Navigate {
            target: String::new(),
            summarize: false,
            instructions: None,
            refill_editor: Some("first".to_string()),
        })
    );

    // Escape with an empty search closes the navigator.
    assert_eq!(sel.key(&press("escape"), &km), Outcome::Cancelled);
}

/// active path highlight
#[test]
fn current_leaf_marked() {
    // u1 -> a1 -> u2 (leaf) and a side branch u1 -> a3. The leaf is the last entry in file order.
    let tree = Tree::new(vec![
        user("u1", None, "first"),
        assistant("a3", Some("u1"), "side"),
        assistant("a1", Some("u1"), "one"),
        user("u2", Some("a1"), "second"),
    ]);
    let mut sel = TreeSelector::new(&tree, TreeFilter::Default, 20, T0);
    let lines = render(&mut sel);

    // The branch of the leaf comes first, and its rows carry the active-path marker.
    let body = rows(&lines);
    let side = body.iter().find(|l| l.contains("side")).expect("side row");
    assert!(!side.contains('•'), "{side:?}");
    for text in ["first", "one", "second"] {
        let row = body.iter().find(|l| l.contains(text)).expect("active row");
        assert!(row.contains('•'), "{row:?}");
    }
    // The selected row is the leaf.
    let selected = body.iter().find(|l| l.starts_with("› ")).expect("cursor row");
    assert!(selected.contains("second"), "{selected:?}");
    // The active branch is listed before the side branch.
    let pos = |text: &str| body.iter().position(|l| l.contains(text)).unwrap();
    assert!(pos("second") < pos("side"));
}

/// set and clear
#[test]
fn label_edit() {
    let tree = Tree::new(vec![user("u1", None, "hello"), assistant("a1", Some("u1"), "hi")]);
    let km = keymap();
    let mut sel = TreeSelector::new(&tree, TreeFilter::Default, 20, T0);

    // Select u1, open the label editor (shift+l), type and confirm.
    sel.key(&press("up"), &km);
    assert_eq!(sel.key(&typed('L'), &km), Outcome::Pending);
    assert!(render(&mut sel)[0].contains("Label"));
    sel.key(&typed('x'), &km);
    assert_eq!(
        sel.key(&press("enter"), &km),
        Outcome::Done(TreePick::SetLabel {
            target: "u1".to_string(),
            label: Some("x".to_string()),
        })
    );

    // The app writes the label and hands back the tree; the selection stays on u1.
    let labeled = Tree::new(vec![
        user("u1", None, "hello"),
        assistant("a1", Some("u1"), "hi"),
        label("lab", Some("a1"), "u1", Some("x"), T0),
    ]);
    sel.set_tree(&labeled);
    let lines = render(&mut sel);
    assert!(has_row(&lines, "[x] user: hello"), "{lines:?}");
    assert!(lines.iter().any(|l| l.starts_with("› ") && l.contains("hello")));

    // Edit again: the field starts with the current label; backspace and confirm clears it.
    sel.key(&typed('L'), &km);
    sel.key(&press("backspace"), &km);
    assert_eq!(
        sel.key(&press("enter"), &km),
        Outcome::Done(TreePick::SetLabel {
            target: "u1".to_string(),
            label: None,
        })
    );

    // Escape while editing drops the edit without a result; escape again closes the navigator.
    sel.key(&typed('L'), &km);
    sel.key(&typed('z'), &km);
    assert_eq!(sel.key(&press("escape"), &km), Outcome::Pending);
    assert!(!render(&mut sel)[0].contains("Label"));
    assert_eq!(sel.key(&press("escape"), &km), Outcome::Cancelled);
}

#[test]
fn search_filters_rows_and_escape_clears() {
    let tree = fold_tree();
    let km = keymap();
    let mut sel = TreeSelector::new(&tree, TreeFilter::Default, 20, T0);
    for c in "reply".chars() {
        sel.key(&typed(c), &km);
    }
    let lines = render(&mut sel);
    assert_eq!(lines[0].trim(), "Type to search: reply");
    assert_eq!(status(&lines), "(1/1)");
    assert!(has_row(&lines, "left reply"));
    // Escape clears the search first (the selection stays on the match); the next escape closes.
    assert_eq!(sel.key(&press("escape"), &km), Outcome::Pending);
    assert_eq!(status(&render(&mut sel)), "(5/5)");
    assert_eq!(sel.key(&press("escape"), &km), Outcome::Cancelled);
}

#[test]
fn tool_calls_render_as_names() {
    let tree = Tree::new(vec![
        user("u1", None, "list"),
        entry(
            "c1",
            Some("u1"),
            EntryBody::Msg {
                wire: WireMsg {
                    images: Vec::new(),
                    tool_calls: vec![ToolCall {
                        id: "call9".to_string(),
                        name: "read".to_string(),
                        args_raw: r#"{"path":"src/main.rs"}"#.to_string(),
                    }],
                    ..WireMsg::assistant("")
                },
                parts: Vec::new(),
                tool: None,
            },
        ),
        entry(
            "t1",
            Some("c1"),
            EntryBody::Msg {
                wire: WireMsg::tool_result("call9", "fn main"),
                parts: Vec::new(),
                tool: None,
            },
        ),
        entry(
            "thinking",
            Some("t1"),
            EntryBody::ThinkingLevelChange(ThinkingLevel::High),
        ),
        assistant("a1", Some("thinking"), "read it"),
    ]);
    let mut sel = TreeSelector::new(&tree, TreeFilter::All, 20, T0);
    let lines = render(&mut sel);
    assert!(has_row(&lines, "[read: src/main.rs]"), "{lines:?}");
    assert!(has_row(&lines, "[thinking: high]"), "{lines:?}");
}

#[test]
fn fork_point_selects_user_message_of_active_branch() {
    let tree = Tree::new(vec![
        user("u1", None, "first"),
        assistant("a1", Some("u1"), "one"),
        user("u2", Some("a1"), "second"),
        assistant("a2", Some("u2"), "two"),
        user("u3", Some("a2"), "third"),
    ]);
    let km = keymap();
    let mut fork = ForkPointSelector::new(&tree, 10);
    let lines = fork.render(80, &Theme::plain());
    assert!(lines.iter().any(|l| l.contains("Fork from Message")));
    assert!(lines.iter().any(|l| l.starts_with("› third")));
    assert!(lines.iter().any(|l| l.contains("Message 3 of 3")));

    // Up goes to the older message; enter returns its id and text.
    fork.key(&press("up"), &km);
    assert_eq!(
        fork.key(&press("enter"), &km),
        Outcome::Done(("u2".to_string(), "second".to_string()))
    );
    assert_eq!(fork.key(&press("escape"), &km), Outcome::Cancelled);
}

/// Wave H: pi's `TreeSelectorComponent` frame (title, key help, borders) around the list, and
/// `app.message.copy` (Ctrl+X) on the selected entry (pi `getEntryCopyText`).
#[test]
fn framed_render_and_copy() {
    let tree = fold_tree();
    let mut sel = TreeSelector::new(&tree, TreeFilter::Default, 20, T0);
    let km = keymap();
    let lines = sel.render_framed(100, &Theme::plain(), &km);
    assert_eq!(lines[0], "");
    assert_eq!(lines[1], "─".repeat(100));
    assert_eq!(lines[2].trim(), "Session Tree");
    // The help wraps at 100 columns (pi's chunk-aware wrapping): it ends at the search line.
    let search = lines.iter().position(|l| l.contains("Type to search:")).unwrap();
    let help = lines[3..search].join(" · ");
    for part in [
        "↑/↓ move",
        "←/→ page",
        "ctrl+←/→ branch",
        "ctrl+x copy",
        "shift+l label",
        "shift+t label time",
        "filters ctrl+d/t/u/l/a",
        "cycle ctrl+o/ctrl+shift+o", // oxa key ids are canonical (ctrl before shift)
    ] {
        assert!(help.contains(part), "{part:?} in {help:?}");
    }
    assert_eq!(search, 5, "two help lines at 100 columns");
    assert_eq!(lines[search + 1], "─".repeat(100));
    assert_eq!(*lines.last().unwrap(), "─".repeat(100));
    assert!(lines.iter().any(|l| l.contains("(3/5)")));
    // The leaf u2 is selected: Ctrl+X copies its text.
    assert_eq!(
        sel.key(&press("ctrl+x"), &km),
        Outcome::Done(TreePick::Copy {
            text: Some("right".to_string())
        })
    );
    // Narrow: the help wraps, nothing is wider than the frame.
    for l in sel.render_framed(40, &Theme::plain(), &km) {
        assert!(oxa_tui::visible_width(&l) <= 40, "{l:?}");
    }
}
