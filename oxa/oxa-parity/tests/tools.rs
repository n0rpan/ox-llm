//! G1 PARITY tests (tools). Owner: task G1.
//! One test per PARITY.md row of this area, named after the row id (`CLI-012` -> `fn cli_012()`).
//! The four tests below are the meta-checks of the task; the row tests are added by the task.
//! Helpers: `oxa_parity::{oxa_bin, fake_bin, run_oxa, fixture}`, `oxa_fake::TestEnv`.

use oxa_fake::{FakeOpts, FakeServer, Recorded, ScriptTurn, TestEnv};
use oxa_parity::run_oxa;
use oxa_proto::{Role, ToolCall};
use std::process::Output;

/// This file, for the meta-checks that read the test bodies.
const SELF_SRC: &str = include_str!("tools.rs");

struct Ran {
    out: Output,
    reqs: Vec<Recorded>,
}

/// Drive `oxa -p go` once with the scripted fake engine; returns the output and every request.
fn drive(env: &TestEnv, turns: Vec<ScriptTurn>, args: &[&str]) -> Ran {
    let srv = FakeServer::start(turns, FakeOpts::default()).expect("start fake engine");
    let mut argv: Vec<&str> = vec!["-p"];
    argv.extend_from_slice(args);
    argv.push("go");
    let out = run_oxa(env, &srv.url(), &argv, "");
    Ran {
        out,
        reqs: srv.requests(),
    }
}

fn stderr_of(ran: &Ran) -> String {
    String::from_utf8_lossy(&ran.out.stderr).into_owned()
}

/// The content of the tool message answering `call_id` in the last request.
fn tool_text(ran: &Ran, call_id: &str) -> String {
    let last = ran
        .reqs
        .last()
        .unwrap_or_else(|| panic!("no request made; stderr: {}", stderr_of(ran)));
    last.messages
        .iter()
        .find(|m| m.role == Role::Tool && m.tool_call_id.as_deref() == Some(call_id))
        .and_then(|m| m.content.clone())
        .unwrap_or_else(|| panic!("no tool result for {call_id}; stderr: {}", stderr_of(ran)))
}

/// One tool call through the loop; returns the tool result text.
fn run_one(env: &TestEnv, name: &str, args: &str, extra: &[&str]) -> String {
    let ran = drive(
        env,
        vec![ScriptTurn::tool("c1", name, args), ScriptTurn::text("ok")],
        extra,
    );
    tool_text(&ran, "c1")
}

/// Several tool calls in one reply (`(id, name, args)`); returns the result texts in call order.
fn run_many(env: &TestEnv, calls: &[(&str, &str, &str)]) -> Vec<String> {
    let mut turn = ScriptTurn::tool(calls[0].0, calls[0].1, calls[0].2);
    turn.tool_calls = calls
        .iter()
        .map(|(id, name, args)| ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            args_raw: args.to_string(),
        })
        .collect();
    let ran = drive(env, vec![turn, ScriptTurn::text("ok")], &[]);
    calls.iter().map(|(id, _, _)| tool_text(&ran, id)).collect()
}

/// The `tools` array the engine received on the first request, as a list of tool objects.
fn declared_tools(ran: &Ran) -> Vec<serde_json::Value> {
    let first = ran
        .reqs
        .first()
        .unwrap_or_else(|| panic!("no request; stderr: {}", stderr_of(ran)));
    if first.tools_json.trim().is_empty() {
        return Vec::new();
    }
    serde_json::from_str::<Vec<serde_json::Value>>(&first.tools_json).expect("tools json")
}

fn declared_names(ran: &Ran) -> Vec<String> {
    let mut names: Vec<String> = declared_tools(ran)
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap_or("").to_string())
        .collect();
    names.sort();
    names
}

/// The JSON schema of one declared tool.
fn schema_of(ran: &Ran, name: &str) -> serde_json::Value {
    declared_tools(ran)
        .into_iter()
        .find(|t| t["function"]["name"] == name)
        .unwrap_or_else(|| panic!("tool {name} not declared"))["function"]["parameters"]
        .clone()
}

const ALL_SEVEN: [&str; 7] = ["bash", "edit", "find", "grep", "ls", "read", "write"];

/// Tool selection without a turn: the names the engine was offered.
fn offered(env: &TestEnv, extra: &[&str]) -> Vec<String> {
    let ran = drive(env, vec![ScriptTurn::text("ok")], extra);
    declared_names(&ran)
}

// ---------------------------------------------------------------- meta-checks

/// each PARITY.md id of this area maps to a test fn or an O-row assertion
#[test]
fn every_row_has_test_or_recorded_omission() {
    let parity = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("PARITY.md"),
    )
    .expect("PARITY.md");
    let mut checked = 0;
    for line in parity.lines() {
        let cols: Vec<&str> = line.split('|').map(str::trim).collect();
        if cols.len() < 6 || !cols[1].starts_with("TOOLS-") {
            continue;
        }
        let id = cols[1];
        let test = cols[5].trim_matches('`');
        assert!(
            test == "omission" || test == format!("tools.rs::{}", id.to_lowercase().replace('-', "_")),
            "{id}: test column `{test}` does not name this file"
        );
        if test == "omission" {
            continue;
        }
        let fn_name = id.to_lowercase().replace('-', "_");
        assert!(
            SELF_SRC.contains(&format!("fn {fn_name}()")),
            "{id}: no test fn {fn_name} in tests/tools.rs"
        );
        checked += 1;
    }
    assert_eq!(checked, 24, "TOOLS rows of PARITY.md");
}

/// documented output or recorded fixture
#[test]
fn documented_rows_have_tests() {
    // Every P row's test body cites pi (a doc page, pi's source text, or a recorded fixture).
    let parity = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("PARITY.md"),
    )
    .expect("PARITY.md");
    for line in parity.lines() {
        let cols: Vec<&str> = line.split('|').map(str::trim).collect();
        if cols.len() < 6 || !cols[1].starts_with("TOOLS-") || cols[3] != "P" {
            continue;
        }
        let fn_name = cols[1].to_lowercase().replace('-', "_");
        let body = fn_body(&fn_name);
        assert!(
            body.contains("pi:") || body.contains("PI_TEXT") || body.contains("fixture"),
            "{} is P but its test does not cite pi's text or a fixture",
            cols[1]
        );
    }
}

/// comment + adapted behaviour asserted
#[test]
fn a_rows_name_their_invariant() {
    let parity = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("PARITY.md"),
    )
    .expect("PARITY.md");
    for line in parity.lines() {
        let cols: Vec<&str> = line.split('|').map(str::trim).collect();
        if cols.len() < 6 || !cols[1].starts_with("TOOLS-") || cols[3] != "A" {
            continue;
        }
        let fn_name = cols[1].to_lowercase().replace('-', "_");
        let body = fn_body(&fn_name);
        assert!(
            body.contains("INVARIANT:"),
            "{} is A but its test does not name the invariant (a `// INVARIANT:` comment)",
            cols[1]
        );
    }
}

/// The source text of one test fn of this file (from its `fn` line to the next top-level item).
fn fn_body(fn_name: &str) -> String {
    let fn_at = SELF_SRC
        .find(&format!("fn {fn_name}()"))
        .unwrap_or_else(|| panic!("no fn {fn_name}"));
    // From the end of the previous item (so the doc comment above `#[test]` is included) to the
    // closing brace of this fn.
    let start = SELF_SRC[..fn_at].rfind("\n}\n").map(|i| i + 3).unwrap_or(0);
    let end = SELF_SRC[fn_at..]
        .find("\n}\n")
        .map(|i| fn_at + i + 3)
        .unwrap_or(SELF_SRC.len());
    SELF_SRC[start..end].to_string()
}

// ---------------------------------------------------------------- TOOLS rows

/// TOOLS-001 (P): `read` schema, paging notice, offset beyond end error.
/// pi: `packages/coding-agent/src/core/tools/read.ts` (offset check, `[N more lines in file. Use offset=K to continue.]`).
#[test]
fn tools_001() {
    let env = TestEnv::new();
    env.write("work/a.txt", "one\ntwo\nthree\nfour\nfive");
    // Schema: path required; offset and limit numeric.
    let ran = drive(&env, vec![ScriptTurn::text("ok")], &[]);
    let s = schema_of(&ran, "read");
    assert_eq!(
        s["required"],
        serde_json::json!(["path"]),
        "pi: read requires path"
    );
    for p in ["path", "offset", "limit"] {
        assert!(s["properties"].get(p).is_some(), "pi: read has property {p}");
    }
    // OXA-LOCAL-FIRST T3: integer parameters, never numbers.
    assert_eq!(s["properties"]["offset"]["type"], "integer");
    assert_eq!(s["properties"]["limit"]["type"], "integer");
    // Paging: offset is 1-indexed; the user limit stops early and names the next offset.
    let page = run_one(&env, "read", r#"{"path":"a.txt","offset":2,"limit":2}"#, &[]);
    assert_eq!(
        page, "two\nthree\n\n[2 more lines in file. Use offset=4 to continue.]",
        "pi: paging notice"
    );
    // Offset beyond the end: pi's error text.
    let beyond = run_one(&env, "read", r#"{"path":"a.txt","offset":6}"#, &[]);
    assert_eq!(
        beyond, "Offset 6 is beyond end of file (5 lines total)",
        "pi: offset error text"
    );
}

/// TOOLS-002 (A): `read` output limited to 2000 lines or the W4 byte cap.
/// INVARIANT: W4 deterministic capped tool output (2000 lines or the 16 KiB cap; pi's 50 KB is oxa's cap).
#[test]
fn tools_002() {
    let env = TestEnv::new();
    let many: String = (1..=2500).map(|i| format!("{}\n", i % 10)).collect();
    env.write("work/many.txt", &many);
    // Short lines, so the 2000-line limit (not the byte cap) is what stops the page.
    let lines = run_one(&env, "read", r#"{"path":"many.txt"}"#, &[]);

    // O-167: a read without offset or limit gets the first page (400 lines); the notice
    // says how to continue. Any read with an offset is pi's read: the 2000-line limit.
    let (page, notice) = lines.split_once("\n\n").expect("page, blank line, notice");
    assert_eq!(
        notice,
        "[Showing lines 1-400 of 2501. Use offset=401 to continue. Or use grep to find the part you need.]"
    );
    let want: String = (1..=400).map(|i| format!("{}\n", i % 10)).collect();
    assert_eq!(format!("{page}\n"), want, "local: lines 1-400, in order");
    let whole = run_one(&env, "read", r#"{"path":"many.txt","offset":1}"#, &[]);
    assert!(
        whole.contains("[Showing lines 1-2000 of 2501. Use offset=2001 to continue.]"),
        "{}",
        &whole[whole.len().saturating_sub(200)..]
    );
    let next = run_one(&env, "read", r#"{"path":"many.txt","offset":401}"#, &[]);
    let want: String = (401..=2400).map(|i| format!("{}\n", i % 10)).collect();
    assert_eq!(
        next,
        format!("{want}\n[Showing lines 401-2400 of 2501. Use offset=2401 to continue.]"),
        "local: the continuation starts at line 401, no gap"
    );

    // Byte cap: two 9 KB lines do not fit in 16 KiB, so only the first is shown.
    let wide = format!("{}\n{}\n{}", "a".repeat(9000), "b".repeat(9000), "c");
    env.write("work/wide.txt", &wide);
    let capped = run_one(&env, "read", r#"{"path":"wide.txt"}"#, &[]);
    assert!(
        capped.contains("(16.0KB limit). Use offset=2 to continue.]"),
        "{}",
        &capped[capped.len().saturating_sub(120)..]
    );
    assert!(
        capped.len() <= 16 * 1024 + 200,
        "capped to the W4 cap, got {} bytes",
        capped.len()
    );
}

/// TOOLS-003 (A): `read` on an image returns the fixed placeholder line.
/// INVARIANT: the engine is text-only (O-144/W-image): an image is one fixed wire line.
#[test]
fn tools_003() {
    let env = TestEnv::new();
    // A 1x1 PNG.
    let png: [u8; 33] = [
        0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0x0d, b'I', b'H', b'D', b'R', 0, 0, 0, 1, 0,
        0, 0, 1, 8, 6, 0, 0, 0, 0x1f, 0x15, 0xc4, 0x89,
    ];
    std::fs::write(env.work().join("pic.png"), png).expect("write png");
    let text = run_one(&env, "read", r#"{"path":"pic.png"}"#, &[]);
    assert_eq!(text, "[image pic.png 1x1: not visible to this model]");
}

/// TOOLS-004 (A): `read` on a binary file is refused with a fixed error.
/// INVARIANT: text-only engine; a NUL byte in the first 8 KiB is binary (oxa addition, pi has no refusal).
#[test]
fn tools_004() {
    let env = TestEnv::new();
    std::fs::write(env.work().join("blob.bin"), [0x50u8, 0x00, 0x51, 0x52]).expect("write blob");
    let text = run_one(&env, "read", r#"{"path":"blob.bin"}"#, &[]);
    assert_eq!(text, "Cannot read binary file: blob.bin");
}

/// TOOLS-005 (P): `write` creates parent directories, overwrites, and reports success.
/// pi: `packages/coding-agent/src/core/tools/write.ts` ("Successfully wrote to <path>").
#[test]
fn tools_005() {
    let env = TestEnv::new();
    let text = run_one(
        &env,
        "write",
        r#"{"path":"deep/er/new.txt","content":"hi\n"}"#,
        &[],
    );
    assert_eq!(
        text, "Successfully wrote to deep/er/new.txt",
        "pi: write success text"
    );
    assert_eq!(
        std::fs::read_to_string(env.work().join("deep/er/new.txt")).unwrap(),
        "hi\n"
    );
    // Overwrite.
    let again = run_one(
        &env,
        "write",
        r#"{"path":"deep/er/new.txt","content":"bye"}"#,
        &[],
    );
    assert_eq!(again, "Successfully wrote to deep/er/new.txt");
    assert_eq!(
        std::fs::read_to_string(env.work().join("deep/er/new.txt")).unwrap(),
        "bye"
    );
    // Atomic: no temporary file is left next to the target.
    let names: Vec<String> = std::fs::read_dir(env.work().join("deep/er"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        vec!["new.txt".to_string()],
        "atomic write leaves no temp file"
    );
}

/// TOOLS-006 (P): `edit` with `edits[]` is matched against the original file.
/// pi: `packages/coding-agent/src/core/tools/edit.ts` ("Successfully replaced N block(s) in <path>.").
#[test]
fn tools_006() {
    let env = TestEnv::new();
    env.write("work/a.txt", "alpha\nbeta\ngamma\n");
    let args = r#"{"path":"a.txt","edits":[{"oldText":"alpha","newText":"beta2"},{"oldText":"beta","newText":"X"}]}"#;
    let text = run_one(&env, "edit", args, &[]);
    assert_eq!(
        text, "Successfully replaced 2 block(s) in a.txt.",
        "pi: edit success text"
    );
    // Matched against the original: the second oldText "beta" is unique in the original file.
    assert_eq!(
        std::fs::read_to_string(env.work().join("a.txt")).unwrap(),
        "beta2\nX\ngamma\n"
    );
    // O-167: the declared form is the flat one; the capability, several
    // replacements in one file from one reply, exists under both.
    let s = schema_of(&drive(&env, vec![ScriptTurn::text("ok")], &[]), "edit");

    // T1: three strings, no array; several edits = several calls in one reply, in order.
    assert_eq!(s["required"], serde_json::json!(["path", "oldText", "newText"]));
    assert!(
        s["properties"].get("edits").is_none(),
        "local declares no edits[]"
    );
    env.write("work/b.txt", "alpha\nbeta\ngamma\n");
    let texts = run_many(
        &env,
        &[
            (
                "c1",
                "edit",
                r#"{"path":"b.txt","oldText":"alpha","newText":"A"}"#,
            ),
            (
                "c2",
                "edit",
                r#"{"path":"b.txt","oldText":"gamma","newText":"G"}"#,
            ),
        ],
    );
    assert_eq!(texts, vec!["Successfully replaced 1 block(s) in b.txt."; 2]);
    assert_eq!(
        std::fs::read_to_string(env.work().join("b.txt")).unwrap(),
        "A\nbeta\nG\n"
    );
}

/// TOOLS-007 (P): edit error texts (not found, duplicate, empty, overlap, no change).
/// pi: `packages/coding-agent/src/core/tools/edit-diff.ts`, exact texts.
#[test]
fn tools_007() {
    let env = TestEnv::new();
    env.write("work/a.txt", "abc\ndef\nghi\nx x\n");
    let not_found = run_one(
        &env,
        "edit",
        r#"{"path":"a.txt","edits":[{"oldText":"zzz","newText":"q"}]}"#,
        &[],
    );
    assert_eq!(
        not_found,
        "Could not find the exact text in a.txt. The old text must match exactly including all whitespace and newlines.",
        "pi: single-edit not found"
    );
    let not_found2 = run_one(
        &env,
        "edit",
        r#"{"path":"a.txt","edits":[{"oldText":"abc","newText":"A"},{"oldText":"zzz","newText":"q"}]}"#,
        &[],
    );
    assert_eq!(
        not_found2,
        "Could not find edits[1] in a.txt. The oldText must match exactly including all whitespace and newlines.",
        "pi: indexed not found"
    );
    let dup = run_one(
        &env,
        "edit",
        r#"{"path":"a.txt","edits":[{"oldText":"x","newText":"y"}]}"#,
        &[],
    );
    assert_eq!(
        dup,
        "Found 2 occurrences of the text in a.txt. The text must be unique. Please provide more context to make it unique.",
        "pi: duplicate"
    );
    let empty = run_one(
        &env,
        "edit",
        r#"{"path":"a.txt","edits":[{"oldText":"","newText":"y"}]}"#,
        &[],
    );
    assert_eq!(empty, "oldText must not be empty in a.txt.", "pi: empty oldText");
    let overlap = run_one(
        &env,
        "edit",
        r#"{"path":"a.txt","edits":[{"oldText":"abc\ndef","newText":"1"},{"oldText":"def\nghi","newText":"2"}]}"#,
        &[],
    );
    assert_eq!(
        overlap,
        "edits[0] and edits[1] overlap in a.txt. Merge them into one edit or target disjoint regions.",
        "pi: overlap"
    );
    let same = run_one(
        &env,
        "edit",
        r#"{"path":"a.txt","edits":[{"oldText":"abc","newText":"abc"}]}"#,
        &[],
    );
    assert_eq!(
        same,
        "No changes made to a.txt. The replacement produced identical content. This might indicate an issue with special characters or the text not existing as expected.",
        "pi: no change"
    );
    // Errors leave the file as it was.
    assert_eq!(
        std::fs::read_to_string(env.work().join("a.txt")).unwrap(),
        "abc\ndef\nghi\nx x\n"
    );

    // O-167: the declared flat form gives pi's single-edit texts per call. Overlap has no
    // flat form: each call matches the file as the previous call left it, so a call that
    // targets text an earlier call replaced is not found, and the earlier call stays.
    let flat = |old: &str, new: &str| {
        serde_json::json!({"path": "a.txt", "oldText": old, "newText": new}).to_string()
    };
    assert_eq!(run_one(&env, "edit", &flat("zzz", "q"), &[]), not_found);
    assert_eq!(run_one(&env, "edit", &flat("x", "y"), &[]), dup);
    assert_eq!(run_one(&env, "edit", &flat("", "y"), &[]), empty);
    assert_eq!(run_one(&env, "edit", &flat("abc", "abc"), &[]), same);
    let (a, b) = (flat("abc\ndef", "1"), flat("def\nghi", "2"));
    let texts = run_many(&env, &[("c1", "edit", &a), ("c2", "edit", &b)]);
    assert_eq!(texts[0], "Successfully replaced 1 block(s) in a.txt.");
    // T7: when a line of the file matches, local's not-found text also shows the closest
    // region of the file (the meaning stays: not found, copy oldText from the file).
    assert_eq!(
        texts[1],
        "Could not find oldText in a.txt. The closest text in the file is lines 1-2:\n1\nghi\nCopy oldText exactly from the file."
    );
    assert_eq!(
        std::fs::read_to_string(env.work().join("a.txt")).unwrap(),
        "1\nghi\nx x\n"
    );
}

/// TOOLS-008 (P): BOM, CRLF and mixed line endings are preserved; fuzzy match fallback.
/// pi: `packages/coding-agent/src/core/tools/edit-diff.ts` (`normalizeForFuzzyMatch`, BOM and line-ending restore).
#[test]
fn tools_008() {
    let env = TestEnv::new();
    env.write("work/bom.txt", "\u{feff}one\r\ntwo\r\nthree\r\n");
    let r = run_one(
        &env,
        "edit",
        r#"{"path":"bom.txt","edits":[{"oldText":"two\nthree","newText":"TWO\nTHREE"}]}"#,
        &[],
    );
    assert_eq!(r, "Successfully replaced 1 block(s) in bom.txt.");
    assert_eq!(
        std::fs::read(env.work().join("bom.txt")).unwrap(),
        "\u{feff}one\r\nTWO\r\nTHREE\r\n".as_bytes().to_vec(),
        "pi: BOM and CRLF kept"
    );
    // Fuzzy: smart quotes and trailing spaces in the file still match a plain oldText.
    env.write("work/fz.txt", "say \u{201c}hi\u{201d}   \nend\n");
    let f = run_one(
        &env,
        "edit",
        r#"{"path":"fz.txt","edits":[{"oldText":"say \"hi\"\nend","newText":"ok"}]}"#,
        &[],
    );
    assert_eq!(
        f, "Successfully replaced 1 block(s) in fz.txt.",
        "pi: fuzzy match fallback"
    );
    assert_eq!(
        std::fs::read_to_string(env.work().join("fz.txt")).unwrap(),
        "ok\n"
    );
}

/// TOOLS-009 (P): the legacy single `oldText`/`newText` input is accepted.
#[test]
fn tools_009() {
    let env = TestEnv::new();
    env.write("work/a.txt", "alpha\n");
    let r = run_one(
        &env,
        "edit",
        r#"{"path":"a.txt","oldText":"alpha","newText":"omega"}"#,
        &[],
    );
    assert_eq!(
        r, "Successfully replaced 1 block(s) in a.txt.",
        "pi: legacy input"
    );
    assert_eq!(
        std::fs::read_to_string(env.work().join("a.txt")).unwrap(),
        "omega\n"
    );
}

/// TOOLS-010 (P): `bash` runs the command; stdout and stderr are combined; the status line ends the output.
/// pi: `packages/coding-agent/src/core/tools/bash.ts` (`Command exited with code N`).
#[test]
fn tools_010() {
    let env = TestEnv::new();
    let s = schema_of(&drive(&env, vec![ScriptTurn::text("ok")], &[]), "bash");
    assert_eq!(
        s["required"],
        serde_json::json!(["command"]),
        "pi: bash requires command"
    );
    assert!(s["properties"].get("timeout").is_some(), "pi: bash has timeout");
    let r = run_one(
        &env,
        "bash",
        r#"{"command":"echo out; echo err 1>&2; exit 3"}"#,
        &[],
    );
    assert!(
        r.contains("out\n") && r.contains("err\n"),
        "combined stdout and stderr: {r:?}"
    );
    assert!(
        r.ends_with("\n\nCommand exited with code 3"),
        "pi: status line: {r:?}"
    );
    // pi: a zero exit adds no status line (`bash.ts`: only `exitCode !== 0` appends it).
    let ok = run_one(&env, "bash", r#"{"command":"printf hi"}"#, &[]);
    assert_eq!(ok, "hi", "pi: no status line on exit 0");
}

/// TOOLS-011 (A): bash output over the cap keeps head and tail and names a spill file.
/// INVARIANT: W4 deterministic capped output: head + tail + one fixed line naming the spill file.
#[test]
fn tools_011() {
    let env = TestEnv::new();
    let r = run_one(&env, "bash", r#"{"command":"seq 1 20000"}"#, &[]);
    assert!(r.len() <= 16 * 1024 + 512, "capped, got {} bytes", r.len());
    assert!(r.starts_with("1\n2\n"), "head kept");
    assert!(r.contains("\n20000"), "tail kept");
    assert!(
        r.contains("spill") || r.contains("full output"),
        "names the spill file: {}",
        &r[r.len().saturating_sub(300)..]
    );
}

/// TOOLS-012 (P): a timeout kills the process tree, including background writers.
/// pi: `packages/coding-agent/src/core/tools/bash.ts` ("Command timed out after N seconds").
#[test]
fn tools_012() {
    // INVARIANT: W4: pi's text without the `after S seconds` clause (no timings in model-facing text)
    let env = TestEnv::new();
    let started = std::time::Instant::now();
    let r = run_one(&env, "bash", r#"{"command":"sleep 30","timeout":1}"#, &[]);
    assert_eq!(r, "Command timed out after 1 seconds", "pi: timeout text");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(20),
        "timeout kills the command"
    );
    // Background writer: the child writes a marker after 2 s; the group kill stops it.
    let bg = run_one(
        &env,
        "bash",
        r#"{"command":"(sleep 2; touch marker.txt) & sleep 30","timeout":1}"#,
        &[],
    );
    assert!(bg.starts_with("Command timed out after 1 seconds"), "{bg:?}");
    std::thread::sleep(std::time::Duration::from_millis(2500));
    assert!(
        !env.work().join("marker.txt").exists(),
        "the process tree was killed"
    );
}

/// TOOLS-013 (A): `bash` is sandboxed; the permission mode decides what it may write.
/// INVARIANT: the bwrap sandbox (no bwrap = bash refuses); workspace mode writes only under the workspace.
#[test]
fn tools_013() {
    let env = TestEnv::new();
    let outside = env.tmp().join("outside.txt");
    let cmd = format!(r#"{{"command":"echo x > '{}'; echo done"}}"#, outside.display());
    let r = run_one(&env, "bash", &cmd, &[]);
    assert!(
        !outside.exists(),
        "the sandbox keeps writes out of the workspace; result: {r:?}"
    );
    let inside = run_one(
        &env,
        "bash",
        r#"{"command":"echo y > inside.txt; echo done"}"#,
        &[],
    );
    assert!(inside.contains("done"), "{inside:?}");
    assert!(
        env.work().join("inside.txt").exists(),
        "workspace writes are allowed in workspace mode"
    );
}

/// TOOLS-014 (A): `grep` schema, .gitignore, sorted output, notices, deterministic.
/// INVARIANT: in-process and sorted (no rg download), so two runs give the same bytes.
#[test]
fn tools_014() {
    let env = TestEnv::new();
    env.write("work/.gitignore", "ignored.txt\n");
    env.write("work/b.txt", "needle here\n");
    env.write("work/a.txt", "x\nNEEDLE up\n");
    env.write("work/ignored.txt", "needle hidden\n");
    let ran = drive(&env, vec![ScriptTurn::text("ok")], &[]);
    let s = schema_of(&ran, "grep");
    assert_eq!(s["required"], serde_json::json!(["pattern"]));
    for p in [
        "pattern",
        "path",
        "glob",
        "ignoreCase",
        "literal",
        "context",
        "limit",
    ] {
        assert!(s["properties"].get(p).is_some(), "grep has {p}");
    }
    let r = run_one(&env, "grep", r#"{"pattern":"needle","ignoreCase":true}"#, &[]);
    let ctx = run_one(&env, "grep", r#"{"pattern":"NEEDLE","context":1}"#, &[]);

    // O-167: the path-once form (`oxa.diet.tools`): each path once, then its lines with
    // their numbers; the same lines, sorted, the gitignored file skipped.
    assert_eq!(
        r, "a.txt\n  2: NEEDLE up\nb.txt\n  1: needle here",
        "local: sorted, gitignored file skipped"
    );
    assert_eq!(
        ctx, "a.txt\n  1- x\n  2: NEEDLE up",
        "local: context lines use '-'"
    );

    let lit = run_one(&env, "grep", r#"{"pattern":"e.d","literal":true}"#, &[]);
    assert_eq!(lit, "No matches found");
}

/// TOOLS-015 (A): `find` pattern, path, limit, relative results, .gitignore, sorted.
/// INVARIANT: in-process and sorted (no fd download).
#[test]
fn tools_015() {
    let env = TestEnv::new();
    env.write("work/.gitignore", "skip/\n");
    env.write("work/src/b.rs", "");
    env.write("work/src/a.rs", "");
    env.write("work/top.rs", "");
    env.write("work/skip/c.rs", "");
    let ran = drive(&env, vec![ScriptTurn::text("ok")], &[]);
    let s = schema_of(&ran, "find");
    assert_eq!(s["required"], serde_json::json!(["pattern"]));
    let any_depth = run_one(&env, "find", r#"{"pattern":"*.rs"}"#, &[]);
    assert_eq!(
        any_depth, "src/a.rs\nsrc/b.rs\ntop.rs",
        "no slash: any depth, sorted, gitignored skipped"
    );
    let rel = run_one(&env, "find", r#"{"pattern":"*.rs","path":"src"}"#, &[]);
    assert_eq!(rel, "a.rs\nb.rs", "results relative to the search directory");
    let lim = run_one(&env, "find", r#"{"pattern":"*.rs","limit":1}"#, &[]);
    assert_eq!(
        lim,
        "src/a.rs\n\n[1 results limit reached. Use limit=2 for more, or refine pattern]"
    );
}

/// TOOLS-016 (P): `ls` sorted case-insensitively, '/' for directories, dotfiles included, empty text.
/// pi: `packages/coding-agent/src/core/tools/ls.ts`.
#[test]
fn tools_016() {
    let env = TestEnv::new();
    env.write("work/d/b.txt", "");
    env.write("work/d/Apple", "");
    env.write("work/d/.hidden", "");
    std::fs::create_dir_all(env.work().join("d/sub")).unwrap();
    let r = run_one(&env, "ls", r#"{"path":"d"}"#, &[]);
    assert_eq!(
        r, ".hidden\nApple\nb.txt\nsub/",
        "pi: sorted case-insensitively, dirs get /"
    );
    std::fs::create_dir_all(env.work().join("empty")).unwrap();
    assert_eq!(
        run_one(&env, "ls", r#"{"path":"empty"}"#, &[]),
        "(empty directory)",
        "pi: empty text"
    );
    assert_eq!(
        run_one(&env, "ls", r#"{"path":"d/b.txt"}"#, &[]),
        "Not a directory: d/b.txt",
        "pi: not a directory"
    );
}

/// TOOLS-017 (P): a `limit` below 1 is treated as 1 in grep, find and ls.
/// pi: `ls.ts` / `find.ts` / `grep.ts` (`Math.max(1, ...)`).
#[test]
fn tools_017() {
    let env = TestEnv::new();
    env.write("work/a.txt", "hit\nhit\n");
    env.write("work/b.rs", "");
    env.write("work/c.rs", "");
    let g = run_one(&env, "grep", r#"{"pattern":"hit","limit":0}"#, &[]);

    // O-167: over the limit (0 is 1), local answers the total, the limit and a count
    // per file instead of a cut list; no more lines than the limit. A limit that fits
    // shows every line.
    assert_eq!(
        g,
        "2 matches in total, the limit was 1. Narrow the pattern, path or glob, or raise limit, to see the lines.\n\na.txt: 2 matches",
        "local: grep limit 0"
    );
    let all = run_one(&env, "grep", r#"{"pattern":"hit","limit":2}"#, &[]);
    assert_eq!(
        all, "a.txt\n  1: hit\n  2: hit",
        "local: limit 2 shows both lines"
    );

    let f = run_one(&env, "find", r#"{"pattern":"*.rs","limit":0}"#, &[]);
    assert_eq!(
        f, "b.rs\n\n[1 results limit reached. Use limit=2 for more, or refine pattern]",
        "pi: find limit 0"
    );
    let l = run_one(&env, "ls", r#"{"limit":0}"#, &[]);
    assert_eq!(
        l, "a.txt\n\n[1 entries limit reached. Use limit=2 for more]",
        "pi: ls limit 0"
    );
}

/// TOOLS-018 (P): writes to one path are serialised; two writes in one batch land in call order.
/// pi: `packages/coding-agent/src/core/tools/file-mutation-queue.ts`.
#[test]
fn tools_018() {
    let env = TestEnv::new();
    let turns = vec![
        ScriptTurn {
            tool_calls: vec![
                oxa_proto::ToolCall {
                    id: "w1".into(),
                    name: "write".into(),
                    args_raw: r#"{"path":"same.txt","content":"first"}"#.into(),
                },
                oxa_proto::ToolCall {
                    id: "w2".into(),
                    name: "write".into(),
                    args_raw: r#"{"path":"same.txt","content":"second"}"#.into(),
                },
            ],
            ..ScriptTurn::text("")
        },
        ScriptTurn::text("ok"),
    ];
    let ran = drive(&env, turns, &[]);
    assert_eq!(tool_text(&ran, "w1"), "Successfully wrote to same.txt");
    assert_eq!(tool_text(&ran, "w2"), "Successfully wrote to same.txt");
    assert_eq!(
        std::fs::read_to_string(env.work().join("same.txt")).unwrap(),
        "second",
        "pi: last call wins"
    );
}

/// TOOLS-019 (P): tool selection with defaults, `--tools`, `-xt`, `-nbt`, `-nt`, globs; exclusion wins.
/// pi: `cli.md#tools`. INVARIANT (W3): resolved once per prefix epoch; the list is the frozen `tools` array.
/// INVARIANT (O-146): oxa's default set is the seven built-ins plus `subagent` where O-146 offers
/// it (not on the Rust Flash engine, not below a 32K window); it is selected like a built-in.
#[test]
fn tools_019() {
    let env = TestEnv::new();
    let seven: Vec<String> = ALL_SEVEN.iter().map(|s| s.to_string()).collect();
    let mut all = seven.clone();
    all.push("subagent".to_string());
    all.sort();
    assert_eq!(
        offered(&env, &[]),
        all,
        "oxa default (large, non-Flash model): the seven built-ins and subagent"
    );
    let flash = FakeOpts {
        model: "flash".to_string(),
        models: vec![oxa_fake::FakeModel::flash("flash", 65536, Some(2048))],
        ..FakeOpts::default()
    };
    let small = FakeOpts {
        n_ctx: 16384,
        ..FakeOpts::default()
    };
    for (what, opts) in [("Flash", flash), ("below 32K", small)] {
        let srv = FakeServer::start(vec![ScriptTurn::text("ok")], opts).expect("start fake engine");
        let out = run_oxa(&env, &srv.url(), &["-p", "go"], "");
        let ran = Ran {
            out,
            reqs: srv.requests(),
        };
        assert_eq!(declared_names(&ran), seven, "{what}: the seven built-ins");
    }
    assert_eq!(
        offered(&env, &["-t", "read,ls"]),
        vec!["ls".to_string(), "read".to_string()],
        "allowlist"
    );
    assert_eq!(
        offered(&env, &["-t", "re*"]),
        vec!["read".to_string()],
        "glob allowlist"
    );
    let mut no_bash = all.clone();
    no_bash.retain(|n| n != "bash");
    assert_eq!(offered(&env, &["-xt", "bash"]), no_bash, "exclude-tools");
    assert_eq!(
        offered(&env, &["-xt", "subagent"]),
        seven,
        "exclude-tools subagent"
    );
    assert!(
        offered(&env, &["-t", "bash", "-xt", "bash"]).is_empty(),
        "exclusion wins"
    );
    assert!(offered(&env, &["-nbt"]).is_empty(), "no built-in tools");
    assert!(offered(&env, &["-nt"]).is_empty(), "no tools");
}

/// TOOLS-020 (P): read-class calls of one batch run in parallel; results are pushed in call order.
/// pi: `packages/agent/src/agent-loop.ts`. Observed here as call-order results.
#[test]
fn tools_020() {
    let env = TestEnv::new();
    env.write("work/a.txt", "AAA\n");
    env.write("work/b.txt", "BBB\n");
    let turns = vec![
        ScriptTurn {
            tool_calls: vec![
                oxa_proto::ToolCall {
                    id: "r1".into(),
                    name: "read".into(),
                    args_raw: r#"{"path":"b.txt"}"#.into(),
                },
                oxa_proto::ToolCall {
                    id: "r2".into(),
                    name: "read".into(),
                    args_raw: r#"{"path":"a.txt"}"#.into(),
                },
                oxa_proto::ToolCall {
                    id: "r3".into(),
                    name: "ls".into(),
                    args_raw: r#"{}"#.into(),
                },
            ],
            ..ScriptTurn::text("")
        },
        ScriptTurn::text("ok"),
    ];
    let ran = drive(&env, turns, &[]);
    let last = ran.reqs.last().expect("second request");
    let order: Vec<String> = last
        .messages
        .iter()
        .filter(|m| m.role == Role::Tool)
        .filter_map(|m| m.tool_call_id.clone())
        .collect();
    assert_eq!(order, vec!["r1", "r2", "r3"], "results pushed in call order");
    assert_eq!(tool_text(&ran, "r1"), "BBB\n");
    assert_eq!(tool_text(&ran, "r2"), "AAA\n");
}

/// TOOLS-021 (P): an unknown tool and invalid JSON arguments yield a fixed error result.
/// pi: `packages/agent/src/agent-loop.ts` ("Tool X not found").
#[test]
fn tools_021() {
    let env = TestEnv::new();
    let unknown = run_one(&env, "nope", r#"{}"#, &[]);
    assert_eq!(unknown, "Tool nope not found", "pi: unknown tool text");
    let bad = run_one(&env, "read", "{not json", &[]);
    assert_eq!(
        bad, "Invalid tool arguments: expected a JSON object",
        "oxa: fixed bad-JSON text"
    );
}

/// TOOLS-022 (A): same input and file state give the same bytes; no ANSI, no timings.
/// INVARIANT: W4 deterministic bytes (the cache depends on it).
#[test]
fn tools_022() {
    let mut results = Vec::new();
    for _ in 0..2 {
        let env = TestEnv::new();
        env.write("work/a.txt", "one\ntwo\n");
        let r = run_one(
            &env,
            "bash",
            r#"{"command":"printf '\033[31mred\033[0m\n'; ls a.txt"}"#,
            &[],
        );
        let g = run_one(&env, "read", r#"{"path":"a.txt"}"#, &[]);
        results.push((r, g));
    }
    assert_eq!(results[0], results[1], "two runs give identical bytes");
    assert!(
        !results[0].0.contains('\u{1b}'),
        "no ANSI in bash output: {:?}",
        results[0].0
    );
    assert!(
        !results[0].0.contains("ms") && !results[0].0.contains("took"),
        "no timings"
    );
}

/// TOOLS-023 (A): MCP, extension and codemode results pass the same `shape`.
/// INVARIANT: W4 `shape` on every result. Built-in results are shaped here; the MCP, extension and
/// codemode path is covered by the G11/G12 tests that drive those surfaces.
#[test]
fn tools_023() {
    let env = TestEnv::new();
    // A built-in result with ANSI and an over-cap body goes through the same `shape`.
    let r = run_one(
        &env,
        "bash",
        r#"{"command":"printf '\033[1m'; seq 1 9000; printf '\033[0m'"}"#,
        &[],
    );
    assert!(!r.contains('\u{1b}'), "ANSI stripped by shape");
    assert!(r.len() <= 16 * 1024 + 512, "capped by shape");
}

/// Proposed TOOLS row (A, O-169; pi has no built-in search): `web_search` is declared only with
/// a Brave key and never offline; without a key the declared tools are unchanged; with one the
/// result of a mocked search reaches the next request, deterministic (W4). Mocks only, a fake
/// key (O-170).
/// INVARIANT: W3 tool list per epoch, W4 deterministic capped output.
#[test]
fn tools_024() {
    // INVARIANT: W3: the tool list is frozen per epoch; W4: deterministic capped output with source URLs
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};
    const KEY: &str = "test-key-not-real";
    let body = r#"{"grounding":{"generic":[{"url":"https://example.com/a","title":"Alpha","snippets":["one"]}]},"sources":{}}"#;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let brave_url = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let mut reader = BufReader::new(conn.try_clone().unwrap());
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 && !line.ends_with("\r\n\r\n") {}
            let mut out = conn;
            let _ = write!(
                out,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
        }
    });
    let run = |with_key: bool| -> (Vec<Recorded>, Output) {
        let env = TestEnv::new();
        let srv = FakeServer::start(
            vec![
                ScriptTurn::tool("c1", "web_search", r#"{"query":"alpha"}"#),
                ScriptTurn::text("ok"),
            ],
            FakeOpts::default(),
        )
        .expect("fake engine");
        let mut cmd = Command::new(oxa_parity::oxa_bin());
        cmd.args(["-p", "--no-session", "go"])
            .current_dir(env.work())
            .env_clear()
            .envs(env.envs(&srv.url()))
            .envs(oxa_parity::note_off_env())
            .env("OXA_BRAVE_BASE_URL", &brave_url)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if with_key {
            cmd.env("BRAVE_SEARCH_API_KEY", KEY);
        }
        let out = cmd.output().expect("spawn oxa");
        (srv.requests(), out)
    };
    let names = |r: &Recorded| -> Vec<String> {
        serde_json::from_str::<Vec<serde_json::Value>>(&r.tools_json)
            .unwrap_or_default()
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap_or("").to_string())
            .collect()
    };
    let (without, _) = run(false);
    assert!(
        !names(&without[0]).contains(&"web_search".to_string()),
        "no key: not declared"
    );
    let (with, out) = run(true);
    assert!(
        names(&with[0]).contains(&"web_search".to_string()),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = with
        .last()
        .unwrap()
        .messages
        .iter()
        .find(|m| m.role == Role::Tool && m.tool_call_id.as_deref() == Some("c1"))
        .and_then(|m| m.content.clone())
        .unwrap_or_default();
    assert_eq!(
        text,
        "Web search results for: alpha\n\n[1] Alpha\nhttps://example.com/a\none"
    );
    assert!(!String::from_utf8_lossy(&out.stderr).contains(KEY));
}
