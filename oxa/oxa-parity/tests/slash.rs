//! Slash commands (PARITY.md SLASH-*), wave H (lane slash): one test per row, named after the
//! row id (`SLASH-012` -> `fn slash_012()`). SLASH-025 (`/changelog`) is removed from oxa and is
//! recorded as an omission by the architect.
//!
//! How the rows are checked:
//! * Every row types the command into the real `oxa` in a pseudo-terminal (the shared rig in
//!   `support/tui_rig.rs`, the tree navigator helpers in `support/tree_pty.rs`) against the fake
//!   engine, and asserts an observable effect: the screen, a selector that opens, a request the
//!   engine recorded, a file written, the session file. Waits are bounded.
//! * Print-mode rows (SLASH-030 extension clash) run `oxa -p` with `oxa_parity::run_oxa`.
//! * P rows: pi 1.5.2's documented text (`docs/slash-commands.md`) is recorded in `DOCUMENTED`
//!   below; `p_rows_compare_with_pi` checks that every P row has one. Each test cites it too.
//! * A rows name their invariant in an `// invariant:` comment.
//! * O rows assert the `unsupported_in_oxa` notice.
//! * A row whose behaviour is missing is a RED test: it panics with `RED SLASH-NNN: ...`.
#![cfg(target_os = "linux")]

#[path = "support/tui_rig.rs"]
mod rig;
#[path = "support/tree_pty.rs"]
mod tree_pty;

use oxa_fake::{FakeModel, FakeOpts, Recorded, ScriptTurn, TestEnv};
use oxa_parity::run_oxa;
use rig::{editor_text, replies, session_lines, user_texts, Rig, RigOpts, Screen, WAIT};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tree_pty::{clear_editor, navigated, open_tree, pick, select_listed};

/// This file, for the meta-checks below.
const SELF_SRC: &str = include_str!("slash.rs");

/// pi 1.5.2 documented text of the P rows (`docs/slash-commands.md`, and the Export/Trust/
/// Reload/Hotkeys/Mcp entries of the same page). Compared by `p_rows_compare_with_pi`.
const DOCUMENTED: &[(&str, &str)] = &[
    ("SLASH-001", "/settings | Open settings"),
    ("SLASH-008", "/new | Start a new session"),
    ("SLASH-009", "/resume | Resume another session"),
    (
        "SLASH-010",
        "/name [name] | Set the session display name, or show the current name when omitted",
    ),
    (
        "SLASH-011",
        "/session | Show current session information and statistics",
    ),
    (
        "SLASH-013",
        "/fork | Create a new session from an earlier user message",
    ),
    (
        "SLASH-014",
        "/clone | Duplicate the current session at its current position",
    ),
    ("SLASH-017", "/copy | Copy the last assistant message"),
    (
        "SLASH-018",
        "/export [path] | Export the session as HTML or JSONL",
    ),
    (
        "SLASH-021",
        "/trust | Save a project trust decision for future Pi processes",
    ),
    (
        "SLASH-022",
        "/mcp | Manage MCP servers: sign in, reconnect, enable or disable, and change exposure",
    ),
    ("SLASH-024", "/hotkeys | Show active keyboard shortcuts"),
    ("SLASH-025", "/changelog | Show changelog entries"),
    ("SLASH-026", "/quit | Quit Pi"),
    (
        "SLASH-028",
        "Skills are available as `/skill:name` when skill commands are enabled.",
    ),
    (
        "SLASH-029",
        "Each prompt template is available under its template name.",
    ),
    (
        "SLASH-030",
        "Extensions can register commands with their own arguments and completion behavior.",
    ),
];

/// The documented pi behaviour recorded for a row id.
fn documented(id: &str) -> &'static str {
    DOCUMENTED
        .iter()
        .find(|(k, _)| *k == id)
        .map(|(_, v)| *v)
        .unwrap_or("")
}

fn repo(rel: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()))
}

/// The body of a test fn of this file, from its `fn` line to the next attribute.
fn fn_body(fname: &str) -> Option<&'static str> {
    let start = SELF_SRC.find(&format!("fn {fname}()"))?;
    let rest = &SELF_SRC[start..];
    let end = rest[1..].find("\n#[").map(|i| i + 1).unwrap_or(rest.len());
    Some(&rest[..end])
}

struct ParityRow {
    id: String,
    verdict: String,
    test: String,
}

/// The SLASH rows of PARITY.md.
fn parity_rows() -> Vec<ParityRow> {
    let text = repo("PARITY.md");
    let mut rows = Vec::new();
    for line in text.lines() {
        if !line.starts_with("| SLASH-") {
            continue;
        }
        // `\|` inside a cell (SLASH-027) is not a separator.
        let normalized = line.replace("\\|", "/");
        let cells: Vec<&str> = normalized.split('|').map(str::trim).collect();
        // ["", id, behaviour, V, note, test, ""]
        rows.push(ParityRow {
            id: cells[1].to_string(),
            verdict: cells[3].to_string(),
            test: cells[5].trim_matches('`').to_string(),
        });
    }
    assert_eq!(
        rows.len(),
        29,
        "SLASH-001..SLASH-030 (minus SLASH-016) of PARITY.md"
    );
    rows
}

fn fname(id: &str) -> String {
    id.to_lowercase().replace('-', "_")
}

// ---------------------------------------------------------------- meta-checks

/// each PARITY.md id of this area maps to a test fn or an O-row assertion
#[test]
fn every_row_has_test_or_recorded_omission() {
    for r in parity_rows() {
        if r.test == "omission" {
            assert_eq!(r.verdict, "O", "{}: only O rows may be omitted", r.id);
            continue;
        }
        let f = fname(&r.id);
        assert_eq!(r.test, format!("slash.rs::{f}"), "{}: test column", r.id);
        assert!(
            SELF_SRC.contains(&format!("fn {f}()")),
            "{}: no test fn {f} in tests/slash.rs",
            r.id
        );
    }
}

/// documented output or recorded fixture
#[test]
fn p_rows_compare_with_pi() {
    for r in parity_rows().iter().filter(|r| r.verdict == "P") {
        assert!(!documented(&r.id).is_empty(), "{}: no documented pi output", r.id);
    }
}

/// comment + adapted behaviour asserted
#[test]
fn a_rows_name_their_invariant() {
    let mut n = 0;
    for r in parity_rows().iter().filter(|r| r.verdict == "A") {
        let body = fn_body(&fname(&r.id)).unwrap_or_else(|| panic!("{}: no fn", r.id));
        assert!(
            body.contains("// invariant:"),
            "{}: no `// invariant:` comment",
            r.id
        );
        n += 1;
    }
    assert_eq!(n, 9, "A rows of SLASH (9)");
}

/// unsupported_in_oxa
#[test]
fn o_rows_assert_unsupported_error() {
    for r in parity_rows().iter().filter(|r| r.verdict == "O") {
        if r.test == "omission" {
            // No callable surface: the omission is recorded in PARITY.md.
            continue;
        }
        let body = fn_body(&fname(&r.id)).unwrap_or_else(|| panic!("{}: no fn", r.id));
        assert!(
            body.contains("unsupported_in_oxa"),
            "{}: no unsupported_in_oxa",
            r.id
        );
    }
}

// ---------------------------------------------------------------- helpers

/// The fake engine's two models: the default one and `other-model`.
fn two_models(script: Vec<ScriptTurn>) -> Rig {
    Rig::start(
        script,
        RigOpts {
            fake: FakeOpts {
                models: vec![
                    FakeModel::moe("fake-model", 1 << 20),
                    FakeModel::moe("other-model", 1 << 20),
                ],
                ..FakeOpts::default()
            },
            ..RigOpts::default()
        },
    )
}

/// The session file of a rig (one session in these tests): its JSON lines.
fn the_session(rig: &Rig) -> Vec<Value> {
    let files = rig.session_files();
    assert!(!files.is_empty(), "a session file exists");
    session_lines(&files[0])
}

/// The header record of a session file.
fn header_of(path: &Path) -> Value {
    session_lines(path).remove(0)
}

/// Entry ids of a session file (every record but the header), in file order.
fn entry_ids(path: &Path) -> Vec<String> {
    session_lines(path)
        .iter()
        .filter(|r| r["type"] != "session")
        .map(|r| r["id"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// Wait until the engine has recorded a request of `kind` (bounded), and return it.
fn wait_kind(rig: &Rig, kind: &str) -> Recorded {
    let t = Instant::now();
    loop {
        if let Some(r) = rig.server.requests().into_iter().find(|r| r.kind == kind) {
            return r;
        }
        assert!(t.elapsed() < WAIT, "no `{kind}` request reached the engine");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The text of a request's last user message.
fn last_user(r: &Recorded) -> String {
    user_texts(r).last().cloned().unwrap_or_default()
}

/// Standard base64 (for OSC 52 payloads).
fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(T[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The footer's second-to-last row: model, thinking and mode (`fake-model • medium ...`).
fn footer_last(s: &Screen) -> String {
    let (rows, _) = s.size();
    s.row(rows - 1)
}

/// Wait until the editor is empty and the run is over (the state after a selector closes).
fn back_to_editor(rig: &Rig) {
    rig.pty
        .wait_for_named(
            "the selector closed",
            |s| editor_text(s).is_empty() && s.contains("ctrl+d exit"),
            WAIT,
        )
        .unwrap();
    rig.pty.wait_stable(150, WAIT).unwrap();
}

// ---------------------------------------------------------------- models and settings

/// `/settings` opens the settings selector; Escape closes it without a request.
#[test]
fn slash_001() {
    // pi docs: slash-commands.md "/settings  Open settings"
    let rig = Rig::start(replies(1), RigOpts::default());
    rig.prompt("hello", "reply 1");
    rig.slash("/settings");
    let s = rig.pty.wait_for_text("Settings").unwrap();
    assert!(s.contains("Settings"), "{s:?}");
    rig.pty.send_key("escape");
    back_to_editor(&rig);
    assert_eq!(rig.generating().len(), 1, "opening the settings sends nothing");
}

/// `/model <pattern>` switches the model: the next request names it and the footer shows it.
#[test]
fn slash_002() {
    // pi docs: slash-commands.md "/model [provider/model]  Select a model"
    // invariant: the pattern is resolved over the engine's model list (`/v1/models`), and a switch
    // is a new epoch (W3): the requests after it name the new model.
    let rig = two_models(replies(2));
    rig.prompt("first", "reply 1");
    rig.slash("/model other");
    rig.pty.wait_for_text("other-model").unwrap();
    rig.prompt("second", "reply 2");
    let reqs = rig.generating();
    assert_eq!(reqs[0].model, "fake-model");
    assert_eq!(reqs[1].model, "other-model", "the switched model is requested");
}

/// `/thinking <level>`: off sends `enable_thinking: false`, any other level sends true (W6).
#[test]
fn slash_003() {
    // pi docs: slash-commands.md "/thinking [level]  Set the thinking level"
    // invariant: W6: off = enable_thinking false, any other level = true; the wire carries a boolean.
    let rig = Rig::start(replies(3), RigOpts::default());
    rig.prompt("one", "reply 1");
    rig.slash("/thinking off");
    rig.pty.wait_for_text("• off").unwrap();
    rig.prompt("two", "reply 2");
    rig.slash("/thinking high");
    rig.pty.wait_for_text("• high").unwrap();
    rig.prompt("three", "reply 3");
    let reqs = rig.generating();
    assert!(reqs[0].thinking, "the default level (medium) enables thinking");
    assert!(!reqs[1].thinking, "/thinking off disables it");
    assert!(reqs[2].thinking, "/thinking high enables it");
}

/// `/scoped-models` opens the list of the engine's models; Escape closes it.
#[test]
fn slash_004() {
    // pi docs: slash-commands.md "/scoped-models  Configure the models used by interactive cycling"
    // invariant: the list is the engine's model list (`/v1/models`); no provider list is shown.
    let rig = two_models(replies(1));
    rig.prompt("hi", "reply 1");
    rig.slash("/scoped-models");
    let s = rig.pty.wait_for_text("other-model").unwrap();
    assert!(s.contains("fake-model"), "both engine models are listed: {s:?}");
    rig.pty.send_key("escape");
    back_to_editor(&rig);
}

/// `/login` is not in oxa: the command answers `unsupported_in_oxa` and sends nothing.
#[test]
fn slash_005() {
    // pi docs: slash-commands.md "/login [provider]  Add provider authentication"
    // O: oxa has no provider login (the engine is the only endpoint); the notice is the error.
    let rig = Rig::start(replies(1), RigOpts::default());
    rig.slash("/login");
    let s = rig.pty.wait_for_text("unsupported_in_oxa").unwrap();
    assert!(s.contains("/login"), "{s:?}");
    assert!(rig.generating().is_empty(), "nothing is sent to the engine");
}

/// `/logout` is not in oxa: the command answers `unsupported_in_oxa` and sends nothing.
#[test]
fn slash_006() {
    // pi docs: slash-commands.md "/logout  Remove provider authentication"
    // O: oxa has no provider login to remove (see slash_005).
    let rig = Rig::start(replies(1), RigOpts::default());
    rig.slash("/logout");
    let s = rig.pty.wait_for_text("unsupported_in_oxa").unwrap();
    assert!(s.contains("/logout"), "{s:?}");
    assert!(rig.generating().is_empty(), "nothing is sent to the engine");
}

/// `/engine` (pi's `/llama`, renamed by design) shows the engine status and loads a model.
#[test]
fn slash_007() {
    // pi docs: slash-commands.md "/llama  Manage models on the configured llama.cpp router"
    // invariant: the engine is the oxe endpoint, so `/llama` is `/engine` (status and load of
    // the engine's models); the name `/llama` is no longer a built-in (BUILTINS: "replaces /llama").
    let rig = two_models(replies(2));
    rig.prompt("first", "reply 1");
    rig.slash("/engine");
    let s = rig.pty.wait_for_text("Tool call repairs:").unwrap();
    assert!(s.contains("fake-model"), "the status names the model: {s:?}");
    rig.slash("/engine load other-model");
    rig.pty.wait_for_text("other-model •").unwrap();
    rig.prompt("second", "reply 2");
    assert_eq!(rig.generating()[1].model, "other-model");
}

// ---------------------------------------------------------------- sessions

/// `/new` starts a new session: the next request holds only the new message.
#[test]
fn slash_008() {
    // pi docs: slash-commands.md "/new  Start a new session"
    let rig = Rig::start(replies(2), RigOpts::default());
    rig.prompt("first question", "reply 1");
    let first = rig.session_files();
    assert_eq!(first.len(), 1, "one session file: {first:?}");
    rig.slash("/new");
    rig.pty.wait_stable(150, WAIT).unwrap();
    rig.prompt("second question", "reply 2");
    let reqs = rig.generating();
    assert_eq!(
        user_texts(&reqs[1]),
        vec!["second question"],
        "a fresh conversation"
    );
    let files = rig.session_files();
    assert_eq!(files.len(), 2, "/new writes another session file: {files:?}");
    assert_ne!(
        header_of(&files[0])["id"],
        header_of(&files[1])["id"],
        "another id"
    );
}

/// `/resume` opens the session list; choosing a session brings its messages back.
#[test]
fn slash_009() {
    // pi docs: slash-commands.md "/resume  Resume another session"
    let rig = Rig::start(replies(3), RigOpts::default());
    rig.prompt("alpha question", "reply 1");
    rig.slash("/new");
    rig.pty.wait_stable(150, WAIT).unwrap();
    rig.prompt("beta question", "reply 2");
    rig.slash("/resume");
    rig.pty.wait_for_text("Resume Session").unwrap();
    select_listed(&rig, "alpha question");
    rig.pty.send_key("enter");
    rig.pty.wait_for_text("reply 1").unwrap();
    rig.settle();
    rig.prompt("third", "reply 3");
    let reqs = rig.generating();
    assert_eq!(
        user_texts(reqs.last().unwrap()),
        vec!["alpha question", "third"],
        "the resumed conversation is sent with the new message"
    );
}

/// `/name <name>` sets the session display name; `/name` alone shows it.
#[test]
fn slash_010() {
    // pi docs: slash-commands.md "/name [name]  Set the session display name, or show the current
    // name when omitted"
    let rig = Rig::start(replies(1), RigOpts::default());
    rig.prompt("hello", "reply 1");
    rig.slash("/name demo");
    rig.pty.wait_stable(150, WAIT).unwrap();
    let named = the_session(&rig)
        .iter()
        .any(|r| r["type"] == json!("session_info") && r["name"] == json!("demo"));
    assert!(named, "a session_info entry named demo is written");
    rig.slash("/name");
    let s = rig.pty.wait_for_text("Session name: demo").unwrap();
    assert!(s.contains("Session name: demo"), "{s:?}");
}

/// `/session` shows the session file, id, message counts and the cache of the last request.
#[test]
fn slash_011() {
    // pi docs: slash-commands.md "/session  Show current session information and statistics";
    // oxa adds the cache statistics of the last request.
    let rig = Rig::start(replies(2), RigOpts::default());
    rig.prompt("first", "reply 1");
    rig.prompt("second", "reply 2");
    rig.slash("/session");
    let s = rig
        .pty
        .wait_for_named(
            "the session info",
            |s| s.contains("Session id:") && s.contains("Cache, last request:"),
            WAIT,
        )
        .unwrap();
    let id = header_of(&rig.session_files()[0])["id"].clone();
    assert!(s.contains("Session file:"), "{s:?}");
    assert!(s.contains(id.as_str().unwrap()), "the session id is shown: {s:?}");
    assert!(s.contains("Messages: user 2, assistant 2"), "{s:?}");
}

/// `/tree`: moving the leaf to an earlier user message is a rebase; the abandoned branch is not sent.
#[test]
fn slash_012() {
    // pi docs: slash-commands.md "/tree  Navigate the session tree"
    // invariant: W1: a leaf move is a rebase; the next request holds the path to the chosen
    // point only. The optional branch summary is declined here ("No summary").
    let rig = Rig::start(
        vec![
            ScriptTurn::text("alpha answer"),
            ScriptTurn::text("beta answer"),
            ScriptTurn::text("gamma answer"),
        ],
        RigOpts::default(),
    );
    rig.prompt("alpha question", "alpha answer");
    rig.prompt("beta question", "beta answer");
    open_tree(&rig);
    pick(&rig, "user: beta question", Some(0));
    navigated(&rig, |s| !s.contains("beta answer"));
    clear_editor(&rig);
    rig.prompt("gamma question", "gamma answer");
    let last = rig.generating().pop().unwrap();
    assert_eq!(
        user_texts(&last),
        vec!["alpha question", "gamma question"],
        "the abandoned branch is not sent"
    );
}

/// `/fork` opens the fork points; choosing a user message writes a new session with a parent.
#[test]
fn slash_013() {
    // pi docs: slash-commands.md "/fork  Create a new session from an earlier user message"
    let rig = Rig::start(replies(1), RigOpts::default());
    rig.prompt("first question", "reply 1");
    let file1 = rig.session_files()[0].clone();
    rig.slash("/fork");
    rig.pty
        .wait_for_named(
            "the fork picker",
            |s| {
                s.rows()
                    .iter()
                    .any(|r| r.starts_with("› ") && r.contains("first question"))
            },
            WAIT,
        )
        .unwrap();
    select_listed(&rig, "first question");
    rig.pty.send_key("enter");
    rig.pty.wait_for_text("Forked to new session").unwrap();
    let files = rig.session_files();
    assert_eq!(files.len(), 2, "a new session file: {files:?}");
    let fork = files.iter().find(|f| **f != file1).expect("the fork");
    assert_eq!(
        header_of(fork)["parentSession"],
        json!(file1.to_string_lossy()),
        "the fork names its parent"
    );
}

/// `/clone` duplicates the active branch into a new session with the same entries.
#[test]
fn slash_014() {
    // pi docs: slash-commands.md "/clone  Duplicate the current session at its current position"
    let rig = Rig::start(replies(2), RigOpts::default());
    rig.prompt("first question", "reply 1");
    rig.prompt("second question", "reply 2");
    let file1 = rig.session_files()[0].clone();
    let original = entry_ids(&file1);
    rig.slash("/clone");
    rig.pty.wait_for_text("Cloned to new session").unwrap();
    let files = rig.session_files();
    assert_eq!(files.len(), 2, "a new session file: {files:?}");
    let clone = files.iter().find(|f| **f != file1).expect("the clone");
    assert_eq!(
        header_of(clone)["parentSession"],
        json!(file1.to_string_lossy()),
        "the clone names the session it copies"
    );
    assert_eq!(entry_ids(clone), original, "the same entries, in the same order");
}

/// `/compact [instructions]` asks the engine for a summary with the instructions appended.
#[test]
fn slash_015() {
    // pi docs: slash-commands.md "/compact [instructions]  Compact the current context, optionally
    // with custom instructions"
    // invariant: W5: the summary request is a pure append to the wire (the last chat request, its
    // reply, then one user instruction); the instructions are the last user message.
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/settings.json",
        &json!({"compaction": {"keepRecentTokens": 1}}).to_string(),
    );
    let rig = Rig::start_in(
        env,
        vec![
            ScriptTurn::text("one"),
            ScriptTurn::text("two"),
            ScriptTurn::text("summary text"),
        ],
        RigOpts::default(),
    );
    rig.pty.wait_for_text("ctrl+d exit").unwrap();
    rig.prompt("first", "one");
    rig.prompt("second", "two");
    rig.slash("/compact Focus on code changes");
    let summary = wait_kind(&rig, "compaction");
    assert!(
        last_user(&summary).ends_with("Focus on code changes"),
        "the instructions are sent: {:?}",
        user_texts(&summary)
    );
    let chat = rig.generating();
    let last_chat = chat
        .iter()
        .rev()
        .find(|r| r.kind == "chat")
        .expect("a chat request");
    let mut prefix = last_chat.messages.clone();
    prefix.push(last_chat.reply.clone().expect("the last reply was stored"));
    assert_eq!(
        &summary.messages[..prefix.len()],
        &prefix[..],
        "the summary request is a pure append to the wire"
    );
}

// ---------------------------------------------------------------- export and share

/// `/copy` puts the last assistant message on the clipboard (OSC 52).
#[test]
fn slash_017() {
    // pi docs: slash-commands.md "/copy  Copy the last assistant message"; tui.md clipboard: OSC 52.
    let reply = "COPY-ME-SLASH";
    let rig = Rig::start(
        vec![ScriptTurn::text(reply)],
        RigOpts {
            profile: rig::TermProfile::kitty(),
            ..RigOpts::default()
        },
    );
    rig.prompt("say it", reply);
    let mark = rig.pty.mark();
    rig.slash("/copy");
    let b64 = base64(reply.as_bytes());
    rig.pty
        .wait_for_raw(mark, format!("\x1b]52;c;{b64}").as_bytes())
        .unwrap();
}

/// `/export <path>` writes the session: JSONL is a copy of the session file; HTML is not built yet.
#[test]
fn slash_018() {
    // pi docs: slash-commands.md "/export [path]  Export the session as HTML or JSONL"
    let rig = Rig::start(replies(1), RigOpts::default());
    rig.prompt("export me", "reply 1");
    let source = rig.session_files()[0].clone();
    let out = rig.env.root().join("exported.jsonl");
    rig.slash(&format!("/export {}", out.display()));
    rig.pty.wait_for_text("Exported the session to").unwrap();
    let written = std::fs::read_to_string(&out).expect("the JSONL export");
    assert_eq!(
        written,
        std::fs::read_to_string(&source).unwrap(),
        "the export is the session file"
    );
    // HTML: a path that does not end in `.jsonl` writes the session as one HTML document.
    let html = rig.env.root().join("exported.html");
    rig.slash(&format!("/export {}", html.display()));
    // The JSONL notice is still on screen: wait for the HTML file itself, complete.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let page = loop {
        if let Ok(text) = std::fs::read_to_string(&html) {
            if text.trim_end().ends_with("</html>") {
                break text;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "/export <file>.html wrote no HTML file"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    assert!(
        page.starts_with("<!DOCTYPE html>"),
        "{:?}",
        &page[..page.len().min(80)]
    );
    let data = session_data(&page);
    assert!(
        data.contains("export me") && data.contains("reply 1"),
        "the transcript is in the HTML"
    );
}

/// The decoded session data of an exported page: the base64 element `session-data`.
fn session_data(page: &str) -> String {
    let open = "<script id=\"session-data\" type=\"application/json\">";
    let start = page.find(open).expect("the session data element") + open.len();
    let end = start + page[start..].find("</script>").expect("the element closes");
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0);
    for c in page[start..end].bytes() {
        let v = match c {
            b'A'..=b'Z' => u32::from(c - b'A'),
            b'a'..=b'z' => u32::from(c - b'a') + 26,
            b'0'..=b'9' => u32::from(c - b'0') + 52,
            b'+' => 62,
            b'/' => 63,
            _ => continue,
        };
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `/share` is not in oxa: `unsupported_in_oxa`, nothing is uploaded.
#[test]
fn slash_019() {
    // pi docs: slash-commands.md "/share  Upload the session and return a viewer link"
    // O: no third-party upload from a local-first tool; the command answers the refusal.
    let rig = Rig::start(replies(1), RigOpts::default());
    rig.prompt("private", "reply 1");
    rig.slash("/share");
    let s = rig.pty.wait_for_text("unsupported_in_oxa").unwrap();
    assert!(s.contains("/share"), "{s:?}");
    assert_eq!(rig.generating().len(), 1, "nothing is sent for the share");
}

/// `/bug <description>` writes a local, redacted bundle; no transcript text, nothing uploaded.
#[test]
fn slash_020() {
    // pi docs: slash-commands.md "/bug [description]  Prepare a private bug report for the Pi
    // developers"
    // invariant: the bundle is written locally and is redacted: the home directory is `~`, the
    // bundle holds the description and block counts, never the transcript text.
    let rig = Rig::start(replies(1), RigOpts::default());
    rig.prompt("TOPSECRET-PROMPT", "reply 1");
    rig.slash("/bug the thing broke");
    rig.pty.wait_for_text("Bug report written to").unwrap();
    let dir = rig.env.agent_dir().join("bug-reports");
    let bundle = std::fs::read_dir(&dir)
        .expect("the bug-reports directory")
        .flatten()
        .map(|e| e.path())
        .next()
        .expect("a bundle");
    let text = std::fs::read_to_string(&bundle).unwrap();
    assert!(text.contains("description: the thing broke"), "{text}");
    assert!(text.contains("nothing was uploaded"), "{text}");
    assert!(!text.contains("TOPSECRET-PROMPT"), "no transcript text: {text}");
    assert!(
        !text.contains(&rig.env.home().display().to_string()),
        "the home directory is redacted: {text}"
    );
}

// ---------------------------------------------------------------- runtime and project

/// `/trust` asks about the project; "Trust this project" saves the decision.
#[test]
fn slash_021() {
    // pi docs: slash-commands.md "/trust  Save a project trust decision for future Pi processes"
    let rig = Rig::start(replies(1), RigOpts::default());
    rig.slash("/trust");
    rig.pty.wait_for_text("Trust this project").unwrap();
    rig.pty.send_key("enter");
    rig.pty.wait_for_text("This project is trusted").unwrap();
    let store =
        std::fs::read_to_string(rig.env.agent_dir().join("trust.json")).expect("the trust store is written");
    assert!(!store.trim().is_empty(), "the decision is saved");
}

/// `/mcp` opens the MCP servers selector; Escape closes it.
#[test]
fn slash_022() {
    // pi docs: slash-commands.md "/mcp  Manage MCP servers: sign in, reconnect, enable or disable,
    // and change exposure"; the exposure change takes effect at the next epoch (not exercised
    // here: no MCP server is configured in this rig).
    let rig = Rig::start(replies(1), RigOpts::default());
    rig.prompt("hi", "reply 1");
    rig.slash("/mcp");
    let s = rig.pty.wait_for_text("MCP").unwrap();
    assert!(s.contains("MCP"), "{s:?}");
    rig.pty.send_key("escape");
    back_to_editor(&rig);
}

/// `/reload` with nothing changed starts no epoch; a changed AGENTS.md starts one, with its cost.
#[test]
fn slash_023() {
    // pi docs: slash-commands.md "/reload  Reload keybindings, extensions, skills, templates, and
    // context files"
    // invariant: unchanged resources = no new epoch (the prefix bytes stay); changed resources =
    // one new prefix epoch whose re-prefill cost is shown (W3).
    let rig = Rig::start(replies(3), RigOpts::default());
    rig.prompt("one", "reply 1");
    rig.slash("/reload");
    rig.pty.wait_stable(200, WAIT).unwrap();
    assert!(
        !rig.screen().contains("new prefix epoch"),
        "nothing changed, so no epoch: {:?}",
        rig.screen().rows()
    );
    rig.prompt("two", "reply 2");
    let reqs = rig.generating();
    assert_eq!(reqs[0].system, reqs[1].system, "the prefix bytes are unchanged");
    rig.env.write("work/AGENTS.md", "Always answer in haiku.\n");
    rig.slash("/reload");
    rig.pty.wait_for_text("new prefix epoch (reload)").unwrap();
    rig.prompt("three", "reply 3");
    let reqs = rig.generating();
    assert!(
        reqs[2].system.contains("Always answer in haiku."),
        "the changed context file is in the prefix"
    );
}

/// `/hotkeys` lists the shortcuts of the live keymap, user overrides included.
#[test]
fn slash_024() {
    // pi docs: slash-commands.md "/hotkeys  Show active keyboard shortcuts"; keybindings.md
    // (a user binding replaces the default).
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/keybindings.json",
        r#"{"app.mode.cycle": "ctrl+y"}"#,
    );
    let rig = Rig::start_in(
        env,
        replies(1),
        RigOpts {
            size: (120, 120),
            ..RigOpts::default()
        },
    );
    rig.pty.wait_for_text("ctrl+d exit").unwrap();
    rig.slash("/hotkeys");
    let s = rig.pty.wait_for_text("Cycle the permission mode").unwrap();
    let row = s
        .rows()
        .into_iter()
        .find(|r| r.contains("Cycle the permission mode"))
        .unwrap_or_default();
    assert!(row.contains("ctrl+y"), "the override is shown: {row:?}");
    assert!(!row.contains("alt+m"), "the default is replaced: {row:?}");
}

/// `/quit` exits oxa with status 0.
#[test]
fn slash_026() {
    // pi docs: slash-commands.md "/quit  Quit Pi"
    let mut rig = Rig::start(replies(1), RigOpts::default());
    rig.slash("/quit");
    let st = rig.pty.wait_exit(WAIT).unwrap();
    assert!(st.success(), "exit status {st}");
}

/// `/mode [plan|workspace|ask]` sets oxa's permission mode; the footer shows it; a bad value is usage.
#[test]
fn slash_027() {
    // pi docs: none (oxa only; pi has no permission modes).
    // invariant: the mode is the W-guard permission mode of oxa (plan, workspace, ask); it is shown
    // in the footer and a value outside the three is refused with the usage line.
    let rig = Rig::start(replies(1), RigOpts::default());
    rig.prompt("hi", "reply 1");
    rig.slash("/mode");
    rig.pty.wait_for_text("Mode: workspace").unwrap();
    rig.slash("/mode plan");
    rig.pty
        .wait_for_named("the plan footer", |s| footer_last(s).contains("plan"), WAIT)
        .unwrap();
    rig.slash("/mode ask");
    rig.pty
        .wait_for_named("the ask footer", |s| footer_last(s).contains("ask"), WAIT)
        .unwrap();
    rig.slash("/mode yolo");
    rig.pty
        .wait_for_text("Usage: /mode [plan|workspace|ask]")
        .unwrap();
}

// ---------------------------------------------------------------- resources

/// `/skill:name [args]` loads the skill and appends the arguments to its instructions.
#[test]
fn slash_028() {
    // pi docs: slash-commands.md "Skills are available as `/skill:name` when skill commands are
    // enabled."
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/skills/pdf-tools/SKILL.md",
        "---\nname: pdf-tools\ndescription: Extract text from PDF files.\n---\n# pdf-tools\n\nBody of pdf-tools.\n",
    );
    let rig = Rig::start_in(env, replies(1), RigOpts::default());
    rig.pty.wait_for_text("ctrl+d exit").unwrap();
    rig.slash("/skill:pdf-tools extract report.pdf");
    rig.pty.wait_for_text("reply 1").unwrap();
    rig.settle();
    let text = last_user(&rig.generating()[0]);
    assert!(text.contains("Body of pdf-tools."), "the skill is loaded: {text}");
    assert!(
        text.contains("extract report.pdf"),
        "the arguments follow: {text}"
    );
}

/// A prompt template is a `/name args` command: its text is expanded with the arguments.
#[test]
fn slash_029() {
    // pi docs: slash-commands.md "Each prompt template is available under its template name."
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/prompts/review.md",
        "---\ndescription: Review staged git changes\n---\nReview ${1:-all} and $@ and $ARGUMENTS.\n",
    );
    let rig = Rig::start_in(env, replies(1), RigOpts::default());
    rig.pty.wait_for_text("ctrl+d exit").unwrap();
    rig.slash("/review concurrency");
    rig.pty.wait_for_text("reply 1").unwrap();
    rig.settle();
    assert_eq!(
        last_user(&rig.generating()[0]).trim_end(),
        "Review concurrency and concurrency and concurrency."
    );
}

/// Name clashes: a built-in wins over a prompt template of the same name; in print mode an
/// extension command wins over a template of the same name.
#[test]
fn slash_030() {
    // pi docs: slash-commands.md "Extensions can register commands with their own arguments and
    // completion behavior." Clash order as pi: built-in, extension, template, skill.
    // invariant: built-ins are not shadowed (the template is never expanded for `/hotkeys`).
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/prompts/hotkeys.md",
        "TEMPLATE-HOTKEYS-EXPANDED\n",
    );
    let rig = Rig::start_in(env, replies(1), RigOpts::default());
    rig.pty.wait_for_text("ctrl+d exit").unwrap();
    rig.slash("/hotkeys");
    rig.pty.wait_for_text("Cycle the permission mode").unwrap();
    assert!(
        rig.generating().is_empty(),
        "the template was not sent: the built-in ran"
    );
    drop(rig);

    // An extension command beats a template of the same name (print mode).
    let env = TestEnv::new();
    env.write("home/.oxa/agent/prompts/greet.md", "TEMPLATE-GREET {{ignored}}\n");
    env.write(
        "home/.oxa/agent/extensions/e.js",
        "const LOG = \"log.txt\";\nfunction note(ctx, s) {\n  const c = ctx || __oxa_make_ctx();\n  let p = \"\";\n  try { p = c.fs.readFile(LOG) || \"\"; } catch (e) {}\n  c.fs.writeFile(LOG, p + s + \"\\n\");\n}\nexport default function (oxa) {\n  oxa.registerCommand('greet', {description: 'greets', handler: (args, ctx) => note(ctx, 'EXT-GREET:' + args)});\n}\n",
    );
    let server = oxa_fake::FakeServer::start(replies(1), FakeOpts::default()).expect("fake engine");
    let out = run_oxa(&env, &server.url(), &["-p", "/greet bob"], "");
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let log = std::fs::read_to_string(env.work().join("log.txt")).unwrap_or_default();
    assert!(
        log.contains("EXT-GREET:bob"),
        "the extension command ran: {log:?}"
    );
    assert!(
        server.requests().iter().all(|r| !r.messages.iter().any(|m| m
            .content
            .clone()
            .unwrap_or_default()
            .contains("TEMPLATE-GREET"))),
        "the template was not expanded"
    );
}
