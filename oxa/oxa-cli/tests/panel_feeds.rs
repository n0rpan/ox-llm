//! Contract and behavioral checks for the panel's QML feed supervisor.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn feeds_source() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("omarchy-panel")
        .join("Feeds.qml");
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn function(source: &str, name: &str) -> String {
    let marker = format!("function {name}(");
    let start = source.find(&marker).unwrap_or_else(|| panic!("missing {marker}"));
    let open = start + source[start..].find('{').expect("function body");
    let mut depth = 0;
    for (offset, byte) in source.as_bytes()[open..].iter().enumerate() {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return source[start..=open + offset].to_owned();
                }
            }
            _ => {}
        }
    }
    panic!("unterminated {marker}")
}

/// Execute the production QML JavaScript functions with deterministic Process and Timer doubles.
/// This drives their state transitions rather than asserting that selected source words exist.
fn run_behavior(script: &str) {
    let source = feeds_source();
    let names = [
        "allowed",
        "procFor",
        "killFor",
        "graceFor",
        "forceFor",
        "run",
        "collect",
        "exited",
        "expire",
        "forceExpired",
        "finish",
        "parseAnswer",
        "applyStatus",
    ];
    let functions = names
        .iter()
        .map(|name| function(&source, name))
        .collect::<Vec<_>>()
        .join("\n");
    let harness = format!(
        r#"
function timer() {{ return {{ runId: 0, running: false, restart() {{ this.running = true }}, stop() {{ this.running = false }} }} }}
function proc() {{ return {{ running: false, command: [], signals: [], signal(n) {{ this.signals.push(n) }} }} }}
const statusProc=proc(), modelsProc=proc(), usageProc=proc(), storageProc=proc();
const statusKill=timer(), modelsKill=timer(), usageKill=timer(), storageKill=timer();
const statusGrace=timer(), modelsGrace=timer(), usageGrace=timer(), storageGrace=timer();
const statusForce=timer(), modelsForce=timer(), usageForce=timer(), storageForce=timer();
const feeds = {{ binary: "/one/oxa", capabilities: [], usageState: "", good: {{}}, runs: {{}},
  nextRunId: 0, opened: false, answers: [], answered(...a) {{ this.answers.push(a) }},
  refreshOnOpen() {{}}, allowed, procFor, killFor, graceFor, forceFor, run, collect, exited,
  expire, forceExpired, finish, parseAnswer, applyStatus }};
function check(value, message) {{ if (!value) throw new Error(message) }}
{functions}
{script}
"#
    );
    let output = Command::new("node")
        .arg("--eval")
        .arg(harness)
        .output()
        .expect("node is required to exercise the QML JavaScript behavior");
    assert!(
        output.status.success(),
        "behavior harness failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn feeds_guard_and_binary_identity_are_behavioral() {
    run_behavior(
        r#"
check(!feeds.run("usage"), "usage started without status capability");
feeds.capabilities = ["usage"];
check(feeds.run("usage"), "advertised usage did not start");
check(usageProc.command.join("|") === "/one/oxa|usage|--json", "command is not an argv list");
feeds.capabilities = ["models"];
check(feeds.run("status"), "status did not start");
feeds.binary = "/two/oxa";
feeds.capabilities = [];
feeds.collect("status", '{"type":"oxa_suite_status","version":1,"capabilities":["usage"]}');
feeds.exited("status", 0);
check(feeds.capabilities.length === 0, "old binary answer reopened guard");
check(feeds.good.status === undefined, "old binary answer replaced cache");
"#,
    );
}

#[test]
fn feeds_preserve_good_data_and_isolate_old_callbacks() {
    run_behavior(
        r#"
feeds.capabilities = ["models"];
feeds.good.models = { text: "previous", atMs: 1 };
check(feeds.run("models"), "models did not start");
feeds.collect("models", ""); feeds.exited("models", 0);
check(feeds.good.models.text === "previous", "empty success replaced last good data");
modelsProc.running = false;
check(feeds.run("models"), "retry did not start");
const current = feeds.runs.models.id;
feeds.finish("models", current - 1);
check(!feeds.runs.models.done, "old grace callback finished retry");
"#,
    );
}

#[test]
fn feeds_timeout_escalates_and_settles() {
    run_behavior(
        r#"
feeds.capabilities = ["usage"];
check(feeds.run("usage"), "usage did not start");
const id = feeds.runs.usage.id;
feeds.expire("usage");
check(feeds.runs.usage.timedOut, "timeout was not recorded");
check(usageForce.running && usageForce.runId === id, "kill grace was not armed");
usageProc.running = true; // model a child that ignored SIGTERM
feeds.forceExpired("usage", id);
check(usageProc.signals[0] === 9, "SIGKILL was not sent");
check(feeds.runs.usage.done, "timed-out run did not settle");
check(feeds.answers.length === 1 && feeds.answers[0][3] === true, "timeout answer missing");
"#,
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
