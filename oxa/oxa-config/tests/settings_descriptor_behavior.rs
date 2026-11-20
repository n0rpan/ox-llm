//! Behaviour preserved when the schema is derived from the settings descriptors.

use oxa_config::{descriptor, schema, validate};
use serde_json::json;

#[test]
fn visible_tool_settings_keep_their_validation_and_provider_default() {
    for key in ["defaultTools", "oxa.webSearch.provider"] {
        assert!(descriptor(key).is_some(), "{key} is editable in settings");
    }
    assert!(validate(
        &json!({"defaultTools": ["read"], "oxa": {"webSearch": {"provider": "none"}}}),
        "settings.json",
    )
    .is_empty());
    for value in [
        json!({"defaultTools": "read"}),
        json!({"oxa": {"webSearch": {"provider": "invalid"}}}),
    ] {
        assert!(!validate(&value, "settings.json").is_empty());
    }
    let provider = schema()
        .iter()
        .find(|key| key.key == "oxa.webSearch.provider")
        .expect("search provider has a schema entry");
    assert_eq!(provider.default, "\"brave\"");
}

#[test]
fn worker_stop_grace_is_validated_without_a_menu_row() {
    let key = "oxa.workers.stopGraceSeconds";
    assert!(descriptor(key).is_none());
    assert!(validate(
        &json!({"oxa": {"workers": {"stopGraceSeconds": 10}}}),
        "settings.json",
    )
    .is_empty());
    assert!(!validate(
        &json!({"oxa": {"workers": {"stopGraceSeconds": "ten"}}}),
        "settings.json",
    )
    .is_empty());
    let grace = schema()
        .iter()
        .find(|entry| entry.key == key)
        .expect("worker stop grace has a schema entry");
    assert_eq!(grace.default, "10");
}
