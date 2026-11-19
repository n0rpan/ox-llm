//! Contract fixtures of the ox AI panel (`oxa/omarchy-panel/tests/fixtures`).
//! The JSON files are the frozen examples of `oxa/docs/pages/panel.md`: every `oxa ... --json`
//! object the panel reads has a `type` and a `version`, and the fault answers are what the panel
//! must recognise. The Rust commands (R-slices) and the panel (P-slices) both test against these
//! files, so a change to a field changes the fixture in the same pull request.

use serde_json::Value;
use std::fs;
use std::path::PathBuf;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("omarchy-panel")
        .join("tests")
        .join("fixtures")
}

fn panel_source(name: &str) -> String {
    let path = fixtures_dir()
        .parent()
        .expect("fixtures directory has a tests parent")
        .parent()
        .expect("tests directory has a panel parent")
        .join(name);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn read_fixture(name: &str) -> String {
    let path = fixtures_dir().join(name);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn read_object(name: &str) -> serde_json::Map<String, Value> {
    let value: Value =
        serde_json::from_str(&read_fixture(name)).unwrap_or_else(|e| panic!("{name} is not valid JSON: {e}"));
    match value {
        Value::Object(map) => map,
        other => panic!("{name} is not a JSON object: {other}"),
    }
}

#[test]
fn panel_fixtures_are_valid_json_with_type_and_version() {
    let expected = [
        ("status.json", "oxa_suite_status"),
        ("models.json", "oxa_models"),
        ("usage.json", "oxa_usage"),
        ("storage.json", "oxa_storage"),
        ("error.json", "oxa_error"),
    ];
    for (name, kind) in expected {
        let object = read_object(name);
        assert_eq!(
            object.get("type").and_then(Value::as_str),
            Some(kind),
            "{name}: type"
        );
        assert_eq!(
            object.get("version").and_then(Value::as_u64),
            Some(1),
            "{name}: version"
        );
    }
}

#[test]
fn panel_error_fixture_has_command_and_code() {
    let object = read_object("error.json");
    assert!(
        object.get("command").and_then(Value::as_str).is_some(),
        "error.json: command"
    );
    assert!(
        object.get("code").and_then(Value::as_str).is_some(),
        "error.json: code"
    );
}

#[test]
fn panel_status_fixture_has_the_sections_the_panel_reads() {
    let object = read_object("status.json");
    for key in [
        "oxa",
        "capabilities",
        "default_model",
        "local",
        "cloud",
        "lead",
        "sessions",
        "workers",
        "usage_state",
    ] {
        assert!(object.contains_key(key), "status.json: missing {key}");
    }
    let capabilities = object
        .get("capabilities")
        .and_then(Value::as_array)
        .expect("status.json: capabilities is an array");
    assert!(
        capabilities.iter().any(|c| c.as_str() == Some("usage")),
        "status.json: the usage capability is listed"
    );
}

#[test]
fn panel_old_binary_fixture_is_text_and_not_an_object() {
    let text = read_fixture("old_binary.txt");
    assert!(
        serde_json::from_str::<Value>(&text).is_err(),
        "old_binary.txt must not parse as JSON"
    );
    assert!(text.contains("Usage:"), "old_binary.txt shows the usage line");
}

#[test]
fn panel_empty_fixture_is_empty() {
    assert!(read_fixture("empty.txt").is_empty(), "empty.txt has no bytes");
}

#[test]
fn panel_root_forwards_the_bar_button_implicit_size() {
    let panel = panel_source("Panel.qml");
    assert!(
        panel.contains("implicitWidth: button.implicitWidth"),
        "Panel.qml must forward the bar button's implicit width"
    );
    assert!(
        panel.contains("implicitHeight: button.implicitHeight"),
        "Panel.qml must forward the bar button's implicit height"
    );
}
