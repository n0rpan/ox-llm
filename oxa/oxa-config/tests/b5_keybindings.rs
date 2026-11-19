//! B5 acceptance tests: Keybindings.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_config::{Dirs, Keymap};
use oxa_fake::TestEnv;
use oxa_proto::KeyId;
use serde_json::json;

fn key(s: &str) -> KeyId {
    KeyId::parse(s).expect("test key parses")
}

/// Every id of keybindings.md (Linux defaults) with its default keys, in the document's order,
/// then oxa's `app.mode.cycle` and `app.yolo.toggle`. Written from the doc, not from the code.
const DOC_TABLE: &[(&str, &[&str])] = &[
    ("tui.editor.cursorUp", &["up"]),
    ("tui.editor.cursorDown", &["down"]),
    ("tui.editor.historyPrevious", &[]),
    ("tui.editor.historyNext", &[]),
    ("tui.editor.cursorLeft", &["left", "ctrl+b"]),
    ("tui.editor.cursorRight", &["right", "ctrl+f"]),
    ("tui.editor.cursorWordLeft", &["alt+left", "ctrl+left", "alt+b"]),
    (
        "tui.editor.cursorWordRight",
        &["alt+right", "ctrl+right", "alt+f"],
    ),
    ("tui.editor.cursorLineStart", &["home", "ctrl+a"]),
    ("tui.editor.cursorLineEnd", &["end", "ctrl+e"]),
    ("tui.editor.jumpForward", &["ctrl+]"]),
    ("tui.editor.jumpBackward", &["ctrl+alt+]"]),
    ("tui.editor.pageUp", &["pageUp", "ctrl+pageUp"]),
    ("tui.editor.pageDown", &["pageDown", "ctrl+pageDown"]),
    ("tui.editor.deleteCharBackward", &["backspace"]),
    ("tui.editor.deleteCharForward", &["delete", "ctrl+d"]),
    ("tui.editor.deleteWordBackward", &["ctrl+w", "alt+backspace"]),
    ("tui.editor.deleteWordForward", &["alt+d", "alt+delete"]),
    ("tui.editor.deleteToLineStart", &["ctrl+u"]),
    ("tui.editor.deleteToLineEnd", &["ctrl+k"]),
    ("tui.editor.yank", &["ctrl+y"]),
    ("tui.editor.yankPop", &["alt+y"]),
    ("tui.editor.undo", &["ctrl+-"]),
    ("tui.input.newLine", &["shift+enter", "ctrl+j"]),
    ("tui.input.submit", &["enter"]),
    ("tui.input.tab", &["tab"]),
    ("tui.input.copy", &["ctrl+c"]),
    ("tui.select.up", &["up"]),
    ("tui.select.down", &["down"]),
    ("tui.select.pageUp", &["pageUp"]),
    ("tui.select.pageDown", &["pageDown"]),
    ("tui.select.confirm", &["enter"]),
    ("tui.select.cancel", &["escape", "ctrl+c"]),
    ("tui.altScreen.pageUp", &["pageUp"]),
    ("tui.altScreen.pageDown", &["pageDown"]),
    ("tui.altScreen.halfPageUp", &[]),
    ("tui.altScreen.halfPageDown", &[]),
    ("tui.altScreen.lineUp", &[]),
    ("tui.altScreen.lineDown", &[]),
    ("tui.altScreen.previousPrompt", &["ctrl+shift+up", "ctrl+up"]),
    ("tui.altScreen.nextPrompt", &["ctrl+shift+down", "ctrl+down"]),
    ("tui.altScreen.search", &["ctrl+shift+f", "alt+s"]),
    ("tui.altScreen.searchNext", &["enter", "ctrl+g"]),
    ("tui.altScreen.searchPrevious", &["shift+enter", "ctrl+shift+g"]),
    ("tui.altScreen.searchClose", &["escape"]),
    ("tui.altScreen.top", &["ctrl+home"]),
    ("tui.altScreen.bottom", &["ctrl+end"]),
    ("app.interrupt", &["escape"]),
    ("app.clear", &["ctrl+c"]),
    ("app.exit", &["ctrl+d"]),
    ("app.suspend", &["ctrl+z"]),
    ("app.editor.external", &["ctrl+g"]),
    ("app.clipboard.pasteImage", &["ctrl+v"]),
    ("app.session.new", &[]),
    ("app.session.tree", &[]),
    ("app.session.fork", &[]),
    ("app.session.resume", &[]),
    ("app.session.togglePath", &["ctrl+p"]),
    ("app.session.toggleSort", &["ctrl+s"]),
    ("app.session.toggleNamedFilter", &["ctrl+n"]),
    ("app.session.rename", &["ctrl+r"]),
    ("app.session.delete", &["ctrl+d"]),
    ("app.session.deleteNoninvasive", &["ctrl+backspace"]),
    ("app.model.select", &["ctrl+l"]),
    ("app.model.cycleForward", &["ctrl+p"]),
    ("app.model.cycleBackward", &["shift+ctrl+p"]),
    ("app.models.save", &["ctrl+s"]),
    ("app.thinking.cycle", &["shift+tab"]),
    ("app.thinking.save", &["ctrl+s"]),
    ("app.thinking.toggle", &["ctrl+t"]),
    ("app.tools.expand", &["ctrl+o"]),
    ("app.message.copy", &["ctrl+x"]),
    ("app.message.followUp", &["alt+enter"]),
    ("app.message.dequeue", &["alt+up"]),
    ("app.tree.foldOrUp", &["ctrl+left", "alt+left"]),
    ("app.tree.unfoldOrDown", &["ctrl+right", "alt+right"]),
    ("app.tree.editLabel", &["shift+l"]),
    ("app.tree.toggleLabelTimestamp", &["shift+t"]),
    ("app.tree.filter.default", &["ctrl+d"]),
    ("app.tree.filter.noTools", &["ctrl+t"]),
    ("app.tree.filter.userOnly", &["ctrl+u"]),
    ("app.tree.filter.labeledOnly", &["ctrl+l"]),
    ("app.tree.filter.all", &["ctrl+a"]),
    ("app.tree.filter.cycleForward", &["ctrl+o"]),
    ("app.tree.filter.cycleBackward", &["shift+ctrl+o"]),
    ("app.models.enableAll", &["ctrl+a"]),
    ("app.models.clearAll", &["ctrl+x"]),
    ("app.models.toggleProvider", &["ctrl+p"]),
    ("app.models.reorderUp", &["alt+up"]),
    ("app.models.reorderDown", &["alt+down"]),
    ("app.mode.cycle", &["alt+m"]),
    ("app.yolo.toggle", &["ctrl+alt+o"]),
];

/// table test against the doc list
#[test]
fn every_id_and_default_of_keybindings_md() {
    let actions = Keymap::actions();
    let ids: Vec<&str> = actions.iter().map(|a| a.id).collect();
    let doc_ids: Vec<&str> = DOC_TABLE.iter().map(|(id, _)| *id).collect();
    assert_eq!(
        ids, doc_ids,
        "action ids and their order must match keybindings.md plus app.mode.cycle"
    );
    assert_eq!(DOC_TABLE.len(), 92);

    let defaults = Keymap::defaults();
    for (id, keys) in DOC_TABLE {
        let expected: Vec<KeyId> = keys.iter().map(|k| key(k)).collect();
        assert_eq!(defaults.keys_for(id), expected, "default keys of {id}");
    }
    for a in actions {
        assert!(!a.description.is_empty(), "{} has a description", a.id);
    }
    // No id appears twice.
    let mut sorted = doc_ids.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), doc_ids.len());
}

/// oxa defaults
#[test]
fn mode_cycle_alt_m_and_shift_tab_thinking() {
    let km = Keymap::defaults();
    assert_eq!(km.keys_for("app.mode.cycle"), vec![key("alt+m")]);
    assert!(km.matches(&key("alt+m"), "app.mode.cycle"));
    assert_eq!(km.action_for(&key("alt+m")), Some("app.mode.cycle"));

    assert_eq!(km.keys_for("app.thinking.cycle"), vec![key("shift+tab")]);
    assert!(km.matches(&key("shift+tab"), "app.thinking.cycle"));
    assert_eq!(km.action_for(&key("shift+tab")), Some("app.thinking.cycle"));
    assert!(!km.matches(&key("shift+tab"), "app.mode.cycle"));
}

/// string and list forms, empty list unbinds
#[test]
fn user_overrides() {
    let env = TestEnv::new();
    let dirs = Dirs::under(env.root());
    env.write(
        "home/.oxa/agent/keybindings.json",
        r#"{
            "$schema": "https://pi.dev/schemas/keybindings.schema.json",
            "app.session.new": "ctrl+shift+n",
            "app.session.tree": ["ctrl+shift+t", "alt+shift+t"],
            "tui.altScreen.pageUp": [],
            "app.clear": ["ctrl+q", "ctrl+q"]
        }"#,
    );
    let (km, diags) = Keymap::load(&dirs);
    assert!(diags.is_empty(), "no diagnostics for a valid file: {diags:?}");

    assert_eq!(km.keys_for("app.session.new"), vec![key("ctrl+shift+n")]);
    assert_eq!(
        km.keys_for("app.session.tree"),
        vec![key("ctrl+shift+t"), key("alt+shift+t")]
    );
    assert!(
        km.keys_for("tui.altScreen.pageUp").is_empty(),
        "empty list unbinds"
    );
    assert!(!km.matches(&key("pageUp"), "tui.altScreen.pageUp"));
    // Other defaults stay, and the key order of a list is kept with duplicates dropped.
    assert_eq!(km.keys_for("tui.altScreen.pageDown"), vec![key("pagedown")]);
    assert_eq!(km.keys_for("app.clear"), vec![key("ctrl+q")]);
    assert!(!km.matches(&key("ctrl+c"), "app.clear"));
    assert!(km.matches(&key("ctrl+c"), "tui.input.copy"));
}

#[test]
fn user_overrides_apply_over_existing_keymap() {
    let mut km = Keymap::defaults();
    let diags = km.apply(&json!({ "app.model.select": "ctrl+shift+l", "app.mode.cycle": [] }));
    assert!(diags.is_empty());
    assert_eq!(km.keys_for("app.model.select"), vec![key("ctrl+shift+l")]);
    assert!(km.keys_for("app.mode.cycle").is_empty());
    // A later apply replaces the earlier one for the same id.
    let diags = km.apply(&json!({ "app.model.select": "f5" }));
    assert!(diags.is_empty());
    assert_eq!(km.keys_for("app.model.select"), vec![key("f5")]);
}

/// default stays
#[test]
fn unknown_id_is_diagnostic() {
    let env = TestEnv::new();
    let dirs = Dirs::under(env.root());
    env.write(
        "home/.oxa/agent/keybindings.json",
        r#"{ "app.nope": "ctrl+q", "app.clear": "hyper+x", "app.exit": 3, "tui.editor.yank": "ctrl+y" }"#,
    );
    let (km, diags) = Keymap::load(&dirs);
    assert_eq!(
        diags.len(),
        3,
        "one diagnostic each for the unknown id, the bad key and the bad type: {diags:?}"
    );
    assert!(diags.iter().any(|d| d.message.contains("app.nope")));
    assert!(diags.iter().any(|d| d.message.contains("app.clear")));
    assert!(diags.iter().any(|d| d.message.contains("app.exit")));
    assert!(diags.iter().all(|d| d.source.ends_with("keybindings.json")));

    // Defaults stay for the rejected entries; the valid override is applied.
    assert_eq!(km.keys_for("app.clear"), vec![key("ctrl+c")]);
    assert_eq!(km.keys_for("app.exit"), vec![key("ctrl+d")]);
    assert!(km.keys_for("app.nope").is_empty());
    assert!(!km.matches(&key("ctrl+q"), "app.nope"));
    assert_eq!(km.keys_for("tui.editor.yank"), vec![key("ctrl+y")]);
}

#[test]
fn unreadable_file_keeps_defaults() {
    let env = TestEnv::new();
    let dirs = Dirs::under(env.root());

    // Absent file: defaults, silently.
    let (km, diags) = Keymap::load(&dirs);
    assert!(diags.is_empty());
    assert_eq!(km, Keymap::defaults());

    // Broken JSON: defaults and one diagnostic.
    env.write("home/.oxa/agent/keybindings.json", "{ not json");
    let (km, diags) = Keymap::load(&dirs);
    assert_eq!(diags.len(), 1);
    assert_eq!(km, Keymap::defaults());

    // Not an object: defaults and one diagnostic.
    let mut km = Keymap::defaults();
    let diags = km.apply(&json!(["ctrl+q"]));
    assert_eq!(diags.len(), 1);
    assert_eq!(km, Keymap::defaults());
}

/// same key, same context
#[test]
fn conflict_report() {
    // Same context: the editor's yank and deleteToLineStart both on ctrl+u.
    let mut km = Keymap::defaults();
    let diags = km.apply(&json!({ "tui.editor.yank": "ctrl+u" }));
    assert!(diags.is_empty());
    let conflicts = km.conflicts();
    let hit = conflicts
        .iter()
        .find(|(k, _)| *k == key("ctrl+u"))
        .expect("ctrl+u is bound twice in tui.editor");
    assert_eq!(
        hit.1,
        vec![
            "tui.editor.deleteToLineStart".to_string(),
            "tui.editor.yank".to_string()
        ]
    );

    // Different context: tui.select.cancel on ctrl+x does not conflict with app.message.copy.
    let mut km = Keymap::defaults();
    km.apply(&json!({ "tui.select.cancel": "ctrl+x" }));
    let conflicts = km.conflicts();
    assert!(
        !conflicts
            .iter()
            .any(|(k, ids)| *k == key("ctrl+x") && ids.iter().any(|i| i == "tui.select.cancel")),
        "tui.select.* and app.* are different contexts"
    );

    // Editor and app context: ctrl+d is on deleteCharForward (tui.editor) and app.exit (app).
    let km = Keymap::defaults();
    assert!(!km.conflicts().iter().any(|(k, ids)| *k == key("ctrl+d")
        && ids.iter().any(|i| i == "tui.editor.deleteCharForward")
        && ids.iter().any(|i| i == "app.exit")));

    // A conflict is reported with the ids in the order of Keymap::actions().
    let mut km = Keymap::defaults();
    km.apply(&json!({ "app.clear": "ctrl+shift+q", "app.exit": "ctrl+shift+q" }));
    let conflicts = km.conflicts();
    let hit = conflicts
        .iter()
        .find(|(k, _)| *k == key("ctrl+shift+q"))
        .expect("app conflict");
    assert_eq!(hit.1, vec!["app.clear".to_string(), "app.exit".to_string()]);
}

/// lookups
#[test]
fn matches_and_keys_for() {
    let km = Keymap::defaults();
    // ctrl+c is bound in three places: tui.input.copy, tui.select.cancel and app.clear.
    assert!(km.matches(&key("ctrl+c"), "tui.input.copy"));
    assert!(km.matches(&key("ctrl+c"), "tui.select.cancel"));
    assert!(km.matches(&key("ctrl+c"), "app.clear"));
    assert!(!km.matches(&key("ctrl+c"), "app.exit"));
    assert_eq!(km.action_for(&key("ctrl+c")), Some("tui.input.copy"));
    assert_eq!(
        km.actions_for(&key("ctrl+c")),
        vec!["tui.input.copy", "tui.select.cancel", "app.clear"]
    );

    // Notation is normalised: shift+ctrl+p is ctrl+shift+p.
    assert_eq!(km.keys_for("app.model.cycleBackward"), vec![key("ctrl+shift+p")]);
    assert!(km.matches(&key("ctrl+shift+p"), "app.model.cycleBackward"));
    assert_eq!(
        km.action_for(&key("shift+ctrl+p")),
        Some("app.model.cycleBackward")
    );

    // Multi-key default and aliases of the key names.
    assert_eq!(
        km.keys_for("tui.input.newLine"),
        vec![key("shift+enter"), key("ctrl+j")]
    );
    assert!(km.matches(&key("return"), "tui.input.submit"));
    assert!(km.matches(&key("esc"), "app.interrupt"));

    // Unbound or unknown lookups are empty, not errors.
    assert!(km.keys_for("app.session.new").is_empty());
    assert!(km.keys_for("no.such.action").is_empty());
    assert!(km.actions_for(&key("ctrl+alt+shift+f12")).is_empty());
    assert_eq!(km.action_for(&key("ctrl+alt+shift+f12")), None);
}

#[test]
fn configured_ids_claiming_one_key_are_reported() {
    // pi KeybindingsManager.rebuild(): a key claimed by more than one configured id is a
    // conflict; both ids keep the key. Defaults never count.
    let mut km = Keymap::defaults();
    let diags = km.apply(&serde_json::json!({
        "app.session.new": "f7",
        "app.session.tree": ["f7", "f8"],
        "app.tools.expand": "ctrl+t",
    }));
    let msgs: Vec<&str> = diags.iter().map(|d| d.message.as_str()).collect();
    assert_eq!(
        msgs,
        ["key conflict: f7 is bound to app.session.new, app.session.tree"],
        "{msgs:?}"
    );
    assert!(km.matches(&key("f7"), "app.session.new") && km.matches(&key("f7"), "app.session.tree"));
}
