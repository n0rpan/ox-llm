//! The ox AI panel manifest (`oxa/omarchy-panel/manifest.json`) names the Omarchy bar widget.
//! The id stays `snapdragon.local-ai` (owner decision 1), the entry point must exist, and every
//! setting the panel reads must be declared with a default.

use serde_json::Value;
use std::fs;
use std::path::PathBuf;

fn panel_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("omarchy-panel")
}

#[test]
fn panel_manifest_is_valid() {
    let dir = panel_dir();
    let text = fs::read_to_string(dir.join("manifest.json")).expect("read manifest.json");
    let manifest: Value = serde_json::from_str(&text).expect("manifest.json is valid JSON");

    assert_eq!(manifest["schemaVersion"].as_u64(), Some(1), "schemaVersion");
    assert_eq!(manifest["id"].as_str(), Some("snapdragon.local-ai"), "id is kept");
    assert_eq!(manifest["name"].as_str(), Some("ox AI"), "name");
    assert!(
        manifest["kinds"]
            .as_array()
            .is_some_and(|kinds| kinds.iter().any(|k| k.as_str() == Some("bar-widget"))),
        "kinds lists bar-widget"
    );

    let entry = manifest["entryPoints"]["barWidget"]
        .as_str()
        .expect("entryPoints.barWidget is a string");
    assert!(
        dir.join(entry).is_file(),
        "entry point {entry} exists in the panel folder"
    );

    let bar = &manifest["barWidget"];
    assert_eq!(
        bar["displayName"].as_str(),
        Some("ox AI"),
        "barWidget.displayName"
    );
    assert!(
        bar["description"].as_str().is_some_and(|d| !d.is_empty()),
        "barWidget.description is set"
    );

    let schema = bar["schema"].as_array().expect("barWidget.schema is an array");
    for key in ["pollEnabled", "oxaPath", "codeDir", "leadUrl"] {
        let entry = schema
            .iter()
            .find(|s| s["key"].as_str() == Some(key))
            .unwrap_or_else(|| panic!("schema declares {key}"));
        assert!(bar["defaults"].get(key).is_some(), "defaults declares {key}");
        assert_eq!(
            entry.get("defaultValue"),
            bar["defaults"].get(key),
            "{key}: schema default equals defaults"
        );
    }
}
