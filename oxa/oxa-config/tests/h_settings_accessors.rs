//! Wave H (lane cli): the typed accessors of the settings that had no consumer before
//! (pi settings.md): `defaultThinkingLevel`, `modelThinkingLevels`, `retry.provider.*`,
//! `httpIdleTimeoutMs`, `sessionDir`, `shellPath`, `shellCommandPrefix`, and the codemode keys
//! that oxa consumes.

use oxa_config::{validate, ProviderRetrySettings, Settings};
use oxa_proto::ThinkingLevel;
use serde_json::json;
use std::path::{Path, PathBuf};

fn s(v: serde_json::Value) -> Settings {
    Settings::from_values(v, json!({}))
}

#[test]
fn startup_thinking_order() {
    assert_eq!(s(json!({})).startup_thinking("m"), ThinkingLevel::Medium);
    assert_eq!(
        s(json!({ "defaultThinkingLevel": "off" })).startup_thinking("m"),
        ThinkingLevel::Off
    );
    let per =
        s(json!({ "defaultThinkingLevel": "low", "modelThinkingLevels": { "m": "max", "oxe/n": "high" } }));
    assert_eq!(per.startup_thinking("m"), ThinkingLevel::Max);
    assert_eq!(
        per.startup_thinking("n"),
        ThinkingLevel::High,
        "pi's provider/modelId key"
    );
    assert_eq!(per.startup_thinking("other"), ThinkingLevel::Low);
    // A wrongly typed default falls back to medium (the value is dropped with a diagnostic).
    assert_eq!(
        s(json!({ "defaultThinkingLevel": "lots" })).startup_thinking("m"),
        ThinkingLevel::Medium
    );
}

#[test]
fn provider_retry_and_idle_timeout() {
    assert_eq!(
        s(json!({})).provider_retry(),
        ProviderRetrySettings {
            max_retries: 0,
            max_retry_delay_ms: 60000,
            timeout_ms: None
        }
    );
    let p =
        s(json!({ "retry": { "provider": { "maxRetries": 2, "maxRetryDelayMs": 0, "timeoutMs": 900 } } }));
    assert_eq!(
        p.provider_retry(),
        ProviderRetrySettings {
            max_retries: 2,
            max_retry_delay_ms: 0,
            timeout_ms: Some(900)
        }
    );
    assert_eq!(s(json!({})).http_idle_timeout_ms(), Some(300_000));
    assert_eq!(
        s(json!({ "httpIdleTimeoutMs": 1234 })).http_idle_timeout_ms(),
        Some(1234)
    );
    assert_eq!(s(json!({ "httpIdleTimeoutMs": 0 })).http_idle_timeout_ms(), None);
    assert_eq!(
        s(json!({ "httpIdleTimeoutMs": "disabled" })).http_idle_timeout_ms(),
        None
    );
}

#[test]
fn paths_and_shell() {
    let home = Path::new("/h");
    let cwd = Path::new("/w");
    assert_eq!(s(json!({})).session_dir(home, cwd), None);
    assert_eq!(
        s(json!({ "sessionDir": "rel/s" })).session_dir(home, cwd),
        Some(PathBuf::from("/w/rel/s"))
    );
    assert_eq!(
        s(json!({ "sessionDir": "~/s" })).session_dir(home, cwd),
        Some(PathBuf::from("/h/s"))
    );
    assert_eq!(
        s(json!({ "sessionDir": "/abs" })).session_dir(home, cwd),
        Some(PathBuf::from("/abs"))
    );
    assert_eq!(
        s(json!({ "shellPath": "~/bin/zsh" })).shell_path(home),
        Some(PathBuf::from("/h/bin/zsh"))
    );
    assert_eq!(s(json!({ "shellPath": "" })).shell_path(home), None);
    assert_eq!(
        s(json!({ "shellCommandPrefix": "shopt -s expand_aliases" })).shell_command_prefix(),
        Some("shopt -s expand_aliases".to_string())
    );
    assert_eq!(s(json!({})).shell_command_prefix(), None);
}

#[test]
fn consumed_codemode_keys_are_known() {
    let d = validate(
        &json!({ "codemode": { "enabled": true, "toolSearch": true, "maxWorkers": 2, "mode": "only" } }),
        "test",
    );
    assert!(d.is_empty(), "{d:?}");
}
