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
    // The entry names the repository helper, which runs the same `cargo fmt --check` over the
    // oxa workspace from any caller directory and with cargo outside PATH.
    assert!(
        done.iter().any(|c| c == "in oxa: ../tools/ci/fmt_check.sh"),
        "rustfmt check missing: {done:?}"
    );
    let helper = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tools/ci/fmt_check.sh");
    let script = std::fs::read_to_string(helper).unwrap_or_else(|e| panic!("{helper}: {e}"));
    assert!(
        script
            .lines()
            .any(|l| l.trim() == r#"exec cargo fmt --check "$@""#),
        "{helper} must still run `cargo fmt --check` over the oxa workspace"
    );
    assert!(
        done.iter()
            .any(|c| c == "in oxc: sh tools/run_lab_tests.sh test_doc_links"),
        "lab wrapper missing: {done:?}"
    );
}
