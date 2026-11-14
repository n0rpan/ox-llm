//! G5 PARITY tests (compaction). Owner: task G5.
//! One test per PARITY.md row of the CMP area, named after the row id (`CMP-012` -> `fn cmp_012()`).
//! The four meta-checks at the end read PARITY.md and this file (they are the task's acceptance).
//!
//! Method: every row runs the real `oxa` binary (`--mode rpc --no-session`) against an in-process
//! fake engine (`oxa_fake::FakeServer`). Summary and prefill requests are read from the fake's
//! records (`x-oxe-turn-kind`, `x-oxe-priority`, `x-oxe-snapshot`); the events and responses come
//! from the RPC output. pi 1.5.2 texts are in `fixtures/pi/compaction/` (`cmp-<nnn>.txt`).
//!
//! A row whose assertion fails today is RED: the message says what differs. It is not ignored
//! (the H wave fixes it). Rows that are not testable here say so in their comment.

use oxa_fake::{parse_script, FakeOpts, FakeServer, Recorded, TestEnv};
use oxa_parity::{fixture, oxa_bin, profile, Profile};
use oxa_proto::{Role, WireMsg};
use serde_json::{json, Value};
use std::path::PathBuf;

/// Context window of every scenario: the frozen prefix (system + tools) is about 7000 tokens.
const N_CTX: u32 = 60_000;

/// A fake-engine script turn: a plain text reply.
fn text(t: &str) -> String {
    json!({ "text": t }).to_string()
}

/// A fake-engine script turn: one `read` tool call.
fn read_call(path: &str) -> String {
    json!({"tool_calls": [{"id": "call_read", "name": "read", "args": json!({"path": path}).to_string()}]})
        .to_string()
}

/// A fake-engine script turn: one `write` tool call.
fn write_call(path: &str, content: &str) -> String {
    json!({"tool_calls": [{"id": "call_write", "name": "write",
        "args": json!({"path": path, "content": content}).to_string()}]})
    .to_string()
}

/// A fake-engine script turn: a reply that the engine cuts (`finish_reason: length`).
fn cut(t: &str) -> String {
    json!({ "text": t, "finish": "length" }).to_string()
}

/// A fake-engine script turn: a 400 `exceed_context_size_error` (overflow).
fn overflow() -> String {
    json!({"fault": {"kind": "overflow"}}).to_string()
}

fn opts() -> FakeOpts {
    FakeOpts {
        n_ctx: N_CTX,
        ..FakeOpts::default()
    }
}

fn engine(turns: &[String], opts: FakeOpts) -> FakeServer {
    let script = parse_script(&turns.join("\n")).expect("fake script parses");
    FakeServer::start(script, opts).expect("start the fake engine")
}

/// A test home whose `settings.json` is the default (`keepRecentTokens` 1, so that a short
/// wire has something to summarise) overlaid with `extra` (`compaction` keys merged one by one).
fn env_with(extra: Value) -> TestEnv {
    let mut settings = json!({"compaction": {"keepRecentTokens": 1}});
    if let Some(obj) = extra.as_object() {
        for (k, v) in obj {
            if k == "compaction" {
                for (ck, cv) in v.as_object().expect("compaction is an object") {
                    settings["compaction"][ck] = cv.clone();
                }
            } else {
                settings[k] = v.clone();
            }
        }
    }
    let env = TestEnv::new();
    env.write("home/.oxa/agent/settings.json", &settings.to_string());
    env
}

fn prompt(message: &str) -> Value {
    json!({"type": "prompt", "message": message})
}

fn compact(instructions: Option<&str>) -> Value {
    match instructions {
        Some(s) => json!({"type": "compact", "customInstructions": s}),
        None => json!({"type": "compact"}),
    }
}

/// The parsed output of one `oxa --mode rpc` run.
struct Run {
    lines: Vec<Value>,
    raw: String,
}

impl Run {
    fn of(&self, ty: &str) -> Vec<&Value> {
        self.lines.iter().filter(|v| v["type"] == ty).collect()
    }
    /// Positions (in output order) of the events of type `ty`.
    fn at(&self, ty: &str) -> Vec<usize> {
        self.lines
            .iter()
            .enumerate()
            .filter(|(_, v)| v["type"] == ty)
            .map(|(i, _)| i)
            .collect()
    }
    fn response(&self, cmd: &str) -> &Value {
        self.lines
            .iter()
            .find(|v| v["type"] == "response" && v["command"] == cmd)
            .unwrap_or_else(|| panic!("no `{cmd}` response in the output:\n{}", self.raw))
    }
    /// The session's entries of the given type, from the `get_entries` the driver sends after
    /// the last command. (`entry_appended` records are pi's for extension entries only.)
    fn entries(&self, entry_type: &str) -> Vec<&Value> {
        self.response("get_entries")["data"]["entries"]
            .as_array()
            .map(|all| all.iter().filter(|e| e["type"] == entry_type).collect())
            .unwrap_or_default()
    }
}

/// Run `oxa --mode rpc --no-session` (plus `extra` flags) and send `cmds` one at a time: each
/// command is sent as soon as the previous one has finished (`agent_settled` for a prompt, the
/// response for the others), as rpc.md tells clients to. A `get_entries` follows the last
/// command (see [`Run::entries`]); then the output is read to the end.
fn rpc(env: &TestEnv, url: &str, extra: &[&str], cmds: &[Value]) -> Run {
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    use std::time::Duration;

    let mut child = Command::new(oxa_bin())
        .args(["--mode", "rpc", "--no-session"])
        .args(extra)
        .current_dir(env.work())
        .env_clear()
        .envs(env.envs(url))
        .envs(oxa_parity::profile_env())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn oxa (build it first: cargo build -p oxa-cli -p oxa-fake)");
    let mut stdin = child.stdin.take().expect("oxa stdin");
    let stdout = child.stdout.take().expect("oxa stdout");
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let timeout = Duration::from_secs(60);
    let mut lines: Vec<Value> = Vec::new();
    let mut raw = String::new();
    let take = |line: String, lines: &mut Vec<Value>, raw: &mut String| {
        raw.push_str(&line);
        raw.push('\n');
        let v: Value =
            serde_json::from_str(&line).unwrap_or_else(|e| panic!("non-JSON output line ({e}): {line}"));
        lines.push(v.clone());
        v
    };
    // No retry and no idle polling: per rpc.md a client may send the next command as soon as
    // it has read `agent_settled` (a prompt) or the response (any other command), and oxa must
    // accept it then.
    let entries = json!({"type": "get_entries"});
    for cmd in cmds.iter().chain(std::iter::once(&entries)) {
        let kind = cmd["type"].as_str().unwrap_or("");
        writeln!(stdin, "{cmd}").expect("send a command");
        stdin.flush().expect("flush the command");
        loop {
            let line = rx
                .recv_timeout(timeout)
                .unwrap_or_else(|e| panic!("oxa did not finish `{kind}` ({e}):\n{raw}"));
            let v = take(line, &mut lines, &mut raw);
            let done = if kind == "prompt" {
                v["type"] == "agent_settled"
                    || (v["type"] == "response" && v["command"] == "prompt" && v["success"] == false)
            } else {
                v["type"] == "response" && v["command"] == kind
            };
            if done {
                break;
            }
        }
    }
    drop(stdin);
    while let Ok(line) = rx.recv_timeout(timeout) {
        take(line, &mut lines, &mut raw);
    }
    let _ = child.wait();
    Run { lines, raw }
}

fn of_kind<'a>(reqs: &'a [Recorded], kind: &str) -> Vec<&'a Recorded> {
    reqs.iter().filter(|r| r.kind == kind).collect()
}

/// Content of the last message of a request ("" when none).
fn last_text(r: &Recorded) -> String {
    r.messages
        .last()
        .and_then(|m| m.content.clone())
        .unwrap_or_default()
}

fn content(m: &WireMsg) -> &str {
    m.content.as_deref().unwrap_or("")
}

/// The text of a fixture below `fixtures/`, without its trailing newline.
fn fixture_text(rel: &str) -> String {
    std::fs::read_to_string(fixture(rel))
        .unwrap_or_else(|e| panic!("fixture {rel}: {e}"))
        .trim_end_matches('\n')
        .to_string()
}

/// pi's compaction wrapper of a summary that re-enters the wire as a user message.
const WRAPPER_PREFIX: &str =
    "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";

/// Invariant: W5 soft/hard split. Auto compaction fires only when the context crosses the soft
/// limit (75% of `n_ctx - max_output - reserveTokens`, the hard limit) at the end of a run; it
/// then runs in idle time (after `agent_settled`) as a background summary request. Below the soft
/// limit nothing happens. The mid-run hard path is the overflow path (CMP-010).
#[test]
fn cmp_001() {
    let env = env_with(json!({"compaction": {"reserveTokens": 16384, "keepRecentTokens": 4000}}));
    let srv = engine(
        &[text("small reply"), text("big reply"), text("summary text")],
        opts(),
    );
    let big = "x".repeat(17_000);
    let run = rpc(&env, &srv.url(), &[], &[prompt("hi"), prompt(&big)]);
    let settled = run.at("agent_settled");
    assert_eq!(settled.len(), 2, "two runs must settle:\n{}", run.raw);
    let starts = run.at("compaction_start");
    assert_eq!(
        starts.len(),
        1,
        "exactly one compaction is expected: the small run stays below the soft limit, the big run crosses it.\n{}",
        run.raw
    );
    let ended = run.at("agent_end");
    assert!(
        starts[0] > ended[1] && ended[1] < settled[1],
        "threshold compaction must run at the end of the run, once the reply is stored (event {} vs agent_end {})",
        starts[0],
        ended[1]
    );
    assert_eq!(run.lines[starts[0]]["reason"], "threshold");
    let reqs = srv.requests();
    let summaries = of_kind(&reqs, "compaction");
    assert_eq!(summaries.len(), 1, "one summary request");
    assert!(
        summaries[0].background,
        "a threshold summary is a background request (F12)"
    );
}

/// Invariant: W5 cut rules. `keepRecentTokens` keeps the tail verbatim; with a budget that
/// covers the whole wire nothing is summarised; with a small budget the kept tail starts at a
/// user or assistant message and never splits a tool call from its result.
#[test]
fn cmp_002() {
    // (a) the budget covers the whole wire: nothing to summarise, no summary request.
    let env = env_with(json!({"compaction": {"keepRecentTokens": 100000}}));
    let srv = engine(&[text("one"), text("two")], opts());
    let run = rpc(
        &env,
        &srv.url(),
        &[],
        &[prompt("first"), prompt("second"), compact(None)],
    );
    let resp = run.response("compact");
    assert_eq!(
        resp["success"], false,
        "a wire that fits the budget is not summarised:\n{}",
        run.raw
    );
    assert_eq!(resp["error"], "nothing to compact");
    assert!(
        of_kind(&srv.requests(), "compaction").is_empty(),
        "no summary request"
    );

    // (b) a one-token budget: the cut lands on a valid point and the kept reply is verbatim.
    let env = env_with(json!({"compaction": {"keepRecentTokens": 1}}));
    env.write("work/a.txt", "alpha\n");
    let srv = engine(
        &[
            read_call("a.txt"),
            text("file read"),
            text("second reply"),
            text("summary text"),
        ],
        opts(),
    );
    let run = rpc(
        &env,
        &srv.url(),
        &[],
        &[prompt("read a.txt"), prompt("second question"), compact(None)],
    );
    assert_eq!(run.response("compact")["success"], true, "{}", run.raw);
    let reqs = srv.requests();
    let prefill = of_kind(&reqs, "prefill");
    let head = &prefill
        .last()
        .expect("the rebased head is prefilled (CMP-018)")
        .messages;
    let tail = &head[1..];
    assert!(!tail.is_empty(), "the kept tail is not empty");
    assert_ne!(
        tail[0].role,
        Role::Tool,
        "the kept tail must not start with a tool result"
    );
    for (i, m) in tail.iter().enumerate() {
        if m.role == Role::Tool {
            assert!(
                i > 0 && !tail[i - 1].tool_calls.is_empty(),
                "tool result {i} of the kept tail lost its tool call"
            );
        }
    }
    assert_eq!(
        content(tail.last().unwrap()),
        "second reply",
        "the kept reply is verbatim"
    );
}

/// Invariant: W5 mid-run path. Overflow recovery keeps the current prompt as its own, unfolded
/// message at the end of the head, and makes one summary request (no second, turn-prefix
/// summary as in pi's split user-message spans).
#[test]
fn cmp_003() {
    let env = env_with(json!({}));
    let srv = engine(
        &[
            text("first reply"),
            overflow(),
            text("summary text"),
            text("retry reply"),
        ],
        opts(),
    );
    let run = rpc(
        &env,
        &srv.url(),
        &[],
        &[prompt("first"), prompt("second question")],
    );
    assert!(
        !run.at("compaction_start").is_empty(),
        "the overflow compacts:\n{}",
        run.raw
    );
    let reqs = srv.requests();
    assert_eq!(
        of_kind(&reqs, "compaction").len(),
        1,
        "one summary request, no turn-prefix summary"
    );
    let retry = reqs.last().expect("the retried request");
    assert_eq!(retry.kind, "chat", "the retry is the chat request");
    assert!(
        retry.messages[0]
            .content
            .as_deref()
            .unwrap_or("")
            .starts_with(WRAPPER_PREFIX),
        "the head is the summary"
    );
    assert_eq!(
        last_text(retry),
        "second question",
        "the current prompt is kept verbatim"
    );
    assert_eq!(retry.messages.last().unwrap().role, Role::User);
}

/// Invariant: W1 pure append. The summary request is the whole wire of the last chat request
/// plus its reply and one user instruction, with the same system text and tools, thinking off,
/// and no background priority for a manual `/compact` (foreground, F12).
#[test]
fn cmp_004() {
    let env = env_with(json!({}));
    let srv = engine(&[text("one"), text("two"), text("summary text")], opts());
    let run = rpc(
        &env,
        &srv.url(),
        &[],
        &[prompt("first"), prompt("second"), compact(None)],
    );
    assert_eq!(run.response("compact")["success"], true, "{}", run.raw);
    let reqs = srv.requests();
    let chat = of_kind(&reqs, "chat");
    let last_chat = chat.last().expect("chat requests");
    let summary = of_kind(&reqs, "compaction")[0];
    let mut prefix = last_chat.messages.clone();
    prefix.push(last_chat.reply.clone().expect("the last reply was stored"));
    assert_eq!(
        summary.messages.len(),
        prefix.len() + 1,
        "wire + reply + one instruction"
    );
    assert_eq!(
        &summary.messages[..prefix.len()],
        &prefix[..],
        "the summary request is a pure append"
    );
    assert_eq!(summary.system, last_chat.system, "same system text");
    assert_eq!(summary.tools_json, last_chat.tools_json, "same tools");
    assert!(!summary.thinking, "thinking is off for the summary");
    assert!(!summary.background, "a manual /compact is foreground");
    assert_eq!(
        summary.messages.last().unwrap().role,
        Role::User,
        "the instruction is a user message"
    );
}

/// P: the summary instruction is pi's `SUMMARIZATION_PROMPT` (fixture `cmp-005.txt`, pi
/// `compaction.ts`), including the six section headings of the summary format.
#[test]
fn cmp_005() {
    let env = env_with(json!({}));
    let srv = engine(&[text("one"), text("two"), text("summary text")], opts());
    let run = rpc(
        &env,
        &srv.url(),
        &[],
        &[prompt("first"), prompt("second"), compact(None)],
    );
    assert_eq!(run.response("compact")["success"], true, "{}", run.raw);
    let reqs = srv.requests();
    let instruction = last_text(of_kind(&reqs, "compaction")[0]);
    let pi = fixture_text("pi/compaction/cmp-005.txt");
    assert!(
        instruction.contains(&pi),
        "the summary instruction is not pi's SUMMARIZATION_PROMPT"
    );
    let headings: Vec<&str> = pi.lines().filter(|l| l.starts_with("## ")).collect();
    assert_eq!(headings.len(), 6, "pi's format has six sections");
    for h in headings {
        assert!(instruction.contains(h), "missing section `{h}`");
    }
}

/// P: repeated compaction sends pi's update instruction, and the previous summary is the head
/// (the first message) of the request (fixture `cmp-006.txt`).
#[test]
fn cmp_006() {
    let env = env_with(json!({}));
    let srv = engine(
        &[text("one"), text("summary A"), text("two"), text("summary B")],
        opts(),
    );
    let run = rpc(
        &env,
        &srv.url(),
        &[],
        &[prompt("first"), compact(None), prompt("second"), compact(None)],
    );
    assert_eq!(run.response("compact")["success"], true, "{}", run.raw);
    let reqs = srv.requests();
    let updates = of_kind(&reqs, "compaction");
    assert_eq!(updates.len(), 2, "two summary requests");
    let second = updates[1];
    assert!(
        second.messages[0]
            .content
            .as_deref()
            .unwrap_or("")
            .starts_with(WRAPPER_PREFIX),
        "the previous summary is the head of the update request"
    );
    let update = fixture_text("pi/compaction/cmp-006.txt");
    assert!(
        last_text(second).contains(&update),
        "the update instruction is pi's UPDATE rules"
    );
}

/// P: cumulative file tracking. Files read and written in the run appear in `details`
/// (`readFiles`, `modifiedFiles`) and as `<read-files>` / `<modified-files>` blocks after the
/// summary (pi `formatFileOperations`).
#[test]
fn cmp_007() {
    let env = env_with(json!({}));
    env.write("work/a.txt", "alpha\n");
    let srv = engine(
        &[
            read_call("a.txt"),
            write_call("b.txt", "beta\n"),
            text("wrote it"),
            text("summary text"),
        ],
        opts(),
    );
    let run = rpc(&env, &srv.url(), &[], &[prompt("read and write"), compact(None)]);
    assert_eq!(run.response("compact")["success"], true, "{}", run.raw);
    let entries = run.entries("compaction");
    assert_eq!(entries.len(), 1, "one compaction entry");
    let details = &entries[0]["details"];
    assert_eq!(details["readFiles"], json!(["a.txt"]), "readFiles");
    assert_eq!(details["modifiedFiles"], json!(["b.txt"]), "modifiedFiles");
    let block = fixture_text("pi/compaction/cmp-007.txt");
    assert!(
        block.contains("<read-files>") && block.contains("<modified-files>"),
        "pi formatFileOperations blocks"
    );
    let summary = run.response("compact")["data"]["summary"]
        .as_str()
        .unwrap_or_default();
    assert!(
        summary.ends_with("<read-files>\na.txt\n</read-files>\n\n<modified-files>\nb.txt\n</modified-files>"),
        "the file blocks follow the summary: {summary:?}"
    );
}

/// P: the `CompactionEntry` shape: the required fields of pi's interface (fixture `cmp-008.txt`)
/// are present in the appended entry with their types.
#[test]
fn cmp_008() {
    let env = env_with(json!({}));
    let srv = engine(&[text("one"), text("two"), text("summary text")], opts());
    let run = rpc(
        &env,
        &srv.url(),
        &[],
        &[prompt("first"), prompt("second"), compact(None)],
    );
    assert_eq!(run.response("compact")["success"], true, "{}", run.raw);
    let entries = run.entries("compaction");
    assert_eq!(entries.len(), 1);
    let entry = entries[0];
    let fixture = fixture_text("pi/compaction/cmp-008.txt");
    let block = fixture
        .split("interface CompactionEntry")
        .nth(1)
        .and_then(|s| s.split('}').next())
        .expect("the CompactionEntry interface in the fixture");
    for line in block.lines() {
        let line = line.trim();
        let Some((name, _)) = line.split_once(':') else {
            continue;
        };
        if name.ends_with('?') || name.contains(' ') || name.is_empty() || name.contains('<') {
            continue;
        }
        assert!(
            entry.get(name).is_some(),
            "required field `{name}` missing from the entry: {entry}"
        );
    }
    assert!(entry["summary"].is_string());
    assert!(entry["firstKeptEntryId"].is_string());
    assert!(entry["tokensBefore"].is_number());
}

/// P: `/compact <instructions>` appends pi's `Additional focus:` (fixture `cmp-009.txt`) to the
/// summary instruction.
#[test]
fn cmp_009() {
    let env = env_with(json!({}));
    let srv = engine(&[text("one"), text("two"), text("summary text")], opts());
    let run = rpc(
        &env,
        &srv.url(),
        &[],
        &[
            prompt("first"),
            prompt("second"),
            compact(Some("Focus on code changes")),
        ],
    );
    assert_eq!(run.response("compact")["success"], true, "{}", run.raw);
    let reqs = srv.requests();
    let focus = fixture_text("pi/compaction/cmp-009.txt");
    let want = format!("{focus}Focus on code changes");
    assert!(
        last_text(of_kind(&reqs, "compaction")[0]).ends_with(&want),
        "Additional focus is appended"
    );
}

/// Invariant: W2 / W5. A context-overflow reply is not on the wire: the run compacts once
/// (`overflow`), the summary replaces the head, and the prompt is retried once. A second
/// overflow after the retry is not compacted again and the run ends.
#[test]
fn cmp_010() {
    let env = env_with(json!({}));
    let srv = engine(
        &[
            text("first reply"),
            overflow(),
            text("summary text"),
            text("retry reply"),
        ],
        opts(),
    );
    let run = rpc(
        &env,
        &srv.url(),
        &[],
        &[prompt("first"), prompt("second question")],
    );
    let starts = run.of("compaction_start");
    assert_eq!(starts.len(), 1, "one recovery compaction:\n{}", run.raw);
    assert_eq!(starts[0]["reason"], "overflow");
    let reqs = srv.requests();
    let retry = reqs.last().unwrap();
    assert_eq!(retry.kind, "chat");
    assert_eq!(
        last_text(retry),
        "second question",
        "the retried request carries the prompt"
    );
    assert!(
        !retry
            .messages
            .iter()
            .any(|m| m.role == Role::Assistant && content(m).is_empty()),
        "the failed attempt is not on the wire"
    );

    // Second overflow: no further compaction, the run ends with the error.
    let env = env_with(json!({}));
    let srv = engine(
        &[text("first reply"), overflow(), text("summary text"), overflow()],
        opts(),
    );
    let run = rpc(
        &env,
        &srv.url(),
        &[],
        &[prompt("first"), prompt("second question")],
    );
    assert_eq!(
        run.of("compaction_start").len(),
        1,
        "one recovery only:\n{}",
        run.raw
    );
    assert_eq!(
        of_kind(&srv.requests(), "compaction").len(),
        1,
        "one summary request"
    );
    assert!(
        !run.at("agent_settled").is_empty(),
        "the run ends after the second overflow"
    );
}

/// Invariant: W2 (cut replies are not run). A `length` reply without tool calls ends the run with
/// stop reason `length` and no automatic continue (O-143). Tool calls inside a cut reply are not
/// run; they get the `was not executed ... Re-issue` error result and the loop goes on.
#[test]
fn cmp_011() {
    let pi = fixture_text("pi/compaction/cmp-011.txt");
    assert!(pi.contains("length"), "pi documents the length stop");
    // No automatic continue: one request, stop reason length.
    let env = env_with(json!({}));
    let srv = engine(&[cut("partial reply")], opts());
    let run = rpc(&env, &srv.url(), &[], &[prompt("first")]);
    assert_eq!(
        srv.requests().len(),
        1,
        "a length reply is not continued automatically"
    );
    let ended = run.of("agent_end");
    assert_eq!(ended.len(), 1, "{}", run.raw);
    let last = ended[0]["messages"]
        .as_array()
        .and_then(|m| m.last())
        .cloned()
        .unwrap_or(Value::Null);
    assert_eq!(
        last["stopReason"], "length",
        "the run ends with stop reason length: {last}"
    );

    // A tool call inside the cut reply is not run and gets the error result; the loop goes on.
    let env = env_with(json!({}));
    env.write("work/a.txt", "alpha\n");
    let srv = engine(&[read_call_cut("a.txt"), text("after the error")], opts());
    let run = rpc(&env, &srv.url(), &[], &[prompt("read a.txt")]);
    let reqs = srv.requests();
    assert_eq!(
        reqs.len(),
        2,
        "the loop goes on after the cut tool call:\n{}",
        run.raw
    );
    let result = reqs[1]
        .messages
        .iter()
        .find(|m| m.role == Role::Tool)
        .map(|m| content(m).to_string())
        .unwrap_or_default();
    assert!(
        result.contains("was not executed") && result.contains("Re-issue"),
        "the cut tool call gets pi's error result, got: {result:?}"
    );
    let ran = run
        .of("tool_execution_end")
        .iter()
        .any(|e| e["toolCallId"] == "call_read" && e["isError"] == false);
    assert!(
        !ran,
        "the cut tool call is never run (no successful execution):\n{}",
        run.raw
    );
}

/// A `read` tool call inside a reply that finishes with `length`.
fn read_call_cut(path: &str) -> String {
    json!({"tool_calls": [{"id": "call_read", "name": "read", "args": json!({"path": path}).to_string()}],
        "finish": "length"})
    .to_string()
}

/// P: `compaction_start` / `compaction_end` carry `reason` `manual`, `threshold` or `overflow`
/// (fixture `cmp-012.txt`). Manual and overflow are checked here; threshold in CMP-001.
#[test]
fn cmp_012() {
    let fixture = fixture_text("pi/compaction/cmp-012.txt");
    for reason in ["manual", "threshold", "overflow"] {
        assert!(fixture.contains(reason), "pi's reason set has `{reason}`");
    }

    let env = env_with(json!({}));
    let srv = engine(&[text("one"), text("two"), text("summary text")], opts());
    let run = rpc(
        &env,
        &srv.url(),
        &[],
        &[prompt("first"), prompt("second"), compact(None)],
    );
    let start = run.of("compaction_start");
    let end = run.of("compaction_end");
    assert_eq!(start.len(), 1, "{}", run.raw);
    assert_eq!(start[0]["reason"], "manual");
    assert_eq!(end.len(), 1);
    assert_eq!(end[0]["reason"], "manual");
    assert!(end[0]["result"].is_object(), "compaction_end carries the result");

    let env = env_with(json!({}));
    let srv = engine(
        &[
            text("first reply"),
            overflow(),
            text("summary text"),
            text("retry"),
        ],
        opts(),
    );
    let run = rpc(&env, &srv.url(), &[], &[prompt("first"), prompt("second")]);
    let start = run.of("compaction_start");
    assert_eq!(start.len(), 1, "{}", run.raw);
    assert_eq!(start[0]["reason"], "overflow");
    assert_eq!(run.of("compaction_end")[0]["reason"], "overflow");
}

/// P: `session_before_compact` can cancel the compaction or supply the summary (extension
/// `cmp-013`: `{cancel: true}` or `{summary: {summary}}`). A supplied summary lands as the head
/// (fixture `cmp-013.txt`, pi docs/extensions.md).
#[test]
fn cmp_013() {
    assert!(fixture_text("pi/compaction/cmp-013.txt").contains("session_before_compact"));
    let custom = env_with(json!({}));
    let ext = custom.write(
        "work/custom.js",
        r#"export default function (oxa) { oxa.on("session_before_compact", () => ({ summary: { summary: "CUSTOM SUMMARY" } })); }"#,
    );
    let srv = engine(&[text("one"), text("two")], opts());
    let run = rpc(
        &custom,
        &srv.url(),
        &["-e", ext.to_str().unwrap()],
        &[prompt("first"), prompt("second"), compact(None)],
    );
    assert_eq!(run.response("compact")["success"], true, "{}", run.raw);
    assert_eq!(run.response("compact")["data"]["summary"], "CUSTOM SUMMARY");
    assert!(
        of_kind(&srv.requests(), "compaction").is_empty(),
        "no summary request for a supplied summary"
    );
    let reqs = srv.requests();
    let prefill = of_kind(&reqs, "prefill");
    assert!(
        prefill.last().is_some_and(|p| p.messages[0]
            .content
            .as_deref()
            .unwrap_or("")
            .contains("CUSTOM SUMMARY")),
        "the custom summary is the head"
    );

    let cancel = env_with(json!({}));
    let ext = cancel.write(
        "work/cancel.js",
        r#"export default function (oxa) { oxa.on("session_before_compact", () => ({ cancel: true, reason: "no" })); }"#,
    );
    let srv = engine(&[text("one"), text("two")], opts());
    let run = rpc(
        &cancel,
        &srv.url(),
        &["-e", ext.to_str().unwrap()],
        &[prompt("first"), prompt("second"), compact(None)],
    );
    assert_eq!(
        run.response("compact")["success"],
        false,
        "a cancelled compaction is not a success"
    );
    assert_eq!(run.of("compaction_end")[0]["aborted"], true);
    assert!(
        of_kind(&srv.requests(), "compaction").is_empty(),
        "no summary request after a cancel"
    );
}

/// P: `session_compact` fires after a compaction and `session_compact_failed` after a failed one
/// (extension notifies through `ctx.ui.notify`; fixture `cmp-014.txt`, pi docs/extensions.md).
#[test]
fn cmp_014() {
    let ext_src = r#"export default function (oxa) {
  oxa.on("session_compact", (event, ctx) => { ctx.ui.notify("hook session_compact " + event.reason, "info"); });
  oxa.on("session_compact_failed", (event, ctx) => { ctx.ui.notify("hook session_compact_failed " + event.reason, "info"); });
}"#;
    let fixture = fixture_text("pi/compaction/cmp-014.txt");
    assert!(
        fixture.contains("session_compact"),
        "pi documents session_compact"
    );

    let env = env_with(json!({}));
    let ext = env.write("work/hooks.js", ext_src);
    let srv = engine(&[text("one"), text("two"), text("summary text")], opts());
    let run = rpc(
        &env,
        &srv.url(),
        &["-e", ext.to_str().unwrap()],
        &[prompt("first"), prompt("second"), compact(None)],
    );
    assert!(
        run.raw.contains("hook session_compact manual"),
        "session_compact fired:\n{}",
        run.raw
    );

    if profile() == Profile::Local {
        // O-167 / R4: under `local` the cut summary is replaced by the mechanical one, so the
        // compaction happens and `session_compact` fires; the extension still learns that the
        // model summary failed, and why (`mechanical`, `summaryError`: pi's failure text).
        let ext_local = r#"export default function (oxa) {
  oxa.on("session_compact", (event, ctx) => { ctx.ui.notify("hook session_compact " + event.reason + " mechanical=" + event.mechanical + " error=" + event.summaryError, "info"); });
  oxa.on("session_compact_failed", (event, ctx) => { ctx.ui.notify("hook session_compact_failed " + event.reason, "info"); });
}"#;
        let env = env_with(json!({}));
        let ext = env.write("work/hooks.js", ext_local);
        let srv = engine(&[text("one"), text("two"), cut("partial summary")], opts());
        let run = rpc(
            &env,
            &srv.url(),
            &["-e", ext.to_str().unwrap()],
            &[prompt("first"), prompt("second"), compact(None)],
        );
        let resp = run.response("compact");
        assert_eq!(
            resp["success"], true,
            "local: the mechanical summary is used\n{}",
            run.raw
        );
        assert_eq!(resp["data"]["details"]["mechanical"], true, "{}", run.raw);
        let want = format!(
            "hook session_compact manual mechanical=true error={}",
            fixture_text("pi/compaction/cmp-016.txt")
        );
        assert!(
            run.raw.contains(&want),
            "session_compact fired with the failure:\n{}",
            run.raw
        );
        assert!(
            !run.raw.contains("hook session_compact_failed"),
            "the wire was compacted, so session_compact_failed does not fire:\n{}",
            run.raw
        );
        return;
    }
    let env = env_with(json!({}));
    let ext = env.write("work/hooks.js", ext_src);
    let srv = engine(&[text("one"), text("two"), cut("partial summary")], opts());
    let run = rpc(
        &env,
        &srv.url(),
        &["-e", ext.to_str().unwrap()],
        &[prompt("first"), prompt("second"), compact(None)],
    );
    assert_eq!(
        run.response("compact")["success"],
        false,
        "the length summary fails"
    );
    assert!(
        run.raw.contains("hook session_compact_failed manual"),
        "session_compact_failed fired:\n{}",
        run.raw
    );
}

/// Invariant: O-144. `compaction.modelOverrides` naming another model is ignored with a warning:
/// the summary runs on the session model (one loaded model; a swap would cold-prefill the wire).
#[test]
fn cmp_015() {
    let env = env_with(json!({"compaction": {"modelOverrides": {"fake-model": "other-model"}}}));
    let srv = engine(&[text("one"), text("two"), text("summary text")], opts());
    let run = rpc(
        &env,
        &srv.url(),
        &[],
        &[prompt("first"), prompt("second"), compact(None)],
    );
    assert_eq!(run.response("compact")["success"], true, "{}", run.raw);
    assert!(
        run.raw.contains("summaries on other-model are not supported"),
        "the override is ignored with a warning:\n{}",
        run.raw
    );
    let reqs = srv.requests();
    let summary = of_kind(&reqs, "compaction")[0];
    assert_eq!(
        summary.model, "fake-model",
        "the summary runs on the session model"
    );
}

/// P: a summary that stops for length is rejected: the compaction fails with pi's text
/// (fixture `cmp-016.txt`) and the wire and the entries stay as they were.
#[test]
fn cmp_016() {
    let env = env_with(json!({}));
    let srv = engine(&[text("one"), text("two"), cut("partial summary")], opts());
    let run = rpc(
        &env,
        &srv.url(),
        &[],
        &[prompt("first"), prompt("second"), compact(None)],
    );
    let resp = run.response("compact");
    if profile() == Profile::Local {
        // O-167 / R4: under `local` the cut summary is never used either; the mechanical summary
        // (the user's messages, without the model) replaces it, and the user is told pi's
        // failure text in a warning.
        assert_eq!(resp["success"], true, "{}", run.raw);
        assert_eq!(resp["data"]["details"]["mechanical"], true, "{}", run.raw);
        let summary = resp["data"]["summary"].as_str().unwrap_or_default();
        assert!(
            !summary.contains("partial summary"),
            "the cut summary is not used: {summary}"
        );
        assert!(
            summary.contains("1. first") && summary.contains("2. second"),
            "the mechanical summary keeps the user's messages: {summary}"
        );
        let pi_text = fixture_text("pi/compaction/cmp-016.txt");
        assert!(
            run.of("notice").iter().any(|n| n["level"] == "warning"
                && n["text"].as_str().unwrap_or_default().contains(pi_text.trim())),
            "a warning carries pi's failure text:\n{}",
            run.raw
        );
        let entries = run.entries("compaction");
        assert_eq!(entries.len(), 1, "one compaction entry");
        assert_eq!(entries[0]["details"]["mechanical"], true);
        return;
    }
    assert_eq!(resp["success"], false, "{}", run.raw);
    assert_eq!(resp["error"], fixture_text("pi/compaction/cmp-016.txt"));
    assert!(
        run.entries("compaction").is_empty(),
        "no compaction entry is persisted"
    );
}

/// P: summarization retry events (fixture `cmp-017.txt`, pi `agent-session.ts`
/// `_summarizationRetryCallbacks` over `retryAssistantCall` in pi-ai `utils/retry.ts`): a
/// transient failure of the summary request is retried with `summarization_retry_scheduled`
/// (attempt, maxAttempts, delayMs = baseDelayMs * 2^(attempt-1), errorMessage) before the
/// backoff, `summarization_retry_attempt_start` (source compaction, reason) after it, and one
/// `summarization_retry_finished` at the end; the compaction then completes.
#[test]
fn cmp_017() {
    let fixture = fixture_text("pi/compaction/cmp-017.txt");
    assert!(fixture.contains("summarization_retry_scheduled"));
    assert!(fixture.contains(r#"reason: "manual" | "threshold" | "overflow""#));
    let env = env_with(json!({"retry": {"baseDelayMs": 50, "maxRetries": 2}}));
    let fault =
        json!({"fault": {"kind": "http", "status": 503, "body": "{\"error\":{\"message\":\"busy\"}}"}})
            .to_string();
    let srv = engine(&[text("one"), text("two"), fault, text("summary text")], opts());
    let run = rpc(
        &env,
        &srv.url(),
        &[],
        &[prompt("first"), prompt("second"), compact(None)],
    );
    let types: Vec<&str> = run.lines.iter().filter_map(|v| v["type"].as_str()).collect();
    let scheduled = run.of("summarization_retry_scheduled");
    assert_eq!(
        scheduled.len(),
        1,
        "one retry is scheduled; the event types seen: {}",
        types.join(" ")
    );
    assert_eq!(scheduled[0]["attempt"], 1);
    assert_eq!(scheduled[0]["maxAttempts"], 2);
    assert_eq!(scheduled[0]["delayMs"], 50, "baseDelayMs * 2^0");
    assert!(
        scheduled[0]["errorMessage"]
            .as_str()
            .is_some_and(|m| m.contains("503")),
        "{}",
        scheduled[0]
    );
    let start = run.of("summarization_retry_attempt_start");
    assert_eq!(start.len(), 1);
    assert_eq!(start[0]["source"], "compaction");
    assert_eq!(start[0]["reason"], "manual");
    assert_eq!(run.of("summarization_retry_finished").len(), 1);
    let (s, a, f, end) = (
        run.at("summarization_retry_scheduled")[0],
        run.at("summarization_retry_attempt_start")[0],
        run.at("summarization_retry_finished")[0],
        run.at("compaction_end")[0],
    );
    assert!(s < a && a < f && f < end, "order: {}", types.join(" "));
    assert_eq!(
        run.response("compact")["success"],
        true,
        "the retried summary completes the compaction"
    );
    assert_eq!(
        of_kind(&srv.requests(), "compaction").len(),
        2,
        "the summary was requested twice"
    );
}

/// Invariant: H3 / H4 / H6. After a compaction the new head is prefilled in idle time with
/// `max_tokens: 0`, background priority and `X-Oxe-Snapshot: keep`, but only when the engine
/// advertises prefill (`oxe_hints`). Without the hint the compaction still completes and no
/// prefill is sent.
#[test]
fn cmp_018() {
    let env = env_with(json!({}));
    let srv = engine(&[text("one"), text("two"), text("summary text")], opts());
    let run = rpc(
        &env,
        &srv.url(),
        &[],
        &[prompt("first"), prompt("second"), compact(None)],
    );
    assert_eq!(run.response("compact")["success"], true, "{}", run.raw);
    let reqs = srv.requests();
    let prefill = of_kind(&reqs, "prefill");
    assert_eq!(prefill.len(), 1, "one idle prefill after the compaction");
    let p = prefill[0];
    assert_eq!(p.max_tokens, 0, "H6: zero-token prefill");
    assert!(p.background, "H3: background priority");
    assert!(p.snapshot_keep, "H4: snapshot keep");
    assert!(
        p.messages[0]
            .content
            .as_deref()
            .unwrap_or("")
            .starts_with(WRAPPER_PREFIX),
        "the prefilled head starts with the summary"
    );
    let summary_pos = reqs.iter().position(|r| r.kind == "compaction").unwrap();
    let prefill_pos = reqs.iter().position(|r| r.kind == "prefill").unwrap();
    assert!(prefill_pos > summary_pos, "the prefill follows the summary");

    // No hint: the compaction completes, nothing is prefilled.
    let env = env_with(json!({}));
    let srv = engine(
        &[text("one"), text("two"), text("summary text")],
        FakeOpts {
            hints: false,
            ..opts()
        },
    );
    let run = rpc(
        &env,
        &srv.url(),
        &[],
        &[prompt("first"), prompt("second"), compact(None)],
    );
    assert_eq!(run.response("compact")["success"], true, "{}", run.raw);
    assert!(
        of_kind(&srv.requests(), "prefill").is_empty(),
        "no prefill without the prefill hint"
    );
}

/// Invariant: W1 / W5. The next prompt after a compaction is sent with the head as its prefix
/// (summary first, then the kept tail, then the new prompt) and reuses the prefilled cache
/// (`cacheRead` > 0). Live delivery of a prompt sent while a run is still in progress (the
/// queue during a run) is the hardware gate 6.6 and is not checkable against the fake.
#[test]
fn cmp_019() {
    let env = env_with(json!({}));
    let srv = engine(
        &[text("one"), text("summary text"), text("after compaction")],
        opts(),
    );
    let run = rpc(
        &env,
        &srv.url(),
        &[],
        &[prompt("first"), compact(None), prompt("next prompt")],
    );
    assert_eq!(run.response("compact")["success"], true, "{}", run.raw);
    let reqs = srv.requests();
    let next = reqs
        .iter()
        .rev()
        .find(|r| r.kind == "chat")
        .expect("the next prompt is a chat request");
    assert!(
        next.messages[0]
            .content
            .as_deref()
            .unwrap_or("")
            .starts_with(WRAPPER_PREFIX),
        "the head is the summary"
    );
    assert_eq!(last_text(next), "next prompt", "the new prompt is last");
    let cached = run
        .entries("usage")
        .into_iter()
        .rfind(|e| e["kind"] == "chat")
        .and_then(|e| e["usage"]["cacheRead"].as_u64())
        .unwrap_or(0);
    assert!(
        cached > 0,
        "the prefilled head is reused from the cache (cacheRead {cached}):\n{}",
        run.raw
    );
}

// ---- meta-checks (acceptance of task G5) ----------------------------------------------------

/// One CMP row of PARITY.md: (id, verdict, test cell).
fn cmp_rows() -> Vec<(String, String, String)> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("PARITY.md");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let mut in_cmp = false;
    let mut rows = Vec::new();
    for line in text.lines() {
        if line.starts_with("## ") {
            in_cmp = line.starts_with("## CMP");
            continue;
        }
        if !in_cmp || !line.starts_with("| CMP-") {
            continue;
        }
        // `\|` inside a cell is an escaped pipe, not a column separator.
        let line = line.replace("\\|", "\u{1}");
        let cells: Vec<&str> = line.split('|').map(str::trim).collect();
        rows.push((
            cells[1].to_string(),
            cells[3].to_string(),
            cells[5].replace('\u{1}', "|"),
        ));
    }
    rows
}

/// This file, for the meta-checks.
const SELF: &str = include_str!("compaction.rs");

/// The body of `fn <name>()` in this file (from its signature to the closing brace at column 0).
fn fn_body(name: &str) -> Option<&'static str> {
    let start = SELF.find(&format!("fn {name}()"))?;
    let rest = &SELF[start..];
    let end = rest.find("\n}\n").map(|e| e + 3).unwrap_or(rest.len());
    Some(&rest[..end])
}

/// The doc comment lines directly above `fn <name>()`.
fn doc_of(name: &str) -> String {
    let lines: Vec<&str> = SELF.lines().collect();
    let idx = lines
        .iter()
        .position(|l| l.starts_with(&format!("fn {name}()")))
        .unwrap_or_else(|| panic!("no fn {name}() in compaction.rs"));
    let mut doc = Vec::new();
    let mut i = idx;
    while i > 0 {
        i -= 1;
        let t = lines[i].trim_start();
        if t.starts_with("///") || t.starts_with("#[") {
            doc.push(t.to_string());
        } else {
            break;
        }
    }
    doc.reverse();
    doc.join("\n")
}

/// Every CMP id of PARITY.md maps to a test fn of this file (or an O-row assertion).
#[test]
fn every_row_has_test_or_recorded_omission() {
    let rows = cmp_rows();
    assert_eq!(rows.len(), 19, "PARITY.md CMP has 19 rows");
    for (id, _, test) in &rows {
        let name = id.to_lowercase().replace('-', "_");
        assert_eq!(
            test,
            &format!("`compaction.rs::{name}`"),
            "{id}: test cell must name compaction.rs::{name}"
        );
        assert!(fn_body(&name).is_some(), "{id}: no fn {name}() in compaction.rs");
    }
}

/// Every P row compares with pi: a non-empty pi 1.5.2 fixture (docs or source excerpt) exists
/// under `fixtures/pi/compaction/`, and the row's test reads it.
#[test]
fn p_rows_compare_with_pi() {
    for (id, _, _) in cmp_rows().iter().filter(|r| r.1 == "P") {
        let name = id.to_lowercase().replace('-', "_");
        let file = format!("pi/compaction/{}.txt", id.to_lowercase());
        let text = std::fs::read_to_string(fixture(&file))
            .unwrap_or_else(|e| panic!("{id}: missing pi fixture {file}: {e}"));
        assert!(!text.trim().is_empty(), "{id}: pi fixture {file} is empty");
        let body = fn_body(&name).unwrap();
        assert!(
            body.contains("fixture") || body.contains("pi/compaction/"),
            "{id}: the test does not compare with the pi fixture"
        );
    }
}

/// Every A row names its invariant (a doc line `Invariant: ...` above the test) and its test
/// asserts the adapted behaviour.
#[test]
fn a_rows_name_their_invariant() {
    let mut n = 0;
    for (id, _, _) in cmp_rows().iter().filter(|r| r.1 == "A") {
        let name = id.to_lowercase().replace('-', "_");
        let doc = doc_of(&name);
        assert!(
            doc.contains("Invariant:"),
            "{id}: the doc comment must name the invariant:\n{doc}"
        );
        assert!(
            fn_body(&name).unwrap().contains("assert"),
            "{id}: no assertion in the test"
        );
        n += 1;
    }
    assert_eq!(n, 8, "PARITY.md CMP has 8 A rows");
}

/// Every O row of the area asserts the `unsupported_in_oxa` error or is recorded as an omission.
/// CMP has no O row (PARITY.md), so this checks the table has none.
#[test]
fn o_rows_assert_unsupported_error() {
    for (id, _, _) in cmp_rows().iter().filter(|r| r.1 == "O") {
        let name = id.to_lowercase().replace('-', "_");
        assert!(
            fn_body(&name).is_some_and(|b| b.contains("unsupported_in_oxa")),
            "{id}: an O row must assert unsupported_in_oxa"
        );
    }
    assert!(
        cmp_rows().iter().all(|r| r.1 != "O"),
        "PARITY.md CMP has no O rows"
    );
}
