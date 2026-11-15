//! G7 PARITY tests (rpc). Owner: task G7.
//! One test per PARITY.md row of this area, named after the row id (`RPC-012` -> `fn rpc_012()`).
//! Each test drives the real `oxa --mode rpc` binary over its stdin and stdout, against the fake
//! engine (`oxa_fake::FakeServer`, in process). P rows compare oxa's records with pi's documented
//! examples (`fixtures/goldens/rpc/documented.json`); A rows assert the adapted behaviour and name the
//! invariant in their doc comment; O rows assert `unsupported_in_oxa`.
//!
//! A row whose behaviour differs from pi is `#[ignore = "RED ..."]` with the precise reason: those
//! are the H-wave input (`cargo test -p oxa-parity --test rpc -- --ignored`).

use oxa_fake::{FakeModel, FakeOpts, FakeServer, Recorded, ScriptTurn, TestEnv};
use oxa_parity::{fixture, oxa_bin};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(20);
/// Keys whose values must be equal, not only of the same kind, in a documented shape.
const LITERAL_KEYS: [&str; 3] = ["type", "command", "success"];

/// The `oxa --mode rpc` child, with the fake engine it talks to.
struct Rpc {
    env: TestEnv,
    server: FakeServer,
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    backlog: VecDeque<Value>,
}

impl Drop for Rpc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Rpc {
    /// Starts `oxa --mode rpc <args>` in the test's work directory with the fake engine `turns`.
    fn start(env: TestEnv, opts: FakeOpts, turns: Vec<ScriptTurn>, args: &[&str]) -> Rpc {
        let server = FakeServer::start(turns, opts).expect("start the fake engine");
        let mut child = Command::new(oxa_bin())
            .arg("--mode")
            .arg("rpc")
            .args(args)
            .current_dir(env.work())
            .env_clear()
            .envs(env.envs(&server.url()))
            .envs(oxa_parity::note_off_env())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn oxa (build it first: cargo build -p oxa-cli -p oxa-fake)");
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("stdout is piped");
        let (tx, lines) = mpsc::channel::<String>();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut raw = String::new();
                match reader.read_line(&mut raw) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if tx.send(raw).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        Rpc {
            env,
            server,
            child,
            stdin,
            lines,
            backlog: VecDeque::new(),
        }
    }

    /// Writes one command line (`value` + LF).
    fn send(&mut self, value: Value) {
        self.send_raw(&format!("{value}\n"));
    }

    /// Writes raw bytes to stdin.
    fn send_raw(&mut self, text: &str) {
        let w = self.stdin.as_mut().expect("stdin is open");
        w.write_all(text.as_bytes()).expect("write a command");
        w.flush().expect("flush a command");
    }

    /// Sends a command with an `id` and returns its response.
    fn cmd(&mut self, value: Value) -> Value {
        let id = value["id"]
            .as_str()
            .expect("a test command has an id")
            .to_string();
        self.send(value);
        self.resp(&id)
    }

    /// The next record from stdout, LF-framed; a line that is not one JSON object fails the test.
    fn pull(&mut self, timeout: Duration) -> Option<Value> {
        let raw = self.lines.recv_timeout(timeout).ok()?;
        let body = raw
            .strip_suffix('\n')
            .unwrap_or_else(|| panic!("a record is not LF-terminated: {raw:?}"));
        assert!(!body.ends_with('\r'), "a record ends with CR before LF: {raw:?}");
        Some(
            serde_json::from_str(body)
                .unwrap_or_else(|e| panic!("stdout line is not one JSON object: {body:?} ({e})")),
        )
    }

    /// The first record (buffered or new) that matches `pred`; other records stay in order.
    fn take(&mut self, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
        if let Some(i) = self.backlog.iter().position(&pred) {
            return self.backlog.remove(i).expect("index in range");
        }
        let deadline = Instant::now() + WAIT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let v = match self.pull(left) {
                Some(v) => v,
                None => panic!("timed out waiting for {what}; buffered: {:?}", self.backlog),
            };
            if pred(&v) {
                return v;
            }
            self.backlog.push_back(v);
        }
    }

    fn any(&mut self) -> Value {
        self.take("any record", |_| true)
    }

    fn resp(&mut self, id: &str) -> Value {
        self.take(&format!("the response to {id}"), |v| {
            v["type"] == "response" && v["id"] == id
        })
    }

    fn kind(&mut self, kind: &str) -> Value {
        self.take(kind, |v| v["type"] == kind)
    }

    fn ui(&mut self, method: &str) -> Value {
        self.take(&format!("extension_ui_request {method}"), |v| {
            v["type"] == "extension_ui_request" && v["method"] == method
        })
    }

    /// Every record up to and including the first one that matches `done`, in order.
    fn collect_until(&mut self, mut done: impl FnMut(&Value) -> bool) -> Vec<Value> {
        let mut out = Vec::new();
        loop {
            let v = match self.backlog.pop_front() {
                Some(v) => v,
                None => self
                    .pull(WAIT)
                    .unwrap_or_else(|| panic!("timed out; records so far: {out:?}")),
            };
            let stop = done(&v);
            out.push(v);
            if stop {
                return out;
            }
        }
    }

    /// The records of the next run, up to its `agent_settled`; then the session is idle and the
    /// next command may follow at once (rpc.md).
    fn run_records(&mut self) -> Vec<Value> {
        self.collect_until(|v| v["type"] == "agent_settled")
    }

    fn requests(&self) -> Vec<Recorded> {
        self.server.requests()
    }

    /// Closes stdin and waits for the process to exit.
    fn close(&mut self) -> ExitStatus {
        drop(self.stdin.take());
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(status) = self.child.try_wait().expect("wait for oxa") {
                return status;
            }
            assert!(Instant::now() < deadline, "oxa did not exit after stdin closed");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn messages(&mut self, id: &str) -> Vec<Value> {
        let r = self.cmd(json!({"id": id, "type": "get_messages"}));
        r["data"]["messages"].as_array().cloned().expect("messages array")
    }

    /// get_state; it reflects every command sent before it (`extra_get_state_reflects_earlier_commands`).
    fn state(&mut self, id: &str) -> Value {
        let r = self.cmd(json!({"id": id, "type": "get_state"}));
        assert_eq!(r["success"], true, "get_state: {r}");
        r["data"].clone()
    }
}

fn default_opts() -> FakeOpts {
    FakeOpts::default()
}

/// Every chat request is held for `ms` before its headers, so a run stays open for the test.
fn held_opts(ms: u64) -> FakeOpts {
    FakeOpts {
        hold_headers_ms: ms,
        ..FakeOpts::default()
    }
}

/// Two models of different size, for the model commands.
fn two_models() -> FakeOpts {
    FakeOpts {
        models: vec![
            FakeModel::moe("alpha", 1 << 17),
            FakeModel::flash("beta", 1 << 14, Some(2048)),
        ],
        ..FakeOpts::default()
    }
}

/// One part (`response`, `request`, ...) of a documented entry.
fn part(id: &str, key: &str) -> Value {
    doc(id)
        .get(key)
        .cloned()
        .unwrap_or_else(|| panic!("{id} has no documented {key}"))
}

/// The documented example (or rule) of a P row, from `fixtures/goldens/rpc/documented.json`.
fn doc(id: &str) -> Value {
    let text = std::fs::read_to_string(fixture("goldens/rpc/documented.json")).expect("read the pi fixture");
    let all: Value = serde_json::from_str(&text).expect("the pi fixture is JSON");
    all.get(id)
        .cloned()
        .unwrap_or_else(|| panic!("no documented entry for {id}"))
}

/// Checks `actual` against pi's documented example: each key of the example is present with the
/// same JSON kind (recursively); arrays hold elements of the example's first element's shape;
/// `null` in the example accepts any value; `?key` is optional; `type`, `command` and `success`
/// must be equal.
fn assert_shape(actual: &Value, documented: &Value) {
    if let Err(e) = shape_at(actual, documented, "$") {
        panic!("does not match pi's documented shape: {e}\nactual: {actual}");
    }
}

fn shape_at(a: &Value, d: &Value, path: &str) -> Result<(), String> {
    match d {
        Value::Null => Ok(()),
        Value::Bool(_) if a.is_boolean() => Ok(()),
        Value::Number(_) if a.is_number() => Ok(()),
        Value::String(_) if a.is_string() => Ok(()),
        Value::Bool(_) | Value::Number(_) | Value::String(_) => {
            Err(format!("{path}: expected a {}, got {a}", kind(d)))
        }
        Value::Array(items) => {
            let arr = a
                .as_array()
                .ok_or_else(|| format!("{path}: expected an array, got {a}"))?;
            if let Some(first) = items.first() {
                for (i, x) in arr.iter().enumerate() {
                    shape_at(x, first, &format!("{path}[{i}]"))?;
                }
            }
            Ok(())
        }
        Value::Object(map) => {
            let am = a
                .as_object()
                .ok_or_else(|| format!("{path}: expected an object, got {a}"))?;
            for (k, dv) in map {
                let (key, optional) = match k.strip_prefix('?') {
                    Some(s) => (s, true),
                    None => (k.as_str(), false),
                };
                match am.get(key) {
                    None if optional => {}
                    None => return Err(format!("{path}.{key}: missing")),
                    Some(_) if dv.is_null() => {}
                    Some(av) if LITERAL_KEYS.contains(&key) => {
                        if av != dv {
                            return Err(format!("{path}.{key}: expected {dv}, got {av}"));
                        }
                    }
                    Some(av) => shape_at(av, dv, &format!("{path}.{key}"))?,
                }
            }
            Ok(())
        }
    }
}

fn kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// The extension used by the extension-UI rows. Each probe is a command (so `get_commands` lists
/// it) and an `input` hook (a prompt whose text is the probe name runs it and is handled, so no
/// model turn follows). Each probe reports its result with `notify` as `<name>:<typeof>:<value>`.
const UI_EXT: &str = r#"export default function (oxa) {
const probes = {
  sel: (ctx) => ctx.ui.notify("sel:" + show_value(ctx.ui.select("Allow dangerous command?", ["Allow", "Block"], {timeout: 10000})), "info"),
  conf: (ctx) => ctx.ui.notify("conf:" + show_value(ctx.ui.confirm("Clear session?", "All messages will be lost.", {timeout: 5000})), "info"),
  inp: (ctx) => ctx.ui.notify("inp:" + show_value(ctx.ui.input("Enter a value", "type something...")), "info"),
  edt: (ctx) => ctx.ui.notify("edt:" + show_value(ctx.ui.editor("Edit some text", "Line 1\nLine 2\nLine 3")), "info"),
  tmo: (ctx) => ctx.ui.notify("tmo:" + show_value(ctx.ui.select("Never answered", ["a", "b"], {timeout: 300})), "info"),
  warn: (ctx) => ctx.ui.notify("Command blocked by user", "warning"),
  status: (ctx) => { ctx.ui.setStatus("my-ext", "Turn 3 running..."); ctx.ui.setStatus("my-ext", undefined); },
  widget: (ctx) => ctx.ui.setWidget("my-ext", ["--- My Widget ---", "Line 1", "Line 2"]),
  title: (ctx) => ctx.ui.setTitle("pi - my project"),
  edtext: (ctx) => ctx.ui.setEditorText("prefilled text for the user"),
};
function show_value(v) { return typeof v + ":" + String(v); }
for (const name of Object.keys(probes)) {
  oxa.registerCommand(name, {description: name + " probe", handler: (args, ctx) => probes[name](ctx)});
}
oxa.on("input", (e, ctx) => {
  if (Object.prototype.hasOwnProperty.call(probes, e.text)) {
    probes[e.text](ctx);
    return {action: "handled"};
  }
  return undefined;
});
}
"#;

/// A fresh test environment with the UI-probe extension installed.
fn ui_env() -> TestEnv {
    let env = TestEnv::new();
    env.write("home/.oxa/agent/extensions/ui.js", UI_EXT);
    env
}

/// The RPC row ids of PARITY.md's RPC section, with their verdict and test column.
fn parity_rows() -> Vec<(String, String, String)> {
    let text =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../PARITY.md")).expect("PARITY.md");
    let mut rows = Vec::new();
    let mut in_section = false;
    for line in text.lines() {
        if line.starts_with("## ") {
            in_section = line.starts_with("## RPC: ");
            continue;
        }
        if !in_section || !line.starts_with("| RPC-") {
            continue;
        }
        let escaped = line.replace("\\|", "\u{0}");
        let cells: Vec<String> = escaped
            .split('|')
            .map(|c| c.trim().replace('\u{0}', "|"))
            .collect();
        rows.push((cells[1].clone(), cells[3].clone(), cells[5].clone()));
    }
    rows
}

/// The source of this file, for the meta-checks.
const SELF_SRC: &str = include_str!("rpc.rs");

/// The doc comment lines directly above `fn <name>(`.
fn doc_comment_of(name: &str) -> String {
    let lines: Vec<&str> = SELF_SRC.lines().collect();
    let at = lines
        .iter()
        .position(|l| l.starts_with(&format!("fn {name}(")))
        .unwrap_or_else(|| panic!("no fn {name}"));
    let mut out = Vec::new();
    let mut i = at;
    while i > 0 {
        i -= 1;
        let l = lines[i].trim_start();
        if let Some(text) = l.strip_prefix("///") {
            out.push(text.trim().to_string());
        } else if l.starts_with("#[") {
            continue;
        } else {
            break;
        }
    }
    out.reverse();
    out.join(" ")
}

// ---------------------------------------------------------------------------------------------
// Meta-checks of the task
// ---------------------------------------------------------------------------------------------

/// each PARITY.md id of this area maps to a test fn or an O-row assertion
#[test]
fn every_row_has_test_or_recorded_omission() {
    let rows = parity_rows();
    assert_eq!(rows.len(), 52, "the RPC area has 52 rows: {rows:?}");
    for (id, verdict, test) in &rows {
        let fn_name = id.to_lowercase().replace('-', "_");
        assert!(
            test == &format!("`oxa-parity/tests/rpc.rs::{fn_name}`")
                || test == &format!("oxa-parity/tests/rpc.rs::{fn_name}")
                || test.contains(&format!("rpc.rs::{fn_name}")),
            "{id} ({verdict}) maps to {test}"
        );
        assert!(
            SELF_SRC.contains(&format!("fn {fn_name}()")),
            "{id} has no test fn {fn_name}"
        );
    }
}

/// documented output or recorded fixture
#[test]
fn documented_rows_have_tests() {
    let mut n = 0;
    for (id, verdict, _) in parity_rows() {
        if verdict != "P" {
            continue;
        }
        n += 1;
        let d = doc(&id);
        assert!(d.get("source").is_some(), "{id}: the pi fixture names its source");
        assert!(
            d.get("response").is_some()
                || d.get("request").is_some()
                || d.get("rule").is_some()
                || d.get("failed_command").is_some()
                || d.get("value_response").is_some(),
            "{id}: the pi fixture has an example or a rule"
        );
    }
    assert_eq!(n, 39, "the RPC area has 39 P rows");
}

/// comment + adapted behaviour asserted
#[test]
fn a_rows_name_their_invariant() {
    let markers = [
        "W1",
        "W2",
        "W3",
        "W4",
        "W5",
        "W6",
        "O-14",
        "oxa",
        "engine",
        "sandbox",
        "invariant",
    ];
    let mut n = 0;
    for (id, verdict, _) in parity_rows() {
        if verdict != "A" {
            continue;
        }
        n += 1;
        let fn_name = id.to_lowercase().replace('-', "_");
        let comment = doc_comment_of(&fn_name);
        assert!(
            markers.iter().any(|m| comment.contains(m)),
            "{id} (A) names its invariant in its doc comment: {comment:?}"
        );
    }
    assert_eq!(n, 13, "the RPC area has 13 A rows");
}

// ---------------------------------------------------------------------------------------------
// CLI-side framing and lifecycle
// ---------------------------------------------------------------------------------------------

/// P: commands and responses correlate by `id`: `{type:"response", command, success, data|error}`.
#[test]
fn rpc_001() {
    let mut r = Rpc::start(
        TestEnv::new(),
        default_opts(),
        vec![ScriptTurn::text("ok")],
        &["--no-session"],
    );
    let a = r.cmd(json!({"id": "req-1", "type": "get_state"}));
    assert_shape(&a, &part("RPC-001", "response"));
    assert_eq!(a["id"], "req-1");
    // Responses are matched by id, not by order: two outstanding queries answered in any order.
    r.send(json!({"id": "req-2", "type": "get_available_thinking_levels"}));
    r.send(json!({"id": "req-3", "type": "get_session_stats"}));
    let b = r.resp("req-3");
    let c = r.resp("req-2");
    assert_eq!(b["command"], "get_session_stats");
    assert_eq!(c["command"], "get_available_thinking_levels");
    let e = r.cmd(json!({"id": "req-4", "type": "nope"}));
    assert_eq!(e["success"], false);
    assert_eq!(e["id"], "req-4");
    assert!(e["error"].is_string());
    assert!(e.get("data").is_none());
}

/// P: framing is one JSON object per LF-terminated line, split on LF only, with no header record;
/// an optional CR before the LF is accepted; U+2028 and U+2029 inside a JSON string are not boundaries.
#[test]
fn rpc_002() {
    let mut r = Rpc::start(
        TestEnv::new(),
        default_opts(),
        vec![ScriptTurn::text("ok")],
        &["--no-session"],
    );
    r.send_raw("{\"id\":\"crlf\",\"type\":\"get_state\"}\r\n");
    let first = r.any();
    assert_eq!(
        first["type"], "response",
        "the first record is a response, not a session header"
    );
    assert_eq!(first["id"], "crlf");
    let msg = "a\u{2028}b\u{2029}c";
    let p = r.cmd(json!({"id": "p", "type": "prompt", "message": msg}));
    assert_eq!(p["success"], true);
    r.run_records();
    let msgs = r.messages("m");
    assert_eq!(msgs[0]["content"], msg, "the separators stay inside the message");
}

/// P: a run is a prompt response, then the events (agent_start .. agent_end), then agent_settled.
#[test]
fn rpc_003() {
    let mut r = Rpc::start(
        TestEnv::new(),
        default_opts(),
        vec![ScriptTurn::text("Hello there.")],
        &["--no-session"],
    );
    r.send(json!({"id": "p", "type": "prompt", "message": "hi"}));
    let records = r.run_records();
    let pos = |pred: &dyn Fn(&Value) -> bool| records.iter().position(pred).expect("record present");
    let resp = pos(&|v| v["type"] == "response" && v["id"] == "p");
    let start = pos(&|v| v["type"] == "agent_start");
    let end = pos(&|v| v["type"] == "agent_end");
    let settled = pos(&|v| v["type"] == "agent_settled");
    assert!(
        resp < start && start < end && end < settled,
        "order: {:?}",
        doc("RPC-003")["order"]
    );
    assert_eq!(records[settled]["aborted"], false);
    let text = records
        .iter()
        .any(|v| v["assistantMessageEvent"]["delta"] == "Hello there.");
    assert!(text, "the text arrives between the response and agent_settled");
}

/// P: error shapes: an unknown command, an invalid payload, a prompt while busy, and a line that is
/// not a command (command `parse`, no id).
#[test]
fn rpc_004() {
    let mut r = Rpc::start(
        TestEnv::new(),
        held_opts(1500),
        vec![ScriptTurn::text("one")],
        &["--no-session"],
    );
    let unknown = r.cmd(json!({"id": "u1", "type": "nope"}));
    assert_eq!(unknown["success"], false);
    assert_eq!(unknown["command"], "nope");
    assert_eq!(unknown["id"], "u1");
    assert!(unknown["error"]
        .as_str()
        .unwrap_or("")
        .contains("Unknown command"));

    let invalid = r.cmd(json!({"id": "i1", "type": "steer"}));
    assert_eq!(invalid["success"], false, "steer without a message: {invalid}");
    assert_eq!(invalid["id"], "i1");

    r.send(json!({"id": "p", "type": "prompt", "message": "one"}));
    let _ = r.resp("p");
    r.kind("agent_start");
    let busy = r.cmd(json!({"id": "b1", "type": "prompt", "message": "two"}));
    assert_eq!(
        busy["success"], false,
        "a prompt during a run without streamingBehavior is refused"
    );
    assert!(busy["error"].as_str().map(|s| !s.is_empty()).unwrap_or(false));
    r.run_records();

    r.send_raw("this is not json\n");
    let parse = r.take("the parse error", |v| v["command"] == "parse");
    assert_shape(&parse, &part("RPC-004", "parse_error"));
    assert!(parse.get("id").is_none());
}

/// P: closing stdin shuts the process down (exit status 0).
#[test]
fn rpc_005() {
    let mut r = Rpc::start(TestEnv::new(), default_opts(), vec![], &["--no-session"]);
    let st = r.cmd(json!({"id": "s", "type": "get_state"}));
    assert_eq!(st["success"], true);
    let status = r.close();
    assert!(status.success(), "exit status {status:?}");
}

/// P: `prompt` (disposition started); with images (A: the wire gets a placeholder, never the data);
/// during a run `streamingBehavior` steer or followUp queues the message.
#[test]
fn rpc_006() {
    let mut r = Rpc::start(
        TestEnv::new(),
        held_opts(1500),
        vec![ScriptTurn::text("first"), ScriptTurn::text("second")],
        &["--no-session"],
    );
    let started = r.cmd(json!({"id": "p", "type": "prompt", "message": "hi"}));
    assert_shape(&started, &part("RPC-006", "response"));
    r.kind("agent_start");
    let steer = r.cmd(json!({"id": "s", "type": "prompt", "message": "s1", "streamingBehavior": "steer"}));
    assert_eq!(steer["data"]["disposition"], "queued", "{steer}");
    let follow =
        r.cmd(json!({"id": "f", "type": "prompt", "message": "f1", "streamingBehavior": "followUp"}));
    assert_eq!(follow["data"]["disposition"], "queued", "{follow}");
    r.run_records();
    let texts: Vec<String> = r
        .messages("m")
        .iter()
        .filter_map(|m| m["content"].as_str().map(str::to_string))
        .collect();
    assert!(
        texts.contains(&"s1".to_string()),
        "the steer is delivered: {texts:?}"
    );
    assert!(
        texts.contains(&"f1".to_string()),
        "the follow-up is delivered: {texts:?}"
    );

    // A (images, text-only engine): the image is accepted; the wire gets a placeholder, not the data.
    let image = "QkFTRTY0LUlNQUdFLURBVEE=";
    let img = r.cmd(json!({
        "id": "img",
        "type": "prompt",
        "message": "what is in this image?",
        "images": [{"type": "image", "data": image, "mimeType": "image/png"}]
    }));
    assert_eq!(img["success"], true, "{img}");
    r.run_records();
    let last = r
        .requests()
        .last()
        .expect("the image run reached the engine")
        .body
        .clone();
    let body = String::from_utf8_lossy(&last).into_owned();
    assert!(
        !body.contains(image),
        "the image data never reaches the text-only engine"
    );
    assert!(
        body.contains("what is in this image?"),
        "the text of the prompt is sent"
    );
}

/// P: `steer` queues a message while the run is in progress; the response is `queued`.
#[test]
fn rpc_007() {
    let mut r = Rpc::start(
        TestEnv::new(),
        held_opts(1500),
        vec![ScriptTurn::text("first"), ScriptTurn::text("second")],
        &["--no-session"],
    );
    r.send(json!({"id": "p", "type": "prompt", "message": "hi"}));
    let _ = r.resp("p");
    r.kind("agent_start");
    let s = r.cmd(json!({"id": "st", "type": "steer", "message": "change course"}));
    assert_shape(&s, &part("RPC-007", "response"));
    r.run_records();
    let texts: Vec<String> = r
        .messages("m")
        .iter()
        .filter_map(|m| m["content"].as_str().map(str::to_string))
        .collect();
    assert!(texts.contains(&"change course".to_string()), "{texts:?}");
    assert!(
        r.requests().len() >= 2,
        "the steer reaches the engine in a later request"
    );
}

/// P: `follow_up` queues a message delivered after the agent finishes; the response is `queued`.
#[test]
fn rpc_008() {
    let mut r = Rpc::start(
        TestEnv::new(),
        held_opts(1500),
        vec![ScriptTurn::text("first"), ScriptTurn::text("second")],
        &["--no-session"],
    );
    r.send(json!({"id": "p", "type": "prompt", "message": "hi"}));
    let _ = r.resp("p");
    r.kind("agent_start");
    let f = r.cmd(json!({"id": "fu", "type": "follow_up", "message": "after you're done"}));
    assert_shape(&f, &part("RPC-008", "response"));
    r.run_records();
    let texts: Vec<String> = r
        .messages("m")
        .iter()
        .filter_map(|m| m["content"].as_str().map(str::to_string))
        .collect();
    assert!(texts.contains(&"after you're done".to_string()), "{texts:?}");
    assert_eq!(r.requests().len(), 2, "the follow-up runs as a second run");
}

/// P: `abort` answers once the session is idle; agent_settled with aborted true comes first.
#[test]
fn rpc_009() {
    let mut r = Rpc::start(
        TestEnv::new(),
        held_opts(3000),
        vec![ScriptTurn::text("never")],
        &["--no-session"],
    );
    r.send(json!({"id": "p", "type": "prompt", "message": "hi"}));
    let _ = r.resp("p");
    r.send(json!({"id": "ab", "type": "abort"}));
    let records = r.collect_until(|v| v["type"] == "response" && v["id"] == "ab");
    let settled = records
        .iter()
        .position(|v| v["type"] == "agent_settled")
        .expect("agent_settled before the abort response");
    assert_eq!(records[settled]["aborted"], true);
    assert_shape(
        records.last().expect("the response"),
        &part("RPC-009", "response"),
    );
}

/// P: `clear_queue` removes queued steering and follow-up messages and returns their text.
#[test]
fn rpc_010() {
    let mut r = Rpc::start(
        TestEnv::new(),
        held_opts(1500),
        vec![ScriptTurn::text("first"), ScriptTurn::text("second")],
        &["--no-session"],
    );
    r.send(json!({"id": "p", "type": "prompt", "message": "hi"}));
    let _ = r.resp("p");
    r.kind("agent_start");
    assert_eq!(
        r.cmd(json!({"id": "s1", "type": "steer", "message": "change direction"}))["success"],
        true
    );
    assert_eq!(
        r.cmd(json!({"id": "f1", "type": "follow_up", "message": "summarize when finished"}))["success"],
        true
    );
    let cleared = r.cmd(json!({"id": "cq", "type": "clear_queue"}));
    assert_shape(&cleared, &part("RPC-010", "response"));
    assert_eq!(cleared["data"]["steering"], json!(["change direction"]));
    assert_eq!(cleared["data"]["followUp"], json!(["summarize when finished"]));
    r.run_records();
    assert_eq!(r.requests().len(), 1, "nothing queued is sent after the run");
}

/// P: `new_session` starts a fresh session and reports `cancelled: false`.
#[test]
fn rpc_011() {
    let mut r = Rpc::start(
        TestEnv::new(),
        default_opts(),
        vec![ScriptTurn::text("hi back")],
        &["--no-session"],
    );
    let before = r.state("s0")["sessionId"].clone();
    let n = r.cmd(json!({"id": "n", "type": "new_session"}));
    assert_shape(&n, &part("RPC-011", "response"));
    assert_ne!(r.state("s1")["sessionId"], before, "a new session id");
    assert_eq!(r.messages("m").len(), 0, "the fresh session has no messages");
}

/// A (W3, W6-adjacent): get_state keeps pi's fields and adds `mode` and the prefix cache hash
/// (W3: the prefix is frozen per epoch, the hash identifies it).
#[test]
fn rpc_012() {
    let mut r = Rpc::start(TestEnv::new(), default_opts(), vec![], &["--no-session"]);
    let st = r.cmd(json!({"id": "g", "type": "get_state"}));
    assert_shape(&st, &part("RPC-001", "response"));
    let data = &st["data"];
    let mode = data["mode"].as_str().expect("mode is a string (oxa only)");
    assert!(["plan", "workspace", "ask"].contains(&mode), "mode {mode}");
    let hash = data["prefixSha256"]
        .as_str()
        .expect("prefixSha256 (W3 cache field)");
    assert_eq!(hash.len(), 64);
    assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
}

/// P: get_messages returns the conversation as AgentMessage objects.
#[test]
fn rpc_013() {
    let mut r = Rpc::start(
        TestEnv::new(),
        default_opts(),
        vec![ScriptTurn::text("Hello there.")],
        &["--no-session"],
    );
    r.cmd(json!({"id": "p", "type": "prompt", "message": "hi"}));
    r.run_records();
    let g = r.cmd(json!({"id": "g", "type": "get_messages"}));
    assert_shape(&g, &part("RPC-013", "response"));
    assert_eq!(g["data"]["messages"][0]["role"], "user");
    assert_eq!(g["data"]["messages"][1]["role"], "assistant");
}

/// A (W3 new epoch): set_model answers with the Model object of the engine's model; the next
/// request names it; an unknown model is refused. (Only one model is listed by the fake engine,
/// see `rpc_015` for the switch between two.)
#[test]
fn rpc_014() {
    let mut r = Rpc::start(
        TestEnv::new(),
        default_opts(),
        vec![ScriptTurn::text("ok")],
        &["--no-session"],
    );
    let s = r.cmd(json!({"id": "sm", "type": "set_model", "provider": "oxe", "modelId": "fake-model"}));
    assert_eq!(s["success"], true, "{s}");
    assert_eq!(s["data"]["id"], "fake-model");
    assert!(
        s["data"]["contextWindow"].is_number(),
        "the response is the Model object"
    );
    assert_eq!(r.state("g")["model"]["id"], "fake-model");
    let bad = r.cmd(json!({"id": "bad", "type": "set_model", "provider": "oxe", "modelId": "nope"}));
    assert_eq!(bad["success"], false);
    r.cmd(json!({"id": "p", "type": "prompt", "message": "hi"}));
    r.run_records();
    assert_eq!(
        r.requests()[0].model,
        "fake-model",
        "the new epoch's request names the model"
    );
}

/// A (W3 new epoch): cycle_model moves to the next engine model and reports the model, its
/// thinkingLevel and isScoped; with one model it reports success and no data.
/// The fake lists every `model_limits` row on `/v1/models`, so the two-model move is real.
#[test]
fn rpc_015() {
    let mut r = Rpc::start(TestEnv::new(), default_opts(), vec![], &["--no-session"]);
    let single = r.cmd(json!({"id": "c0", "type": "cycle_model"}));
    assert_eq!(single["success"], true);
    assert!(single.get("data").is_none(), "one model: no data");
    let mut two = Rpc::start(TestEnv::new(), two_models(), vec![], &["--no-session"]);
    let start = two.state("s")["model"]["id"].clone();
    let c = two.cmd(json!({"id": "c", "type": "cycle_model"}));
    assert_eq!(c["success"], true, "{c}");
    assert!(
        c["data"].is_object(),
        "the engine offers a second model to cycle to: {c}"
    );
    assert_ne!(c["data"]["model"]["id"], start, "cycle moves to the other model");
    assert!(c["data"]["thinkingLevel"].is_string());
    assert_eq!(c["data"]["isScoped"], false);
}

/// A (engine models; Model fields that do not apply are fixed): get_available_models lists the
/// engine's models as Model objects with the documented fields and a fixed zero cost.
#[test]
fn rpc_016() {
    let mut r = Rpc::start(TestEnv::new(), default_opts(), vec![], &["--no-session"]);
    let g = r.cmd(json!({"id": "a", "type": "get_available_models"}));
    assert_eq!(g["success"], true);
    let models = g["data"]["models"].as_array().expect("models array");
    assert_eq!(models.len(), 1, "the engine's model list (one model in the fake)");
    let m = &models[0];
    for key in [
        "id",
        "name",
        "api",
        "provider",
        "baseUrl",
        "reasoning",
        "input",
        "contextWindow",
        "maxTokens",
        "cost",
    ] {
        assert!(m.get(key).is_some(), "Model.{key} present: {m}");
    }
    assert_eq!(m["provider"], "oxe", "one provider");
    assert_eq!(
        m["cost"],
        json!({"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}),
        "engine costs are fixed at zero"
    );
}

/// A (one provider, costs 0, contextWindow = n_ctx): the Model object of the engine's model: the
/// provider is `oxe`, the cost is zero, and contextWindow is the engine's context size.
#[test]
fn rpc_040() {
    let opts = FakeOpts {
        n_ctx: 65536,
        ..FakeOpts::default()
    };
    let mut r = Rpc::start(TestEnv::new(), opts, vec![], &["--no-session"]);
    let st = r.state("g");
    assert_eq!(st["model"]["provider"], "oxe", "one provider");
    let g = r.cmd(json!({"id": "a", "type": "get_available_models"}));
    let m = &g["data"]["models"][0];
    assert_eq!(m["contextWindow"], 65536, "contextWindow is the engine's n_ctx");
    assert_eq!(m["cost"]["input"], 0);
    assert_eq!(m["cost"]["output"], 0);
}

/// A (permission modes, oxa only): set_mode takes plan, workspace or ask; get_state reports it;
/// any other value is refused.
#[test]
fn rpc_039() {
    let mut r = Rpc::start(TestEnv::new(), default_opts(), vec![], &["--no-session"]);
    for mode in ["plan", "ask", "workspace"] {
        let s = r.cmd(json!({"id": format!("m-{mode}"), "type": "set_mode", "mode": mode}));
        assert_eq!(s["success"], true, "{s}");
        assert_eq!(r.state("g")["mode"], mode);
    }
    let bad = r.cmd(json!({"id": "bad", "type": "set_mode", "mode": "bogus"}));
    assert_eq!(bad["success"], false);
}

/// A (W6 explicit thinking): set_thinking_level takes the level; `off` is enable_thinking false and
/// any other level is true; a level the model does not offer is refused.
#[test]
fn rpc_017() {
    let mut r = Rpc::start(
        TestEnv::new(),
        default_opts(),
        vec![ScriptTurn::text("a"), ScriptTurn::text("b")],
        &["--no-session"],
    );
    let high = r.cmd(json!({"id": "h", "type": "set_thinking_level", "level": "high"}));
    assert_eq!(high["success"], true, "{high}");
    assert_eq!(r.state("s")["thinkingLevel"], "high");
    r.cmd(json!({"id": "p", "type": "prompt", "message": "one"}));
    r.run_records();
    assert!(
        r.requests()[0].thinking,
        "W6: a level other than off sends enable_thinking true"
    );
    let off = r.cmd(json!({"id": "o", "type": "set_thinking_level", "level": "off"}));
    assert_eq!(off["success"], true);
    r.cmd(json!({"id": "p2", "type": "prompt", "message": "two"}));
    r.run_records();
    assert!(!r.requests()[1].thinking, "W6: off sends enable_thinking false");
    let xhigh = r.cmd(json!({"id": "x", "type": "set_thinking_level", "level": "xhigh"}));
    assert_eq!(
        xhigh["success"], false,
        "xhigh is only exposed for models that support it"
    );
}

/// A (W6 boolean levels): cycle_thinking_level walks the levels the engine offers and wraps.
#[test]
fn rpc_018() {
    // Start from `off` (the default is pi's `medium`, settings.md `defaultThinkingLevel`).
    let mut r = Rpc::start(
        TestEnv::new(),
        default_opts(),
        vec![],
        &["--no-session", "--thinking", "off"],
    );
    let mut seen = Vec::new();
    for i in 0..5 {
        let c = r.cmd(json!({"id": format!("c{i}"), "type": "cycle_thinking_level"}));
        assert_eq!(c["success"], true);
        seen.push(c["data"]["level"].as_str().expect("level").to_string());
    }
    assert_eq!(seen, ["minimal", "low", "medium", "high", "off"]);
}

/// A (W6 boolean levels, clamped to the model): the list is the levels the engine offers, starting
/// with off; xhigh and max are absent.
#[test]
fn rpc_019() {
    let mut r = Rpc::start(TestEnv::new(), default_opts(), vec![], &["--no-session"]);
    let l = r.cmd(json!({"id": "l", "type": "get_available_thinking_levels"}));
    assert_shape(&l, &part("RPC-019", "response"));
    assert_eq!(
        l["data"]["levels"],
        json!(["off", "minimal", "low", "medium", "high"])
    );
}

/// P: set_steering_mode is acknowledged and get_state reports the mode.
#[test]
fn rpc_020() {
    let mut r = Rpc::start(TestEnv::new(), default_opts(), vec![], &["--no-session"]);
    let s = r.cmd(json!({"id": "m", "type": "set_steering_mode", "mode": "all"}));
    assert_shape(&s, &part("RPC-020", "response"));
    assert_eq!(r.state("g")["steeringMode"], "all");
}

/// P: set_follow_up_mode is acknowledged and get_state reports the mode.
#[test]
fn rpc_021() {
    let mut r = Rpc::start(TestEnv::new(), default_opts(), vec![], &["--no-session"]);
    let s = r.cmd(json!({"id": "m", "type": "set_follow_up_mode", "mode": "all"}));
    assert_shape(&s, &part("RPC-021", "response"));
    assert_eq!(r.state("g")["followUpMode"], "all");
}

/// A (W5 compaction by appends): compact summarises the older history into a compaction entry; the
/// response reports the summary and the tokens before and after.
#[test]
fn rpc_022() {
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"compaction": {"keepRecentTokens": 1}}"#,
    );
    let turns = vec![
        ScriptTurn::text("answer one, long enough to be cut"),
        ScriptTurn::text("answer two, long enough to be cut"),
        ScriptTurn::text("## Goal\nsummary of the two answers"),
    ];
    let mut r = Rpc::start(env, default_opts(), turns, &["--no-session"]);
    r.cmd(json!({"id": "p1", "type": "prompt", "message": "question one"}));
    r.run_records();
    r.cmd(json!({"id": "p2", "type": "prompt", "message": "question two"}));
    r.run_records();
    let c = r.cmd(json!({"id": "c", "type": "compact"}));
    assert_eq!(c["success"], true, "{c}");
    assert_shape(&c, &part("RPC-022", "response"));
    assert!(c["data"]["summary"].as_str().unwrap_or("").contains("summary"));
}

/// P: set_auto_compaction is acknowledged and get_state reports the flag.
#[test]
fn rpc_023() {
    let mut r = Rpc::start(TestEnv::new(), default_opts(), vec![], &["--no-session"]);
    let s = r.cmd(json!({"id": "a", "type": "set_auto_compaction", "enabled": false}));
    assert_shape(&s, &part("RPC-023", "response"));
    assert_eq!(r.state("g")["autoCompactionEnabled"], false);
}

/// P: set_auto_retry is acknowledged and get_state reports the flag.
#[test]
fn rpc_024() {
    let mut r = Rpc::start(TestEnv::new(), default_opts(), vec![], &["--no-session"]);
    let s = r.cmd(json!({"id": "a", "type": "set_auto_retry", "enabled": false}));
    assert_shape(&s, &part("RPC-024", "response"));
    assert_eq!(r.state("g")["autoRetryEnabled"], false);
}

/// P: abort_retry is acknowledged (no retry in progress is not an error).
#[test]
fn rpc_025() {
    let mut r = Rpc::start(TestEnv::new(), default_opts(), vec![], &["--no-session"]);
    let a = r.cmd(json!({"id": "ar", "type": "abort_retry"}));
    assert_shape(&a, &part("RPC-025", "response"));
}

/// P: bash runs the command and returns its output; the output reaches the next prompt as
/// "Ran `<command>`".
#[test]
fn rpc_026() {
    let mut r = Rpc::start(
        TestEnv::new(),
        default_opts(),
        vec![ScriptTurn::text("ok")],
        &["--no-session"],
    );
    let b = r.cmd(json!({"id": "b", "type": "bash", "command": "echo hello"}));
    assert_shape(&b, &part("RPC-026", "response"));
    assert_eq!(b["data"]["output"], "hello\n");
    r.cmd(json!({"id": "p", "type": "prompt", "message": "next"}));
    r.run_records();
    let body = String::from_utf8_lossy(&r.requests()[0].body).into_owned();
    assert!(
        body.contains("Ran `echo hello`"),
        "the bash output is sent with the next prompt"
    );
}

/// P: abort_bash cancels a running bash command; the bash response reports cancelled.
#[test]
fn rpc_027() {
    let mut r = Rpc::start(TestEnv::new(), default_opts(), vec![], &["--no-session"]);
    let a = r.cmd(json!({"id": "ab0", "type": "abort_bash"}));
    assert_shape(&a, &part("RPC-027", "response"));
    r.send(json!({"id": "sl", "type": "bash", "command": "sleep 5"}));
    thread::sleep(Duration::from_millis(400));
    let cancel = r.cmd(json!({"id": "ab", "type": "abort_bash"}));
    assert_eq!(cancel["success"], true);
    let b = r.resp("sl");
    assert_eq!(b["data"]["cancelled"], true, "{b}");
}

/// A (W4 cost is 0, exact cache statistics): get_session_stats has the documented fields; the
/// cost is 0 for the engine.
#[test]
fn rpc_028() {
    let mut r = Rpc::start(
        TestEnv::new(),
        default_opts(),
        vec![ScriptTurn::text("Hello there.")],
        &["--no-session"],
    );
    r.cmd(json!({"id": "p", "type": "prompt", "message": "hi"}));
    r.run_records();
    let s = r.cmd(json!({"id": "st", "type": "get_session_stats"}));
    assert_shape(&s, &part("RPC-028", "response"));
    assert_eq!(s["data"]["userMessages"], 1);
    assert_eq!(s["data"]["assistantMessages"], 1);
    assert_eq!(s["data"]["cost"], 0, "the engine has no cost");
    assert_eq!(s["data"]["contextUsage"]["contextWindow"], 1 << 20);
}

/// P: export_html writes an HTML file and reports its path (the path may be given).
#[test]
fn rpc_029() {
    let env = TestEnv::new();
    let out = env.work().join("out.html");
    let mut r = Rpc::start(
        env,
        default_opts(),
        vec![ScriptTurn::text("ok")],
        &["--no-session"],
    );
    r.cmd(json!({"id": "p", "type": "prompt", "message": "hi"}));
    r.run_records();
    let e = r.cmd(json!({"id": "e", "type": "export_html", "outputPath": out.display().to_string()}));
    assert_shape(&e, &part("RPC-029", "response"));
    assert_eq!(e["data"]["path"], out.display().to_string());
    let html = std::fs::read_to_string(&out).expect("the HTML file exists");
    assert!(html.to_lowercase().contains("<html"), "an HTML document");
}

/// P: switch_session loads another session file; the reply reports cancelled false.
#[test]
fn rpc_030() {
    let turns = vec![ScriptTurn::text("first answer"), ScriptTurn::text("other answer")];
    let mut r = Rpc::start(TestEnv::new(), default_opts(), turns, &[]);
    r.cmd(json!({"id": "p", "type": "prompt", "message": "first"}));
    r.run_records();
    let first = r.state("s1");
    let first_path = first["sessionFile"]
        .as_str()
        .expect("a persistent session has a file")
        .to_string();
    let first_id = first["sessionId"].clone();
    r.cmd(json!({"id": "n", "type": "new_session"}));
    r.cmd(json!({"id": "p2", "type": "prompt", "message": "other"}));
    r.run_records();
    let s = r.cmd(json!({"id": "sw", "type": "switch_session", "sessionPath": first_path}));
    assert_shape(&s, &part("RPC-030", "response"));
    assert_eq!(r.state("s2")["sessionId"], first_id);
}

/// P: fork starts a new session at the chosen user message and returns that message's text.
#[test]
fn rpc_031() {
    let turns = vec![ScriptTurn::text("answer one"), ScriptTurn::text("answer two")];
    let mut r = Rpc::start(TestEnv::new(), default_opts(), turns, &[]);
    r.cmd(json!({"id": "p1", "type": "prompt", "message": "first"}));
    r.run_records();
    r.cmd(json!({"id": "p2", "type": "prompt", "message": "second"}));
    r.run_records();
    let before = r.state("s1")["sessionId"].clone();
    let forks = r.cmd(json!({"id": "gf", "type": "get_fork_messages"}));
    let entry = forks["data"]["messages"][1]["entryId"]
        .as_str()
        .expect("entry id")
        .to_string();
    let f = r.cmd(json!({"id": "f", "type": "fork", "entryId": entry}));
    assert_shape(&f, &part("RPC-031", "response"));
    assert_eq!(f["data"]["text"], "second");
    assert_ne!(r.state("s2")["sessionId"], before, "a fork is a new session");
}

/// Rows RPC-017, -020, -023, -037, -039: a get_state sent right after a command that changes the
/// state reports the state after that command (commands are processed in order, pi rpc-mode.ts
/// handles each line in turn), whether it is pipelined behind the command or sent after its
/// response.
#[test]
fn extra_get_state_reflects_earlier_commands() {
    let mut r = Rpc::start(TestEnv::new(), default_opts(), vec![], &["--no-session"]);
    for (i, mode) in ["all", "one-at-a-time"].iter().cycle().take(40).enumerate() {
        // Both lines in one write: the state change is queued when the query is read.
        r.send_raw(&format!(
            "{{\"id\":\"ss{i}\",\"type\":\"set_steering_mode\",\"mode\":\"{mode}\"}}\n{{\"id\":\"gs{i}\",\"type\":\"get_state\"}}\n"
        ));
        let st = r.resp(&format!("gs{i}"));
        assert_eq!(st["data"]["steeringMode"], *mode, "pipelined: {st}");
        // Sent after the response of the change.
        let name = format!("n{i}");
        assert_eq!(
            r.cmd(json!({"id": format!("sn{i}"), "type": "set_session_name", "name": name}))["success"],
            true
        );
        let st = r.cmd(json!({"id": format!("gn{i}"), "type": "get_state"}));
        assert_eq!(st["data"]["sessionName"], name, "after the response: {st}");
    }
}

/// RPC-031: a fork at a user message (position before) leaves that message out of the new
/// branch; pi moves the leaf to the message's parent (agent-session-runtime.ts, `fork`:
/// `targetLeafId = selectedEntry.parentId`) and returns its text.
#[test]
fn extra_fork_drops_the_forked_message() {
    let turns = vec![ScriptTurn::text("answer one"), ScriptTurn::text("answer two")];
    let mut r = Rpc::start(TestEnv::new(), default_opts(), turns, &[]);
    r.cmd(json!({"id": "p1", "type": "prompt", "message": "first"}));
    r.run_records();
    r.cmd(json!({"id": "p2", "type": "prompt", "message": "second"}));
    r.run_records();
    let forks = r.cmd(json!({"id": "gf", "type": "get_fork_messages"}));
    let entry = forks["data"]["messages"][1]["entryId"]
        .as_str()
        .expect("entry id")
        .to_string();
    let f = r.cmd(json!({"id": "f", "type": "fork", "entryId": entry}));
    assert_eq!(f["data"]["text"], "second");
    let msgs = r.messages("m");
    let forked_present = msgs
        .iter()
        .any(|m| m["role"] == "user" && m["content"] == "second");
    assert!(
        !forked_present,
        "the forked message is not part of the new branch"
    );
}

/// P: clone duplicates the active branch into a new session; the messages are kept.
#[test]
fn rpc_032() {
    let mut r = Rpc::start(
        TestEnv::new(),
        default_opts(),
        vec![ScriptTurn::text("Hello there.")],
        &[],
    );
    r.cmd(json!({"id": "p", "type": "prompt", "message": "hi"}));
    r.run_records();
    let before_id = r.state("s1")["sessionId"].clone();
    let before_msgs = r.messages("m1").len();
    let c = r.cmd(json!({"id": "c", "type": "clone"}));
    assert_shape(&c, &part("RPC-032", "response"));
    assert_ne!(r.state("s2")["sessionId"], before_id);
    assert_eq!(r.messages("m2").len(), before_msgs);
}

/// P: get_fork_messages lists the user messages that can be forked from, with their text.
#[test]
fn rpc_033() {
    let turns = vec![ScriptTurn::text("answer one"), ScriptTurn::text("answer two")];
    let mut r = Rpc::start(TestEnv::new(), default_opts(), turns, &[]);
    r.cmd(json!({"id": "p1", "type": "prompt", "message": "first"}));
    r.run_records();
    r.cmd(json!({"id": "p2", "type": "prompt", "message": "second"}));
    r.run_records();
    let f = r.cmd(json!({"id": "gf", "type": "get_fork_messages"}));
    assert_shape(&f, &part("RPC-033", "response"));
    let texts: Vec<&str> = f["data"]["messages"]
        .as_array()
        .expect("array")
        .iter()
        .map(|m| m["text"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(texts, ["first", "second"]);
}

/// P: get_entries returns the entries in append order with a leafId; `since` is an exclusive cursor;
/// an unknown `since` is refused.
#[test]
fn rpc_034() {
    let mut r = Rpc::start(
        TestEnv::new(),
        default_opts(),
        vec![ScriptTurn::text("Hello there.")],
        &[],
    );
    r.cmd(json!({"id": "p", "type": "prompt", "message": "hi"}));
    r.run_records();
    let all = r.cmd(json!({"id": "e", "type": "get_entries"}));
    assert_shape(&all, &part("RPC-034", "response"));
    let entries = all["data"]["entries"].as_array().expect("entries").clone();
    let last = entries.last().expect("entries")["id"].clone();
    assert_eq!(all["data"]["leafId"], last, "the leaf is the last entry");
    let first = entries[0]["id"].as_str().expect("id").to_string();
    let since = r.cmd(json!({"id": "s", "type": "get_entries", "since": first}));
    assert_eq!(
        since["data"]["entries"].as_array().expect("entries").len(),
        entries.len() - 1
    );
    let bad = r.cmd(json!({"id": "b", "type": "get_entries", "since": "no-such-entry"}));
    assert_eq!(bad["success"], false);
}

/// P: get_tree returns the entries as a tree of {entry, children} nodes and the leafId.
#[test]
fn rpc_035() {
    let mut r = Rpc::start(
        TestEnv::new(),
        default_opts(),
        vec![ScriptTurn::text("Hello there.")],
        &[],
    );
    r.cmd(json!({"id": "p", "type": "prompt", "message": "hi"}));
    r.run_records();
    let t = r.cmd(json!({"id": "t", "type": "get_tree"}));
    assert_shape(&t, &part("RPC-035", "response"));
    let mut node = t["data"]["tree"][0].clone();
    loop {
        let children = node["children"].as_array().expect("children").clone();
        if children.is_empty() {
            break;
        }
        for c in &children {
            assert_eq!(
                c["entry"]["parentId"], node["entry"]["id"],
                "children point at their parent"
            );
        }
        node = children[0].clone();
    }
    assert_eq!(
        node["entry"]["id"], t["data"]["leafId"],
        "the leaf ends the chain"
    );
}

/// P: get_last_assistant_text returns the text of the last assistant message, or null.
#[test]
fn rpc_036() {
    let mut r = Rpc::start(
        TestEnv::new(),
        default_opts(),
        vec![ScriptTurn::text("Hello there.")],
        &[],
    );
    let none = r.cmd(json!({"id": "l0", "type": "get_last_assistant_text"}));
    assert_shape(&none, &part("RPC-036", "response"));
    assert!(none["data"]["text"].is_null());
    r.cmd(json!({"id": "p", "type": "prompt", "message": "hi"}));
    r.run_records();
    let some = r.cmd(json!({"id": "l1", "type": "get_last_assistant_text"}));
    assert_eq!(some["data"]["text"], "Hello there.");
}

/// P: set_session_name is acknowledged and get_state reports the name.
#[test]
fn rpc_037() {
    let mut r = Rpc::start(TestEnv::new(), default_opts(), vec![], &["--no-session"]);
    let s = r.cmd(json!({"id": "n", "type": "set_session_name", "name": "my-feature-work"}));
    assert_shape(&s, &part("RPC-037", "response"));
    assert_eq!(r.state("g")["sessionName"], "my-feature-work");
}

/// P: get_commands lists the extension commands with their source.
#[test]
fn rpc_038() {
    let mut r = Rpc::start(ui_env(), default_opts(), vec![], &["--no-session"]);
    let c = r.cmd(json!({"id": "c", "type": "get_commands"}));
    assert_shape(&c, &part("RPC-038", "response"));
    let commands = c["data"]["commands"].as_array().expect("commands");
    let sel = commands
        .iter()
        .find(|v| v["name"] == "sel")
        .expect("the sel command is listed");
    assert_eq!(sel["source"], "extension");
}

/// P: the select dialog is a request with method select, its options and timeout.
#[test]
fn rpc_041() {
    let mut r = Rpc::start(ui_env(), default_opts(), vec![], &["--no-session"]);
    r.cmd(json!({"id": "p", "type": "prompt", "message": "sel"}));
    let req = r.ui("select");
    assert_shape(&req, &part("RPC-041", "request"));
    assert_eq!(req["title"], "Allow dangerous command?");
    assert_eq!(req["options"], json!(["Allow", "Block"]));
    assert_eq!(req["timeout"], 10000);
    r.send(json!({"type": "extension_ui_response", "id": req["id"], "value": "Allow"}));
    let note = r.ui("notify");
    assert!(
        note["message"].as_str().unwrap_or("").starts_with("sel:"),
        "{note}"
    );
}

/// P: the confirm dialog is a request with method confirm, the message and the timeout.
#[test]
fn rpc_042() {
    let mut r = Rpc::start(ui_env(), default_opts(), vec![], &["--no-session"]);
    r.cmd(json!({"id": "p", "type": "prompt", "message": "conf"}));
    let req = r.ui("confirm");
    assert_shape(&req, &part("RPC-042", "request"));
    assert_eq!(req["message"], "All messages will be lost.");
    assert_eq!(req["timeout"], 5000);
    r.send(json!({"type": "extension_ui_response", "id": req["id"], "confirmed": true}));
    let note = r.ui("notify");
    assert_eq!(note["message"], "conf:boolean:true");
}

/// P: the input dialog is a request with method input and the placeholder.
#[test]
fn rpc_043() {
    let mut r = Rpc::start(ui_env(), default_opts(), vec![], &["--no-session"]);
    r.cmd(json!({"id": "p", "type": "prompt", "message": "inp"}));
    let req = r.ui("input");
    assert_shape(&req, &part("RPC-043", "request"));
    assert_eq!(req["placeholder"], "type something...");
    r.send(json!({"type": "extension_ui_response", "id": req["id"], "value": "typed"}));
    let note = r.ui("notify");
    assert_eq!(note["message"], "inp:string:typed");
}

/// P: the editor dialog is a request with method editor and the prefilled text.
#[test]
fn rpc_044() {
    let mut r = Rpc::start(ui_env(), default_opts(), vec![], &["--no-session"]);
    r.cmd(json!({"id": "p", "type": "prompt", "message": "edt"}));
    let req = r.ui("editor");
    assert_shape(&req, &part("RPC-044", "request"));
    assert_eq!(req["prefill"], "Line 1\nLine 2\nLine 3");
    r.send(json!({"type": "extension_ui_response", "id": req["id"], "value": "edited"}));
    let note = r.ui("notify");
    assert_eq!(note["message"], "edt:string:edited");
}

/// P: notify is a fire-and-forget request with its message and notifyType.
#[test]
fn rpc_045() {
    let mut r = Rpc::start(ui_env(), default_opts(), vec![], &["--no-session"]);
    r.cmd(json!({"id": "p", "type": "prompt", "message": "warn"}));
    let req = r.ui("notify");
    assert_shape(&req, &part("RPC-045", "request"));
    assert_eq!(req["message"], "Command blocked by user");
    assert_eq!(req["notifyType"], "warning");
}

/// P: setStatus sets a status entry by key; without a text the key is cleared (no statusText).
#[test]
fn rpc_046() {
    let mut r = Rpc::start(ui_env(), default_opts(), vec![], &["--no-session"]);
    r.cmd(json!({"id": "p", "type": "prompt", "message": "status"}));
    let set = r.ui("setStatus");
    assert_shape(&set, &part("RPC-046", "request"));
    assert_eq!(set["statusKey"], "my-ext");
    assert_eq!(set["statusText"], "Turn 3 running...");
    let clear = r.ui("setStatus");
    assert_eq!(clear["statusKey"], "my-ext");
    assert!(
        clear.get("statusText").is_none(),
        "a cleared status has no statusText"
    );
}

/// P: setWidget sets lines by key; the placement defaults to aboveEditor.
#[test]
fn rpc_047() {
    let mut r = Rpc::start(ui_env(), default_opts(), vec![], &["--no-session"]);
    r.cmd(json!({"id": "p", "type": "prompt", "message": "widget"}));
    let w = r.ui("setWidget");
    assert_shape(&w, &part("RPC-047", "request"));
    assert_eq!(w["widgetKey"], "my-ext");
    assert_eq!(w["widgetLines"], json!(["--- My Widget ---", "Line 1", "Line 2"]));
    assert_eq!(w["widgetPlacement"], "aboveEditor");
}

/// P: setTitle sets the window title.
#[test]
fn rpc_048() {
    let mut r = Rpc::start(ui_env(), default_opts(), vec![], &["--no-session"]);
    r.cmd(json!({"id": "p", "type": "prompt", "message": "title"}));
    let t = r.ui("setTitle");
    assert_shape(&t, &part("RPC-048", "request"));
    assert_eq!(t["title"], "pi - my project");
}

/// P: set_editor_text sets the editor text.
#[test]
fn rpc_049() {
    let mut r = Rpc::start(ui_env(), default_opts(), vec![], &["--no-session"]);
    r.cmd(json!({"id": "p", "type": "prompt", "message": "edtext"}));
    let t = r.ui("set_editor_text");
    assert_shape(&t, &part("RPC-049", "request"));
    assert_eq!(t["text"], "prefilled text for the user");
}

/// P: a dialog is resolved by the extension_ui_response with the matching id: a value, a
/// confirmation, a cancellation; a dialog with a timeout resolves by itself when nobody answers.
#[test]
fn rpc_050() {
    let mut r = Rpc::start(ui_env(), default_opts(), vec![], &["--no-session"]);
    // A value answer resumes the command.
    let p1 = r.cmd(json!({"id": "p1", "type": "prompt", "message": "sel"}));
    assert_eq!(p1["success"], true, "{p1}");
    let req = r.ui("select");
    r.send(json!({"type": "extension_ui_response", "id": req["id"], "value": "Allow"}));
    assert!(r.ui("notify")["message"]
        .as_str()
        .unwrap_or("")
        .starts_with("sel:"));
    r.run_records();
    // A confirmation answer resumes it with the boolean.
    let p2 = r.cmd(json!({"id": "p2", "type": "prompt", "message": "conf"}));
    assert_eq!(p2["success"], true, "{p2}");
    let req = r.ui("confirm");
    r.send(json!({"type": "extension_ui_response", "id": req["id"], "confirmed": true}));
    assert_eq!(r.ui("notify")["message"], "conf:boolean:true");
    r.run_records();
    // A cancellation resumes it too.
    let p3 = r.cmd(json!({"id": "p3", "type": "prompt", "message": "conf"}));
    assert_eq!(p3["success"], true, "{p3}");
    let req = r.ui("confirm");
    r.send(json!({"type": "extension_ui_response", "id": req["id"], "cancelled": true}));
    assert!(r.ui("notify")["message"]
        .as_str()
        .unwrap_or("")
        .starts_with("conf:"));
    r.run_records();
    // A dialog with a timeout resolves without an answer.
    let p4 = r.cmd(json!({"id": "p4", "type": "prompt", "message": "tmo"}));
    assert_eq!(p4["success"], true, "{p4}");
    let note = r.ui("notify");
    assert!(
        note["message"].as_str().unwrap_or("").starts_with("tmo:"),
        "{note}"
    );
}

/// A (permission Ask, oxa only): in ask mode a tool that needs permission relays the ask as an
/// extension-UI confirm; the answer decides whether the tool runs.
#[test]
fn rpc_051() {
    let mut r = Rpc::start(
        ui_env(),
        default_opts(),
        vec![
            ScriptTurn::tool("c1", "write", r#"{"path": "made.txt", "content": "hi\n"}"#),
            ScriptTurn::text("done"),
        ],
        &["--no-session", "--standard"],
    );
    assert_eq!(
        r.cmd(json!({"id": "m", "type": "set_mode", "mode": "ask"}))["success"],
        true
    );
    r.cmd(json!({"id": "p", "type": "prompt", "message": "write it"}));
    let ask = r.ui("confirm");
    assert!(ask["title"].as_str().unwrap_or("").contains("write"), "{ask}");
    r.send(json!({"type": "extension_ui_response", "id": ask["id"], "confirmed": true}));
    r.run_records();
    assert!(r.env.work().join("made.txt").exists(), "the allowed write ran");
}

/// A (Rust client, examples/rpc-client flow): pi's `examples/rpc-client.ts` spawns the agent
/// with `--no-session`, prints text deltas and tool starts from the events, sends one prompt,
/// waits for the run to end and stops the child.
/// invariant: no TS SDK; `oxa-rpc` ships a Rust client (`RpcClient`) for the same flow.
#[test]
fn rpc_052() {
    use oxa_rpc::RpcClient;
    let env = TestEnv::new();
    env.write("work/notes.md", "the notes\n");
    let server = FakeServer::start(
        vec![
            ScriptTurn::tool("call_1", "read", r#"{"path":"notes.md"}"#),
            ScriptTurn {
                chunk: 4,
                ..ScriptTurn::text("This repository holds notes.")
            },
        ],
        FakeOpts::default(),
    )
    .expect("fake engine");
    let mut client = RpcClient::spawn(
        &oxa_bin(),
        &["--no-session".to_string()],
        &[env.envs(&server.url()), oxa_parity::note_off_env()].concat(),
        &env.work(),
    )
    .expect("spawn oxa --mode rpc");
    let resp = client
        .request(
            json!({"type": "prompt", "message": "Explain this repository in one paragraph."}),
            30_000,
        )
        .expect("prompt accepted");
    assert_eq!(resp["success"], json!(true));
    let events = client.wait_for("agent_end", 30_000).expect("the run ends");
    // What rpc-client.ts prints: the text deltas, and `[tool: <name>]` for tool starts.
    let mut stdout = String::new();
    let mut tools = Vec::new();
    for e in &events {
        if e["type"] == "message_update" && e["assistantMessageEvent"]["type"] == "text_delta" {
            stdout.push_str(e["assistantMessageEvent"]["delta"].as_str().unwrap_or(""));
        } else if e["type"] == "tool_execution_start" {
            tools.push(e["toolName"].as_str().unwrap_or("").to_string());
        }
    }
    assert_eq!(stdout, "This repository holds notes.");
    assert_eq!(tools, vec!["read".to_string()]);
    assert_eq!(client.close().expect("stop"), 0, "a clean stop");
    // --no-session: nothing was written.
    let sessions = env.agent_dir().join("sessions");
    let files = std::fs::read_dir(&sessions).map(|d| d.count()).unwrap_or(0);
    assert_eq!(files, 0, "--no-session keeps nothing on disk");
}

// ---------------------------------------------------------------------------------------------
// Extra checks beyond the rows
// ---------------------------------------------------------------------------------------------

/// P (rpc-commands.md#prompt): an extension command is handled, not started as a run: the
/// command runs (its notify arrives), no run starts, and the next prompt is accepted at once.
#[test]
fn extra_extension_command_disposition_is_handled() {
    let mut r = Rpc::start(
        ui_env(),
        default_opts(),
        vec![ScriptTurn::text("after")],
        &["--no-session"],
    );
    let p = r.cmd(json!({"id": "p", "type": "prompt", "message": "/warn"}));
    assert_eq!(p["data"]["disposition"], "handled", "{p}");
    let note = r.ui("notify");
    assert_eq!(note["message"], "Command blocked by user", "{note}");
    let next = r.cmd(json!({"id": "p2", "type": "prompt", "message": "hello"}));
    assert_eq!(next["data"]["disposition"], "started", "{next}");
    let records = r.run_records();
    let starts = records.iter().filter(|v| v["type"] == "agent_start").count();
    assert_eq!(starts, 1, "only the second prompt runs: {records:?}");
    assert!(r.requests().len() == 1, "the command reached no model");
}

/// P (rpc-extension-ui.md, select): the dialog result is the chosen option string and a
/// cancelled dialog gives undefined (select/input/editor) or false (confirm).
#[test]
fn extra_dialog_results_match_pi_api() {
    let mut r = Rpc::start(ui_env(), default_opts(), vec![], &["--no-session"]);
    r.cmd(json!({"id": "p1", "type": "prompt", "message": "sel"}));
    let req = r.ui("select");
    r.send(json!({"type": "extension_ui_response", "id": req["id"], "value": "Allow"}));
    assert_eq!(r.ui("notify")["message"], "sel:string:Allow");
    // The next command goes out as soon as the run has settled (no wait for "idle").
    r.run_records();
    r.cmd(json!({"id": "p2", "type": "prompt", "message": "conf"}));
    let req = r.ui("confirm");
    r.send(json!({"type": "extension_ui_response", "id": req["id"], "cancelled": true}));
    assert_eq!(r.ui("notify")["message"], "conf:boolean:false");
    // The next command goes out as soon as the run has settled (no wait for "idle").
    r.run_records();
    r.cmd(json!({"id": "p3", "type": "prompt", "message": "tmo"}));
    assert_eq!(r.ui("notify")["message"], "tmo:undefined:undefined");
}
