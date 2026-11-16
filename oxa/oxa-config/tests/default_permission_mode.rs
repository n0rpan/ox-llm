//! #202: `permissions.defaultMode` decides the permission mode at start. A fresh install starts
//! in `auto`; a project may lower the user's mode but never raise it.

use oxa_config::Settings;
use oxa_proto::PermissionMode;
use serde_json::{json, Value};

fn mode(user: Value, project: Value) -> PermissionMode {
    Settings::from_values(user, project).default_permission_mode()
}

fn dm(m: &str) -> Value {
    json!({ "permissions": { "defaultMode": m } })
}

#[test]
fn a_fresh_install_starts_in_auto() {
    assert_eq!(mode(json!({}), Value::Null), PermissionMode::Auto);
}

#[test]
fn the_users_saved_default_is_used() {
    for m in ["standard", "auto", "yolo", "plan"] {
        assert_eq!(mode(dm(m), Value::Null).as_str(), m);
    }
}

#[test]
fn a_project_may_lower_but_never_raise() {
    // Lower: taken.
    assert_eq!(mode(dm("yolo"), dm("standard")), PermissionMode::Standard);
    assert_eq!(mode(dm("auto"), dm("plan")), PermissionMode::Plan);
    assert_eq!(mode(json!({}), dm("standard")), PermissionMode::Standard);
    // Raise: ignored, the user's mode stands.
    assert_eq!(mode(json!({}), dm("yolo")), PermissionMode::Auto);
    assert_eq!(mode(dm("standard"), dm("yolo")), PermissionMode::Standard);
    assert_eq!(mode(dm("standard"), dm("auto")), PermissionMode::Standard);
    assert_eq!(mode(dm("plan"), dm("standard")), PermissionMode::Plan);
}

#[test]
fn an_unknown_value_is_reported_and_ignored() {
    let s = Settings::from_values(dm("root"), Value::Null);
    assert_eq!(s.default_permission_mode(), PermissionMode::Auto);
}
