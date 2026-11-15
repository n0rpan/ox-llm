//! D7 acceptance tests: Model, thinking, theme, settings selectors. Owner: task D7.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_config::{Dirs, Keymap, Settings};
use oxa_fake::TestEnv;
use oxa_interactive::selectors::{
    cycle_model, cycle_thinking, setting_items, ModelChoice, ModelSelector, ScopedModelsSelector,
    SettingChange, SettingKind, SettingsSelector, ThemeSelector, ThinkingSelector,
};
use oxa_interactive::Outcome;
use oxa_proto::{ThinkingLevel, Value};
use oxa_tui::{Key, Theme, ThemeInfo};
use std::path::PathBuf;

fn model(id: &str, endpoint: &str, n_ctx: Option<u32>, loaded: bool) -> ModelChoice {
    ModelChoice {
        id: id.to_string(),
        endpoint: endpoint.to_string(),
        n_ctx,
        loaded,
    }
}

fn catalogue() -> Vec<ModelChoice> {
    vec![
        model("qwen3-8b", "laptop", Some(32768), true),
        model("llama-70b", "lan", Some(8192), false),
        model("qwen3-30b", "lan", None, false),
    ]
}

/// Printable keys, one press each.
fn typed(text: &str) -> Vec<Key> {
    text.chars().map(Key::char).collect()
}

fn ids(values: &[&str]) -> Vec<String> {
    values.iter().map(|v| v.to_string()).collect()
}

/// filter and choose
#[test]
fn model_selector_search_and_select() {
    let km = Keymap::defaults();
    let mut sel = ModelSelector::new(catalogue(), "qwen3-8b", &[]);
    let lines = sel.render(80, &Theme::plain());
    assert!(
        lines.iter().any(|l| l.contains("✓ qwen3-8b")),
        "current model marked: {lines:?}"
    );

    for key in typed("llama") {
        assert_eq!(sel.key(&key, &km), Outcome::Pending);
    }
    let lines = sel.render(80, &Theme::plain());
    assert!(
        lines.iter().any(|l| l.contains("llama-70b")),
        "filtered row shown: {lines:?}"
    );
    assert!(
        !lines.iter().any(|l| l.contains("qwen3-8b")),
        "other rows filtered out: {lines:?}"
    );
    assert_eq!(
        sel.key(&Key::press("enter"), &km),
        Outcome::Done("llama-70b".to_string())
    );

    // Escape cancels; the search is typed and cleared with backspace.
    let mut sel = ModelSelector::new(catalogue(), "qwen3-8b", &[]);
    assert_eq!(sel.key(&Key::press("escape"), &km), Outcome::Cancelled);
    let mut sel = ModelSelector::new(catalogue(), "qwen3-8b", &[]);
    for key in typed("zzz") {
        sel.key(&key, &km);
    }
    assert!(sel
        .render(80, &Theme::plain())
        .iter()
        .any(|l| l.contains("No matching items")));
    for _ in 0..3 {
        sel.key(&Key::press("backspace"), &km);
    }
    assert_eq!(
        sel.key(&Key::press("enter"), &km),
        Outcome::Done("qwen3-8b".to_string())
    );
}

#[test]
fn model_selector_cloud_descriptions() {
    for id in [
        "anthropic/claude-haiku-5-5",
        "openai/gpt-6.1-sol",
        "openrouter/deepseek-v4-pro",
    ] {
        for ctx in [None, Some(200000)] {
            for loaded in [false, true] {
                let mut sel = ModelSelector::new(vec![model(id, "oxe", ctx, loaded)], id, &[]);
                let lines = sel.render(240, &Theme::plain());
                let row = &lines[2];
                assert!(row.contains("cloud"), "{row}");
                assert!(!row.contains("loaded") && !row.contains("oxe"), "{row}");
                assert_eq!(row.contains("ctx 200000"), ctx.is_some(), "{row}");
            }
        }
    }
    let mut sel = ModelSelector::new(catalogue(), "qwen3-8b", &[]);
    let lines = sel.render(240, &Theme::plain());
    assert!(lines[2].contains("laptop · ctx 32768 · loaded"));
    assert!(lines[3].contains("lan · ctx 8192 · not loaded"));
}

#[test]
fn model_selector_long_ids_use_available_width() {
    let ids = [
        "openrouter/deepseek-v4.1-flash-alpha",
        "openrouter/deepseek-v4.1-flash-beta",
    ];
    let mut sel = ModelSelector::new(
        ids.iter().map(|id| model(id, "oxe", None, false)).collect(),
        "",
        &[],
    );
    let lines = sel.render(240, &Theme::plain());
    for (row, id) in lines[2..4].iter().zip(ids) {
        assert!(row.contains(id), "{row}");
    }
    for width in [0, 8, 24, 41, 60] {
        let lines = sel.render(width, &Theme::plain());
        assert!(lines
            .iter()
            .all(|line| oxa_tui::visible_width(line) <= width as usize));
    }
}

/// ctrl+p over scoped models
#[test]
fn model_cycle_scoped() {
    let all = ids(&["a", "b", "c", "d"]);
    let scoped = ids(&["a", "c"]);
    // Scoped list wins over all models.
    assert_eq!(cycle_model(&all, &scoped, "a", true), Some("c".to_string()));
    assert_eq!(cycle_model(&all, &scoped, "c", true), Some("a".to_string()));
    assert_eq!(cycle_model(&all, &scoped, "a", false), Some("c".to_string()));
    // Current model outside the scope: forward starts at the first, backward at the last.
    assert_eq!(cycle_model(&all, &scoped, "b", true), Some("a".to_string()));
    assert_eq!(cycle_model(&all, &scoped, "b", false), Some("c".to_string()));
    // No scope: all models.
    assert_eq!(cycle_model(&all, &[], "a", true), Some("b".to_string()));
    // At most one candidate: nothing to cycle to.
    assert_eq!(cycle_model(&all, &ids(&["a"]), "a", true), None);
    assert_eq!(cycle_model(&ids(&["a"]), &[], "a", false), None);
}

/// levels offered per model
#[test]
fn thinking_selector_clamps() {
    let km = Keymap::defaults();

    // A model whose template only allows `off`: only `off` is offered, with the reason.
    let mut sel = ThinkingSelector::new(
        &[ThinkingLevel::Off],
        ThinkingLevel::High,
        Some("this model cannot keep thinking across turns"),
    );
    let lines = sel.render(80, &Theme::plain());
    assert!(
        lines.iter().any(|l| l.contains("cannot keep thinking")),
        "{lines:?}"
    );
    assert!(lines.iter().any(|l| l.contains("off")));
    assert!(
        !lines.iter().any(|l| l.contains("high")),
        "unavailable level not offered: {lines:?}"
    );
    assert_eq!(
        sel.key(&Key::press("enter"), &km),
        Outcome::Done(ThinkingLevel::Off)
    );

    // Current level preselected; Down moves to the next offered level only.
    let mut sel = ThinkingSelector::new(
        &[ThinkingLevel::Low, ThinkingLevel::Medium, ThinkingLevel::High],
        ThinkingLevel::Medium,
        None,
    );
    let lines = sel.render(80, &Theme::plain());
    assert!(lines.iter().any(|l| l.contains("✓ medium")), "{lines:?}");
    assert_eq!(sel.key(&Key::press("down"), &km), Outcome::Pending);
    assert_eq!(
        sel.key(&Key::press("enter"), &km),
        Outcome::Done(ThinkingLevel::High)
    );
    let mut sel = ThinkingSelector::new(&[ThinkingLevel::Low], ThinkingLevel::Medium, None);
    assert_eq!(sel.key(&Key::press("escape"), &km), Outcome::Cancelled);

    // The cycle key walks the offered levels only and wraps.
    let offered = [ThinkingLevel::Off, ThinkingLevel::Low, ThinkingLevel::High];
    assert_eq!(cycle_thinking(&offered, ThinkingLevel::Low), ThinkingLevel::High);
    assert_eq!(cycle_thinking(&offered, ThinkingLevel::High), ThinkingLevel::Off);
    assert_eq!(cycle_thinking(&offered, ThinkingLevel::Max), ThinkingLevel::Off);
    assert_eq!(cycle_thinking(&[], ThinkingLevel::Low), ThinkingLevel::Off);
}

/// saved to settings
#[test]
fn theme_selector_live_preview_and_save() {
    let km = Keymap::defaults();
    let env = TestEnv::new();
    let dirs = Dirs::under(env.root());
    let cwd = env.work();
    let themes = vec![
        ThemeInfo {
            name: "dark".to_string(),
            path: None,
        },
        ThemeInfo {
            name: "light".to_string(),
            path: None,
        },
        ThemeInfo {
            name: "solarized".to_string(),
            path: Some(PathBuf::from("/nonexistent/solarized.json")),
        },
    ];

    let mut sel = ThemeSelector::new(themes.clone(), "dark");
    assert_eq!(sel.original(), "dark");
    assert_eq!(sel.preview(), None, "nothing to preview before the cursor moves");
    assert_eq!(sel.key(&Key::press("down"), &km), Outcome::Pending);
    assert_eq!(sel.preview(), Some("light".to_string()));
    assert_eq!(sel.preview(), None, "reported once");
    assert_eq!(sel.key(&Key::press("down"), &km), Outcome::Pending);
    assert_eq!(sel.preview(), Some("solarized".to_string()));
    let chosen = match sel.key(&Key::press("enter"), &km) {
        Outcome::Done(name) => name,
        other => panic!("expected Done, got {other:?}"),
    };
    assert_eq!(chosen, "solarized");

    // The app persists the choice as the `theme` setting.
    let mut settings = Settings::load(&dirs, &cwd, false);
    settings
        .set(&dirs, &cwd, "theme", Value::String(chosen.clone()), false)
        .expect("save theme");
    assert_eq!(Settings::load(&dirs, &cwd, false).string("theme"), Some(chosen));

    // Escape restores: the selector reports the original theme and Cancelled.
    let mut sel = ThemeSelector::new(themes, "dark");
    assert_eq!(sel.key(&Key::press("down"), &km), Outcome::Pending);
    assert_eq!(sel.key(&Key::press("escape"), &km), Outcome::Cancelled);
    assert_eq!(sel.original(), "dark");
}

/// values persisted
#[test]
fn settings_selector_toggles_save() {
    let km = Keymap::defaults();
    let env = TestEnv::new();
    let dirs = Dirs::under(env.root());
    let cwd = env.work();
    let settings = Settings::load(&dirs, &cwd, false);
    let items = setting_items(&settings);
    assert!(items
        .iter()
        .any(|i| i.key == "compaction.enabled" && i.kind == SettingKind::Bool));

    // Bool toggle: search for the row, Enter flips it.
    let mut sel = SettingsSelector::new(items.clone());
    for key in typed("compactionenabled") {
        assert_eq!(sel.key(&key, &km), Outcome::Pending);
    }
    assert_eq!(sel.key(&Key::press("enter"), &km), Outcome::Pending);
    let changes = sel.take_changes();
    assert_eq!(
        changes,
        vec![SettingChange {
            key: "compaction.enabled".to_string(),
            value: Value::Bool(false),
        }]
    );
    assert!(sel.take_changes().is_empty(), "changes are reported once");

    // The app saves each change to the user settings file; a fresh load sees it.
    let mut settings = settings;
    for change in changes {
        settings
            .set(&dirs, &cwd, &change.key, change.value, false)
            .expect("save setting");
    }
    assert!(!Settings::load(&dirs, &cwd, false).bool("compaction.enabled", true));

    // Choice cycle: space advances a choice of at most four values (oxa.mode: workspace -> ask).
    let mut sel = SettingsSelector::new(setting_items(&Settings::load(&dirs, &cwd, false)));
    for key in typed("oxamode") {
        sel.key(&key, &km);
    }
    assert_eq!(sel.key(&Key::press("space"), &km), Outcome::Pending);
    assert_eq!(
        sel.take_changes(),
        vec![SettingChange {
            key: "oxa.mode".to_string(),
            value: Value::String("ask".to_string()),
        }]
    );

    // A choice of more than four values opens a submenu; Enter there picks the value.
    let mut sel = SettingsSelector::new(setting_items(&Settings::load(&dirs, &cwd, false)));
    for key in typed("defaultthinkinglevel") {
        sel.key(&key, &km);
    }
    assert_eq!(
        sel.key(&Key::press("enter"), &km),
        Outcome::Pending,
        "submenu opens"
    );
    assert_eq!(
        sel.key(&Key::press("down"), &km),
        Outcome::Pending,
        "medium -> high"
    );
    assert_eq!(sel.key(&Key::press("enter"), &km), Outcome::Pending);
    assert_eq!(
        sel.take_changes(),
        vec![SettingChange {
            key: "defaultThinkingLevel".to_string(),
            value: Value::String("high".to_string()),
        }]
    );

    // Escape closes the selector.
    assert_eq!(sel.key(&Key::press("escape"), &km), Outcome::Done(()));
}

/// Number rows: Enter opens an input, digits only, Enter commits, Escape drops the edit.
#[test]
fn settings_selector_number_edit() {
    let km = Keymap::defaults();
    let env = TestEnv::new();
    let dirs = Dirs::under(env.root());
    let cwd = env.work();
    let items = setting_items(&Settings::load(&dirs, &cwd, false));
    let mut sel = SettingsSelector::new(items);
    for key in typed("keeprecenttokens") {
        sel.key(&key, &km);
    }
    assert_eq!(
        sel.key(&Key::press("enter"), &km),
        Outcome::Pending,
        "opens the editor"
    );
    // Escape drops the edit and keeps the selector open.
    assert_eq!(sel.key(&Key::press("escape"), &km), Outcome::Pending);
    assert!(sel.take_changes().is_empty());

    assert_eq!(sel.key(&Key::press("enter"), &km), Outcome::Pending);
    for _ in 0..4 {
        sel.key(&Key::press("backspace"), &km);
    }
    // A letter is refused in a number field.
    assert_eq!(sel.key(&Key::char('x'), &km), Outcome::Ignored);
    for key in typed("1234") {
        sel.key(&key, &km);
    }
    assert_eq!(sel.key(&Key::press("enter"), &km), Outcome::Pending);
    assert_eq!(
        sel.take_changes(),
        vec![SettingChange {
            key: "compaction.keepRecentTokens".to_string(),
            value: Value::from(1234u64),
        }]
    );
}

/// enable/disable/reorder/save
#[test]
fn scoped_models_editor_keys() {
    let km = Keymap::defaults();
    let models = vec![
        model("a", "laptop", None, false),
        model("b", "lan", None, false),
        model("c", "laptop", None, false),
    ];
    let mut sel = ScopedModelsSelector::new(models.clone(), &ids(&["a"]));
    let step = |sel: &mut ScopedModelsSelector, name: &str| {
        let out = sel.key(&Key::press(name), &km);
        assert_eq!(out, Outcome::Pending, "key {name}");
    };
    step(&mut sel, "down"); // cursor on b
    assert_eq!(sel.key(&Key::press("space"), &km), Outcome::Pending); // enable b: [a, b, c] on/on/off
    step(&mut sel, "alt+down"); // reorder b below c: [a, c, b]
    step(&mut sel, "ctrl+x"); // clear all
    step(&mut sel, "up"); // cursor on c
    step(&mut sel, "space"); // enable c: [a off, c on, b off]
    step(&mut sel, "alt+up"); // reorder c to the top: [c on, a off, b off]
    step(&mut sel, "ctrl+a"); // enable all: [c, a, b]
    step(&mut sel, "ctrl+p"); // toggle the laptop provider under the cursor (c, a): off, off
    assert_eq!(
        sel.key(&Key::press("ctrl+s"), &km),
        Outcome::Done((ids(&["b"]), true)),
        "save persists the enabled list in order"
    );

    // Escape after an edit keeps the change for this session only.
    let mut sel = ScopedModelsSelector::new(models.clone(), &ids(&["a"]));
    step(&mut sel, "space"); // disable a
    assert_eq!(
        sel.key(&Key::press("escape"), &km),
        Outcome::Done((Vec::new(), false))
    );

    // Escape without an edit is a plain cancel.
    let mut sel = ScopedModelsSelector::new(models, &ids(&["a"]));
    assert_eq!(sel.key(&Key::press("escape"), &km), Outcome::Cancelled);
}

/// The settings rows follow pi's kinds: bool, choice, number and text.
#[test]
fn setting_items_kinds() {
    let env = TestEnv::new();
    let dirs = Dirs::under(env.root());
    let items = setting_items(&Settings::load(&dirs, &env.work(), false));
    let kind_of = |key: &str| items.iter().find(|i| i.key == key).map(|i| i.kind.clone());
    assert_eq!(kind_of("compaction.enabled"), Some(SettingKind::Bool));
    assert_eq!(kind_of("compaction.reserveTokens"), Some(SettingKind::Number));
    assert!(
        matches!(kind_of("oxa.mode"), Some(SettingKind::Choice(v)) if v.contains(&"workspace".to_string()))
    );
    assert_eq!(kind_of("theme"), None, "the theme has its own selector");
    assert_eq!(kind_of("enabledModels"), None, "lists are edited elsewhere");
}

#[test]
fn ctrl_s_selects_and_marks_the_choice_as_the_default() {
    // pi: app.thinking.save / app.models.save (ctrl+s) select the highlighted entry and set it
    // as the default (thinking-selector.ts, model-selector.ts `onSelectAsDefault`).
    let km = Keymap::defaults();
    let mut sel = ThinkingSelector::new(&ThinkingLevel::ALL, ThinkingLevel::Off, None);
    assert!(!sel.saves_default());
    assert!(matches!(sel.key(&Key::press("down"), &km), Outcome::Pending));
    assert_eq!(
        sel.key(&Key::press("ctrl+s"), &km),
        Outcome::Done(ThinkingLevel::Minimal)
    );
    assert!(sel.saves_default());
    let mut sel = ThinkingSelector::new(&ThinkingLevel::ALL, ThinkingLevel::Off, None);
    assert_eq!(
        sel.key(&Key::press("enter"), &km),
        Outcome::Done(ThinkingLevel::Off)
    );
    assert!(!sel.saves_default(), "enter only selects");

    let mut sel = ModelSelector::new(catalogue(), "qwen3-8b", &[]);
    let Outcome::Done(id) = sel.key(&Key::press("ctrl+s"), &km) else {
        panic!("ctrl+s selects")
    };
    assert_eq!(id, "qwen3-8b");
    assert!(sel.saves_default());
}

#[test]
fn scoped_models_status_survives_a_narrow_width() {
    // The enabled count has its own line; the long key hint is the one that is cut.
    let mut sel = ScopedModelsSelector::new(catalogue(), &[]);
    let lines = sel.render(40, &Theme::plain());
    let n = catalogue().len();
    assert!(
        lines.iter().any(|l| l.contains(&format!("0/{n} enabled"))),
        "{lines:?}"
    );
}
