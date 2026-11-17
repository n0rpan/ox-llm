//! G6 PARITY tests (json).
//! One test per PARITY.md row of this area, named after the row id (`JSON-012` -> `fn json_012()`).
//!
//! Verdicts (see PARITY.md): P rows compare with pi's documented output (`fixtures/goldens/json/
//! documented-fields.json`, copied from pi 1.5.2 `docs/json.md`); A rows assert the adapted
//! behaviour and carry an `Invariant:` line; O rows assert `unsupported_in_oxa` (none in JSON).
//!
//! A row that does not match pi fails with a message starting `RED <id>:` and the precise reason.
//! Those failures are the H-wave input; they are not hidden with `#[ignore]`.
//!
//! Isolation: `oxa_fake::TestEnv` for every directory; the fake engine is in-process
//! (`FakeServer`); the `oxa` binary is `oxa_parity::oxa_bin()` (build with
//! `cargo build -p oxa-cli -p oxa-fake` first).

use oxa_fake::{FakeOpts, FakeServer, Fault, ScriptTurn, TestEnv};
use oxa_parity::{fixture, oxa_bin, run_oxa};
use serde_json::Value;
use std::io::Write;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

// ---------------------------------------------------------------------------------------------
// helpers

/// The documented field table (pi 1.5.2 `docs/json.md`).
fn documented_doc() -> Value {
    let path = fixture("goldens/json/documented-fields.json");
    let text = std::fs::read_to_string(&path).expect("read documented-fields.json");
    serde_json::from_str(&text).expect("documented-fields.json parses")
}

/// Fail the row with `RED <id>: <why>` unless `ok`.
fn check(id: &str, ok: bool, why: impl Into<String>) {
    if !ok {
        panic!("RED {id}: {}", why.into());
    }
}

/// The record is the documented event `event`: its type and every required field is present.
fn documented(id: &str, rec: &Value, event: &str) {
    let doc = documented_doc();
    let spec = &doc["events"][event];
    check(
        id,
        !spec.is_null(),
        format!("`{event}` is not in the documented table"),
    );
    check(
        id,
        rec["type"] == event,
        format!("record type is {}, documented {event}", rec["type"]),
    );
    for f in spec["required"].as_array().expect("required list") {
        let f = f.as_str().expect("field name");
        check(
            id,
            rec.get(f).is_some(),
            format!("{event} lacks documented field `{f}`"),
        );
    }
}

fn start(script: Vec<ScriptTurn>) -> FakeServer {
    FakeServer::start(script, FakeOpts::default()).expect("fake engine")
}

fn text(t: &str) -> ScriptTurn {
    ScriptTurn::text(t)
}

/// A 503 that the engine answers before any stream (retried by oxa, `auto_retry_*`).
fn overloaded() -> ScriptTurn {
    ScriptTurn::fault(Fault::Http {
        status: 503,
        body: r#"{"error":{"message":"overloaded","type":"unavailable_error"}}"#.to_string(),
    })
}

/// Parse JSONL stdout: one JSON object per LF-terminated record.
fn parse_records(bytes: &[u8]) -> Vec<Value> {
    let text = std::str::from_utf8(bytes).expect("utf-8 stdout");
    text.split('\n')
        .filter(|l| !l.is_empty())
        .map(|l| {
            serde_json::from_str::<Value>(l.trim_end_matches('\r'))
                .unwrap_or_else(|e| panic!("stdout line is not a JSON record ({e}): {l}"))
        })
        .collect()
}

fn of<'a>(recs: &'a [Value], ty: &str) -> Vec<&'a Value> {
    recs.iter().filter(|r| r["type"] == ty).collect()
}

/// Run `oxa` with `args` and `stdin` against `server`; returns the process output and its records.
fn run_json(env: &TestEnv, server: &FakeServer, args: &[&str], stdin: &str) -> (Output, Vec<Value>) {
    let out = run_oxa(env, &server.url(), args, stdin);
    let recs = parse_records(&out.stdout);
    (out, recs)
}

/// Run `oxa` with stdin lines written with a pause (ms) before each line; stdin is closed after
/// the last line. Used where a command must land while a run is in progress.
fn run_paced(env: &TestEnv, server: &FakeServer, args: &[&str], steps: Vec<(u64, String)>) -> Output {
    let mut child = Command::new(oxa_bin())
        .args(args)
        .current_dir(env.work())
        .env_clear()
        .envs(env.envs(&server.url()))
        .envs(oxa_parity::note_off_env())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn oxa (build it first: cargo build -p oxa-cli -p oxa-fake)");
    let mut stdin = child.stdin.take().expect("stdin pipe");
    let writer = std::thread::spawn(move || {
        for (ms, line) in steps {
            std::thread::sleep(Duration::from_millis(ms));
            let _ = writeln!(stdin, "{line}");
            let _ = stdin.flush();
        }
    });
    let out = child.wait_with_output().expect("wait for oxa");
    writer.join().expect("stdin writer");
    out
}

/// The reply of a plain text run: `text`, then one assistant message.
fn text_run(env: &TestEnv, reply: &str) -> (Output, Vec<Value>, FakeServer) {
    let server = start(vec![text(reply)]);
    let (out, recs) = run_json(env, &server, &["--mode", "json", "say hi"], "");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    (out, recs, server)
}

/// The tool run used by most rows: `read note.txt`, then a text answer.
fn tool_run(env: &TestEnv) -> (Output, Vec<Value>, FakeServer) {
    env.write("work/note.txt", "hello world\n");
    let server = start(vec![
        ScriptTurn::tool("call_1", "read", r#"{"path": "note.txt"}"#),
        text("Done."),
    ]);
    let (out, recs) = run_json(env, &server, &["--mode", "json", "read note"], "");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    (out, recs, server)
}

/// A reply of about 3000 bytes, enough history for a compaction to cut.
fn big(tag: &str) -> ScriptTurn {
    text(&format!("{tag} {}", "a".repeat(3000)))
}

/// Four prompts in one session (`-p`, then `-c -p`), so a compaction has history to cut.
/// The fake's script must hold the four replies first, then whatever the compaction needs.
fn seed(env: &TestEnv, server: &FakeServer) {
    let runs: [&[&str]; 4] = [
        &["-p", "one"],
        &["-c", "-p", "two"],
        &["-c", "-p", "three"],
        &["-c", "-p", "four"],
    ];
    for (i, args) in runs.iter().enumerate() {
        let out = run_oxa(env, &server.url(), args, "");
        assert!(
            out.status.success(),
            "seed run {i}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// The RPC compaction of the seeded session (`compact` command), with its records.
fn compact_rpc(env: &TestEnv, server: &FakeServer) -> (Output, Vec<Value>) {
    run_json(
        env,
        server,
        &["-c", "--mode", "rpc"],
        "{\"id\":\"c\",\"type\":\"compact\"}\n",
    )
}

fn seeded_server(after_seed: Vec<ScriptTurn>) -> FakeServer {
    let mut script = vec![big("r1"), big("r2"), big("r3"), text("r4")];
    script.extend(after_seed);
    start(script)
}

fn parity_rows() -> Vec<(String, String)> {
    // (id, verdict) for the JSON section of PARITY.md. Escaped `\|` inside a cell is not a
    // column separator.
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../PARITY.md");
    let text = std::fs::read_to_string(path).expect("read PARITY.md");
    let mut in_section = false;
    let mut out = Vec::new();
    for line in text.lines() {
        if line.starts_with("## ") {
            in_section = line.starts_with("## JSON:");
            continue;
        }
        if !in_section || !line.starts_with("| JSON-") {
            continue;
        }
        let cells: Vec<String> = line
            .replace("\\|", "\u{1}")
            .split('|')
            .map(|c| c.trim().replace('\u{1}', "|"))
            .collect();
        out.push((cells[1].clone(), cells[3].clone()));
    }
    out
}

/// The source of this file, for the meta-checks.
const SOURCE: &str = include_str!("json.rs");

/// The doc comment block directly above `fn <name>()` in this file.
fn doc_block_of(name: &str) -> Vec<String> {
    let at = SOURCE
        .find(&format!("\nfn {name}()"))
        .unwrap_or_else(|| panic!("no test fn {name}"));
    SOURCE[..at + 1]
        .lines()
        .rev()
        .skip_while(|l| l.trim_start().starts_with("#["))
        .take_while(|l| l.trim_start().starts_with("///"))
        .map(|l| l.trim_start().trim_start_matches("///").trim().to_string())
        .collect()
}

// ---------------------------------------------------------------------------------------------
// meta-checks

/// each PARITY.md id of this area maps to a test fn or an O-row assertion
#[test]
fn every_row_has_test_or_recorded_omission() {
    let rows = parity_rows();
    assert_eq!(rows.len(), 28, "the JSON section has 28 rows: {rows:?}");
    for (id, _) in &rows {
        let name = id.to_lowercase().replace('-', "_");
        assert!(
            SOURCE.contains(&format!("\nfn {name}()")),
            "{id} has no test fn `{name}` in json.rs"
        );
    }
}

/// documented output or recorded fixture
#[test]
fn documented_rows_have_tests() {
    let doc = documented_doc();
    let rows = &doc["rows"];
    let mut p_count = 0;
    for (id, verdict) in parity_rows() {
        if verdict != "P" {
            continue;
        }
        p_count += 1;
        let event = rows[id.as_str()]
            .as_str()
            .unwrap_or_else(|| panic!("{id} (P) has no documented event in the fixture"));
        if event != "-" {
            assert!(
                !doc["events"][event].is_null(),
                "{id} maps to `{event}`, which the documented table does not list"
            );
        }
    }
    assert_eq!(p_count, 25, "the JSON section has 25 P rows");
}

/// comment + adapted behaviour asserted
#[test]
fn a_rows_name_their_invariant() {
    let mut a_count = 0;
    for (id, verdict) in parity_rows() {
        if verdict != "A" {
            continue;
        }
        a_count += 1;
        let name = id.to_lowercase().replace('-', "_");
        let doc = doc_block_of(&name);
        assert!(
            doc.iter().any(|l| l.contains("Invariant:")),
            "{id} (A) has no `Invariant:` line in its doc comment: {doc:?}"
        );
    }
    assert_eq!(a_count, 3, "the JSON section has 3 A rows");
}

// ---------------------------------------------------------------------------------------------
// rows

/// JSON-001 (P): JSONL framing is LF only. A reply with U+2028 and a carriage return inside
/// the text stays one record; every record ends with LF.
#[test]
fn json_001() {
    let env = TestEnv::new();
    let reply = "a\u{2028}b\rc";
    let (out, recs, _s) = text_run(&env, reply);
    check(
        "JSON-001",
        out.stdout.last() == Some(&b'\n'),
        "stdout does not end with LF",
    );
    check(
        "JSON-001",
        !out.stdout.contains(&b'\r'),
        "a raw CR byte reached stdout",
    );
    let lf = out.stdout.iter().filter(|&&b| b == b'\n').count();
    check(
        "JSON-001",
        lf == recs.len(),
        format!("{lf} LF but {} records", recs.len()),
    );
    // pi's sequence (json.md) ends the user message first; the reply is the assistant's.
    let end: Vec<&Value> = of(&recs, "message_end")
        .into_iter()
        .filter(|r| r["message"]["role"] == "assistant")
        .collect();
    let got = end[0]["message"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    check("JSON-001", got == reply, format!("reply text changed: {got:?}"));
}

/// JSON-002 (P): the first record is the session header, version 3, one per run.
#[test]
fn json_002() {
    let env = TestEnv::new();
    let (_out, recs, _s) = text_run(&env, "ok");
    check(
        "JSON-002",
        recs[0]["type"] == "session",
        "first record is not the session header",
    );
    documented("JSON-002", &recs[0], "session");
    check("JSON-002", recs[0]["version"] == 3, "header version is not 3");
    check(
        "JSON-002",
        of(&recs, "session").len() == 1,
        "more than one session header",
    );
}

/// JSON-003 (P): `agent_start` opens one run.
#[test]
fn json_003() {
    let env = TestEnv::new();
    let (_out, recs, _s) = text_run(&env, "ok");
    let starts = of(&recs, "agent_start");
    check(
        "JSON-003",
        starts.len() == 1,
        format!("{} agent_start records", starts.len()),
    );
    documented("JSON-003", starts[0], "agent_start");
}

/// JSON-004 (P): `agent_end` carries the run's `messages` and `willRetry`.
#[test]
fn json_004() {
    let env = TestEnv::new();
    let (_out, recs, _s) = tool_run(&env);
    let ends = of(&recs, "agent_end");
    check(
        "JSON-004",
        ends.len() == 1,
        format!("{} agent_end records", ends.len()),
    );
    documented("JSON-004", ends[0], "agent_end");
    let roles: Vec<&str> = ends[0]["messages"]
        .as_array()
        .expect("messages array")
        .iter()
        .filter_map(|m| m["role"].as_str())
        .collect();
    check(
        "JSON-004",
        roles == ["assistant", "toolResult", "assistant"],
        format!("messages roles {roles:?}"),
    );
    check(
        "JSON-004",
        ends[0]["willRetry"] == false,
        "willRetry is not false",
    );
}

/// JSON-005 (P): `agent_settled` carries `aborted`; false after a finished run, true after an
/// abort. Invariant: none (plain documented behaviour).
#[test]
fn json_005() {
    let env = TestEnv::new();
    let (_out, recs, _s) = text_run(&env, "ok");
    let settled = of(&recs, "agent_settled");
    check(
        "JSON-005",
        settled.len() == 1,
        format!("{} agent_settled records", settled.len()),
    );
    documented("JSON-005", settled[0], "agent_settled");
    check(
        "JSON-005",
        settled[0]["aborted"] == false,
        "a finished run is not aborted:false",
    );

    let env2 = TestEnv::new();
    let server = start(vec![ScriptTurn {
        fault: Some(Fault::HoldHeaders { ms: 3000 }),
        ..text("held")
    }]);
    let out = run_paced(
        &env2,
        &server,
        &["--mode", "rpc"],
        vec![
            (0, r#"{"id":"p","type":"prompt","message":"go"}"#.to_string()),
            (300, r#"{"id":"a","type":"abort"}"#.to_string()),
        ],
    );
    let recs = parse_records(&out.stdout);
    let settled = of(&recs, "agent_settled");
    check("JSON-005", !settled.is_empty(), "no agent_settled after abort");
    check(
        "JSON-005",
        settled.last().map(|r| r["aborted"] == true).unwrap_or(false),
        "agent_settled after abort does not say aborted:true",
    );
}

/// JSON-006 (P): `turn_start` opens each assistant turn; a text reply is one turn.
#[test]
fn json_006() {
    let env = TestEnv::new();
    let (_out, recs, _s) = text_run(&env, "ok");
    let turns = of(&recs, "turn_start");
    check(
        "JSON-006",
        turns.len() == 1,
        format!("{} turn_start records", turns.len()),
    );
    documented("JSON-006", turns[0], "turn_start");
}

/// JSON-007 (P): `turn_end` carries the assistant `message` and the `toolResults` of the turn.
#[test]
fn json_007() {
    let env = TestEnv::new();
    let (_out, recs, _s) = tool_run(&env);
    let ends = of(&recs, "turn_end");
    check(
        "JSON-007",
        ends.len() == 2,
        format!("{} turn_end records, expected 2", ends.len()),
    );
    documented("JSON-007", ends[0], "turn_end");
    let results = ends[0]["toolResults"].as_array().expect("toolResults array");
    check(
        "JSON-007",
        results.len() == 1,
        format!("first turn has {} tool results", results.len()),
    );
    check(
        "JSON-007",
        results[0]["toolCallId"] == "call_1",
        "tool result is not call_1",
    );
    check(
        "JSON-007",
        ends[0]["message"]["role"] == "assistant",
        "turn_end message is not assistant",
    );
}

/// JSON-008 (P): `message_start` carries the `message`. The documented sequence opens every
/// message, the user prompt included.
#[test]
fn json_008() {
    let env = TestEnv::new();
    let (_out, recs, _s) = text_run(&env, "ok");
    let starts = of(&recs, "message_start");
    documented("JSON-008", starts[0], "message_start");
    check(
        "JSON-008",
        starts.iter().any(|r| r["message"]["role"] == "user"),
        "no message_start for the user prompt (pi's documented sequence opens it with message_start/message_end)",
    );
}

/// JSON-009 (P): `message_update` carries `usage` and `assistantMessageEvent` and is delta-only:
/// no cumulative `message` and no `partial` snapshot.
#[test]
fn json_009() {
    let env = TestEnv::new();
    let (_out, recs, _s) = text_run(&env, "Hello there.");
    let updates = of(&recs, "message_update");
    check("JSON-009", !updates.is_empty(), "no message_update records");
    for u in &updates {
        documented("JSON-009", u, "message_update");
        check(
            "JSON-009",
            u.get("message").is_none(),
            "message_update carries a cumulative message",
        );
        check(
            "JSON-009",
            u["assistantMessageEvent"].get("partial").is_none(),
            "assistantMessageEvent carries a partial snapshot",
        );
    }
}

/// JSON-010 (P): `assistantMessageEvent` types are `*_start|delta|end`; each block that starts
/// ends with its authoritative `*_end` for the same `contentIndex`. `toolcall_delta` carries the
/// raw argument bytes.
#[test]
fn json_010() {
    let env = TestEnv::new();
    env.write("work/note.txt", "hello world\n");
    let mut call = ScriptTurn::tool("call_1", "read", r#"{"path": "note.txt"}"#);
    call.reasoning = Some("look first".to_string());
    let server = start(vec![call, text("Done.")]);
    let (out, recs) = run_json(&env, &server, &["--mode", "json", "read note"], "");
    assert!(out.status.success());
    let doc = documented_doc();
    let allowed: Vec<&str> = doc["assistant_message_event_types"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    let events: Vec<&Value> = of(&recs, "message_update")
        .iter()
        .map(|u| &u["assistantMessageEvent"])
        .collect();
    for e in &events {
        let ty = e["type"].as_str().unwrap_or_default();
        check(
            "JSON-010",
            allowed.contains(&ty),
            format!("event type `{ty}` is not documented"),
        );
    }
    let raw: String = events
        .iter()
        .filter(|e| e["type"] == "toolcall_delta")
        .filter_map(|e| e["delta"].as_str())
        .collect();
    check(
        "JSON-010",
        raw == r#"{"path": "note.txt"}"#,
        format!("toolcall_delta is not the raw bytes: {raw:?}"),
    );
    let pairs = [
        ("thinking_start", "thinking_end"),
        ("text_start", "text_end"),
        ("toolcall_start", "toolcall_end"),
    ];
    for (start_ty, end_ty) in pairs {
        for s in events.iter().filter(|e| e["type"] == start_ty) {
            let idx = &s["contentIndex"];
            let ended = events
                .iter()
                .any(|e| e["type"] == end_ty && &e["contentIndex"] == idx);
            check(
                "JSON-010",
                ended,
                format!("{start_ty} at contentIndex {idx} has no {end_ty}; the authoritative end event is never emitted"),
            );
        }
    }
}

/// JSON-011 (P): `message_end` is authoritative: its text is the concatenation of the deltas.
#[test]
fn json_011() {
    let env = TestEnv::new();
    let (_out, recs, _s) = text_run(&env, "Hello there.");
    let deltas: String = of(&recs, "message_update")
        .iter()
        .filter(|u| u["assistantMessageEvent"]["type"] == "text_delta")
        .filter_map(|u| u["assistantMessageEvent"]["delta"].as_str())
        .collect();
    let end = of(&recs, "message_end")
        .into_iter()
        .find(|r| r["message"]["role"] == "assistant");
    let end = end.expect("assistant message_end");
    documented("JSON-011", end, "message_end");
    let text = end["message"]["content"][0]["text"].as_str().unwrap_or_default();
    check(
        "JSON-011",
        text == deltas,
        format!("message_end text {text:?} != deltas {deltas:?}"),
    );
}

/// JSON-012 (A): the message `usage` object has `input`, `output`, `cacheRead`, `cacheWrite`,
/// `totalTokens` and `cost`. Invariant: exact engine counts (`input = prompt - cached`,
/// `cacheRead = cached`, `output = completion`); `cacheWrite` and every `cost` field are 0.
#[test]
fn json_012() {
    let env = TestEnv::new();
    let (_out, recs, server) = text_run(&env, "Hello there.");
    let end = of(&recs, "message_end")
        .into_iter()
        .find(|r| r["message"]["role"] == "assistant")
        .unwrap();
    let u = &end["message"]["usage"];
    let want = server.requests().last().unwrap().usage.expect("engine usage");
    check(
        "JSON-012",
        u["input"] == want.prompt - want.cached,
        "input is not prompt - cached",
    );
    check(
        "JSON-012",
        u["cacheRead"] == want.cached,
        "cacheRead is not the cached count",
    );
    check(
        "JSON-012",
        u["output"] == want.completion,
        "output is not the completion count",
    );
    check("JSON-012", u["cacheWrite"] == 0, "cacheWrite is not 0");
    check(
        "JSON-012",
        u["totalTokens"] == want.prompt as u64 + want.completion as u64,
        "totalTokens is not prompt + completion",
    );
    for k in ["input", "output", "cacheRead", "cacheWrite", "total"] {
        check("JSON-012", u["cost"][k] == 0, format!("cost.{k} is not 0"));
    }
}

/// JSON-013 (P): `tool_execution_start` carries `toolCallId`, `toolName` and `args`.
#[test]
fn json_013() {
    let env = TestEnv::new();
    let (_out, recs, _s) = tool_run(&env);
    let starts = of(&recs, "tool_execution_start");
    check(
        "JSON-013",
        starts.len() == 1,
        format!("{} start records", starts.len()),
    );
    documented("JSON-013", starts[0], "tool_execution_start");
    check(
        "JSON-013",
        starts[0]["toolCallId"] == "call_1",
        "toolCallId is not call_1",
    );
    check(
        "JSON-013",
        starts[0]["args"]["path"] == "note.txt",
        "args are not the call's arguments",
    );
}

/// JSON-014 (P): `tool_execution_update` carries the latest `partialResult` while a tool runs.
#[test]
fn json_014() {
    let env = TestEnv::new();
    let server = start(vec![
        ScriptTurn::tool("b1", "bash", r#"{"command": "echo one; sleep 1; echo two"}"#),
        text("done"),
    ]);
    let (out, recs) = run_json(&env, &server, &["--mode", "json", "run"], "");
    assert!(out.status.success());
    let updates = of(&recs, "tool_execution_update");
    check(
        "JSON-014",
        !updates.is_empty(),
        "no tool_execution_update while bash ran",
    );
    for u in &updates {
        documented("JSON-014", u, "tool_execution_update");
        check("JSON-014", u["toolCallId"] == "b1", "update is for another call");
    }
    check(
        "JSON-014",
        updates[0]["partialResult"]["content"][0]["text"] == "one\n",
        "first partial result is not the first output line",
    );
}

/// JSON-015 (P): `tool_execution_end` carries `result`, `isError` and `durationMs`.
/// Invariant: W4, `durationMs` is an event field only; the next request to the engine does not
/// carry it.
#[test]
fn json_015() {
    let env = TestEnv::new();
    let (_out, recs, server) = tool_run(&env);
    let ends = of(&recs, "tool_execution_end");
    check("JSON-015", ends.len() == 1, format!("{} end records", ends.len()));
    documented("JSON-015", ends[0], "tool_execution_end");
    check(
        "JSON-015",
        ends[0]["durationMs"].as_u64().is_some(),
        "durationMs is not a non-negative integer",
    );
    let reqs = server.requests();
    check(
        "JSON-015",
        reqs.len() >= 2,
        "the tool result was not sent back to the engine",
    );
    let body = String::from_utf8_lossy(&reqs[1].body);
    check(
        "JSON-015",
        !body.contains("durationMs"),
        "durationMs reached the engine request (W4)",
    );
}

/// JSON-016 (P): `queue_update` carries the complete `steering` and `followUp` queues.
#[test]
fn json_016() {
    let env = TestEnv::new();
    let mut first = text("first");
    first.fault = Some(Fault::HoldHeaders { ms: 800 });
    let server = start(vec![first, text("second"), text("third")]);
    let out = run_paced(
        &env,
        &server,
        &["--mode", "rpc"],
        vec![
            (0, r#"{"id":"p","type":"prompt","message":"go"}"#.to_string()),
            (
                200,
                r#"{"id":"s","type":"steer","message":"also this"}"#.to_string(),
            ),
            (
                0,
                r#"{"id":"f","type":"follow_up","message":"then that"}"#.to_string(),
            ),
        ],
    );
    let recs = parse_records(&out.stdout);
    let updates = of(&recs, "queue_update");
    check("JSON-016", !updates.is_empty(), "no queue_update record");
    documented("JSON-016", updates[0], "queue_update");
    check(
        "JSON-016",
        updates
            .iter()
            .any(|u| u["followUp"] == serde_json::json!(["then that"])),
        "no queue_update lists the queued follow-up",
    );
    check(
        "JSON-016",
        updates
            .iter()
            .any(|u| u["steering"] == serde_json::json!(["also this"])),
        format!(
            "no queue_update lists the queued steer `also this` (queue_update records: {:?})",
            updates
                .iter()
                .map(|u| (&u["steering"], &u["followUp"]))
                .collect::<Vec<_>>()
        ),
    );
}

/// JSON-017 (P): `entry_appended` carries the persisted `entry`. pi emits it for an extension's
/// `pi.appendEntry()`; the message and usage entries of a run are not announced this way.
#[test]
fn json_017() {
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/extensions/note.js",
        "export default function (oxa) { oxa.on(\"turn_end\", () => { oxa.appendEntry(\"note\", {n: 1}); }); }\n",
    );
    let (_out, recs, _s) = text_run(&env, "ok");
    let appended = of(&recs, "entry_appended");
    check(
        "JSON-017",
        !appended.is_empty(),
        "no entry_appended record for the extension's appendEntry",
    );
    documented("JSON-017", appended[0], "entry_appended");
    check(
        "JSON-017",
        appended[0]["entry"]["type"] == "custom" && appended[0]["entry"]["customType"] == "note",
        format!("the record is not the appended custom entry: {}", appended[0]),
    );
    let kinds: Vec<&str> = appended
        .iter()
        .filter_map(|r| r["entry"]["type"].as_str())
        .collect();
    check(
        "JSON-017",
        kinds.iter().all(|k| *k == "custom"),
        format!("entry_appended is also emitted for other entries (kinds {kinds:?}); pi documents it for extension entries only"),
    );
}

/// JSON-018 (P): `session_info_changed` carries the new `name`.
#[test]
fn json_018() {
    let env = TestEnv::new();
    let server = start(vec![]);
    let (out, recs) = run_json(
        &env,
        &server,
        &["--mode", "rpc"],
        "{\"id\":\"n\",\"type\":\"set_session_name\",\"name\":\"demo\"}\n",
    );
    assert!(out.status.success());
    let changed = of(&recs, "session_info_changed");
    check(
        "JSON-018",
        changed.len() == 1,
        format!("{} session_info_changed records", changed.len()),
    );
    documented("JSON-018", changed[0], "session_info_changed");
    check("JSON-018", changed[0]["name"] == "demo", "name is not demo");
}

/// JSON-019 (P): `thinking_level_changed` carries the new `level`.
#[test]
fn json_019() {
    let env = TestEnv::new();
    let server = start(vec![]);
    let (out, recs) = run_json(
        &env,
        &server,
        &["--mode", "rpc"],
        "{\"id\":\"t\",\"type\":\"set_thinking_level\",\"level\":\"high\"}\n",
    );
    assert!(out.status.success());
    let changed = of(&recs, "thinking_level_changed");
    check(
        "JSON-019",
        changed.len() == 1,
        format!("{} thinking_level_changed records", changed.len()),
    );
    documented("JSON-019", changed[0], "thinking_level_changed");
    check("JSON-019", changed[0]["level"] == "high", "level is not high");
}

/// JSON-020 (P): `compaction_start` carries `reason`, one of `manual`, `threshold`, `overflow`.
#[test]
fn json_020() {
    let env = TestEnv::new();
    let server = seeded_server(vec![text("## Summary\nso far")]);
    seed(&env, &server);
    let (out, recs) = compact_rpc(&env, &server);
    assert!(out.status.success());
    let starts = of(&recs, "compaction_start");
    check(
        "JSON-020",
        starts.len() == 1,
        format!("{} compaction_start records", starts.len()),
    );
    documented("JSON-020", starts[0], "compaction_start");
    let reason = starts[0]["reason"].as_str().unwrap_or_default();
    check(
        "JSON-020",
        ["manual", "threshold", "overflow"].contains(&reason),
        format!("reason `{reason}` is not documented"),
    );
}

/// JSON-021 (P): `compaction_end` carries `result` (summary, firstKeptEntryId, tokensBefore,
/// estimatedTokensAfter, usage, details), `aborted` false and `willRetry` false.
#[test]
fn json_021() {
    let env = TestEnv::new();
    let server = seeded_server(vec![text("## Summary\nso far")]);
    seed(&env, &server);
    let (_out, recs) = compact_rpc(&env, &server);
    let ends = of(&recs, "compaction_end");
    check(
        "JSON-021",
        ends.len() == 1,
        format!("{} compaction_end records", ends.len()),
    );
    documented("JSON-021", ends[0], "compaction_end");
    let r = &ends[0]["result"];
    for k in [
        "summary",
        "firstKeptEntryId",
        "tokensBefore",
        "estimatedTokensAfter",
        "usage",
        "details",
    ] {
        check("JSON-021", r.get(k).is_some(), format!("result lacks `{k}`"));
    }
    check("JSON-021", ends[0]["aborted"] == false, "aborted is not false");
    check(
        "JSON-021",
        ends[0]["willRetry"] == false,
        "willRetry is not false",
    );
}

/// JSON-022 (P): a retryable failure emits `auto_retry_start` (attempt, maxAttempts, delayMs,
/// errorMessage) and `auto_retry_end` (success); when every attempt fails the end has
/// `success: false` and `finalError`.
#[test]
fn json_022() {
    let env = TestEnv::new();
    let server = start(vec![overloaded(), text("after the retry")]);
    let (out, recs) = run_json(&env, &server, &["--mode", "json", "hi"], "");
    assert!(out.status.success());
    let starts = of(&recs, "auto_retry_start");
    check(
        "JSON-022",
        starts.len() == 1,
        format!("{} auto_retry_start records", starts.len()),
    );
    documented("JSON-022", starts[0], "auto_retry_start");
    let ends = of(&recs, "auto_retry_end");
    check(
        "JSON-022",
        ends.last().map(|e| e["success"] == true).unwrap_or(false),
        "retry did not end with success",
    );
    documented("JSON-022", ends[0], "auto_retry_end");

    let env2 = TestEnv::new();
    let server2 = start(vec![
        overloaded(),
        overloaded(),
        overloaded(),
        overloaded(),
        overloaded(),
    ]);
    let (out2, recs2) = run_json(&env2, &server2, &["--mode", "json", "hi"], "");
    let _ = out2;
    let ends2 = of(&recs2, "auto_retry_end");
    check(
        "JSON-022",
        !ends2.is_empty(),
        "no auto_retry_end after the final failure",
    );
    let last = ends2.last().unwrap();
    check(
        "JSON-022",
        last["success"] == false,
        "final failure is not success:false",
    );
    check(
        "JSON-022",
        last["finalError"].as_str().is_some(),
        "final failure has no finalError",
    );
}

/// JSON-023 (P): a failed summary is retried with `summarization_retry_scheduled` (attempt,
/// maxAttempts, delayMs, errorMessage) before the attempt and the finish events.
#[test]
fn json_023() {
    let env = TestEnv::new();
    let server = seeded_server(vec![overloaded(), text("## Summary\nafter the retry")]);
    seed(&env, &server);
    let (_out, recs) = compact_rpc(&env, &server);
    let scheduled = of(&recs, "summarization_retry_scheduled");
    check(
        "JSON-023",
        !scheduled.is_empty(),
        "a summary that fails with HTTP 503 is not retried: no summarization_retry_scheduled record",
    );
    documented("JSON-023", scheduled[0], "summarization_retry_scheduled");
}

/// JSON-024 (P, RPC only): a direct `bash` command streams `bash_execution_update` records with
/// its command id and each output chunk.
#[test]
fn json_024() {
    let env = TestEnv::new();
    let server = start(vec![]);
    let (out, recs) = run_json(
        &env,
        &server,
        &["--mode", "rpc"],
        "{\"id\":\"b\",\"type\":\"bash\",\"command\":\"echo one; sleep 1; echo two\"}\n",
    );
    assert!(out.status.success());
    let updates = of(&recs, "bash_execution_update");
    check(
        "JSON-024",
        updates.len() >= 2,
        format!("{} bash_execution_update records", updates.len()),
    );
    for u in &updates {
        documented("JSON-024", u, "bash_execution_update");
        check("JSON-024", u["id"] == "b", "update does not carry the command id");
    }
    let all: String = updates.iter().filter_map(|u| u["delta"].as_str()).collect();
    check("JSON-024", all == "one\ntwo\n", format!("chunks {all:?}"));
}

/// JSON-025 (P, RPC only): an extension handler that throws is reported as `extension_error`
/// (extensionPath, event, error).
#[test]
fn json_025() {
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/extensions/boom.js",
        "export default function (oxa) { oxa.on(\"tool_call\", () => { throw new Error(\"boom\"); }); }\n",
    );
    env.write("work/note.txt", "hello world\n");
    let server = start(vec![
        ScriptTurn::tool("call_1", "read", r#"{"path": "note.txt"}"#),
        text("Done."),
    ]);
    let (out, recs) = run_json(
        &env,
        &server,
        &["--mode", "rpc"],
        "{\"id\":\"p\",\"type\":\"prompt\",\"message\":\"read note\"}\n",
    );
    assert!(out.status.success());
    let errors = of(&recs, "extension_error");
    check(
        "JSON-025",
        !errors.is_empty(),
        "a throwing tool_call handler produced no extension_error record (oxa reports it as an extension_ui_request notify)",
    );
    documented("JSON-025", errors[0], "extension_error");
    check(
        "JSON-025",
        errors[0]["event"] == "tool_call",
        "event is not tool_call",
    );
}

/// JSON-026 (A): oxa's own records (`prefill`, `prefix_epoch`, `cache_miss`, `mode_changed`,
/// `notice`) are not pi event names. Invariant: a pi client ignores unknown types, so every record
/// type oxa writes in JSON and RPC is either a documented pi type or one of these oxa types.
#[test]
fn json_026() {
    let env = TestEnv::new();
    let server = seeded_server(vec![text("## Summary\nso far")]);
    seed(&env, &server);
    let (_out, mut recs) = compact_rpc(&env, &server);
    let (out2, recs2) = run_json(
        &env,
        &server,
        &["-c", "--mode", "rpc"],
        "{\"id\":\"m\",\"type\":\"set_mode\",\"mode\":\"plan\"}\n",
    );
    assert!(out2.status.success());
    recs.extend(recs2);

    let doc = documented_doc();
    let pi: Vec<&str> = doc["events"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    let oxa_only = [
        "prefill",
        "engine_queue",
        "prefix_epoch",
        "cache_miss",
        "mode_changed",
        "notice",
        "model_select",
    ];
    let rpc_frames = ["response", "extension_ui_request"];
    let prefill = of(&recs, "prefill");
    check(
        "JSON-026",
        !prefill.is_empty(),
        "compaction emitted no prefill record",
    );
    check(
        "JSON-026",
        prefill[0].get("done").is_some() && prefill[0].get("total").is_some(),
        "prefill lacks done/total",
    );
    let mode = of(&recs, "mode_changed");
    check(
        "JSON-026",
        mode.len() == 1 && mode[0]["mode"] == "plan",
        "set_mode did not emit mode_changed:plan",
    );
    for r in &recs {
        let ty = r["type"].as_str().unwrap_or_default();
        check(
            "JSON-026",
            pi.contains(&ty) || oxa_only.contains(&ty) || rpc_frames.contains(&ty),
            format!("record type `{ty}` is neither documented by pi nor an oxa type"),
        );
        check(
            "JSON-026",
            !(oxa_only.contains(&ty) && pi.contains(&ty)),
            format!("oxa type `{ty}` collides with a pi event name"),
        );
    }
}

/// JSON-027 (P): print mode writes only the final assistant text, and no records.
#[test]
fn json_027() {
    let env = TestEnv::new();
    env.write("work/note.txt", "hello world\n");
    let server = start(vec![
        ScriptTurn::tool("call_1", "read", r#"{"path": "note.txt"}"#),
        text("Done."),
    ]);
    let out = run_oxa(&env, &server.url(), &["-p", "read note"], "");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    check(
        "JSON-027",
        stdout == "Done.\n",
        format!("stdout is {stdout:?}, expected only the final text"),
    );
}

/// JSON-028 (A): in JSON mode a permission Ask is a Deny: the tool result is an error and the
/// file is not written. Invariant: no dialog exists in JSON mode, so Ask resolves to Deny
/// (print/json: Ask -> Deny) and the policy is never bypassed.
#[test]
fn json_028() {
    let env = TestEnv::new();
    let server = start(vec![
        ScriptTurn::tool("w1", "write", r#"{"path": "denied.txt", "content": "x"}"#),
        text("after deny"),
    ]);
    let (out, recs) = run_json(
        &env,
        &server,
        &["--mode", "json", "--permission-mode", "ask", "write it"],
        "",
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let ends = of(&recs, "tool_execution_end");
    check(
        "JSON-028",
        ends.len() == 1,
        format!("{} tool_execution_end records", ends.len()),
    );
    check(
        "JSON-028",
        ends[0]["isError"] == true,
        "the write was not refused in ask mode",
    );
    check(
        "JSON-028",
        !env.work().join("denied.txt").exists(),
        "the refused write still created the file",
    );
}
