//! Static checks of `oxa/omarchy-panel/Feeds.qml` (design section 2.5): the capability guard, the
//! timeouts and cadence, and no file or network use. The QML is not run here.

use std::fs;
use std::path::PathBuf;

fn feeds_source() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("omarchy-panel")
        .join("Feeds.qml");
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The text of `function run(kind)`, up to the next function.
fn run_body(source: &str) -> &str {
    let start = source
        .find("function run(kind)")
        .expect("Feeds.qml: function run(kind)");
    let rest = &source[start..];
    let end = rest[1..]
        .find("\n    function ")
        .map(|i| i + 1)
        .expect("Feeds.qml: a function follows run");
    &rest[..end]
}

#[test]
fn feeds_guard_present() {
    let source = feeds_source();
    assert!(
        source.contains("function allowed(kind)"),
        "Feeds.qml: the capability guard exists"
    );
    let body = run_body(&source);
    let guard = body
        .find("if (!feeds.allowed(kind)) return false")
        .expect("run(kind) checks the guard first");
    let start = body
        .find("p.running = true")
        .expect("run(kind) is the one place a command starts");
    assert!(guard < start, "the guard comes before the process starts");
    assert_eq!(
        source.matches("running = true").count(),
        1,
        "Feeds.qml: a command starts only in run(kind)"
    );
    assert!(
        body.contains("[feeds.binary, kind, \"--json\"]"),
        "run(kind) passes an argument list, never a shell string"
    );
    assert!(
        source.contains("[\"models\", \"usage\", \"storage\"].indexOf(kind)"),
        "only the three known guarded words pass the guard"
    );
}

#[test]
fn feeds_timeouts_and_cadence_match_the_contract() {
    let source = feeds_source();
    for line in [
        "id: statusKill; interval: 3000",
        "id: modelsKill; interval: 5000",
        "id: usageKill; interval: 5000",
        "id: storageKill; interval: 10000",
        "id: statusTimer; interval: 5000",
        "id: modelsTimer; interval: 60000",
        "id: usageTimer; interval: 30000",
        "id: storageTimer; interval: 60000",
    ] {
        assert!(source.contains(line), "Feeds.qml: `{line}`");
    }
}

#[test]
fn feeds_open_no_file_and_make_no_request() {
    let source = feeds_source();
    for word in [
        "FileView",
        "XMLHttpRequest",
        "fetch(",
        "openUrlExternally",
        "http://",
        "https://",
        "secrets",
        "auth.json",
        "cloud-ledger",
    ] {
        assert!(!source.contains(word), "Feeds.qml must not mention `{word}`");
    }
}
