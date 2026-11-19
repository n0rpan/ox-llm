//! The repository's own project settings (`<repo>/.oxa/settings.json`) declare the `done`
//! checks that an agent must pass before a run that edited files finishes (issue #106).

use oxa_config::Settings;
use serde_json::json;

#[test]
fn repo_done_list_runs_the_documented_checks() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../.oxa/settings.json");
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let value: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
    let done = Settings::from_values(value, json!({})).done_commands();
    assert!(
        done.iter().any(|c| c == "in oxa: ../tools/ci/fmt_check.sh"),
        "rustfmt check missing: {done:?}"
    );
}
