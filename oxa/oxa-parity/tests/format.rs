//! Wave H PARITY tests (lane fmt): the session file format, PARITY.md section "FMT: Session file
//! format" (`FMT-001` -> `fn fmt_001()`), plus the four meta-checks every parity file has.
//!
//! Every row test drives the real `oxa` binary (print mode, `--mode json`, `--mode rpc`, or the
//! pty for `/import`) against the in-process fake engine inside an isolated `TestEnv`, then reads
//! the session `.jsonl` file the binary wrote and the requests the fake recorded.
//! Conventions (see `oxa-parity/src/lib.rs`):
//! * `pi docs: <page>` names the pi 1.5.2 documented behaviour a P row compares with
//!   (session-format.md, message-types.md); the documented JSON examples are quoted below.
//! * `invariant: <name>` names the W-invariant an A row adapts to.
//! * A row the product does not implement fails with a message that starts `RED <ID>:`.
//!
//! The parity crate reads the session files as JSON lines, in the documented pi format, so the
//! checks are on the bytes pi tooling would read.

use oxa_fake::{FakeModel, FakeOpts, FakeServer, Fault, Recorded, ScriptTurn, TestEnv};
use oxa_parity::oxa_bin;
use oxa_proto::Role;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

#[path = "support/tui_rig.rs"]
mod rig;
#[path = "support/tree_pty.rs"]
mod tree_pty;
use rig::{Rig, RigOpts, WAIT};
use tree_pty::dialog_choice;

// ---------------------------------------------------------------- pi's documented examples

/// pi docs: session-format.md "SessionHeader" (both documented header lines).
const PI_HEADER: &str = r#"{"type":"session","version":3,"id":"uuid","timestamp":"2024-12-03T14:00:00.000Z","cwd":"/path/to/project"}"#;
const PI_HEADER_PARENT: &str = r#"{"type":"session","version":3,"id":"uuid","timestamp":"2024-12-03T14:00:00.000Z","cwd":"/path/to/project","parentSession":"/path/to/original/session.jsonl"}"#;
/// pi docs: session-format.md "ModelChangeEntry".
const PI_MODEL_CHANGE: &str = r#"{"type":"model_change","id":"d4e5f6g7","parentId":"c3d4e5f6","timestamp":"2024-12-03T14:05:00.000Z","provider":"openai","modelId":"gpt-4o"}"#;
/// pi docs: session-format.md "ThinkingLevelChangeEntry".
const PI_THINKING_CHANGE: &str = r#"{"type":"thinking_level_change","id":"e5f6g7h8","parentId":"d4e5f6g7","timestamp":"2024-12-03T14:06:00.000Z","thinkingLevel":"high"}"#;
/// pi docs: session-format.md "BranchSummaryEntry" (required fields; `details`, `fromHook` and
/// `usage` are optional).
const PI_BRANCH_SUMMARY: &str = r#"{"type":"branch_summary","id":"g7h8i9j0","parentId":"a1b2c3d4","timestamp":"2024-12-03T14:15:00.000Z","fromId":"f6g7h8i9","summary":"Branch explored approach A..."}"#;
/// pi docs: session-format.md "CustomEntry".
const PI_CUSTOM: &str = r#"{"type":"custom","id":"h8i9j0k1","parentId":"g7h8i9j0","timestamp":"2024-12-03T14:20:00.000Z","customType":"my-extension","data":{"count":42}}"#;
/// pi docs: session-format.md "CustomMessageEntry" (`details` optional).
const PI_CUSTOM_MESSAGE: &str = r#"{"type":"custom_message","id":"i9j0k1l2","parentId":"h8i9j0k1","timestamp":"2024-12-03T14:25:00.000Z","customType":"my-extension","content":"Injected context...","display":true}"#;
/// pi docs: session-format.md "LabelEntry".
const PI_LABEL: &str = r#"{"type":"label","id":"j0k1l2m3","parentId":"i9j0k1l2","timestamp":"2024-12-03T14:30:00.000Z","targetId":"a1b2c3d4","label":"checkpoint-1"}"#;
/// pi docs: session-format.md "SessionInfoEntry".
const PI_SESSION_INFO: &str = r#"{"type":"session_info","id":"k1l2m3n4","parentId":"j0k1l2m3","timestamp":"2024-12-03T14:35:00.000Z","name":"Refactor auth module"}"#;
/// pi docs: session-format.md "UsageEntry": the token fields of `usage` (pi's `Usage`).
const PI_USAGE_TOKENS: [&str; 5] = ["input", "output", "cacheRead", "cacheWrite", "totalTokens"];
/// pi docs: message-types.md "BashExecutionMessage": the required fields (`fullOutputPath` and
/// `excludeFromContext` are optional).
const PI_BASH_REQUIRED: [&str; 7] = [
    "role",
    "command",
    "output",
    "exitCode",
    "cancelled",
    "truncated",
    "timestamp",
];

// ---------------------------------------------------------------- helpers

/// Every wait of this file is bounded by this.
const LIMIT: Duration = Duration::from_secs(90);

/// Fail with the RED marker of a row (the H-wave convention of `oxa-parity/src/lib.rs`).
fn red(row: &str, reason: &str) -> ! {
    panic!("RED {row}: {reason}");
}

fn fake(script: Vec<ScriptTurn>, opts: FakeOpts) -> FakeServer {
    FakeServer::start(script, opts).expect("fake engine")
}

fn text(t: &str) -> ScriptTurn {
    ScriptTurn::text(t)
}

/// One finished `oxa` run.
struct Ran {
    code: Option<i32>,
    out: String,
    err: String,
}

impl Ran {
    fn ok(&self) -> &Ran {
        assert_eq!(
            self.code,
            Some(0),
            "oxa failed\nstdout: {}\nstderr: {}",
            self.out,
            self.err
        );
        self
    }
}

/// `oxa <args>` in `cwd` with `env.envs(url)` + `extra`, `stdin` fed and
/// closed; killed and failed after [`LIMIT`].
fn oxa_in(env: &TestEnv, cwd: &Path, url: &str, args: &[&str], stdin: &str, extra: &[(&str, &str)]) -> Ran {
    let mut child = Command::new(oxa_bin())
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .envs(env.envs(url))
        .envs(oxa_parity::note_off_env())
        .envs(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn oxa (build it first: cargo build -p oxa-cli -p oxa-fake)");
    if let Some(mut si) = child.stdin.take() {
        let _ = si.write_all(stdin.as_bytes());
    }
    let mut so = child.stdout.take().expect("stdout");
    let mut se = child.stderr.take().expect("stderr");
    let to = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = so.read_to_string(&mut s);
        s
    });
    let te = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = se.read_to_string(&mut s);
        s
    });
    let start = Instant::now();
    let status = loop {
        if let Some(st) = child.try_wait().expect("wait for oxa") {
            break st;
        }
        if start.elapsed() > LIMIT {
            let _ = child.kill();
            let _ = child.wait();
            panic!("oxa {args:?} did not finish within {LIMIT:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    Ran {
        code: status.code(),
        out: to.join().unwrap_or_default(),
        err: te.join().unwrap_or_default(),
    }
}

fn oxa(env: &TestEnv, url: &str, args: &[&str]) -> Ran {
    oxa_in(env, &env.work(), url, args, "", &[])
}

/// Every session file below `<agent dir>/sessions`, sorted.
fn session_files(env: &TestEnv) -> Vec<PathBuf> {
    rig::session_files(env)
}

/// The one session file of the env.
fn only_file(env: &TestEnv) -> PathBuf {
    let files = session_files(env);
    assert_eq!(files.len(), 1, "one session file: {files:?}");
    files[0].clone()
}

/// The JSON records of a session file; every line must be one JSON object.
fn lines(path: &Path) -> Vec<Value> {
    let text = std::fs::read_to_string(path).expect("read session file");
    text.lines()
        .map(|l| {
            let v: Value = serde_json::from_str(l).unwrap_or_else(|e| panic!("not JSON ({e}): {l}"));
            assert!(v.is_object(), "every line is a JSON object: {l}");
            v
        })
        .collect()
}

/// The records of `ty` (header excluded).
fn of_type(ls: &[Value], ty: &str) -> Vec<Value> {
    ls.iter().skip(1).filter(|l| l["type"] == ty).cloned().collect()
}

/// The `message` entries whose message role is `role`.
fn with_role(ls: &[Value], role: &str) -> Vec<Value> {
    ls.iter()
        .filter(|l| l["type"] == "message" && l["message"]["role"] == role)
        .cloned()
        .collect()
}

/// The entry with this id.
fn by_id<'a>(ls: &'a [Value], id: &str) -> &'a Value {
    ls.iter()
        .find(|l| l["type"] != "session" && l["id"] == id)
        .unwrap_or_else(|| panic!("no entry {id}"))
}

/// The message entry whose `message.content` is `text` (string content).
fn msg_id(ls: &[Value], text: &str) -> String {
    ls.iter()
        .find(|l| l["type"] == "message" && l["message"]["content"] == text)
        .and_then(|l| l["id"].as_str())
        .unwrap_or_else(|| panic!("no message entry {text:?}"))
        .to_string()
}

/// The ids from the root to `leaf`, following `parentId`.
fn path_to(ls: &[Value], leaf: &str) -> Vec<String> {
    let mut out = vec![leaf.to_string()];
    let mut cur = by_id(ls, leaf).clone();
    while let Some(p) = cur["parentId"].as_str() {
        out.push(p.to_string());
        cur = by_id(ls, p).clone();
        assert!(out.len() <= ls.len(), "a parentId cycle");
    }
    out.reverse();
    out
}

fn keys(v: &Value) -> BTreeSet<String> {
    v.as_object()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default()
}

fn doc(s: &str) -> Value {
    serde_json::from_str(s).expect("pi documented example parses")
}

/// pi's entry id: 8 lowercase hex characters.
fn is_hex8(s: &str) -> bool {
    s.len() == 8
        && s.bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

/// A lowercase uuid v4 text (`8-4-4-4-12`, version nibble 4).
fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_digit() || (b'a'..=b'f').contains(c),
        })
        && b[14] == b'4'
}

/// `YYYY-MM-DDTHH:MM:SS.mmmZ` -> Unix milliseconds (pi's ISO-8601 entry timestamps).
fn iso_ms(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    let shape = b.len() == 24
        && b.iter().enumerate().all(|(i, c)| match i {
            4 | 7 => *c == b'-',
            10 => *c == b'T',
            13 | 16 => *c == b':',
            19 => *c == b'.',
            23 => *c == b'Z',
            _ => c.is_ascii_digit(),
        });
    if !shape {
        return None;
    }
    let n = |a: usize, z: usize| s[a..z].parse::<i64>().ok();
    let (y, m, d) = (n(0, 4)?, n(5, 7)?, n(8, 10)?);
    let (hh, mm, ss, ms) = (n(11, 13)?, n(14, 16)?, n(17, 19)?, n(20, 23)?);
    // days from civil (proleptic Gregorian).
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(((days * 24 + hh) * 60 + mm) * 60 * 1000 + ss * 1000 + ms).ok()
}

/// The generating requests (`max_tokens > 0`), in arrival order.
fn generating(server: &FakeServer) -> Vec<Recorded> {
    let mut reqs = server.requests();
    reqs.sort_by_key(|r| r.seq);
    reqs.retain(|r| r.max_tokens > 0);
    reqs
}

fn contents(r: &Recorded) -> Vec<String> {
    r.messages
        .iter()
        .map(|m| m.content.clone().unwrap_or_default())
        .collect()
}

/// A global extension: `body` is the factory body of `export default function (oxa) {..}`.
fn extension(env: &TestEnv, name: &str, body: &str) {
    env.write(
        &format!("home/.oxa/agent/extensions/{name}.js"),
        &format!("export default function (oxa) {{\n{body}\n}}\n"),
    );
}

/// The `/goto <text> [summary]` command: navigate the tree to the message entry whose content is
/// `<text>` (the leaf moves there), with a branch summary when the second word is `summary`.
const GOTO: &str = r#"oxa.registerCommand('goto', {description: 'move the leaf', handler: (args, ctx) => {
  const words = args.split('|');
  const all = ctx.sessionManager.getEntries();
  const t = all.find(e => e.type === 'message' && e.content === words[0]);
  ctx.navigateTree(t.id, {summarize: words[1] === 'summary'});
}});"#;

/// A line-oriented driver of `oxa --mode rpc` with bounded waits; the child is killed on drop.
struct Rpc {
    child: Child,
    rx: Receiver<Value>,
    next: u64,
    events: Vec<Value>,
}

impl Rpc {
    fn start(env: &TestEnv, url: &str) -> Rpc {
        let mut child = Command::new(oxa_bin())
            .args(["--mode", "rpc"])
            .current_dir(env.work())
            .env_clear()
            .envs(env.envs(url))
            .envs(oxa_parity::note_off_env())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn oxa --mode rpc");
        let stdout = child.stdout.take().expect("rpc stdout");
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Ok(v) = serde_json::from_str::<Value>(&line) {
                    if tx.send(v).is_err() {
                        break;
                    }
                }
            }
        });
        Rpc {
            child,
            rx,
            next: 0,
            events: Vec::new(),
        }
    }

    fn next_record(&mut self) -> Value {
        self.rx.recv_timeout(LIMIT).unwrap_or_else(|_| {
            panic!(
                "no rpc record within {LIMIT:?}; events so far: {:#?}",
                self.events
            )
        })
    }

    /// Send one command and return its response; events read meanwhile are kept.
    fn call(&mut self, mut cmd: Value) -> Value {
        self.next += 1;
        let id = format!("fmt-{}", self.next);
        cmd["id"] = json!(id);
        {
            let stdin = self.child.stdin.as_mut().expect("rpc stdin");
            writeln!(stdin, "{cmd}").expect("write rpc command");
            stdin.flush().expect("flush rpc command");
        }
        loop {
            let v = self.next_record();
            if v["type"] == "response" && v["id"] == json!(id) {
                return v;
            }
            self.events.push(v);
        }
    }

    fn ok(&mut self, cmd: Value) -> Value {
        let r = self.call(cmd.clone());
        assert_eq!(r["success"], json!(true), "{cmd} failed: {r}");
        r["data"].clone()
    }

    fn settle(&mut self) {
        if let Some(i) = self.events.iter().position(|e| e["type"] == "agent_settled") {
            self.events.remove(i);
            return;
        }
        loop {
            let v = self.next_record();
            if v["type"] == "agent_settled" {
                return;
            }
            self.events.push(v);
        }
    }

    /// A prompt that starts a run; waits for its `agent_settled`.
    fn prompt(&mut self, text: &str) {
        self.ok(json!({"type": "prompt", "message": text}));
        self.settle();
    }

    /// An extension command (`/name args`): answered once its handler returned, no run.
    fn command(&mut self, text: &str) {
        self.ok(json!({"type": "prompt", "message": text}));
    }

    fn file(&mut self) -> PathBuf {
        let st = self.ok(json!({"type": "get_state"}));
        PathBuf::from(st["sessionFile"].as_str().expect("sessionFile"))
    }

    /// Close stdin (shutdown) and return the exit code.
    fn finish(mut self) -> Option<i32> {
        drop(self.child.stdin.take());
        let start = Instant::now();
        loop {
            if let Some(st) = self.child.try_wait().expect("wait for oxa") {
                return st.code();
            }
            assert!(
                start.elapsed() < LIMIT,
                "oxa --mode rpc did not exit after shutdown"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Rpc {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// A session of three turns (alpha, beta, gamma) over RPC, with the `/goto` command loaded.
fn three_turns(env: &TestEnv, server: &FakeServer) -> (Rpc, PathBuf) {
    extension(env, "goto", GOTO);
    let mut rpc = Rpc::start(env, &server.url());
    rpc.prompt("alpha question");
    rpc.prompt("beta question");
    let file = rpc.file();
    (rpc, file)
}

// ---------------------------------------------------------------- rows

/// pi docs: session-format.md "SessionHeader": the first line is
/// `{"type":"session","version":3,"id":"uuid","timestamp":..,"cwd":..}`, metadata only (no
/// `id`/`parentId` tree fields), plus `parentSession` for a session made by `/fork` or `/clone`;
/// "File Location": `<timestamp>_<session-id>.jsonl`, the id a UUID by default.
#[test]
fn fmt_001() {
    let env = TestEnv::new();
    let server = fake(vec![text("one"), text("two")], FakeOpts::default());
    oxa(&env, &server.url(), &["-p", "hello"]).ok();
    let file = only_file(&env);
    let h = lines(&file)[0].clone();
    assert_eq!(keys(&h), keys(&doc(PI_HEADER)), "pi's header fields: {h}");
    assert_eq!(h["type"], "session");
    assert_eq!(h["version"], 3);
    let id = h["id"].as_str().expect("id");
    assert!(is_uuid(id), "the session id is a uuid v4: {id}");
    let name = file.file_name().unwrap().to_string_lossy().into_owned();
    assert!(name.ends_with(&format!("_{id}.jsonl")), "{name} names the id");
    assert!(iso_ms(h["timestamp"].as_str().unwrap_or("")).is_some(), "{h}");
    assert_eq!(h["cwd"], json!(env.work().to_string_lossy()), "{h}");

    // A fork: the same fields plus `parentSession` = the source file.
    let src = file.to_string_lossy().into_owned();
    oxa(&env, &server.url(), &["-p", "--fork", &src, "forked"]).ok();
    let forked: Vec<PathBuf> = session_files(&env).into_iter().filter(|f| *f != file).collect();
    assert_eq!(forked.len(), 1, "{forked:?}");
    let fh = lines(&forked[0])[0].clone();
    assert_eq!(keys(&fh), keys(&doc(PI_HEADER_PARENT)), "{fh}");
    assert_eq!(fh["parentSession"], json!(src));
    assert_ne!(fh["id"], h["id"], "a new id");
}

/// pi docs: session-format.md "Entry Base": every entry but the header has `type`, `id` ("Usually
/// an 8-char hex ID"), `parentId` (`null` for the root) and an ISO-8601 `timestamp`; "Messages":
/// "The nested message timestamp is a Unix timestamp in milliseconds".
#[test]
fn fmt_002() {
    let env = TestEnv::new();
    std::fs::write(env.work().join("notes.md"), "the notes\n").unwrap();
    let server = fake(
        vec![
            ScriptTurn::tool("call_1", "read", r#"{"path":"notes.md"}"#),
            text("read it"),
            text("second answer"),
        ],
        FakeOpts::default(),
    );
    oxa(&env, &server.url(), &["-p", "first question"]).ok();
    oxa(&env, &server.url(), &["-c", "-p", "second question"]).ok();
    let ls = lines(&only_file(&env));
    let mut seen: Vec<String> = Vec::new();
    let mut last_ms = 0;
    for (i, e) in ls.iter().enumerate().skip(1) {
        assert!(e["type"].is_string(), "type: {e}");
        let id = e["id"].as_str().unwrap_or_else(|| panic!("id: {e}"));
        assert!(is_hex8(id), "8 lowercase hex: {e}");
        assert!(!seen.iter().any(|s| s == id), "ids are unique: {id}");
        if i == 1 {
            assert_eq!(e["parentId"], Value::Null, "the first entry is the root: {e}");
        } else {
            let p = e["parentId"].as_str().unwrap_or_else(|| panic!("parentId: {e}"));
            assert!(
                seen.iter().any(|s| s == p),
                "parentId names an earlier entry: {e}"
            );
        }
        let ms = iso_ms(e["timestamp"].as_str().unwrap_or(""))
            .unwrap_or_else(|| panic!("ISO-8601 timestamp: {e}"));
        assert!(ms >= last_ms, "timestamps do not go back: {e}");
        last_ms = ms;
        if e["type"] == "message" {
            assert_eq!(
                e["message"]["timestamp"].as_u64(),
                Some(ms),
                "the nested message timestamp is the same instant in Unix ms: {e}"
            );
        }
        seen.push(id.to_string());
    }
    assert!(seen.len() >= 8, "a header and the entries of two runs: {ls:#?}");
}

/// pi docs: session-format.md "SessionMessageEntry" (`message` = an AgentMessage of role user,
/// assistant or toolResult; message-types.md).
/// invariant: W2: an assistant entry also stores the exact wire bytes as streamed (`wire`: the
/// untrimmed content, `reasoning_content`, each tool call's raw `arguments`), and a resumed
/// session sends exactly those bytes again.
#[test]
fn fmt_003() {
    let env = TestEnv::new();
    std::fs::write(env.work().join("notes.md"), "the notes\n").unwrap();
    let raw_args = "{ \"path\" :  \"notes.md\" }";
    let mut first = ScriptTurn::tool("call_1", "read", raw_args);
    first.reasoning = Some("look at the notes first  ".to_string());
    first.text = Some("  checking the notes\n".to_string());
    let server = fake(
        vec![first, text("done.  \n\n"), text("again")],
        FakeOpts::default(),
    );
    oxa(&env, &server.url(), &["-p", "read the notes"]).ok();
    let file = only_file(&env);
    let ls = lines(&file);

    let users = with_role(&ls, "user");
    assert_eq!(users.len(), 1, "{ls:#?}");
    assert_eq!(users[0]["message"]["content"], "read the notes");
    let assistants = with_role(&ls, "assistant");
    assert_eq!(assistants.len(), 2, "{ls:#?}");
    let w = &assistants[0]["wire"];
    assert_eq!(w["role"], "assistant");
    assert_eq!(w["content"], "  checking the notes\n", "untrimmed content: {w}");
    assert_eq!(w["reasoning_content"], "look at the notes first  ", "{w}");
    assert_eq!(w["tool_calls"][0]["id"], "call_1");
    assert_eq!(w["tool_calls"][0]["name"], "read");
    assert_eq!(w["tool_calls"][0]["arguments"], raw_args, "raw arguments: {w}");
    assert_eq!(assistants[1]["wire"]["content"], "done.  \n\n", "untrimmed");
    let results = with_role(&ls, "toolResult");
    assert_eq!(results.len(), 1, "{ls:#?}");
    assert_eq!(results[0]["message"]["toolCallId"], "call_1");
    assert_eq!(results[0]["wire"]["role"], "tool");
    assert_eq!(results[0]["wire"]["tool_call_id"], "call_1");

    // Resume: the stored assistant bytes go back on the wire exactly.
    oxa(&env, &server.url(), &["-c", "-p", "once more"]).ok();
    let last = generating(&server).pop().expect("the resumed request");
    let sent: Vec<_> = last
        .messages
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .collect();
    assert_eq!(sent.len(), 2, "{:?}", contents(&last));
    assert_eq!(sent[0].content.as_deref(), Some("  checking the notes\n"));
    assert_eq!(sent[0].tool_calls[0].args_raw, raw_args);
    if let Some(r) = &sent[0].reasoning {
        assert_eq!(r, "look at the notes first  ");
    }
    assert_eq!(sent[1].content.as_deref(), Some("done.  \n\n"));
}

/// pi docs: session-format.md "SessionMessageEntry": the first request persists one system
/// message with every prompt section and tool declaration.
/// invariant: W3: that leading system message is the frozen prefix of the epoch (`sections`,
/// oxa's `toolsJson` and `sha256`) and is replayed from the file byte for byte on resume, never
/// rebuilt (a context file added before the resume does not change the resumed prefix).
#[test]
fn fmt_004() {
    let env = TestEnv::new();
    let server = fake(vec![text("one"), text("two")], FakeOpts::default());
    oxa(&env, &server.url(), &["-p", "first"]).ok();
    let file = only_file(&env);
    let ls = lines(&file);
    let first = &ls[1];
    assert_eq!(first["type"], "message", "the first entry: {first}");
    assert_eq!(first["parentId"], Value::Null);
    let m = &first["message"];
    assert_eq!(m["role"], "system", "{first}");
    let sections = m["sections"].as_object().expect("sections by name");
    assert!(!sections.is_empty());
    let req0 = generating(&server)[0].clone();
    for (name, t) in sections {
        let t = t.as_str().unwrap_or_else(|| panic!("section {name} is text"));
        assert!(req0.system.contains(t), "section {name} is in the system prompt");
    }
    assert_eq!(
        m["toolsJson"],
        json!(req0.tools_json),
        "the stored tool declarations"
    );
    let sha = m["sha256"].as_str().unwrap_or("");
    assert!(
        sha.len() == 64 && sha.bytes().all(|c| c.is_ascii_hexdigit()),
        "{sha}"
    );

    // A context file appears between the runs; the resume replays the stored prefix.
    std::fs::write(env.work().join("AGENTS.md"), "PARITY-LATE-CONTEXT\n").unwrap();
    oxa(&env, &server.url(), &["-c", "-p", "second"]).ok();
    let req1 = generating(&server)[1].clone();
    assert_eq!(
        req1.system, req0.system,
        "the system prompt is replayed byte for byte"
    );
    assert_eq!(
        req1.tools_json, req0.tools_json,
        "the tools are replayed byte for byte"
    );
    let systems = with_role(&lines(&file), "system");
    assert_eq!(systems.len(), 1, "no second prefix on resume");
}

/// pi docs: session-format.md "SessionMessageEntry": later changes persist as system messages
/// that patch `sections` by name and list `toolsAdded`/`toolsRemoved`.
/// invariant: W3: no mid-session prompt change. The prompt of an epoch never changes (a context
/// file written mid-session is not picked up); a change (here a model switch) is a new epoch: a
/// complete system entry (every section, the whole tool list), never a patch.
#[test]
fn fmt_005() {
    let env = TestEnv::new();
    let server = fake(
        vec![text("one"), text("two"), text("three")],
        FakeOpts {
            model: "model-a".to_string(),
            models: vec![
                FakeModel::moe("model-a", 131_072),
                FakeModel::moe("model-b", 131_072),
            ],
            ..FakeOpts::default()
        },
    );
    let mut rpc = Rpc::start(&env, &server.url());
    rpc.prompt("first");
    std::fs::write(env.work().join("AGENTS.md"), "PARITY-MID-SESSION\n").unwrap();
    rpc.prompt("second");
    rpc.ok(json!({"type": "set_model", "provider": "oxe", "modelId": "model-b"}));
    rpc.prompt("third");
    let file = rpc.file();
    assert_eq!(rpc.finish(), Some(0));

    let reqs = generating(&server);
    assert_eq!(reqs.len(), 3);
    assert_eq!(reqs[1].system, reqs[0].system, "no change inside an epoch");
    assert_eq!(reqs[1].tools_json, reqs[0].tools_json);
    let ls = lines(&file);
    let systems = with_role(&ls, "system");
    assert_eq!(systems.len(), 2, "one system entry per epoch: {ls:#?}");
    let pos = |id: &Value| ls.iter().position(|l| l["id"] == *id).unwrap();
    let change: Vec<Value> = of_type(&ls, "model_change")
        .into_iter()
        .filter(|c| c["modelId"] == "model-b")
        .collect();
    assert_eq!(change.len(), 1, "{ls:#?}");
    assert!(
        pos(&change[0]["id"]) < pos(&systems[1]["id"]),
        "the epoch follows the switch"
    );
    for s in &systems {
        let m = &s["message"];
        assert_eq!(
            keys(m),
            ["role", "sections", "sha256", "timestamp", "toolsJson"]
                .iter()
                .map(|k| k.to_string())
                .collect::<BTreeSet<_>>(),
            "a complete prefix, no toolsAdded/toolsRemoved patch: {m}"
        );
        assert!(
            m["sections"].as_object().unwrap().values().all(Value::is_string),
            "no section is removed by a null patch: {m}"
        );
    }
    let a: BTreeSet<_> = keys(&systems[0]["message"]["sections"]);
    let b: BTreeSet<_> = keys(&systems[1]["message"]["sections"]);
    assert!(
        a.is_subset(&b),
        "the new epoch carries every section: {a:?} vs {b:?}"
    );
    assert_eq!(reqs[2].model, "model-b");
    assert_eq!(
        reqs[2].tools_json,
        systems[1]["message"]["toolsJson"].as_str().unwrap()
    );
    for t in systems[1]["message"]["sections"].as_object().unwrap().values() {
        assert!(
            reqs[2].system.contains(t.as_str().unwrap()),
            "the new epoch's prompt is sent"
        );
    }
}

/// pi docs: session-format.md "ModelChangeEntry":
/// `{"type":"model_change",..,"provider":"openai","modelId":"gpt-4o"}`, emitted when the model is
/// switched mid-session.
#[test]
fn fmt_006() {
    let env = TestEnv::new();
    let server = fake(
        vec![text("one"), text("two")],
        FakeOpts {
            model: "model-a".to_string(),
            models: vec![
                FakeModel::moe("model-a", 131_072),
                FakeModel::moe("model-b", 131_072),
            ],
            ..FakeOpts::default()
        },
    );
    let mut rpc = Rpc::start(&env, &server.url());
    rpc.prompt("first");
    rpc.ok(json!({"type": "set_model", "provider": "oxe", "modelId": "model-b"}));
    rpc.prompt("second");
    let file = rpc.file();
    assert_eq!(rpc.finish(), Some(0));
    let ls = lines(&file);
    // The session's start records its model too; the switch is the entry naming model-b.
    let changes: Vec<Value> = of_type(&ls, "model_change")
        .into_iter()
        .filter(|c| c["modelId"] == "model-b")
        .collect();
    assert_eq!(changes.len(), 1, "{ls:#?}");
    let c = &changes[0];
    if keys(c) != keys(&doc(PI_MODEL_CHANGE)) {
        red(
            "FMT-006",
            &format!(
                "the model_change entry lacks pi's documented `provider` field: {c} (oxa-session/src/store.rs body_fields, EntryBody::ModelChange)"
            ),
        );
    }
    assert_eq!(c["provider"], "oxe", "the one provider type: the engine");
    assert_eq!(generating(&server)[1].model, "model-b");
}

/// pi docs: session-format.md "ThinkingLevelChangeEntry":
/// `{"type":"thinking_level_change",..,"thinkingLevel":"high"}`, emitted when the user changes
/// the thinking level.
#[test]
fn fmt_007() {
    let env = TestEnv::new();
    let server = fake(vec![text("one"), text("two")], FakeOpts::default());
    let mut rpc = Rpc::start(&env, &server.url());
    rpc.ok(json!({"type": "set_thinking_level", "level": "off"}));
    rpc.prompt("first");
    rpc.ok(json!({"type": "set_thinking_level", "level": "high"}));
    rpc.prompt("second");
    let file = rpc.file();
    assert_eq!(rpc.finish(), Some(0));
    let ls = lines(&file);
    let changes = of_type(&ls, "thinking_level_change");
    let last = changes
        .last()
        .unwrap_or_else(|| panic!("a thinking_level_change entry: {ls:#?}"));
    assert_eq!(keys(last), keys(&doc(PI_THINKING_CHANGE)), "{last}");
    assert_eq!(last["thinkingLevel"], "high");
    let reqs = generating(&server);
    assert!(
        !reqs[0].thinking && reqs[1].thinking,
        "the level reaches the request (W6)"
    );
}

/// pi docs: session-format.md "UsageEntry": `{"type":"usage",..,"kind":..,"usage":{"input",
/// "output","cacheRead","cacheWrite","totalTokens",..}}` records model usage that is not an
/// assistant message; consumers treat any `kind` as usage. PARITY note: the kinds oxa writes are
/// `chat`, `compaction`, `branch_summary`, `prefill`; a subagent child runs without a session
/// file, so no `subagent` kind is recorded.
#[test]
fn fmt_008() {
    // invariant: usage is recorded per engine request of this session; a subagent child has no session file.
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"compaction":{"keepRecentTokens":1}}"#,
    );
    env.write(
        "home/.oxa/agent/agents/scout.md",
        "---\nname: scout\ndescription: Looks around\ntools: read\n---\nYou are a scout.\n",
    );
    let server = fake(
        vec![
            text("alpha answer"),
            text("beta answer"),
            text("BRANCH SUMMARY"),
            text("gamma answer"),
            text("COMPACT SUMMARY"),
            ScriptTurn::tool("c1", "subagent", r#"{"agent":"scout","task":"look"}"#),
            text("child result"),
            text("parent done"),
        ],
        FakeOpts::default(),
    );
    let (mut rpc, file) = three_turns(&env, &server);
    rpc.command("/goto alpha answer|summary");
    rpc.prompt("gamma question");
    rpc.ok(json!({"type": "compact"}));
    rpc.prompt("delegate a look");
    assert_eq!(rpc.finish(), Some(0));

    let ls = lines(&file);
    let usage = of_type(&ls, "usage");
    for u in &usage {
        assert_eq!(
            keys(u),
            ["id", "kind", "parentId", "timestamp", "type", "usage"]
                .iter()
                .map(|k| k.to_string())
                .collect::<BTreeSet<_>>(),
            "{u}"
        );
        for k in PI_USAGE_TOKENS {
            assert!(u["usage"][k].is_u64(), "usage.{k}: {u}");
        }
        let (i, o) = (
            u["usage"]["input"].as_u64().unwrap(),
            u["usage"]["output"].as_u64().unwrap(),
        );
        assert_eq!(u["usage"]["totalTokens"].as_u64(), Some(i + o), "{u}");
    }
    let kinds: BTreeSet<String> = usage
        .iter()
        .map(|u| u["kind"].as_str().unwrap_or_default().to_string())
        .collect();
    // The counts are the engine's: the compaction usage is the summary request's.
    let reqs = server.requests();
    let summary_req = reqs
        .iter()
        .find(|r| r.kind == "compaction")
        .expect("a compaction request");
    let cu = usage
        .iter()
        .find(|u| u["kind"] == "compaction")
        .unwrap_or_else(|| panic!("a compaction usage entry: {kinds:?}"));
    assert_eq!(
        cu["usage"]["input"].as_u64(),
        summary_req.usage.map(|u| u64::from(u.prompt)),
        "{cu}"
    );
    for k in ["chat", "compaction", "branch_summary", "prefill"] {
        assert!(kinds.contains(k), "a usage entry of kind {k}: {kinds:?}");
    }
    // The subagent call above ran a child without a session file: the parent records no usage.
    assert!(
        !kinds.contains("subagent"),
        "no usage entry of kind `subagent` after a subagent call: {kinds:?}"
    );
}

/// pi docs: session-format.md "CompactionEntry" (`summary`, `firstKeptEntryId`, `tokensBefore`,
/// `details`, `fromHook`, and a `systemMessage` checkpoint).
/// invariant: W5: the head after compaction is `[system, user:<summary>, tail]`. The checkpoint
/// of the prompt is the epoch's prefix entry (oxa-session/README.md: pi's `systemMessage` field
/// is not written), so the system of the next request is the stored prefix, unchanged.
#[test]
fn fmt_009() {
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"compaction":{"keepRecentTokens":1}}"#,
    );
    let server = fake(
        vec![
            text("alpha answer"),
            text("beta answer"),
            text("THE SUMMARY"),
            text("after"),
        ],
        FakeOpts::default(),
    );
    let (mut rpc, file) = three_turns(&env, &server);
    rpc.ok(json!({"type": "compact"}));
    rpc.prompt("after compaction");
    assert_eq!(rpc.finish(), Some(0));
    let ls = lines(&file);
    let cs = of_type(&ls, "compaction");
    assert_eq!(cs.len(), 1, "{ls:#?}");
    let c = &cs[0];
    for k in [
        "summary",
        "firstKeptEntryId",
        "tokensBefore",
        "details",
        "fromHook",
    ] {
        assert!(c.get(k).is_some(), "compaction.{k}: {c}");
    }
    assert!(c["summary"].as_str().unwrap_or("").contains("THE SUMMARY"), "{c}");
    let kept = c["firstKeptEntryId"].as_str().expect("firstKeptEntryId");
    let _ = by_id(&ls, kept);
    assert!(c["tokensBefore"].as_u64().unwrap_or(0) > 0, "{c}");
    assert_eq!(c["fromHook"], false);
    assert!(
        c["details"]["readFiles"].is_array() && c["details"]["modifiedFiles"].is_array(),
        "{c}"
    );
    // The head: the same prefix, then the summary as a user message.
    let reqs = generating(&server);
    let before = &reqs[1];
    let after = reqs.last().unwrap();
    assert_eq!(after.system, before.system, "the prefix entry is the checkpoint");
    assert_eq!(after.tools_json, before.tools_json);
    assert_eq!(after.messages[0].role, Role::User);
    assert_eq!(
        after.messages[0].content.as_deref(),
        c["summary"].as_str(),
        "user:<summary> first"
    );
    assert_eq!(
        contents(after).last().map(String::as_str),
        Some("after compaction")
    );
}

/// pi docs: session-format.md "ContextEditEntry": `{"type":"context_edit",..,"targetId":..,
/// "replacement":null}` changes only future model context; the target stays in raw history;
/// `null` omits the target, a string replaces its content. The entries are appended to an oxa
/// file here, because oxa itself never writes a `context_edit`.
#[test]
fn fmt_010() {
    // invariant: W1: a stored context edit changes the wire on load (a rebase); oxa never writes one.
    let env = TestEnv::new();
    let server = fake(
        vec![text("alpha answer"), text("beta answer"), text("after edit")],
        FakeOpts::default(),
    );
    oxa(&env, &server.url(), &["-p", "alpha question"]).ok();
    oxa(&env, &server.url(), &["-c", "-p", "beta question"]).ok();
    let file = only_file(&env);
    let ls = lines(&file);
    let alpha_a = msg_id(&ls, "alpha answer");
    let beta_q = msg_id(&ls, "beta question");
    let leaf = ls.last().unwrap()["id"].as_str().unwrap().to_string();
    let edit1 = json!({"type":"context_edit","id":"ce000001","parentId":leaf,
        "timestamp":"2026-01-01T00:00:00.000Z","targetId":alpha_a,"replacement":"EDITED ANSWER"});
    let edit2 = json!({"type":"context_edit","id":"ce000002","parentId":"ce000001",
        "timestamp":"2026-01-01T00:00:01.000Z","targetId":beta_q,"replacement":null});
    let raw_before = std::fs::read_to_string(&file).unwrap();
    std::fs::write(&file, format!("{raw_before}{edit1}\n{edit2}\n")).unwrap();

    oxa(
        &env,
        &server.url(),
        &["--mode", "json", "-c", "-p", "after the edit"],
    )
    .ok();
    let last = generating(&server).pop().unwrap();
    assert_eq!(
        contents(&last),
        vec!["alpha question", "EDITED ANSWER", "beta answer", "after the edit"],
        "the replacement and the omission reach the wire"
    );
    // The raw history keeps the targets unchanged.
    let after = lines(&file);
    assert_eq!(by_id(&after, &alpha_a)["message"]["content"], "alpha answer");
    assert_eq!(by_id(&after, &beta_q)["message"]["content"], "beta question");
}

/// pi docs: session-format.md "BranchSummaryEntry": `{"type":"branch_summary",..,"fromId":..,
/// "summary":..}` (+ optional `details`, `fromHook`), written when `/tree` leaves a branch with a
/// summary; `parentId` is the entry the new branch continues from, `fromId` the previous leaf.
#[test]
fn fmt_011() {
    let env = TestEnv::new();
    let server = fake(
        vec![
            text("alpha answer"),
            text("beta answer"),
            text("BRANCH SUMMARY"),
            text("gamma answer"),
        ],
        FakeOpts::default(),
    );
    let (mut rpc, file) = three_turns(&env, &server);
    let old_leaf = lines(&file).last().unwrap()["id"].as_str().unwrap().to_string();
    rpc.command("/goto alpha answer|summary");
    rpc.prompt("gamma question");
    assert_eq!(rpc.finish(), Some(0));
    let ls = lines(&file);
    let bs = of_type(&ls, "branch_summary");
    assert_eq!(bs.len(), 1, "{ls:#?}");
    let b = &bs[0];
    for k in keys(&doc(PI_BRANCH_SUMMARY)) {
        assert!(b.get(&k).is_some(), "branch_summary.{k}: {b}");
    }
    let extra: Vec<String> = keys(b)
        .difference(&keys(&doc(PI_BRANCH_SUMMARY)))
        .cloned()
        .collect();
    assert!(
        extra
            .iter()
            .all(|k| k == "details" || k == "fromHook" || k == "usage"),
        "only pi's optional fields besides: {extra:?}"
    );
    assert!(
        b["summary"].as_str().unwrap_or("").contains("BRANCH SUMMARY"),
        "{b}"
    );
    assert_eq!(b["fromId"], json!(old_leaf), "fromId = the leaf that was left");
    let alpha_a = msg_id(&ls, "alpha answer");
    assert_eq!(
        b["parentId"],
        json!(alpha_a),
        "the new branch continues at alpha's answer"
    );
    // The summary is in context at its position on the new branch.
    let last = generating(&server).pop().unwrap();
    let c = contents(&last);
    assert_eq!(c[..2], ["alpha question", "alpha answer"]);
    assert!(c[2].contains("BRANCH SUMMARY"), "{c:?}");
    assert_eq!(c.last().map(String::as_str), Some("gamma question"));
    assert!(!c.iter().any(|t| t.contains("beta")), "{c:?}");
}

/// pi docs: session-format.md "CustomEntry": `{"type":"custom",..,"customType":..,"data":..}`,
/// extension state that does NOT participate in LLM context (`pi.appendEntry`).
#[test]
fn fmt_012() {
    let env = TestEnv::new();
    extension(
        &env,
        "state",
        "oxa.on('agent_end', (e, ctx) => { oxa.appendEntry('my-extension', {count: 42, marker: 'PARITY-STATE'}); });",
    );
    let server = fake(vec![text("one"), text("two")], FakeOpts::default());
    oxa(&env, &server.url(), &["-p", "first"]).ok();
    oxa(&env, &server.url(), &["-c", "-p", "second"]).ok();
    let ls = lines(&only_file(&env));
    // The extension's entries (oxa may keep state of its own as custom entries too).
    let cs: Vec<Value> = of_type(&ls, "custom")
        .into_iter()
        .filter(|c| c["customType"] == "my-extension")
        .collect();
    assert_eq!(cs.len(), 2, "one per run: {ls:#?}");
    let c = &cs[0];
    assert_eq!(keys(c), keys(&doc(PI_CUSTOM)), "{c}");
    assert_eq!(c["data"], json!({"count": 42, "marker": "PARITY-STATE"}));
    let last = generating(&server).pop().unwrap();
    assert!(
        !format!("{} {:?}", last.system, contents(&last)).contains("PARITY-STATE"),
        "custom entries are never in context"
    );
}

/// pi docs: session-format.md "CustomMessageEntry": `{"type":"custom_message",..,"customType",
/// "content","display"}` + optional `details` ("not sent to LLM"); extension messages that DO
/// participate in LLM context (`pi.sendMessage`).
#[test]
fn fmt_013() {
    let env = TestEnv::new();
    extension(
        &env,
        "msg",
        "oxa.registerCommand('inject', {description: 'inject', handler: (args, ctx) => {
  oxa.sendMessage({customType: 'my-extension', content: 'PARITY-INJECTED', display: true, details: {hidden: 'PARITY-DETAILS'}}, {triggerTurn: true});
}});",
    );
    let server = fake(vec![text("one"), text("two"), text("three")], FakeOpts::default());
    let mut rpc = Rpc::start(&env, &server.url());
    rpc.prompt("first");
    rpc.command("/inject");
    rpc.settle();
    let file = rpc.file();
    assert_eq!(rpc.finish(), Some(0));
    oxa(&env, &server.url(), &["-c", "-p", "third"]).ok();
    let ls = lines(&file);
    let cms = of_type(&ls, "custom_message");
    assert_eq!(cms.len(), 1, "{ls:#?}");
    let c = &cms[0];
    let mut want = keys(&doc(PI_CUSTOM_MESSAGE));
    want.insert("details".to_string());
    assert_eq!(keys(c), want, "{c}");
    assert_eq!(c["customType"], "my-extension");
    assert_eq!(c["content"], "PARITY-INJECTED");
    assert_eq!(c["display"], true);
    assert_eq!(c["details"], json!({"hidden": "PARITY-DETAILS"}));
    let reqs = generating(&server);
    assert_eq!(reqs.len(), 3);
    // In context at its position, also after a resume; the details never reach the engine.
    assert_eq!(contents(&reqs[1]), vec!["first", "one", "PARITY-INJECTED"]);
    assert_eq!(
        contents(&reqs[2]),
        vec!["first", "one", "PARITY-INJECTED", "two", "third"]
    );
    for r in &reqs {
        assert!(
            !String::from_utf8_lossy(&r.body).contains("PARITY-DETAILS"),
            "details are not sent"
        );
    }
    // W1: an append; everything sent before stays a prefix.
    for w in reqs.windows(2) {
        let n = w[0].messages.len();
        assert_eq!(w[1].messages[..n], w[0].messages[..], "an append");
    }
}

/// pi docs: session-format.md "LabelEntry": `{"type":"label",..,"targetId":..,"label":..}`; set
/// `label` to `undefined` to clear it.
#[test]
fn fmt_014() {
    let env = TestEnv::new();
    extension(
        &env,
        "mark",
        r#"oxa.registerCommand('mark', {description: 'label', handler: (args, ctx) => {
  const t = ctx.sessionManager.getEntries().find(e => e.type === 'message' && e.content === 'alpha question');
  oxa.setLabel(t.id, args === '' ? null : args);
}});"#,
    );
    let server = fake(vec![text("alpha answer")], FakeOpts::default());
    let mut rpc = Rpc::start(&env, &server.url());
    rpc.prompt("alpha question");
    rpc.command("/mark checkpoint-1");
    let file = rpc.file();
    let ls = lines(&file);
    let target = msg_id(&ls, "alpha question");
    let tree = rpc.ok(json!({"type": "get_tree"}));
    assert!(
        tree.to_string().contains("checkpoint-1"),
        "get_tree shows it: {tree}"
    );
    rpc.command("/mark");
    assert_eq!(rpc.finish(), Some(0));
    let labels = of_type(&lines(&file), "label");
    assert_eq!(labels.len(), 2, "set, then cleared");
    assert_eq!(keys(&labels[0]), keys(&doc(PI_LABEL)), "{}", labels[0]);
    assert_eq!(labels[0]["targetId"], json!(target));
    assert_eq!(labels[0]["label"], "checkpoint-1");
    assert_eq!(labels[1]["targetId"], json!(target));
    assert!(
        labels[1].get("label").is_none_or(Value::is_null),
        "a cleared label has no value: {}",
        labels[1]
    );
}

/// pi docs: session-format.md "SessionInfoEntry": `{"type":"session_info",..,"name":..}`, set via
/// `/name`, `--name`/`-n`, or `pi.setSessionName()`.
#[test]
fn fmt_015() {
    let env = TestEnv::new();
    let server = fake(vec![text("one"), text("two")], FakeOpts::default());
    oxa(&env, &server.url(), &["-p", "-n", "First name", "hello"]).ok();
    let file = only_file(&env);
    let info = of_type(&lines(&file), "session_info");
    assert_eq!(info.len(), 1);
    assert_eq!(keys(&info[0]), keys(&doc(PI_SESSION_INFO)), "{}", info[0]);
    assert_eq!(info[0]["name"], "First name");
    // `/name` (RPC set_session_name) on the continued session.
    let mut rpc = Rpc::start(&env, &server.url());
    rpc.ok(json!({"type": "switch_session", "sessionPath": file.to_string_lossy()}));
    rpc.ok(json!({"type": "set_session_name", "name": "Refactor auth module"}));
    assert_eq!(rpc.finish(), Some(0));
    let info = of_type(&lines(&file), "session_info");
    assert_eq!(info.len(), 2);
    assert_eq!(keys(&info[1]), keys(&doc(PI_SESSION_INFO)));
    assert_eq!(info[1]["name"], "Refactor auth module");
}

/// pi docs: message-types.md "BashExecutionMessage": `role: "bashExecution"`, `command`, `output`,
/// `exitCode`, `cancelled`, `truncated`, optional `excludeFromContext`, `timestamp`; "Unless
/// `excludeFromContext` is true, Pi converts this message to user-role text before the next
/// model request".
#[test]
fn fmt_016() {
    let env = TestEnv::new();
    let server = fake(vec![text("one"), text("two")], FakeOpts::default());
    let mut rpc = Rpc::start(&env, &server.url());
    rpc.prompt("first");
    let r = rpc.ok(json!({"type": "bash", "command": "echo PARITY-IN"}));
    assert_eq!(r["exitCode"], 0, "{r}");
    rpc.ok(json!({"type": "bash", "command": "echo PARITY-OUT", "excludeFromContext": true}));
    rpc.prompt("second");
    let file = rpc.file();
    assert_eq!(rpc.finish(), Some(0));
    let ls = lines(&file);
    let bash = with_role(&ls, "bashExecution");
    assert_eq!(bash.len(), 2, "{ls:#?}");
    let m = &bash[0]["message"];
    assert_eq!(m["command"], "echo PARITY-IN");
    assert_eq!(m["output"].as_str().map(str::trim_end), Some("PARITY-IN"), "{m}");
    assert_eq!(m["exitCode"], 0);
    assert_eq!(m["excludeFromContext"], false);
    assert!(m["timestamp"].is_u64());
    assert_eq!(bash[1]["message"]["excludeFromContext"], true);
    // In context unless excluded.
    let last = generating(&server).pop().unwrap();
    let sent = format!("{:?}", contents(&last));
    assert!(sent.contains("PARITY-IN"), "{sent}");
    assert!(!sent.contains("PARITY-OUT"), "{sent}");
    let missing: Vec<&str> = PI_BASH_REQUIRED
        .iter()
        .copied()
        .filter(|k| m.get(*k).is_none())
        .collect();
    if !missing.is_empty() {
        red(
            "FMT-016",
            &format!(
                "the bashExecution message lacks pi's required field(s) {missing:?}: {m} (oxa-session/src/store.rs body_fields, EntryBody::BashExecution)"
            ),
        );
    }
    assert_eq!(m["cancelled"], false);
    assert_eq!(m["truncated"], false);
}

/// pi docs: session-format.md "Tree Structure" (`id`/`parentId`; branching creates new children
/// of an earlier entry; the leaf is the current position) and "Context Building" (walk from the
/// leaf to the root; only the active path reaches the LLM). The leaf of a stored file is its last
/// entry.
/// invariant: W1: the wire is a pure fold of the active branch.
#[test]
fn fmt_017() {
    let env = TestEnv::new();
    let server = fake(
        vec![
            text("alpha answer"),
            text("beta answer"),
            text("gamma answer"),
            text("delta answer"),
        ],
        FakeOpts::default(),
    );
    let (mut rpc, file) = three_turns(&env, &server);
    rpc.command("/goto alpha answer");
    rpc.prompt("gamma question");
    assert_eq!(rpc.finish(), Some(0));
    let ls = lines(&file);
    let alpha_a = msg_id(&ls, "alpha answer");
    let beta_q = msg_id(&ls, "beta question");
    let gamma_q = msg_id(&ls, "gamma question");
    // Two children of the branch point: beta's branch and gamma's.
    let leaf = ls.last().unwrap()["id"].as_str().unwrap().to_string();
    let active = path_to(&ls, &leaf);
    assert!(
        active.contains(&gamma_q) && active.contains(&alpha_a),
        "{active:?}"
    );
    assert!(
        !active.contains(&beta_q),
        "beta's branch is not on the active path"
    );
    let beta_path = path_to(&ls, &beta_q);
    assert!(beta_path.contains(&alpha_a), "both branches share alpha");
    let gamma_req = generating(&server)[2].clone();
    assert_eq!(
        contents(&gamma_req),
        vec!["alpha question", "alpha answer", "gamma question"]
    );
    // A new process takes the last entry as the leaf and folds that branch only.
    oxa(&env, &server.url(), &["-c", "-p", "delta question"]).ok();
    let delta_req = generating(&server).pop().unwrap();
    assert_eq!(
        contents(&delta_req),
        vec![
            "alpha question",
            "alpha answer",
            "gamma question",
            "gamma answer",
            "delta question"
        ]
    );
    assert_eq!(
        delta_req.messages[..3],
        gamma_req.messages[..],
        "a pure fold (W1)"
    );
}

/// pi docs: session-format.md "Context Building": "The compaction summary replaces entries before
/// `firstKeptEntryId`" and the range from that entry on stays.
/// invariant: W5: the compaction is a rebase to `[user:<summary>, messages from firstKeptEntryId
/// on]`; a resumed session folds the same head.
#[test]
fn fmt_018() {
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"compaction":{"keepRecentTokens":1}}"#,
    );
    let server = fake(
        vec![
            text("alpha answer"),
            text("beta answer"),
            text("THE SUMMARY"),
            text("gamma answer"),
            text("delta answer"),
        ],
        FakeOpts::default(),
    );
    let (mut rpc, file) = three_turns(&env, &server);
    rpc.ok(json!({"type": "compact"}));
    rpc.prompt("gamma question");
    assert_eq!(rpc.finish(), Some(0));
    let ls = lines(&file);
    let c = of_type(&ls, "compaction").pop().expect("a compaction entry");
    let kept = c["firstKeptEntryId"].as_str().unwrap().to_string();
    let cpos = ls.iter().position(|l| l["id"] == c["id"]).unwrap();
    let kpos = ls.iter().position(|l| l["id"] == kept.as_str()).unwrap();
    assert!(kpos < cpos, "the kept range starts before the compaction entry");
    // The messages of the kept range, in order.
    let tail: Vec<String> = ls[kpos..cpos]
        .iter()
        .filter(|l| l["type"] == "message" && l["message"]["role"] != "system")
        .map(|l| l["wire"]["content"].as_str().unwrap_or_default().to_string())
        .collect();
    let dropped: Vec<String> = ls[1..kpos]
        .iter()
        .filter(|l| l["type"] == "message" && l["message"]["role"] != "system")
        .map(|l| l["wire"]["content"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(!dropped.is_empty(), "something was summarised: {ls:#?}");
    let mut want = vec![c["summary"].as_str().unwrap().to_string()];
    want.extend(tail.iter().cloned());
    want.push("gamma question".to_string());
    let gamma = generating(&server).pop().unwrap();
    assert_eq!(
        contents(&gamma),
        want,
        "summary + the kept range + the new prompt"
    );
    // Resume folds the same head.
    oxa(&env, &server.url(), &["-c", "-p", "delta question"]).ok();
    let delta = generating(&server).pop().unwrap();
    let n = gamma.messages.len();
    assert_eq!(
        delta.messages[..n],
        gamma.messages[..],
        "the same bytes after resume (W5)"
    );
}

/// pi docs: session-format.md "Session Version": versions 1 (linear, no ids) and 2 (tree, the
/// `hookMessage` role) are migrated to version 3 on load.
/// invariant: through `/import` (oxa migrates pi files into a new v3 session; pi's assistant
/// entries lack the exact wire bytes, so the first request starts cold).
#[test]
fn fmt_019() {
    let env = TestEnv::new();
    let proj = env.root().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    let v1_id = "0f6c2d9e-1111-4222-8333-944455556661";
    let v2_id = "0f6c2d9e-1111-4222-8333-944455556662";
    let jsonl = |vs: Vec<Value>| vs.iter().map(|v| format!("{v}\n")).collect::<String>();
    let v1 = env.write(
        "import/v1.jsonl",
        &jsonl(vec![
            json!({"type":"session","id":v1_id,"timestamp":"2025-01-01T00:00:00.000Z","cwd":proj}),
            json!({"type":"message","timestamp":"2025-01-01T00:00:01.000Z",
                   "message":{"role":"user","content":"v1 question","timestamp":1}}),
            json!({"type":"message","timestamp":"2025-01-01T00:00:02.000Z",
                   "message":{"role":"assistant","content":[{"type":"text","text":"v1 answer"}],
                              "stopReason":"stop","timestamp":2}}),
        ]),
    );
    let v2 = env.write(
        "import/v2.jsonl",
        &jsonl(vec![
            json!({"type":"session","version":2,"id":v2_id,"timestamp":"2025-01-02T00:00:00.000Z","cwd":proj}),
            json!({"type":"message","id":"a1b2c3d4","parentId":null,"timestamp":"2025-01-02T00:00:01.000Z",
                   "message":{"role":"user","content":"v2 question","timestamp":1}}),
            json!({"type":"message","id":"b2c3d4e5","parentId":"a1b2c3d4","timestamp":"2025-01-02T00:00:02.000Z",
                   "message":{"role":"hookMessage","customType":"hook-x","content":"v2 hook text","display":true,"timestamp":2}}),
            json!({"type":"message","id":"c3d4e5f6","parentId":"b2c3d4e5","timestamp":"2025-01-02T00:00:03.000Z",
                   "message":{"role":"assistant","content":[{"type":"text","text":"v2 answer"}],
                              "stopReason":"stop","timestamp":3}}),
        ]),
    );
    let rig = Rig::start_in(env, rig::replies(2), RigOpts::default());
    rig.pty.wait_for_text("ctrl+d exit").unwrap();
    let import = |path: &Path, answer: &str| {
        rig.slash(&format!("/import {}", path.display()));
        rig.pty.wait_for_text("Replace current session with").unwrap();
        rig.pty
            .wait_for_named("Yes chosen", |s| dialog_choice(s) == "Yes", WAIT)
            .unwrap();
        rig.pty.send_key("enter");
        rig.pty.wait_for_text("Session imported from:").unwrap();
        rig.pty.wait_for_text(answer).unwrap();
        rig.settle();
    };
    let stored = |id: &str| {
        rig.session_files()
            .into_iter()
            .find(|f| lines(f)[0]["id"] == id)
            .unwrap_or_else(|| panic!("an imported session with id {id}"))
    };

    // v1: ids generated, one linear chain, version 3.
    import(&v1, "v1 answer");
    rig.prompt("follow up one", "reply 1");
    let last = rig.generating().pop().unwrap();
    assert_eq!(contents(&last), vec!["v1 question", "v1 answer", "follow up one"]);
    let ls = lines(&stored(v1_id));
    assert_eq!(ls[0]["version"], 3, "migrated to version 3");
    assert_eq!(ls[0]["cwd"], json!(proj));
    let mut prev = Value::Null;
    for e in ls.iter().skip(1) {
        assert!(is_hex8(e["id"].as_str().unwrap_or("")), "an id: {e}");
        assert_eq!(e["parentId"], prev, "one linear chain: {e}");
        prev = e["id"].clone();
    }

    // v2: ids kept, `hookMessage` becomes a custom message (v3's rename), in context.
    import(&v2, "v2 answer");
    rig.prompt("follow up two", "reply 2");
    let last = rig.generating().pop().unwrap();
    assert_eq!(
        contents(&last),
        vec!["v2 question", "v2 hook text", "v2 answer", "follow up two"]
    );
    let ls = lines(&stored(v2_id));
    assert_eq!(ls[0]["version"], 3);
    assert_eq!(msg_id(&ls, "v2 question"), "a1b2c3d4", "v2 ids are kept");
    let cm = of_type(&ls, "custom_message");
    assert_eq!(cm.len(), 1, "{ls:#?}");
    assert_eq!(cm[0]["id"], "b2c3d4e5");
    assert_eq!(cm[0]["customType"], "hook-x");
    assert!(
        !std::fs::read_to_string(stored(v2_id))
            .unwrap()
            .contains("hookMessage"),
        "no v2 role is left"
    );
}

/// pi docs: message-types.md "ImageContent" (`{type:"image", data, mimeType}`) in user messages.
/// invariant: text-only engine: the image is stored with the user message as a `Part::Image`
/// (`parts`: path, mime, sha256, size) and never serialised to the wire (no image bytes in any
/// request, also after a resume).
#[test]
fn fmt_020() {
    let env = TestEnv::new();
    // a 1x1 PNG
    let png: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, 0x00,
        0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00,
        0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0xF8, 0xFF, 0xFF, 0x3F, 0x00, 0x05, 0xFE,
        0x02, 0xFE, 0xA7, 0x35, 0x81, 0x84, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60,
        0x82,
    ];
    std::fs::write(env.work().join("img.png"), png).expect("write png");
    let server = fake(vec![text("seen"), text("again")], FakeOpts::default());
    oxa(&env, &server.url(), &["-p", "look", "@img.png"]).ok();
    oxa(&env, &server.url(), &["-c", "-p", "and now"]).ok();
    let ls = lines(&only_file(&env));
    let user = with_role(&ls, "user")[0].clone();
    let parts = user["parts"].as_array().expect("parts");
    let img = parts
        .iter()
        .find(|p| p["type"] == "image")
        .unwrap_or_else(|| panic!("an image part: {user}"));
    let stored = std::fs::read(img["path"].as_str().unwrap_or("")).unwrap_or_default();
    assert_eq!(
        stored, png,
        "the stored image file holds the attached bytes: {img}"
    );
    assert_eq!(img["mime"], "image/png");
    assert_eq!(
        (img["w"].as_u64(), img["h"].as_u64()),
        (Some(1), Some(1)),
        "{img}"
    );
    let sha = img["sha256"].as_str().unwrap_or("");
    assert_eq!(sha.len(), 64, "{img}");
    for r in server.requests() {
        let body = String::from_utf8_lossy(&r.body).into_owned();
        assert!(!body.contains("iVBOR"), "no image bytes in a request");
        assert!(
            !body.contains("image_url") && !body.contains("\"image\""),
            "no image block"
        );
    }
    let wire = user["wire"]["content"].as_str().unwrap_or("").to_string();
    let reqs = generating(&server);
    assert_eq!(reqs[0].messages[0].content.as_deref(), Some(wire.as_str()));
    assert_eq!(
        reqs[1].messages[0].content.as_deref(),
        Some(wire.as_str()),
        "resume: same text"
    );
}

/// invariant: W2: the oxa-only entry types `shown` (a reply that was aborted, failed or cut:
/// shown to the user, never sent) and `mode_change` (W3: no mode text in the prompt) carry pi's
/// entry base, so pi tooling that switches on `type` skips them; neither reaches the wire.
#[test]
fn fmt_021() {
    let env = TestEnv::new();
    let server = fake(
        vec![
            ScriptTurn::fault(Fault::Http {
                status: 400,
                body: r#"{"error":{"message":"PARITY-REFUSED","type":"invalid_request_error","code":400}}"#
                    .to_string(),
            }),
            text("after the failure"),
        ],
        FakeOpts::default(),
    );
    let mut rpc = Rpc::start(&env, &server.url());
    rpc.prompt("first");
    rpc.ok(json!({"type": "set_mode", "mode": "plan"}));
    rpc.prompt("second");
    let file = rpc.file();
    assert_eq!(rpc.finish(), Some(0));
    let ls = lines(&file);
    let shown = of_type(&ls, "shown");
    let failed = shown
        .iter()
        .find(|s| s["why"] == "failed")
        .unwrap_or_else(|| panic!("a shown entry for the failed reply: {ls:#?}"));
    let base: BTreeSet<String> = ["id", "parentId", "timestamp", "type"]
        .iter()
        .map(|k| k.to_string())
        .collect();
    assert_eq!(
        keys(failed),
        base.union(&["text", "why"].iter().map(|k| k.to_string()).collect())
            .cloned()
            .collect(),
        "{failed}"
    );
    let shown_text = failed["text"].as_str().unwrap_or("").to_string();
    assert!(!shown_text.is_empty(), "{failed}");
    // The session's start records its mode; the switch is the entry naming plan.
    let modes: Vec<Value> = of_type(&ls, "mode_change")
        .into_iter()
        .filter(|m| m["mode"] == "plan")
        .collect();
    assert_eq!(modes.len(), 1, "{ls:#?}");
    assert_eq!(
        keys(&modes[0]),
        base.union(&["mode".to_string()].into_iter().collect())
            .cloned()
            .collect()
    );
    let reqs = generating(&server);
    let last = reqs.last().unwrap();
    assert!(
        !contents(last).iter().any(|c| c == &shown_text),
        "a shown text is never sent"
    );
    assert_eq!(last.system, reqs[0].system, "no mode text in the prompt (W3)");
    // The documented pi types do not include them.
    let pi_types = [
        "session",
        "message",
        "model_change",
        "thinking_level_change",
        "usage",
        "compaction",
        "context_edit",
        "branch_summary",
        "custom",
        "custom_message",
        "label",
        "session_info",
    ];
    assert!(!pi_types.contains(&"shown") && !pi_types.contains(&"mode_change"));
}

// ---------------------------------------------------------------- meta checks

/// The PARITY.md file of the workspace root.
fn parity_md() -> String {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("PARITY.md");
    std::fs::read_to_string(&root).unwrap_or_else(|e| panic!("read {}: {e}", root.display()))
}

/// The FMT rows of PARITY.md: (id, verdict, test column).
fn fmt_rows() -> Vec<(String, String, String)> {
    let mut rows = Vec::new();
    for line in parity_md().lines() {
        let cells: Vec<&str> = line.split('|').map(str::trim).collect();
        if cells.len() < 6 || !cells[1].starts_with("FMT-") {
            continue;
        }
        rows.push((cells[1].to_string(), cells[3].to_string(), cells[5].to_string()));
    }
    rows
}

/// The source of this file.
const SELF: &str = include_str!("format.rs");

/// The source text of one test item: its doc comment, `#[test]` and `fn`. Items end at the
/// column-0 closing brace of the function.
fn fn_body(id: &str) -> &'static str {
    let needle = format!("fn {}()", id.to_ascii_lowercase().replace('-', "_"));
    let fn_at = SELF
        .find(&needle)
        .unwrap_or_else(|| panic!("no test fn for {id}"));
    let start = SELF[..fn_at].rfind("\n}\n").map_or(0, |i| i + 3);
    let end = fn_at + SELF[fn_at..].find("\n}\n").map_or(SELF.len() - fn_at, |i| i + 3);
    &SELF[start..end]
}

/// each PARITY.md id of this area maps to a test fn or an O-row assertion
#[test]
fn every_row_has_test_or_recorded_omission() {
    let rows = fmt_rows();
    assert_eq!(rows.len(), 21, "the FMT area has 21 rows: {rows:?}");
    for (id, verdict, test) in &rows {
        let fn_name = id.to_ascii_lowercase().replace('-', "_");
        assert!(
            SELF.contains(&format!("fn {fn_name}()")),
            "{id}: test fn {fn_name} exists in format.rs"
        );
        assert!(
            matches!(verdict.as_str(), "P" | "A" | "O"),
            "{id} has a verdict P, A or O: {verdict}"
        );
        assert!(
            test.contains(&format!("format.rs::{fn_name}")),
            "{id} maps to format.rs::{fn_name} in PARITY.md (test column `{test}`)"
        );
    }
}

/// documented output or recorded fixture
#[test]
fn p_rows_compare_with_pi() {
    for (id, verdict, _) in fmt_rows() {
        if verdict != "P" {
            continue;
        }
        let body = fn_body(&id);
        assert!(
            body.contains("pi docs:"),
            "{id} (P) cites the pi documented behaviour it compares with"
        );
    }
}

/// comment + adapted behaviour asserted
#[test]
fn a_rows_name_their_invariant() {
    for (id, verdict, _) in fmt_rows() {
        if verdict != "A" {
            continue;
        }
        let body = fn_body(&id);
        assert!(
            body.contains("invariant:"),
            "{id} (A) names its invariant in a comment"
        );
        assert!(
            body.contains("assert") || body.contains("red("),
            "{id} (A) asserts its adapted behaviour"
        );
    }
}

/// unsupported_in_oxa
#[test]
fn o_rows_assert_unsupported_error() {
    for (id, verdict, _) in fmt_rows() {
        if verdict != "O" {
            continue;
        }
        assert!(
            fn_body(&id).contains("unsupported_in_oxa"),
            "{id} (O) asserts the unsupported_in_oxa error"
        );
    }
}
