//! A11 acceptance tests: settings.json schema, merge and typed access.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_config::{apply_list_patch, merge_settings, schema, validate, Dirs, Kind, QueueMode, Settings};
use oxa_fake::TestEnv;
use serde_json::{json, Map, Value};
use std::path::Path;

/// Every key documented in pi's settings.md (version 1.5.2 at commit 47e9d74), in document order.
const DOC_KEYS: &[&str] = &[
    "defaultProvider",
    "defaultModel",
    "defaultThinkingLevel",
    "modelThinkingLevels",
    "enabledModels",
    "hideThinkingBlock",
    "showCacheMissNotices",
    "cacheWarming",
    "steeringMode",
    "followUpMode",
    "externalEditor",
    "doubleEscapeAction",
    "treeFilterMode",
    "defaultProjectTrust",
    "defaultTools",
    "codemode.mode",
    "sessionDir",
    "sessionShareDestination",
    "compaction.enabled",
    "compaction.reserveTokens",
    "compaction.keepRecentTokens",
    "compaction.modelOverrides",
    "branchSummary.reserveTokens",
    "branchSummary.skipPrompt",
    "theme",
    "quietStartup",
    "tuiMode",
    "fullscreenExitOutput",
    "fullscreenScrollbar",
    "fullscreenCopyOnSelect",
    "fullscreenWheelScrollLines",
    "editorPaddingX",
    "outputPad",
    "autocompleteMaxVisible",
    "terminal.showImages",
    "terminal.imageWidthCells",
    "terminal.clearOnShrink",
    "images.autoResize",
    "images.blockImages",
    "markdown.codeBlockIndent",
    "markdown.mermaid",
    "transport",
    "httpProxy",
    "httpIdleTimeoutMs",
    "websocketConnectTimeoutMs",
    "retry.enabled",
    "retry.maxRetries",
    "retry.baseDelayMs",
    "retry.maxAgentDelayMs",
    "retry.provider.timeoutMs",
    "retry.provider.maxRetries",
    "retry.provider.maxRetryDelayMs",
    "shellPath",
    "shellCommandPrefix",
    "selfUpdateCommand",
    "packages",
    "extensions",
    "skills",
    "prompts",
    "themes",
    "enableSkillCommands",
    "enableInstallTelemetry",
    "enableAnalytics",
    "warnings.anthropicExtraUsage",
];

/// oxa's own keys (plan sec. 1 and PARITY SET-027, SET-028, SET-061, KEY-015).
const OXA_KEYS: &[&str] = &[
    "$schema",
    "oxa.compaction.softTokens",
    "oxa.toolOutputCap",
    "oxa.lead.serve",
    "oxa.mode",
    "hooks",
];

/// The keys that oxa accepts and ignores with a warning (verdict O in PARITY.md).
const VERDICT_O_KEYS: &[&str] = &[
    "defaultProvider",
    "sessionShareDestination",
    "images.autoResize",
    "transport",
    "websocketConnectTimeoutMs",
    "selfUpdateCommand",
    "enableInstallTelemetry",
    "enableAnalytics",
    "warnings.anthropicExtraUsage",
];

fn strs(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

/// Insert `value` at a dotted key into a JSON object, creating intermediate objects.
fn put(root: &mut Value, key: &str, value: Value) {
    let mut cur = root;
    let parts: Vec<&str> = key.split('.').collect();
    for part in &parts[..parts.len() - 1] {
        let obj = cur.as_object_mut().expect("object");
        cur = obj
            .entry((*part).to_string())
            .or_insert_with(|| Value::Object(Map::new()));
    }
    cur.as_object_mut()
        .expect("object")
        .insert(parts[parts.len() - 1].to_string(), value);
}

/// A sample value of the right kind for a schema key.
fn sample(kind: Kind) -> Value {
    match kind {
        Kind::Bool => json!(true),
        Kind::Number => json!(7),
        Kind::String => json!("sample"),
        Kind::Enum(options) => json!(options[0]),
        Kind::StringList => json!(["a"]),
        Kind::Object => json!({}),
        Kind::Any => json!("auto"),
    }
}

fn write_json(path: &Path, value: &Value) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, serde_json::to_string_pretty(value).expect("json")).expect("write");
}

/// /a file with every documented key yields no diagnostic for supported keys
#[test]
fn every_settings_md_key_parses() {
    // Every documented key is in the schema.
    for key in DOC_KEYS.iter().chain(OXA_KEYS.iter()) {
        assert!(
            schema().iter().any(|d| d.key == *key),
            "undocumented in schema: {key}"
        );
    }
    // A file with every schema key, each with a sample of the right kind, yields one warning per
    // verdict-O key and nothing else.
    let mut file = json!({});
    for def in schema() {
        put(&mut file, def.key, sample(def.kind));
    }
    let diags = validate(&file, "settings.json");
    assert_eq!(diags.len(), VERDICT_O_KEYS.len(), "diagnostics: {diags:?}");
    for d in &diags {
        assert!(
            VERDICT_O_KEYS
                .iter()
                .any(|k| d.message.contains(&format!("`{k}`"))),
            "supported key warned: {}",
            d.message
        );
    }
}

/// nested objects merge, scalars override
#[test]
fn user_project_merge() {
    let env = TestEnv::new();
    let dirs = Dirs::under(env.root());
    let cwd = env.work();
    write_json(
        &dirs.settings_file(),
        &json!({"theme": "dark", "compaction": {"reserveTokens": 1000, "enabled": true}}),
    );
    write_json(
        &dirs.project_settings_file(&cwd),
        &json!({"compaction": {"reserveTokens": 2000}}),
    );

    let s = Settings::load(&dirs, &cwd, true);
    assert!(s.diagnostics().is_empty(), "{:?}", s.diagnostics());
    assert_eq!(s.string("theme").as_deref(), Some("dark"));
    assert_eq!(
        s.compaction().reserve_tokens,
        2000,
        "project scalar overrides user"
    );
    assert!(s.compaction().enabled, "user nested key survives the merge");

    // The pure merge agrees.
    let merged = merge_settings(&json!({"a": {"x": 1, "y": 2}}), &json!({"a": {"y": 3}}));
    assert_eq!(merged, json!({"a": {"x": 1, "y": 3}}));
}

/// `user_bool` ignores the project layer, so a trusted project cannot enable a user-only switch
#[test]
fn user_bool_ignores_the_project_layer() {
    let key = "oxa.jobs.wakeOnFinish";
    let off = Settings::from_values(json!({}), json!({"oxa": {"jobs": {"wakeOnFinish": true}}}));
    assert!(off.bool(key, false), "the merged read sees the project value");
    assert!(!off.user_bool(key, false), "the user-only read does not");
    let denied = Settings::from_values(
        json!({"oxa": {"jobs": {"wakeOnFinish": false}}}),
        json!({"oxa": {"jobs": {"wakeOnFinish": true}}}),
    );
    assert!(!denied.user_bool(key, true));
    let on = Settings::from_values(json!({"oxa": {"jobs": {"wakeOnFinish": true}}}), json!({}));
    assert!(on.user_bool(key, false));
}

/// +x / -x patches
#[test]
fn plus_minus_lists() {
    // A patch over a lower-layer list.
    assert_eq!(
        apply_list_patch(&strs(&["read", "bash"]), &strs(&["-bash", "+grep"])),
        strs(&["read", "grep"])
    );
    // A patch without +/- entries replaces the base.
    assert_eq!(
        apply_list_patch(&strs(&["read", "bash"]), &strs(&["grep"])),
        strs(&["grep"])
    );
    // Exclusion wins when both are given for the same name; no duplicates on add.
    assert_eq!(
        apply_list_patch(&strs(&["read"]), &strs(&["+x", "-x", "+read"])),
        strs(&["read"])
    );
    // The `+x` entries of a project list patch the user list.
    let merged = merge_settings(
        &json!({"defaultTools": ["read", "bash"]}),
        &json!({"defaultTools": ["-bash", "+grep"]}),
    );
    assert_eq!(merged, json!({"defaultTools": ["read", "grep"]}));
    // A plain project list replaces the user list.
    let merged = merge_settings(
        &json!({"defaultTools": ["read", "bash"]}),
        &json!({"defaultTools": ["grep"]}),
    );
    assert_eq!(merged, json!({"defaultTools": ["grep"]}));

    // A patch with no explicit base patches the default built-ins, including lead-only ask.
    let s = Settings::from_values(
        json!({"defaultTools": ["-bash", "+powershell", "+grep"]}),
        json!({}),
    );
    let tools = s.default_tools().expect("explicit list");
    assert!(!tools.contains(&"bash".to_string()));
    assert!(tools.contains(&"read".to_string()) && tools.contains(&"ls".to_string()));
    assert!(tools.contains(&"grep".to_string()));
    assert!(
        tools.contains(&"ask".to_string()),
        "patches must preserve lead-only ask"
    );
    assert_eq!(
        tools.len(),
        8,
        "eight built-ins minus bash, plus powershell and grep was already present: {tools:?}"
    );
}

/// wrong type falls back to default
#[test]
fn bad_value_is_diagnostic_not_failure() {
    let env = TestEnv::new();
    let dirs = Dirs::under(env.root());
    let cwd = env.work();
    write_json(
        &dirs.settings_file(),
        &json!({"compaction": {"reserveTokens": "lots"}, "steeringMode": "bogus", "theme": "dark"}),
    );
    let s = Settings::load(&dirs, &cwd, true);
    assert_eq!(s.diagnostics().len(), 2, "{:?}", s.diagnostics());
    assert_eq!(s.compaction().reserve_tokens, 16384, "default after a wrong type");
    assert_eq!(s.steering_mode(), QueueMode::OneAtATime);
    assert_eq!(
        s.string("theme").as_deref(),
        Some("dark"),
        "other keys still load"
    );

    // A bad project value falls back to the user's value, not the default.
    let s = Settings::from_values(
        json!({"compaction": {"reserveTokens": 5000}}),
        json!({"compaction": {"reserveTokens": false}}),
    );
    assert_eq!(s.compaction().reserve_tokens, 5000);
    assert_eq!(s.diagnostics().len(), 1);
}

/// one diagnostic
#[test]
fn unknown_key_warns() {
    let file = json!({"fooBar": 1});
    let diags = validate(&file, "settings.json");
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert!(diags[0].message.contains("fooBar"));

    // A misspelt nested key is one diagnostic too.
    let diags = validate(&json!({"compaction": {"reserveToken": 1}}), "settings.json");
    assert_eq!(diags.len(), 1, "{diags:?}");
}

/// O-175: `npmCommand` (pi settings.md) is gone with npm package sources; a file that still
/// names it gets the plain unknown-setting warning.
#[test]
fn removed_npm_command_is_unknown() {
    let diags = validate(&json!({"npmCommand": ["npm"]}), "settings.json");
    assert_eq!(diags.len(), 1, "{diags:?}");
    let unknown = validate(&json!({"fooBar": 1}), "settings.json");
    assert_eq!(
        diags[0].message.replace("npmCommand", "K"),
        unknown[0].message.replace("fooBar", "K"),
        "same text as any unknown key"
    );
}

/// verdict-O keys accepted with a warning
#[test]
fn unsupported_pi_key_warns() {
    let s = Settings::from_values(
        json!({"transport": "sse", "defaultProvider": "anthropic"}),
        json!({}),
    );
    assert_eq!(s.diagnostics().len(), 2, "{:?}", s.diagnostics());
    // Accepted: the value is still in the merged settings.
    assert_eq!(s.string("transport").as_deref(), Some("sse"));
    assert!(s
        .diagnostics()
        .iter()
        .all(|d| d.level == oxa_proto::Level::Warning));
}

/// project_trusted = false
#[test]
fn untrusted_project_file_not_read() {
    let env = TestEnv::new();
    let dirs = Dirs::under(env.root());
    let cwd = env.work();
    write_json(
        &dirs.project_settings_file(&cwd),
        &json!({"theme": "project", "oxa": {"toolOutputCap": 1}}),
    );

    let s = Settings::load(&dirs, &cwd, false);
    assert_eq!(s.string("theme"), None, "untrusted project file must not be read");
    assert_eq!(s.tool_output_cap(), 16384);
    assert!(s.diagnostics().is_empty(), "{:?}", s.diagnostics());

    let trusted = Settings::load(&dirs, &cwd, true);
    assert_eq!(trusted.string("theme").as_deref(), Some("project"));
}

/// #1449: a present but unusable user settings file keeps the most restrictive security
/// settings, says so, and leaves other settings at their defaults. Absence and `{}` do not.
#[test]
fn unusable_user_settings_fail_safe_for_security_keys() {
    let restricted = |s: &Settings| {
        s.bool("oxa.offline", false)
            && s.string("oxa.sandbox.net").as_deref() == Some("off")
            && !s.bool("oxa.engine.allowRemote", false)
            && s.string("defaultProjectTrust").as_deref() == Some("never")
    };
    // Genuine first launch (no file) and a valid empty object: normal defaults, no warning.
    for content in [None, Some("{}")] {
        let env = TestEnv::new();
        let dirs = Dirs::under(env.root());
        if let Some(text) = content {
            std::fs::create_dir_all(dirs.settings_file().parent().unwrap()).unwrap();
            std::fs::write(dirs.settings_file(), text).unwrap();
        }
        let s = Settings::load(&dirs, &env.work(), false);
        assert!(!restricted(&s), "{content:?}");
        assert!(!s.bool("oxa.offline", false));
        assert!(s.diagnostics().is_empty(), "{:?}", s.diagnostics());
    }
    // Invalid JSON, empty bytes, a non-object root and an unreadable path all fail safe.
    for (name, content) in [
        ("invalid json", Some("{ \"oxa\": { \"offline\": true ")),
        ("empty bytes", Some("")),
        ("array root", Some("[1]")),
        ("null root", Some("null")),
        ("unreadable", None),
    ] {
        let env = TestEnv::new();
        let dirs = Dirs::under(env.root());
        let path = dirs.settings_file();
        match content {
            Some(text) => {
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, text).unwrap();
            }
            // A directory where the file should be: a read error that is not "not found".
            None => std::fs::create_dir_all(&path).unwrap(),
        }
        let s = Settings::load(&dirs, &env.work(), false);
        assert!(restricted(&s), "{name}: network and trust must stay restricted");
        assert!(s.layer_failed(), "{name}");
        // Unrelated settings keep their defaults.
        assert_eq!(s.tool_output_cap(), 16384, "{name}");
        let text: Vec<String> = s.diagnostics().iter().map(|d| d.message.clone()).collect();
        assert!(
            text.iter()
                .any(|m| m.contains("most restrictive") && m.contains("Fix or restore")),
            "{name}: owner must be told: {text:?}"
        );
    }
    // Recovery: a valid file takes effect, including an intentional removal of restrictions.
    let env = TestEnv::new();
    let dirs = Dirs::under(env.root());
    std::fs::create_dir_all(dirs.settings_file().parent().unwrap()).unwrap();
    std::fs::write(dirs.settings_file(), "{ broken").unwrap();
    assert!(restricted(&Settings::load(&dirs, &env.work(), false)));
    write_json(&dirs.settings_file(), &json!({"oxa": {"offline": false}}));
    let ok = Settings::load(&dirs, &env.work(), false);
    assert!(!ok.bool("oxa.offline", false));
    assert!(ok.diagnostics().is_empty());
}

/// keepRecentTokens 4000, toolOutputCap 16384, all tools
#[test]
fn oxa_defaults() {
    let env = TestEnv::new();
    let dirs = Dirs::under(env.root());
    let s = Settings::load(&dirs, &env.work(), true);
    assert!(s.diagnostics().is_empty());
    let c = s.compaction();
    assert!(c.enabled);
    assert_eq!(c.keep_recent_tokens, 4000);
    assert_eq!(c.reserve_tokens, 16384);
    assert_eq!(c.soft_tokens, None);
    assert_eq!(s.tool_output_cap(), 16384);
    assert_eq!(
        s.default_tools(),
        None,
        "no layer sets defaultTools: oxa's default is all seven"
    );
    assert_eq!(s.steering_mode(), QueueMode::OneAtATime);
    assert_eq!(s.follow_up_mode(), QueueMode::OneAtATime);
    let r = s.retry();
    assert!(r.enabled);
    assert_eq!((r.max_retries, r.base_delay_ms, r.max_delay_ms), (3, 2000, 60000));

    // The schema documents the same defaults.
    let def = schema()
        .iter()
        .find(|d| d.key == "defaultTools")
        .expect("defaultTools");
    let tools: Vec<String> = serde_json::from_str(def.default).expect("json default");
    assert_eq!(
        tools,
        strs(&["read", "bash", "edit", "write", "grep", "find", "ls"])
    );
}

/// user and project files
#[test]
fn set_persists_and_keeps_other_keys() {
    let env = TestEnv::new();
    let dirs = Dirs::under(env.root());
    let cwd = env.work();
    write_json(
        &dirs.settings_file(),
        &json!({"theme": "dark", "hideThinkingBlock": true, "compaction": {"enabled": false}}),
    );
    let mut s = Settings::load(&dirs, &cwd, true);

    // User file: a nested key is added; unrelated keys and their order stay.
    s.set(&dirs, &cwd, "compaction.reserveTokens", json!(9000), false)
        .expect("set user");
    assert_eq!(s.compaction().reserve_tokens, 9000);
    let text = std::fs::read_to_string(dirs.settings_file()).expect("read");
    let on_disk: Value = serde_json::from_str(&text).expect("json");
    assert_eq!(on_disk["theme"], json!("dark"));
    assert_eq!(on_disk["hideThinkingBlock"], json!(true));
    assert_eq!(
        on_disk["compaction"],
        json!({"enabled": false, "reserveTokens": 9000})
    );
    let root_keys: Vec<&str> = on_disk
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(root_keys, ["theme", "hideThinkingBlock", "compaction"]);

    // Project file: created, and it overrides the user value in memory.
    s.set(&dirs, &cwd, "theme", json!("light"), true)
        .expect("set project");
    assert_eq!(s.string("theme").as_deref(), Some("light"));
    let project: Value =
        serde_json::from_str(&std::fs::read_to_string(dirs.project_settings_file(&cwd)).expect("read"))
            .expect("json");
    assert_eq!(project, json!({"theme": "light"}));
    let user_again: Value =
        serde_json::from_str(&std::fs::read_to_string(dirs.settings_file()).expect("read")).expect("json");
    assert_eq!(
        user_again["theme"],
        json!("dark"),
        "user file untouched by a project set"
    );

    // A reload sees the same result.
    let reloaded = Settings::load(&dirs, &cwd, true);
    assert_eq!(reloaded.string("theme").as_deref(), Some("light"));
    assert_eq!(reloaded.compaction().reserve_tokens, 9000);
}

#[test]
fn project_edit_does_not_readopt_user_only_setting() {
    let env = TestEnv::new();
    let dirs = Dirs::under(env.root());
    let cwd = env.work();
    write_json(
        &dirs.settings_file(),
        &json!({"oxa": {"sandbox": {"net": "off"}}}),
    );
    write_json(
        &dirs.project_settings_file(&cwd),
        &json!({"oxa": {"sandbox": {"net": "on"}}}),
    );
    let mut settings = Settings::load(&dirs, &cwd, true);
    assert_eq!(settings.string("oxa.sandbox.net").as_deref(), Some("off"));

    settings
        .set(&dirs, &cwd, "theme", json!("dark"), true)
        .expect("set unrelated project setting");

    assert_eq!(settings.string("oxa.sandbox.net").as_deref(), Some("off"));
    assert_eq!(settings.string("theme").as_deref(), Some("dark"));

    settings
        .unset(&dirs, &cwd, "theme", true)
        .expect("unset unrelated project setting");
    assert_eq!(settings.string("oxa.sandbox.net").as_deref(), Some("off"));
}

/// Extra: a set with a value of the wrong type for a known key is refused and writes nothing.
#[test]
fn set_refuses_wrong_type_without_writing() {
    let env = TestEnv::new();
    let dirs = Dirs::under(env.root());
    let cwd = env.work();
    let mut s = Settings::load(&dirs, &cwd, true);
    let err = s
        .set(&dirs, &cwd, "compaction.reserveTokens", json!("lots"), false)
        .expect_err("wrong type");
    assert_eq!(err.code, oxa_proto::codes::INVALID);
    assert!(!dirs.settings_file().exists());
}

/// A key defined twice would shadow one of its two definitions.
#[test]
fn schema_keys_are_unique() {
    let keys: Vec<&str> = schema().iter().map(|d| d.key).collect();
    let unique: std::collections::BTreeSet<&str> = keys.iter().copied().collect();
    assert_eq!(unique.len(), keys.len());
}

#[test]
fn thinking_budgets_is_an_unknown_setting() {
    let settings = Settings::from_values(json!({"thinkingBudgets": {"high": 1000}}), json!({}));
    assert!(settings
        .diagnostics()
        .iter()
        .any(|d| d.message.contains("unknown setting `thinkingBudgets`")));
}

#[test]
fn ui_notify_schema_defaults_and_validation() {
    let def = schema()
        .iter()
        .find(|def| def.key == "ui.notify")
        .expect("ui.notify schema");
    assert_eq!(def.default, "\"unfocused\"");
    for policy in ["off", "unfocused", "always"] {
        let settings = Settings::from_values(json!({"ui": {"notify": policy}}), json!({}));
        assert!(settings.diagnostics().is_empty(), "{policy}");
        assert_eq!(settings.string("ui.notify").as_deref(), Some(policy));
    }
    let invalid = Settings::from_values(json!({"ui": {"notify": "sometimes"}}), json!({}));
    assert!(!invalid.diagnostics().is_empty());
}
