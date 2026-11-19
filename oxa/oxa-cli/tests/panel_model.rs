//! Tests of the ox AI panel's model, part 1 (`oxa/omarchy-panel/Model.js`): the status answer
//! parser (`parseStatus`), the hero headline (`headline`) and the state chips (`chips`).
//! The functions are pure, so the file runs here in the embedded QuickJS of `oxa-script` with no
//! process, file or network; the QML panel itself is not loaded by these tests.

use oxa_script::{Limits, Runtime};
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;

fn panel_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("omarchy-panel")
}

fn read_fixture(name: &str) -> String {
    let path = panel_dir().join("tests").join("fixtures").join(name);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn model_runtime() -> Runtime {
    let source = fs::read_to_string(panel_dir().join("Model.js")).expect("read Model.js");
    // `.pragma library` is a QML directive; the script engine only sees the functions.
    let script: String = source
        .lines()
        .filter(|line| !line.starts_with(".pragma"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut runtime = Runtime::new(Limits::EXTENSION).expect("runtime starts");
    runtime
        .eval_script("Model.js", &script)
        .expect("Model.js evaluates");
    runtime
}

fn js(runtime: &mut Runtime, expression: &str) -> Value {
    runtime
        .eval_script("panel-model-test", expression)
        .unwrap_or_else(|e| panic!("{expression}: {e:?}"))
}

// A JSON string is a valid JavaScript string literal.
fn js_text(text: &str) -> String {
    serde_json::to_string(text).expect("string literal")
}

fn status_value() -> Value {
    serde_json::from_str(&read_fixture("status.json")).expect("status.json is JSON")
}

fn parse_with(runtime: &mut Runtime, value: &Value, exit: i64) -> Value {
    let text = js_text(&value.to_string());
    js(runtime, &format!("parseStatus({text}, {exit}, false)"))
}

fn assert_js_number(value: &Value, expected: f64) {
    assert_eq!(value.as_f64(), Some(expected));
}

#[test]
fn status_fixture_parses() {
    let mut rt = model_runtime();
    let text = js_text(&read_fixture("status.json"));
    let s = js(&mut rt, &format!("parseStatus({text}, 0, false)"));
    assert_eq!(s["fault"], json!(null));
    assert_eq!(s["oxaVersion"], json!("0.2.1"));
    assert_eq!(
        s["capabilities"],
        json!(["models", "usage", "storage", "panel_install"])
    );
    assert_eq!(s["defaultModel"], json!("openai/gpt-6.2-sol"));
    assert_eq!(s["local"]["state"], json!("not_wired"));
    assert_eq!(s["local"]["fix"], json!("local_add"));
    assert_eq!(s["cloud"]["state"], json!("ready"));
    assert_eq!(s["cloud"]["keyFile"], json!("present"));
    assert_eq!(s["cloud"]["chatgptLogin"], json!("missing"));
    assert_eq!(s["lead"]["state"], json!("awaiting_owner"));
    assert_js_number(&s["lead"]["openAsks"], 2.0);
    assert_eq!(s["lead"]["serve"]["url"], json!("http://127.0.0.1:8743/"));
    assert_js_number(&s["sessions"]["working"], 3.0);
    assert_eq!(s["workers"]["rows"][0]["name"], json!("lander-1"));
    assert_eq!(s["workers"]["rows"][0]["state"], json!("working"));
    assert_eq!(s["usageState"], json!("low"));
}

#[test]
fn old_binary_output_is_fault_old() {
    let mut rt = model_runtime();
    let text = js_text(&read_fixture("old_binary.txt"));
    // clap's usage error exits 2; any non-zero exit with no oxa_error object is "old".
    for exit in [1, 2] {
        let s = js(&mut rt, &format!("parseStatus({text}, {exit}, false)"));
        assert_eq!(s["fault"], json!("old"), "exit {exit}");
    }
}

#[test]
fn missing_binary_is_fault_missing() {
    let mut rt = model_runtime();
    assert_eq!(
        js(&mut rt, "parseStatus('', null, false).fault"),
        json!("missing")
    );
    assert_eq!(js(&mut rt, "parseStatus('', 127, false).fault"), json!("missing"));
    assert_eq!(js(&mut rt, "parseStatus('', 126, false).fault"), json!("missing"));
    assert_eq!(js(&mut rt, "parseStatus('', -2, false).fault"), json!("missing"));
}

#[test]
fn empty_answer_with_exit_zero_is_unreadable() {
    let mut rt = model_runtime();
    let text = js_text(&read_fixture("empty.txt"));
    assert_eq!(
        js(&mut rt, &format!("parseStatus({text}, 0, false).fault")),
        json!("unreadable")
    );
}

#[test]
fn failed_answer_keeps_its_code() {
    let mut rt = model_runtime();
    let text = js_text(&read_fixture("error.json"));
    let s = js(&mut rt, &format!("parseStatus({text}, 1, false)"));
    assert_eq!(s["fault"], json!("failed"));
    assert_eq!(s["code"], json!("ledger_unreadable"));
}

#[test]
fn newer_version_and_timeout_are_their_faults() {
    let mut rt = model_runtime();
    let mut newer = status_value();
    newer["version"] = json!(2);
    assert_eq!(parse_with(&mut rt, &newer, 0)["fault"], json!("newer"));
    assert_eq!(js(&mut rt, "parseStatus('', null, true).fault"), json!("timeout"));
}

#[test]
fn headline_order_asks_before_budget() {
    let mut rt = model_runtime();
    let fixture = status_value();

    // Open asks come before a low budget pool (the fixture has both).
    let s = parse_with(&mut rt, &fixture, 0);
    assert_eq!(
        js(&mut rt, &format!("headline({})", s)),
        json!({"id": "asks", "count": 2, "name": "", "state": ""})
    );

    // No asks: the budget line.
    let mut no_asks = status_value();
    no_asks["lead"]["open_asks"] = json!(0);
    let s = parse_with(&mut rt, &no_asks, 0);
    assert_eq!(
        js(&mut rt, &format!("headline({})", s)),
        json!({"id": "usage", "count": 0, "name": "", "state": "low"})
    );

    // A stalled worker comes before the budget line, and names the worker.
    let mut stalled = no_asks.clone();
    stalled["workers"]["stalled"] = json!(1);
    stalled["workers"]["rows"][0]["stalled"] = json!(true);
    let s = parse_with(&mut rt, &stalled, 0);
    assert_eq!(
        js(&mut rt, &format!("headline({})", s)),
        json!({"id": "stalled", "count": 1, "name": "lander-1", "state": ""})
    );

    // Usage ok and sessions working: working. No model at all: no_model.
    let mut quiet = no_asks.clone();
    quiet["usage_state"] = json!("ok");
    quiet["sessions"]["working"] = json!(0);
    quiet["default_model"] = json!(null);
    let s = parse_with(&mut rt, &quiet, 0);
    assert_eq!(js(&mut rt, &format!("headline({})", s))["id"], json!("no_model"));
    let mut busy = no_asks.clone();
    busy["usage_state"] = json!("ok");
    let s = parse_with(&mut rt, &busy, 0);
    assert_eq!(js(&mut rt, &format!("headline({})", s))["id"], json!("working"));

    // A fault replaces every rule.
    let s = js(&mut rt, "parseStatus('', null, false)");
    assert_eq!(js(&mut rt, &format!("headline({})", s))["id"], json!("missing"));
}

#[test]
fn chips_are_local_cloud_lead_and_name_their_tab() {
    let mut rt = model_runtime();
    let s = js(
        &mut rt,
        &format!("parseStatus({}, 0, false)", js_text(&read_fixture("status.json"))),
    );
    let c = js(&mut rt, &format!("chips({s}, {{}})"));
    assert_eq!(c.as_array().map(Vec::len), Some(3));
    assert_eq!(c[0]["id"], json!("local"));
    assert_eq!(c[0]["state"], json!("not_wired"));
    assert_eq!(c[0]["tab"], json!("models"));
    assert_eq!(c[1]["id"], json!("cloud"));
    assert_eq!(c[1]["state"], json!("ready"));
    assert_eq!(c[2]["id"], json!("lead"));
    assert_eq!(c[2]["state"], json!("awaiting_owner"));
    assert_eq!(c[2]["tab"], json!("lead"));
    assert_eq!(c[2]["count"], json!(2));

    // A lead that is off here but has an address in the widget setting is "remote".
    let mut off = status_value();
    off["lead"]["state"] = json!("off");
    let s = parse_with(&mut rt, &off, 0);
    let c = js(
        &mut rt,
        &format!("chips({s}, {{leadUrl: 'http://lead.example:8743/'}})"),
    );
    assert_eq!(c[2]["state"], json!("remote"));
    assert_eq!(c[2]["url"], json!("http://lead.example:8743/"));
    let c = js(&mut rt, &format!("chips({s}, {{}})"));
    assert_eq!(c[2]["state"], json!("off"));
}

#[test]
fn unknown_enum_is_safe() {
    let mut rt = model_runtime();
    let mut v = status_value();
    v["local"]["state"] = json!("warp");
    v["cloud"]["state"] = json!("quantum");
    v["cloud"]["fix"] = json!("reboot_the_planet");
    v["lead"]["state"] = json!("sleeping");
    v["lead"]["open_asks"] = json!(-3);
    v["lead"]["goal_status"] = json!({"nested": true});
    v["usage_state"] = json!("weird");
    v["capabilities"] = json!(["usage", "teleport"]);
    v["workers"]["rows"] = json!("not a list");
    v["sessions"]["live"] = json!(1.5);
    let s = parse_with(&mut rt, &v, 0);

    assert_eq!(s["fault"], json!(null));
    assert_eq!(s["local"]["state"], json!("unknown"));
    assert_eq!(s["local"]["raw"], json!("warp"));
    assert_eq!(s["cloud"]["state"], json!("unknown"));
    assert_eq!(s["cloud"]["fix"], json!(null));
    assert_eq!(s["lead"]["state"], json!("unknown"));
    assert_eq!(s["lead"]["openAsks"], json!(0));
    assert_eq!(s["lead"]["goalStatus"], json!(""));
    assert_eq!(s["usageState"], json!("unknown"));
    assert_eq!(s["capabilities"], json!(["usage"]));
    assert_eq!(s["workers"]["rows"], json!([]));
    assert_eq!(s["sessions"]["live"], json!(0));

    let c = js(&mut rt, &format!("chips({s}, {{}})"));
    assert_eq!(c[0]["state"], json!("unknown"));
    assert_eq!(c[0]["raw"], json!("warp"));
    // Sessions still working (3 in the fixture) make the headline "working", with no budget line.
    let h = js(&mut rt, &format!("headline({s})"));
    assert_eq!(h["id"], json!("working"));
    assert_eq!(h["count"], json!(3));
}
