//! G4 PARITY tests (tree). Owner: task G4.
//! One test per PARITY.md row of this area, named after the row id (`CLI-012` -> `fn cli_012()`).
//! The four tests below are the meta-checks of the task; the row tests are added by the task.
//! Helpers: `oxa_parity::{oxa_bin, fake_bin, run_oxa, fixture}`, `oxa_fake::TestEnv`.
//!
//! Conventions of this file:
//! * Every row test drives the real `oxa` binary (`--mode rpc`, `-p`, `--mode json`) against the
//!   in-process fake engine. Nothing touches the real home (`TestEnv`).
//! * A row whose behaviour exists only in the interactive TUI (`/tree` navigator, Double Escape,
//!   branch summaries, tree hooks) has no print or RPC surface. Its test calls [`red`], which
//!   fails with the precise reason; those failures are the H-wave input and are not hacked
//!   around.
//! * P rows name the pi 1.5.2 documentation page they compare with (`pi-doc:` comment).
//! * A rows name the invariant that forces the adaptation (`invariant:` comment).

use oxa_fake::{
    cache_bytes, FakeCaps, FakeModel, FakeOpts, FakeServer, Fault, Recorded, ScriptTurn, TestEnv,
};
use oxa_parity::run_oxa;
use serde_json::{json, Value};
use std::process::Output;

#[cfg(target_os = "linux")]
#[path = "support/tui_rig.rs"]
mod rig;
#[cfg(target_os = "linux")]
#[path = "support/tree_pty.rs"]
mod tree_pty;
#[cfg(target_os = "linux")]
use rig::{editor_text, session_lines, user_texts, Rig, RigOpts, WAIT};
#[cfg(target_os = "linux")]
use tree_pty::{clear_editor, nav_rows, navigated, open_tree, pick, position, select_row, selected};

fn server(script: Vec<ScriptTurn>, opts: FakeOpts) -> FakeServer {
    FakeServer::start(script, opts).expect("fake engine")
}

/// The generating requests of the fake, in arrival order (idle prefill requests, which have
/// `max_tokens` 0, are not replies and are left out).
fn sorted(server: &FakeServer) -> Vec<Recorded> {
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

fn header<'a>(r: &'a Recorded, name: &str) -> Option<&'a str> {
    r.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

/// Run the binary in print mode with one message.
fn print_mode(env: &TestEnv, url: &str, message: &str) -> Output {
    run_oxa(env, url, &["-p", message], "")
}

/// Run the binary in JSON mode with one message.
fn json_mode(env: &TestEnv, url: &str, message: &str) -> Output {
    run_oxa(env, url, &["--mode", "json", "-p", message], "")
}

/// Send JSON-lines commands to `oxa --mode rpc`, close stdin, and return every stdout line that
/// parses as JSON (responses and events, in order).
fn rpc(env: &TestEnv, url: &str, cmds: &[Value]) -> Vec<Value> {
    let mut stdin = String::new();
    for c in cmds {
        stdin.push_str(&c.to_string());
        stdin.push('\n');
    }
    let out = run_oxa(env, url, &["--mode", "rpc"], &stdin);
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect()
}

/// The response with this command id; panics with every line when it is missing or failed.
fn ok_response<'a>(lines: &'a [Value], id: &str) -> &'a Value {
    let found = lines
        .iter()
        .find(|v| v.get("type") == Some(&json!("response")) && v.get("id") == Some(&json!(id)));
    match found {
        Some(v) if v.get("success") == Some(&json!(true)) => v,
        other => panic!("command {id} failed or is missing: {other:?}\nall lines: {lines:#?}"),
    }
}

/// The `data` of a successful response.
fn data<'a>(lines: &'a [Value], id: &str) -> &'a Value {
    ok_response(lines, id).get("data").unwrap_or(&Value::Null)
}

/// The source text of this file, and the body of one test fn (from its signature to the next
/// `#[test]`).
fn fn_body(name: &str) -> Option<&'static str> {
    let src: &'static str = include_str!("tree.rs");
    let start = src.find(&format!("fn {name}("))?;
    let rest = &src[start..];
    let end = rest.find("\n#[test]").unwrap_or(rest.len());
    Some(&rest[..end])
}

/// Rows of PARITY.md as (id, verdict, test column) for the sections this file owns: TREE and
/// the ENG rows whose test column names `tree.rs`.
fn tree_rows() -> Vec<(String, String, String)> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../PARITY.md");
    let text = std::fs::read_to_string(path).expect("read PARITY.md");
    let mut rows = Vec::new();
    for line in text.lines() {
        if !(line.starts_with("| TREE-") || line.starts_with("| ENG-")) {
            continue;
        }
        let cols: Vec<&str> = line.split('|').map(str::trim).collect();
        if cols.len() < 6 {
            continue;
        }
        let test = cols[5].trim_matches('`').to_string();
        if test.starts_with("tree.rs::") {
            rows.push((cols[1].to_string(), cols[3].to_string(), test));
        }
    }
    rows
}

fn fn_name_of(test: &str) -> String {
    test.trim_start_matches("tree.rs::").replace('-', "_")
}

/// A running `oxa --mode rpc` whose stdin stays open, so a test can read each response before it
/// sends the next command (a fork needs the entry ids the previous command returned).
struct Rpc {
    child: std::process::Child,
    out: std::io::BufReader<std::process::ChildStdout>,
    stdin: Option<std::process::ChildStdin>,
    lines: Vec<Value>,
}

impl Rpc {
    fn start(env: &TestEnv, url: &str) -> Rpc {
        use std::process::{Command, Stdio};
        let mut child = Command::new(oxa_parity::oxa_bin())
            .args(["--mode", "rpc"])
            .current_dir(env.work())
            .env_clear()
            .envs(env.envs(url))
            .envs(oxa_parity::note_off_env())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn oxa --mode rpc (build it first: cargo build -p oxa-cli -p oxa-fake)");
        let stdin = child.stdin.take();
        let out = std::io::BufReader::new(child.stdout.take().expect("stdout"));
        Rpc {
            child,
            out,
            stdin,
            lines: Vec::new(),
        }
    }

    /// Send one command and return its response, whatever its success (every line read on the
    /// way is kept).
    fn exchange(&mut self, cmd: Value) -> Value {
        use std::io::{BufRead, Write};
        let id = cmd["id"].as_str().expect("command id").to_string();
        let stdin = self.stdin.as_mut().expect("stdin open");
        writeln!(stdin, "{cmd}").expect("write command");
        stdin.flush().expect("flush command");
        loop {
            let mut line = String::new();
            let n = self.out.read_line(&mut line).expect("read rpc line");
            assert!(n > 0, "oxa closed stdout before answering {id}");
            let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            let is_mine = v.get("type") == Some(&json!("response")) && v.get("id") == Some(&json!(id));
            self.lines.push(v.clone());
            if is_mine {
                return v;
            }
        }
    }

    /// Send one command that must succeed and return its response.
    fn send(&mut self, cmd: Value) -> Value {
        let v = self.exchange(cmd);
        assert_eq!(v.get("success"), Some(&json!(true)), "command failed: {v}");
        v
    }

    /// Send a prompt and wait until the run has settled (the prompt response comes before the
    /// run ends; a second prompt before that is refused as "already processing").
    fn prompt(&mut self, id: &str, message: &str) {
        self.send(json!({"id": id, "type": "prompt", "message": message}));
        self.wait_event("agent_settled");
    }

    /// Read lines until an event of this type arrives.
    fn wait_event(&mut self, name: &str) {
        use std::io::BufRead;
        loop {
            let mut line = String::new();
            let n = self.out.read_line(&mut line).expect("read rpc line");
            assert!(n > 0, "oxa closed stdout before the {name} event");
            if let Ok(v) = serde_json::from_str::<Value>(line.trim()) {
                let hit = v.get("type") == Some(&json!(name));
                self.lines.push(v);
                if hit {
                    return;
                }
            }
        }
    }

    /// Close stdin (oxa exits when it closes) and return every JSON line seen.
    fn finish(mut self) -> Vec<Value> {
        use std::io::Read;
        self.stdin.take();
        let mut rest = String::new();
        let _ = self.out.read_to_string(&mut rest);
        let _ = self.child.wait();
        for l in rest.lines() {
            if let Ok(v) = serde_json::from_str::<Value>(l) {
                self.lines.push(v);
            }
        }
        self.lines
    }
}

// ---------------------------------------------------------------------------------------------
// Meta checks (WAVES.toml acceptance of G4)
// ---------------------------------------------------------------------------------------------

/// each PARITY.md id of this area maps to a test fn or an O-row assertion
#[test]
fn every_row_has_test_or_recorded_omission() {
    let rows = tree_rows();
    let tree_section = rows.iter().filter(|(id, _, _)| id.starts_with("TREE-")).count();
    assert_eq!(tree_section, 12, "TREE rows in PARITY.md: {rows:?}");
    let eng_section = rows.iter().filter(|(id, _, _)| id.starts_with("ENG-")).count();
    assert_eq!(
        eng_section, 6,
        "ENG rows mapped to tree.rs in PARITY.md: {rows:?}"
    );
    for (id, verdict, test) in &rows {
        if verdict == "O" {
            assert!(test == "omission" || test.contains("tree.rs::"), "{id}: {test}");
            continue;
        }
        let name = fn_name_of(test);
        assert!(
            fn_body(&name).is_some(),
            "{id} maps to `{test}` but tree.rs has no fn {name}"
        );
    }
}

/// documented output or recorded fixture
#[test]
fn documented_rows_have_tests() {
    const DOCS: &[&str] = &[
        "sessions.md",
        "compaction.md",
        "settings.md",
        "keybindings.md",
        "rpc-commands.md",
        "session-format.md",
    ];
    let mut checked = 0;
    for (id, verdict, test) in tree_rows() {
        if verdict != "P" {
            continue;
        }
        checked += 1;
        let body = fn_body(&fn_name_of(&test)).expect("row fn");
        let marker = body
            .lines()
            .find_map(|l| l.trim().strip_prefix("// pi-doc:"))
            .unwrap_or_else(|| panic!("{id}: P row test has no `// pi-doc:` comparison marker"));
        let doc = marker.trim().split('#').next().unwrap_or("").trim();
        assert!(
            DOCS.contains(&doc),
            "{id}: pi doc `{doc}` is not one of the pi 1.5.2 pages in use"
        );
        assert!(
            body.contains("assert") || body.contains("red("),
            "{id}: P row test asserts nothing"
        );
    }
    assert_eq!(
        checked, 9,
        "TREE has nine P rows (001 002 003 005 007 008 009 010 011)"
    );
}

/// comment + adapted behaviour asserted
#[test]
fn a_rows_name_their_invariant() {
    let mut checked = 0;
    for (id, verdict, test) in tree_rows() {
        if verdict != "A" {
            continue;
        }
        checked += 1;
        let body = fn_body(&fn_name_of(&test)).expect("row fn");
        assert!(
            body.contains("// invariant:"),
            "{id}: A row test does not name its invariant (`// invariant: ...`)"
        );
    }
    assert!(checked > 0, "no A rows found");
}

// ---------------------------------------------------------------------------------------------
// TREE rows
// ---------------------------------------------------------------------------------------------

// ---------------------------------------------------------------------------------------------
// The /tree rows through the pty (lane tui, wave H)
// ---------------------------------------------------------------------------------------------

/// A two-branch session: `alpha question` (a `read` of notes.md, then `alpha answer`),
/// `beta question` -> `beta answer`, then `/tree` back to before `beta question` (no summary)
/// and `gamma question` -> `gamma answer` on the new branch. `extra` scripted replies follow.
#[cfg(target_os = "linux")]
fn two_branches(env: TestEnv, extra: Vec<ScriptTurn>, opts: RigOpts) -> Rig {
    std::fs::write(env.work().join("notes.md"), "the notes say OMEGA-42\n").unwrap();
    let mut script = vec![
        ScriptTurn::tool("call_1", "read", r#"{"path":"notes.md"}"#),
        ScriptTurn::text("alpha answer"),
        ScriptTurn::text("beta answer"),
        ScriptTurn::text("gamma answer"),
    ];
    script.extend(extra);
    let rig = Rig::start_in(env, script, opts);
    rig.pty.wait_for_text("ctrl+d exit").unwrap();
    rig.prompt("alpha question", "alpha answer");
    rig.prompt("beta question", "beta answer");
    open_tree(&rig);
    pick(&rig, "user: beta question", Some(0));
    navigated(&rig, |s| !s.contains("beta answer"));
    clear_editor(&rig);
    rig.prompt("gamma question", "gamma answer");
    rig
}

/// The session file's JSON lines (one session in these tests).
#[cfg(target_os = "linux")]
fn the_session(rig: &Rig) -> Vec<Value> {
    let files = rig.session_files();
    assert_eq!(files.len(), 1, "one session file: {files:?}");
    session_lines(&files[0])
}

/// The requests the engine got for branch summaries.
#[cfg(target_os = "linux")]
fn summary_requests(rig: &Rig) -> Vec<Recorded> {
    rig.generating()
        .into_iter()
        .filter(|r| r.kind == "branch_summary")
        .collect()
}

/// The `branch_summary` entries of the session file.
#[cfg(target_os = "linux")]
fn summary_entries(rig: &Rig) -> Vec<Value> {
    the_session(rig)
        .into_iter()
        .filter(|l| l["type"] == "branch_summary")
        .collect()
}

/// The id of the message entry whose user/assistant content is `text`.
#[cfg(target_os = "linux")]
fn entry_id(lines: &[Value], text: &str) -> String {
    lines
        .iter()
        .find(|l| l["type"] == "message" && l["message"]["content"] == text)
        .and_then(|l| l["id"].as_str())
        .unwrap_or_else(|| panic!("no entry {text:?} in {lines:#?}"))
        .to_string()
}

/// The parent of an entry, skipping oxa's usage records (an idle prefill after a navigation
/// appends one at the new leaf; pi has no such entries).
#[cfg(target_os = "linux")]
fn parent_of(lines: &[Value], id: &str) -> String {
    let mut p = raw_parent(lines, id);
    while lines
        .iter()
        .any(|l| l["id"] == p.as_str() && l["type"] == "usage")
    {
        p = raw_parent(lines, &p);
    }
    p
}

#[cfg(target_os = "linux")]
fn raw_parent(lines: &[Value], id: &str) -> String {
    lines
        .iter()
        .find(|l| l["id"] == id)
        .and_then(|l| l["parentId"].as_str())
        .unwrap_or_default()
        .to_string()
}

#[test]
#[cfg(target_os = "linux")]
fn tree_001() {
    // pi-doc: sessions.md#choose-how-to-branch
    // (keybindings.md "Tree Navigation"; pi tree-selector.ts for the rows and the status labels.)
    let rig = two_branches(TestEnv::new(), Vec::new(), RigOpts::default());
    let s = open_tree(&rig);
    // pi's frame: title, key help, search line; the leaf is selected.
    assert!(
        s.contains("Session Tree") && s.contains("Type to search:"),
        "{s:?}"
    );
    assert!(s.contains("filters ctrl+d/t/u/l/a"), "key help\n{s:?}");
    assert!(selected(&s).contains("assistant: gamma answer"), "{s:?}");
    let rows = nav_rows(&s);
    // The branch point shows pi's connectors, the active path its markers.
    assert!(
        rows.iter().any(|r| r.contains("├─") || r.contains("├⊟")),
        "{rows:#?}"
    );
    assert!(
        rows.iter().any(|r| r.contains("└─") || r.contains("└⊟")),
        "{rows:#?}"
    );
    assert!(
        rows.iter().any(|r| r.contains("• user: gamma question")),
        "{rows:#?}"
    );
    assert!(
        rows.iter()
            .any(|r| r.contains("user: beta question") && !r.contains("• ")),
        "the abandoned branch is not on the active path\n{rows:#?}"
    );

    // Five filters: no-tools, user-only, labeled-only, all, back to default (Ctrl+D).
    rig.pty.send_key("ctrl+t");
    let s = rig
        .pty
        .wait_for_named("[no-tools]", |s| position(s).ends_with("[no-tools]"), WAIT)
        .unwrap();
    assert!(
        !nav_rows(&s)
            .iter()
            .any(|r| r.contains("notes.md") && r.contains("tool")),
        "{s:?}"
    );
    rig.pty.send_key("ctrl+u");
    let s = rig
        .pty
        .wait_for_named("[user]", |s| position(s).ends_with("[user]"), WAIT)
        .unwrap();
    let rows = nav_rows(&s);
    assert_eq!(rows.len(), 3, "three user messages\n{rows:#?}");
    assert!(rows.iter().all(|r| r.contains("user: ")), "{rows:#?}");
    rig.pty.send_key("ctrl+l");
    let s = rig
        .pty
        .wait_for_named("[labeled]", |s| position(s).ends_with("[labeled]"), WAIT)
        .unwrap();
    assert!(
        s.contains("No entries found") && position(&s).starts_with("(0/0)"),
        "{s:?}"
    );
    rig.pty.send_key("ctrl+a");
    let s = rig
        .pty
        .wait_for_named("[all]", |s| position(s).ends_with("[all]"), WAIT)
        .unwrap();
    assert!(
        s.contains("[model: fake-model]"),
        "settings entries in the all view\n{s:?}"
    );
    rig.pty.send_key("ctrl+d");
    let s = rig
        .pty
        .wait_for_named("default", |s| !position(s).contains('['), WAIT)
        .unwrap();
    // Ctrl+O cycles forward (default -> no-tools), Shift+Ctrl+O backward.
    rig.pty.send_key("ctrl+o");
    rig.pty
        .wait_for_named("cycled", |s| position(s).ends_with("[no-tools]"), WAIT)
        .unwrap();
    rig.pty
        .send_key_as("ctrl+shift+o", oxa_parity::pty::KeyEncoding::Kitty(1));
    rig.pty
        .wait_for_named("cycled back", |s| !position(s).contains('['), WAIT)
        .unwrap();
    let _ = s;

    // Fold / unfold: the gamma branch start folds its subtree (⊞) and unfolds again.
    select_row(&rig, "user: gamma question", "up");
    rig.pty.send_key("ctrl+left");
    let s = rig
        .pty
        .wait_for_named("folded", |s| selected(s).contains("⊞"), WAIT)
        .unwrap();
    assert!(!nav_rows(&s).iter().any(|r| r.contains("gamma answer")), "{s:?}");
    rig.pty.send_key("ctrl+right");
    let s = rig
        .pty
        .wait_for_named(
            "unfolded",
            |s| nav_rows(s).iter().any(|r| r.contains("gamma answer")),
            WAIT,
        )
        .unwrap();
    assert!(!selected(&s).contains("⊞"), "{s:?}");

    // Labels + timestamps: Shift+L labels the entry, Shift+T shows the label time.
    select_row(&rig, "user: alpha question", "up");
    rig.pty.send_key("shift+l");
    rig.pty.wait_for_text("Label (empty to remove)").unwrap();
    rig.pty.send_text("start here");
    rig.pty.send_key("enter");
    let s = rig
        .pty
        .wait_for_named(
            "labelled row",
            |s| selected(s).contains("[start here] ") && selected(s).contains("user: alpha question"),
            WAIT,
        )
        .unwrap();
    assert!(s.contains("Session Tree"), "the navigator stays open");
    rig.pty.send_key("shift+t");
    let s = rig
        .pty
        .wait_for_named("label time", |s| position(s).ends_with("[+label time]"), WAIT)
        .unwrap();
    let row = selected(&s);
    let after = row.split("[start here] ").nth(1).unwrap_or_default();
    let ts: String = after.chars().take(5).collect();
    assert!(
        ts.len() == 5 && ts.as_bytes()[2] == b':' && ts.chars().filter(|c| c.is_ascii_digit()).count() == 4,
        "HH:MM label time today: {row:?}"
    );
    rig.pty.send_key("ctrl+l");
    let s = rig
        .pty
        .wait_for_named("[labeled]", |s| position(s).contains("[labeled]"), WAIT)
        .unwrap();
    assert_eq!(nav_rows(&s).len(), 1, "one labeled entry\n{s:?}");
    // The label is an entry of the session file (pi `appendLabelChange`).
    let lines = the_session(&rig);
    let alpha = entry_id(&lines, "alpha question");
    assert!(
        lines
            .iter()
            .any(|l| l["type"] == "label" && l["targetId"] == alpha.as_str() && l["label"] == "start here"),
        "label entry\n{lines:#?}"
    );
    // Escape closes the navigator, nothing moved.
    rig.pty.send_key("escape");
    rig.pty.wait_for_text_gone("Session Tree").unwrap();
}

#[test]
#[cfg(target_os = "linux")]
fn tree_002() {
    // pi-doc: sessions.md#choose-how-to-branch
    // "In /tree, select a user message to put its text back in the editor. Edit and submit it to
    // create another branch."
    let rig = two_branches(TestEnv::new(), Vec::new(), RigOpts::default());
    // The screen after the navigation that `two_branches` made showed beta's text in the
    // editor; gamma was sent instead: the request after the move holds alpha only.
    let reqs = rig.generating();
    let last = reqs.last().unwrap();
    assert_eq!(user_texts(last), vec!["alpha question", "gamma question"]);
    assert!(
        !contents(last).iter().any(|c| c.contains("beta")),
        "the abandoned branch is not sent: {:?}",
        contents(last)
    );
    // The gamma message is a sibling of beta in the file (same parent: the leaf moved to beta's
    // parent).
    let lines = the_session(&rig);
    let beta = entry_id(&lines, "beta question");
    let gamma = entry_id(&lines, "gamma question");
    assert_eq!(parent_of(&lines, &gamma), parent_of(&lines, &beta), "{lines:#?}");

    // Once more on screen: the user message fills the editor, the transcript ends before it.
    open_tree(&rig);
    pick(&rig, "user: gamma question", Some(0));
    let s = navigated(&rig, |s| !s.contains("gamma answer"));
    let s = rig
        .pty
        .wait_for_named(
            "gamma back in the editor",
            |s| editor_text(s) == "gamma question",
            WAIT,
        )
        .unwrap_or(s);
    assert_eq!(editor_text(&s), "gamma question");
    assert!(s.contains("alpha answer") && !s.contains("gamma answer"), "{s:?}");
}

#[test]
#[cfg(target_os = "linux")]
fn tree_003() {
    // pi-doc: sessions.md#choose-how-to-branch
    // "Selecting an assistant response or another entry continues after that entry with an
    // empty editor."
    let rig = two_branches(
        TestEnv::new(),
        vec![ScriptTurn::text("delta answer")],
        RigOpts::default(),
    );
    open_tree(&rig);
    pick(&rig, "assistant: beta answer", Some(0));
    let s = navigated(&rig, |s| s.contains("beta answer") && !s.contains("gamma answer"));
    assert_eq!(editor_text(&s), "", "the editor stays empty");
    assert!(s.contains("beta answer") && !s.contains("gamma answer"), "{s:?}");
    rig.prompt("delta question", "delta answer");
    let last = rig.generating().last().cloned().unwrap();
    assert_eq!(
        user_texts(&last),
        vec!["alpha question", "beta question", "delta question"],
        "the branch continues after the beta answer"
    );
    let lines = the_session(&rig);
    let beta_answer = entry_id(&lines, "beta answer");
    let delta = entry_id(&lines, "delta question");
    // The new message hangs below the beta answer.
    assert_eq!(parent_of(&lines, &delta), beta_answer, "{lines:#?}");
}

#[test]
fn tree_004() {
    // pi-doc: sessions.md#choose-how-to-branch
    // invariant: W1 (append-only wire, exactly one rebase per leaf move). A leaf move back to
    // the parent of an earlier user message is observed through `fork`, the surface the binary
    // has for it: the next request carries the kept prefix only, never the abandoned message.
    let env = TestEnv::new();
    let srv = server(
        vec![
            ScriptTurn::text("first reply"),
            ScriptTurn::text("second reply"),
            ScriptTurn::text("third reply"),
        ],
        FakeOpts::default(),
    );
    let mut p = Rpc::start(&env, &srv.url());
    p.prompt("1", "one");
    p.prompt("2", "two");
    let fm = p.send(json!({"id": "3", "type": "get_fork_messages"}));
    let entry_two = fm["data"]["messages"][1]["entryId"]
        .as_str()
        .expect("entry id of `two`")
        .to_string();
    p.send(json!({"id": "4", "type": "fork", "entryId": entry_two}));
    p.prompt("5", "three");
    p.finish();
    let reqs = sorted(&srv);
    assert_eq!(reqs.len(), 3, "one request per prompt");
    let texts = contents(&reqs[2]);
    // RED TREE-004 (real divergence): pi's fork default is `position: "before"` (the leaf moves to
    // the parent of the chosen user message, its text goes back to the editor; rpc-commands.md
    // `fork`). oxa's `fork_at` keeps the chosen message on the wire (leaf = entry), so the
    // abandoned message is still sent.
    assert_eq!(
        texts,
        vec!["one".to_string(), "first reply".to_string(), "three".to_string()],
        "RED TREE-004: fork at a user message keeps that message on the wire (oxa fork_at: leaf = \
         entry); pi's default leaves the chosen user message off the branch (position before)"
    );
}

#[test]
#[cfg(target_os = "linux")]
fn tree_005() {
    // pi-doc: compaction.md#branch-summarization
    // "When you use /tree to navigate to a different branch, Pi offers to summarize the work
    // you're leaving." interactive-mode.ts: "Summarize branch?" -> "No summary" / "Summarize" /
    // "Summarize with custom prompt" (an editor for the instructions); settings.md
    // `branchSummary.skipPrompt` skips the question (no summary).
    let rig = two_branches(
        TestEnv::new(),
        vec![ScriptTurn::text("SUMMARY ONE"), ScriptTurn::text("SUMMARY TWO")],
        RigOpts::default(),
    );
    // "No summary" was answered in `two_branches`: no summary request so far.
    assert!(summary_requests(&rig).is_empty());

    // "Summarize": one summary request, the summary block on the new branch.
    open_tree(&rig);
    pick(&rig, "assistant: beta answer", Some(1));
    let s = navigated(&rig, |s| {
        s.contains("Branch summary") && !s.contains("gamma answer")
    });
    assert!(s.contains("beta answer"), "{s:?}");
    assert_eq!(summary_requests(&rig).len(), 1);

    // "Summarize with custom prompt": the instructions editor, then a request that carries them.
    open_tree(&rig);
    pick(&rig, "assistant: gamma answer", Some(2));
    rig.pty
        .wait_for_text("Custom summarization instructions")
        .unwrap();
    rig.pty.send_text("focus on OMEGA-42");
    rig.pty.wait_for_text("focus on OMEGA-42").unwrap();
    rig.pty.send_key("enter");
    navigated(&rig, |s| s.contains("gamma answer"));
    let reqs = summary_requests(&rig);
    assert_eq!(reqs.len(), 2);
    let last_user = user_texts(&reqs[1]).pop().unwrap();
    assert!(last_user.contains("focus on OMEGA-42"), "{last_user}");
    assert_eq!(summary_entries(&rig).len(), 2);
    drop(rig);

    // branchSummary.skipPrompt: no question, no summary.
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/settings.json",
        &json!({"branchSummary": {"skipPrompt": true}}).to_string(),
    );
    let rig = Rig::start_in(env, rig::replies(3), RigOpts::default());
    rig.pty.wait_for_text("ctrl+d exit").unwrap();
    rig.prompt("first question", "reply 1");
    rig.prompt("second question", "reply 2");
    open_tree(&rig);
    let mark = rig.pty.mark();
    pick(&rig, "assistant: reply 1", None);
    navigated(&rig, |s| !s.contains("reply 2"));
    let raw = String::from_utf8_lossy(&rig.pty.raw_since(mark)).to_string();
    assert!(!raw.contains("Summarize branch?"), "the question was skipped");
    assert!(summary_requests(&rig).is_empty());
    assert!(summary_entries(&rig).is_empty());
}

#[test]
#[cfg(target_os = "linux")]
fn tree_006() {
    // pi-doc: compaction.md#branch-summarization
    // invariant: pure append (the request is the abandoned branch's wire plus one instruction,
    // foreground because the user waits on it, F12).
    let rig = two_branches(
        TestEnv::new(),
        vec![ScriptTurn::text("SUMMARY ONE")],
        RigOpts::default(),
    );
    let gamma_req = rig.generating().last().cloned().unwrap();
    open_tree(&rig);
    pick(&rig, "assistant: beta answer", Some(1));
    navigated(&rig, |s| s.contains("Branch summary"));
    let reqs = summary_requests(&rig);
    assert_eq!(reqs.len(), 1);
    let sum = &reqs[0];
    // The abandoned branch's wire (what the gamma request sent, plus the gamma reply) ...
    let n = gamma_req.messages.len();
    assert_eq!(
        sum.messages[..n],
        gamma_req.messages[..],
        "the old wire is the prefix"
    );
    assert_eq!(sum.messages[n].content.as_deref(), Some("gamma answer"));
    // ... plus exactly one instruction (a user message), nothing else.
    assert_eq!(sum.messages.len(), n + 2, "{:?}", contents(sum));
    assert_eq!(sum.messages[n + 1].role, oxa_proto::Role::User);
    assert_eq!(sum.system, gamma_req.system, "same frozen prefix");
    assert!(!sum.background, "foreground: the user waits on it");
    // The summary request appends to the gamma request's cache bytes (W1: one rebase later).
    let held = cache_bytes(&gamma_req.system, &gamma_req.tools_json, &gamma_req.messages);
    let now = cache_bytes(&sum.system, &sum.tools_json, &sum.messages);
    assert!(now.starts_with(&held), "pure append over the old branch");
}

#[test]
#[cfg(target_os = "linux")]
fn tree_007() {
    // pi-doc: compaction.md#cumulative-file-tracking
    // "Branch summarization carries file lists from Pi-generated branch summaries in the
    // entries it summarizes."
    let env = TestEnv::new();
    std::fs::write(env.work().join("notes.md"), "the notes say OMEGA-42\n").unwrap();
    let script = vec![
        ScriptTurn::text("alpha answer"),
        ScriptTurn::tool("call_1", "read", r#"{"path":"notes.md"}"#),
        ScriptTurn::text("beta answer"),
        ScriptTurn::text("SUMMARY ONE"),
        ScriptTurn::tool("call_2", "write", r#"{"path":"made.txt","content":"made\n"}"#),
        ScriptTurn::text("gamma answer"),
        ScriptTurn::text("SUMMARY TWO"),
    ];
    let rig = Rig::start_in(env, script, RigOpts::default());
    rig.pty.wait_for_text("ctrl+d exit").unwrap();
    rig.prompt("alpha question", "alpha answer");
    rig.prompt("beta question", "beta answer");
    // Leave the beta branch (it read notes.md) with a summary.
    open_tree(&rig);
    pick(&rig, "assistant: alpha answer", Some(1));
    navigated(&rig, |s| {
        s.contains("Branch summary") && !s.contains("beta answer")
    });
    let first = summary_entries(&rig);
    assert_eq!(first.len(), 1);
    assert_eq!(
        first[0]["details"]["readFiles"],
        json!(["notes.md"]),
        "{first:#?}"
    );
    // On the new branch (below the first summary) write a file, then leave it with a summary.
    rig.prompt("gamma question", "gamma answer");
    assert!(rig.env.work().join("made.txt").exists(), "the write ran");
    open_tree(&rig);
    pick(&rig, "assistant: alpha answer", Some(1));
    navigated(&rig, |s| !s.contains("gamma answer"));
    let both = summary_entries(&rig);
    assert_eq!(both.len(), 2, "{both:#?}");
    let second = &both[1];
    // Cumulative: the nested summary's read file is carried, the new write is listed.
    assert_eq!(second["details"]["readFiles"], json!(["notes.md"]), "{second:#?}");
    assert_eq!(
        second["details"]["modifiedFiles"],
        json!(["made.txt"]),
        "{second:#?}"
    );
}

#[test]
#[cfg(target_os = "linux")]
fn tree_008() {
    // pi-doc: compaction.md#branch-summarization
    // "Append entry: Save BranchSummaryEntry at navigation point": parentId = the new leaf,
    // fromId = the entry navigated from; the summary heads the new branch's context.
    let rig = two_branches(
        TestEnv::new(),
        vec![ScriptTurn::text("SUMMARY ONE"), ScriptTurn::text("delta answer")],
        RigOpts::default(),
    );
    let before = the_session(&rig);
    let gamma_answer = entry_id(&before, "gamma answer");
    let beta_answer = entry_id(&before, "beta answer");
    open_tree(&rig);
    pick(&rig, "assistant: beta answer", Some(1));
    navigated(&rig, |s| s.contains("Branch summary"));
    let lines = the_session(&rig);
    let entries = summary_entries(&rig);
    assert_eq!(entries.len(), 1);
    let e = &entries[0];
    assert_eq!(
        e["parentId"].as_str(),
        Some(beta_answer.as_str()),
        "at the navigation point"
    );
    let from = e["fromId"].as_str().unwrap_or_default().to_string();
    // The old leaf: the gamma answer (oxa's usage record after it counts as the same point).
    assert!(
        from == gamma_answer || (raw_parent(&lines, &from) == gamma_answer),
        "fromId {from} is the old leaf\n{lines:#?}"
    );
    assert!(
        e["summary"].as_str().unwrap_or_default().contains("SUMMARY ONE"),
        "{e:#}"
    );
    assert_eq!(e["fromHook"], json!(false));
    // The next request carries the summary after beta's branch and before the new prompt.
    rig.prompt("delta question", "delta answer");
    let last = rig.generating().last().cloned().unwrap();
    let texts = contents(&last);
    let k = texts
        .iter()
        .position(|t| t.contains("SUMMARY ONE"))
        .expect("summary in context");
    let beta = texts.iter().position(|t| t == "beta answer").unwrap();
    assert!(beta < k && k + 1 == texts.len() - 1, "{texts:#?}");
    assert!(!texts.iter().any(|t| t.contains("gamma")), "{texts:#?}");
}

#[test]
#[cfg(target_os = "linux")]
fn tree_009() {
    // pi-doc: sessions.md#choose-how-to-branch
    // The model and thinking level in force are recorded on the branch entered when they
    // differ from what that branch recorded (they stay in force; the next request uses them).
    let rig = two_branches(
        TestEnv::new(),
        vec![ScriptTurn::text("delta answer")],
        RigOpts::default(),
    );
    // Change the thinking level on the gamma branch (Shift+Tab cycles it).
    let before = rig.screen().row(29);
    rig.pty.send_key("shift+tab");
    let s = rig
        .pty
        .wait_for_named("thinking changed", |s| s.row(29) != before, WAIT)
        .unwrap();
    let level = s
        .row(29)
        .rsplit('•')
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    assert_ne!(level, "off", "{s:?}");
    let lines = the_session(&rig);
    assert!(
        lines
            .iter()
            .any(|l| l["type"] == "thinking_level_change" && l["thinkingLevel"] == level.as_str()),
        "recorded on the gamma branch\n{lines:#?}"
    );
    open_tree(&rig);
    pick(&rig, "assistant: beta answer", Some(0));
    let s = navigated(&rig, |s| !s.contains("gamma answer"));
    assert!(
        s.row(29).ends_with(&level),
        "the level stays in force: {:?}",
        s.row(29)
    );
    // The beta branch recorded `off`: the current level is appended on it, below beta answer.
    let lines = the_session(&rig);
    let beta_answer = entry_id(&lines, "beta answer");
    let rec = lines
        .iter()
        .rev()
        .find(|l| l["type"] == "thinking_level_change")
        .cloned()
        .unwrap();
    assert_eq!(rec["thinkingLevel"], json!(level));
    assert_eq!(
        parent_of(&lines, rec["id"].as_str().unwrap()),
        beta_answer,
        "{lines:#?}"
    );
    // The model is the same on both branches: no model_change on the new branch.
    let models = lines.iter().filter(|l| l["type"] == "model_change").count();
    assert_eq!(models, 1, "{lines:#?}");
}

#[test]
#[cfg(target_os = "linux")]
fn tree_010() {
    // pi-doc: settings.md
    // `doubleEscapeAction` "tree" | "fork" | "none", default "tree": "Action for double Escape
    // with an empty editor." (interactive-mode.ts: two Escapes within 500 ms.)
    for (setting, expect) in [
        (None, Some("Session Tree")),
        (Some("tree"), Some("Session Tree")),
        (Some("fork"), Some("Fork from Message")),
        (Some("none"), None),
    ] {
        let env = TestEnv::new();
        if let Some(v) = setting {
            env.write(
                "home/.oxa/agent/settings.json",
                &json!({"doubleEscapeAction": v}).to_string(),
            );
        }
        let rig = Rig::start_in(env, rig::replies(2), RigOpts::default());
        rig.pty.wait_for_text("ctrl+d exit").unwrap();
        rig.prompt("first question", "reply 1");
        // Two Escapes in one write arrive well within the 500 ms window. (A lone ESC needs the
        // ESC timeout to pass before it counts, so they go as two writes.)
        rig.pty.send_key("escape");
        rig.pty.send_key("escape");
        match expect {
            Some(title) => {
                let s = rig.pty.wait_for_text(title).unwrap();
                assert!(s.contains("first question"), "{setting:?}: {s:?}");
                rig.pty.send_key("escape");
                rig.pty.wait_for_text_gone(title).unwrap();
            }
            None => {
                let s = rig.pty.wait_stable(700, WAIT).unwrap();
                assert!(
                    !s.contains("Session Tree") && !s.contains("Fork from Message"),
                    "none: nothing opens\n{s:?}"
                );
            }
        }
        // A non-empty editor: Escape twice opens nothing (and keeps the text).
        rig.type_text("draft");
        rig.pty.send_key("escape");
        rig.pty.send_key("escape");
        let s = rig.pty.wait_stable(700, WAIT).unwrap();
        assert!(
            !s.contains("Session Tree") && !s.contains("Fork from Message"),
            "{s:?}"
        );
        assert_eq!(editor_text(&s), "draft", "{setting:?}");
    }
}

#[test]
#[cfg(target_os = "linux")]
fn tree_011() {
    // pi-doc: compaction.md#extension-hooks
    // session_before_tree gets `preparation` {targetId, oldLeafId, commonAncestorId,
    // entriesToSummarize, userWantsSummary}; it may cancel or supply the summary (used when the
    // user wants one); session_tree fires after with {newLeafId, oldLeafId, summaryEntry,
    // fromExtension} (pi extensions/types.ts).
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/extensions/treehook.js",
        r#"const LOG = "tree-log.txt";
function note(ctx, s) {
  let p = "";
  try { p = ctx.fs.readFile(LOG) || ""; } catch (e) {}
  ctx.fs.writeFile(LOG, p + s + "\n");
}
export default function (oxa) {
  oxa.on("session_before_tree", (e, ctx) => {
    const p = e.preparation;
    note(ctx, "before " + p.targetId + " " + p.oldLeafId + " " + p.commonAncestorId + " " +
      p.entriesToSummarize.length + " " + p.userWantsSummary);
    if (p.userWantsSummary) {
      return { summary: { summary: "HOOK SUMMARY TEXT", details: { by: "hook" } } };
    }
    let flag = "";
    try { flag = ctx.fs.readFile("cancel.flag") || ""; } catch (e) {}
    if (flag) return { cancel: true };
  });
  oxa.on("session_tree", (e, ctx) => {
    note(ctx, "after " + e.newLeafId + " " + e.oldLeafId + " " +
      (e.summaryEntry ? e.summaryEntry.type : "none") + " " + e.fromExtension);
  });
}
"#,
    );
    let rig = two_branches(env, vec![ScriptTurn::text("delta answer")], RigOpts::default());
    let log = || std::fs::read_to_string(rig.env.work().join("tree-log.txt")).unwrap_or_default();
    // `two_branches` navigated once ("No summary", no cancel flag): the hooks saw it.
    let first = log();
    assert!(
        first.starts_with("before ") && first.contains("\nafter "),
        "{first:?}"
    );
    let lines = the_session(&rig);
    let beta_answer = entry_id(&lines, "beta answer");
    let gamma_answer = entry_id(&lines, "gamma answer");

    // Cancel: "No summary" -> the hook cancels, the leaf stays (pi "Navigation cancelled").
    std::fs::write(rig.env.work().join("cancel.flag"), "yes").unwrap();
    open_tree(&rig);
    pick(&rig, "assistant: beta answer", Some(0));
    rig.pty.wait_for_text("Navigation cancelled").unwrap();
    let s = rig.settle();
    assert!(s.contains("gamma answer"), "nothing moved\n{s:?}");
    assert!(summary_entries(&rig).is_empty());
    std::fs::remove_file(rig.env.work().join("cancel.flag")).unwrap();

    // Summary from the hook: "Summarize" -> no engine request, the hook's text is stored.
    let n = rig.generating().len();
    open_tree(&rig);
    pick(&rig, "assistant: beta answer", Some(1));
    navigated(&rig, |s| s.contains("Branch summary"));
    assert_eq!(rig.generating().len(), n, "no summary request to the engine");
    let e = summary_entries(&rig).pop().unwrap();
    assert!(
        e["summary"].as_str().unwrap().contains("HOOK SUMMARY TEXT"),
        "{e:#}"
    );
    assert_eq!(e["fromHook"], json!(true));
    assert_eq!(e["details"], json!({"by": "hook"}));
    let log = log();
    let lines: Vec<&str> = log.lines().collect();
    // The last navigation: before (with the preparation) and after (with the summary entry).
    let before = lines[lines.len() - 2];
    let after = lines[lines.len() - 1];
    let fields: Vec<&str> = before.split(' ').collect();
    assert_eq!(fields[1], beta_answer, "targetId: {before}");
    let sess = the_session(&rig);
    assert!(
        fields[2] == gamma_answer || raw_parent(&sess, fields[2]) == gamma_answer,
        "oldLeafId is the gamma point: {before}"
    );
    // Both branches leave from where beta question and gamma question hang.
    let fork_point = raw_parent(&the_session(&rig), &entry_id(&the_session(&rig), "beta question"));
    assert_eq!(fields[3], fork_point, "commonAncestorId: {before}");
    assert_ne!(fields[4], "0", "entriesToSummarize: {before}");
    assert_eq!(fields[5], "true", "userWantsSummary: {before}");
    assert!(
        after.starts_with("after ") && after.ends_with(" branch_summary true"),
        "{after}"
    );
    assert_eq!(
        after.split(' ').nth(1),
        e["id"].as_str(),
        "newLeafId = the summary entry: {after}"
    );
}

#[test]
fn tree_012() {
    // pi-doc: sessions.md#choose-how-to-branch
    // invariant: W1 + engine cache keyed by session (X-Session-Id). Returning to an earlier leaf
    // must reuse the held cache: the request's cache bytes extend the held bytes of the session.
    let env = TestEnv::new();
    let srv = server(
        vec![
            ScriptTurn::text("first reply"),
            ScriptTurn::text("second reply"),
            ScriptTurn::text("third reply"),
        ],
        FakeOpts::default(),
    );
    let mut p = Rpc::start(&env, &srv.url());
    p.prompt("1", "one");
    p.prompt("2", "two");
    let fm = p.send(json!({"id": "3", "type": "get_fork_messages"}));
    let entry_two = fm["data"]["messages"][1]["entryId"]
        .as_str()
        .expect("entry id of `two`")
        .to_string();
    p.send(json!({"id": "4", "type": "fork", "entryId": entry_two}));
    p.prompt("5", "three");
    p.finish();
    let reqs = sorted(&srv);
    let before = &reqs[1];
    let after = &reqs[2];
    let kept = &before.messages[..2];
    let prefix = cache_bytes(&after.system, &after.tools_json, kept);
    let held = srv
        .held(&after.session)
        .unwrap_or_else(|| panic!("no cache held for session {} after the fork", after.session));
    assert!(
        held.starts_with(&prefix),
        "the engine did not keep the cache of the kept prefix for session {}",
        after.session
    );
}

// ---------------------------------------------------------------------------------------------
// ENG rows (oxa only, O-144): the engine has one slot, queues, and advertises its hints
// ---------------------------------------------------------------------------------------------

/// `oxa --mode rpc` read on a thread, so every wait has a deadline (the ENG rows race a held
/// request against user input; a hang must fail with the lines seen, not block the suite).
struct TimedRpc {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    rx: std::sync::mpsc::Receiver<Value>,
    lines: Vec<Value>,
}

impl TimedRpc {
    fn start(env: &TestEnv, url: &str) -> TimedRpc {
        use std::io::BufRead;
        use std::process::{Command, Stdio};
        let mut child = Command::new(oxa_parity::oxa_bin())
            .args(["--mode", "rpc"])
            .current_dir(env.work())
            .env_clear()
            .envs(env.envs(url))
            .envs(oxa_parity::note_off_env())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn oxa --mode rpc (build it first: cargo build -p oxa-cli -p oxa-fake)");
        let stdin = child.stdin.take();
        let out = std::io::BufReader::new(child.stdout.take().expect("stdout"));
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in out.lines().map_while(Result::ok) {
                if let Ok(v) = serde_json::from_str::<Value>(line.trim()) {
                    if tx.send(v).is_err() {
                        break;
                    }
                }
            }
        });
        TimedRpc {
            child,
            stdin,
            rx,
            lines: Vec::new(),
        }
    }

    fn post(&mut self, cmd: Value) {
        use std::io::Write;
        let stdin = self.stdin.as_mut().expect("stdin open");
        writeln!(stdin, "{cmd}").expect("write command");
        stdin.flush().expect("flush command");
    }

    /// The first line (from now) matching `f`, within 30 s.
    fn wait(&mut self, what: &str, f: impl Fn(&Value) -> bool) -> Value {
        let end = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let left = end.saturating_duration_since(std::time::Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(v) => {
                    self.lines.push(v.clone());
                    if f(&v) {
                        return v;
                    }
                }
                Err(_) => {
                    let _ = self.child.kill();
                    panic!("no {what} within 30 s; lines: {:#?}", self.lines);
                }
            }
        }
    }

    fn response(&mut self, id: &str) -> Value {
        self.wait(&format!("response {id}"), |v| {
            v.get("type") == Some(&json!("response")) && v.get("id") == Some(&json!(id))
        })
    }

    fn event(&mut self, name: &str) -> Value {
        self.wait(name, |v| v.get("type") == Some(&json!(name)))
    }

    /// A prompt that must be accepted, then its run settled.
    fn prompt(&mut self, id: &str, message: &str) {
        self.post(json!({"id": id, "type": "prompt", "message": message}));
        let r = self.response(id);
        assert_eq!(r.get("success"), Some(&json!(true)), "{r}");
        self.event("agent_settled");
    }

    fn finish(mut self) -> Vec<Value> {
        self.stdin.take();
        let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while let Ok(v) = self
            .rx
            .recv_timeout(end.saturating_duration_since(std::time::Instant::now()))
        {
            self.lines.push(v);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.lines
    }
}

/// Wait (up to 10 s) until the fake has recorded a request matching `f`.
fn wait_request(srv: &FakeServer, f: impl Fn(&Recorded) -> bool) {
    let t = std::time::Instant::now();
    while !srv.requests().iter().any(&f) {
        assert!(
            t.elapsed() < std::time::Duration::from_secs(10),
            "the expected request never came"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[test]
fn eng_001() {
    // invariant: one engine slot; a second in-flight request would only queue and park the first
    // one's state. Observed with the fake's in-flight counter over a session that sends a chat
    // turn, a compaction summary, an idle prefill (held 400 ms by the fake) and two more turns,
    // the first of them typed while the prefill runs.
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"compaction": {"keepRecentTokens": 1}}"#,
    );
    let srv = server(
        vec![
            ScriptTurn::text("first reply"),
            ScriptTurn::text("first b reply"),
            ScriptTurn::text("summary of the session"),
            ScriptTurn::text("second reply"),
            ScriptTurn::text("third reply"),
        ],
        FakeOpts {
            hold_prefill_ms: 400,
            done_gap_ms: 50,
            ..FakeOpts::default()
        },
    );
    let mut p = TimedRpc::start(&env, &srv.url());
    p.prompt("1", "one");
    p.prompt("1b", "one b");
    p.post(json!({"id": "2", "type": "compact"}));
    wait_request(&srv, |r| r.max_tokens == 0);
    p.post(json!({"id": "3", "type": "prompt", "message": "two", "streamingBehavior": "followUp"}));
    let r3 = p.response("3");
    assert_eq!(
        r3.get("success"),
        Some(&json!(true)),
        "the follow-up is accepted: {r3}"
    );
    p.event("agent_settled");
    p.prompt("4", "three");
    p.finish();
    let kinds: Vec<String> = srv.requests().iter().map(|r| r.kind.clone()).collect();
    assert!(
        kinds.iter().any(|k| k == "compaction") && kinds.iter().any(|k| k == "prefill"),
        "the session sent a summary and a prefill: {kinds:?}"
    );
    assert_eq!(
        srv.max_in_flight(),
        1,
        "two model requests of one instance were in flight at once: {:?}",
        srv.timings()
    );
    let mut t = srv.timings();
    t.sort_by_key(|x| x.start_ms);
    for w in t.windows(2) {
        assert!(w[0].end_ms <= w[1].start_ms, "overlapping requests: {w:?}");
    }
}

#[test]
fn eng_002() {
    // invariant: H3 orders the queue but does not cut a running request; a cancelled prefill is
    // skipped silently and the engine keeps its progress. The fake holds the idle prefill after
    // `compact` for 5 s; a prompt typed meanwhile cancels it (the client closes the connection),
    // runs at once, and no warning is shown. The prompt is a follow-up (`streamingBehavior`), as pi
    // requires while the session works (the `/compact` is still running its prefill).
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"compaction": {"keepRecentTokens": 1}}"#,
    );
    let srv = server(
        vec![
            ScriptTurn::text("first reply"),
            ScriptTurn::text("first b reply"),
            ScriptTurn::text("summary of the session"),
            ScriptTurn::text("second reply"),
        ],
        FakeOpts {
            hold_prefill_ms: 5000,
            ..FakeOpts::default()
        },
    );
    let mut p = TimedRpc::start(&env, &srv.url());
    p.prompt("1", "one");
    p.prompt("1b", "one b");
    p.post(json!({"id": "2", "type": "compact"}));
    wait_request(&srv, |r| r.max_tokens == 0);
    let typed = std::time::Instant::now();
    p.post(json!({"id": "3", "type": "prompt", "message": "two", "streamingBehavior": "followUp"}));
    let r3 = p.response("3");
    assert_eq!(
        r3.get("success"),
        Some(&json!(true)),
        "the follow-up is accepted: {r3}"
    );
    p.event("agent_settled");
    let took = typed.elapsed();
    let lines = p.finish();
    assert!(
        took < std::time::Duration::from_millis(3000),
        "the prompt waited for the held prefill ({took:?})"
    );
    let t = srv.timings();
    let prefill = t
        .iter()
        .find(|x| x.max_tokens == Some(0))
        .unwrap_or_else(|| panic!("no prefill timing: {t:?}"));
    assert!(
        prefill.client_closed,
        "the prefill was cancelled by the client: {t:?}"
    );
    assert!(prefill.end_ms - prefill.start_ms < 4000, "{t:?}");
    let last = t.iter().max_by_key(|x| x.start_ms).unwrap();
    assert!(
        last.start_ms >= prefill.end_ms,
        "the prompt ran after the cancel: {t:?}"
    );
    let reqs = sorted(&srv);
    let texts = contents(reqs.last().unwrap());
    assert_eq!(texts.last().map(String::as_str), Some("two"), "{texts:?}");
    let warned = lines.iter().any(|v| {
        v.to_string().contains("cache warming skipped")
            || v.to_string().contains("cache warming not recorded")
    });
    assert!(!warned, "a cancelled prefill is skipped silently");
}

#[test]
fn eng_003() {
    // invariant: controller queue (round-robin per session, cap 8 waiters); the queued comments
    // never time out and never change the exit code.
    let env = TestEnv::new();
    let srv = server(
        vec![ScriptTurn {
            fault: Some(Fault::Queued { count: 2, pos: true }),
            ..ScriptTurn::text("answer after the queue")
        }],
        FakeOpts::default(),
    );
    let out = json_mode(&env, &srv.url(), "hello");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let queued: Vec<Value> = stdout
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v.get("type") == Some(&json!("engine_queue")))
        .collect();
    assert_eq!(
        queued.len(),
        2,
        "two queued comments are shown as engine_queue records: {stdout}"
    );
    assert_eq!(queued[0]["pos"], json!(2), "{stdout}");
    assert_eq!(queued[1]["pos"], json!(1), "{stdout}");
    assert!(stdout.contains("answer after the queue"), "{stdout}");
}

#[test]
fn eng_003_busy_retry_keeps_idempotency_key() {
    // invariant: a 503 (engine_busy) is retried after Retry-After with the same Idempotency-Key
    // (the H2 replay key), so the engine can answer the same request once.
    let env = TestEnv::new();
    let srv = server(
        vec![
            ScriptTurn::fault(Fault::Busy { retry_after: Some(0) }),
            ScriptTurn::text("served after busy"),
        ],
        FakeOpts::default(),
    );
    let out = print_mode(&env, &srv.url(), "hello");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("served after busy"), "{stdout}");
    let reqs = sorted(&srv);
    assert!(reqs.len() >= 2, "the busy request is retried: {}", reqs.len());
    assert_eq!(
        reqs[0].idem_key, reqs[1].idem_key,
        "the retry keeps the Idempotency-Key"
    );
}

#[test]
fn eng_004() {
    // invariant: nothing is sent to a target model whose hard limit is below the current
    // context (O-145: compact first on the source model; the switch happens only when the rebased
    // head fits). Flash 16K with max_output 2048 and reserve 16384 has no hard limit above zero.
    let env = TestEnv::new();
    let srv = server(
        vec![
            ScriptTurn::text("first reply"),
            ScriptTurn::text("summary of the session"),
            ScriptTurn::text("reply on the source model"),
        ],
        FakeOpts {
            model: "big".to_string(),
            models: vec![
                FakeModel::moe("big", 131072),
                FakeModel::flash("small", 16384, Some(2048)),
            ],
            ..FakeOpts::default()
        },
    );
    let mut p = Rpc::start(&env, &srv.url());
    p.prompt("1", "hello");
    let switch = p.exchange(json!({"id": "2", "type": "set_model", "modelId": "small"}));
    assert_eq!(
        switch.get("success"),
        Some(&json!(false)),
        "the switch is refused: {switch}"
    );
    let err = switch["error"].as_str().unwrap_or("");
    assert!(
        err.contains("still big") && err.contains("small"),
        "both models named: {err}"
    );
    let state = p.send(json!({"id": "3", "type": "get_state"}));
    p.finish();
    assert_eq!(
        state["data"]["model"]["id"],
        json!("big"),
        "the switch above the limit did not happen"
    );
    let reqs = sorted(&srv);
    assert!(
        reqs.iter().all(|r| r.model != "small"),
        "a request reached the target model above its limit: {:?}",
        reqs.iter().map(|r| r.model.as_str()).collect::<Vec<_>>()
    );
    assert!(
        reqs.iter().any(|r| r.kind == "compaction" && r.model == "big"),
        "compact first, on the SOURCE model: {:?}",
        reqs.iter()
            .map(|r| (r.kind.as_str(), r.model.as_str()))
            .collect::<Vec<_>>()
    );

    // Control: a target whose hard limit is above the context is switched to, and the next
    // request goes to it (the fake lists both models on /v1/models).
    let env = TestEnv::new();
    let srv = server(
        vec![ScriptTurn::text("first reply"), ScriptTurn::text("on mid")],
        FakeOpts {
            model: "big".to_string(),
            models: vec![FakeModel::moe("big", 131072), FakeModel::moe("mid", 65536)],
            ..FakeOpts::default()
        },
    );
    let mut p = Rpc::start(&env, &srv.url());
    p.prompt("1", "hello");
    p.send(json!({"id": "2", "type": "set_model", "modelId": "mid"}));
    p.prompt("3", "again");
    p.finish();
    let models: Vec<String> = sorted(&srv).iter().map(|r| r.model.clone()).collect();
    assert_eq!(models, ["big", "mid"]);
}

#[test]
fn eng_005() {
    // invariant: per-alias limits come from /_local/status model_limits (X3), never from a table
    // in oxa. get_available_models must report the served model's own cap and max output.
    let env = TestEnv::new();
    let srv = server(
        vec![ScriptTurn::text("ok")],
        FakeOpts {
            model: "small".to_string(),
            models: vec![FakeModel::flash("small", 16384, Some(2048))],
            ..FakeOpts::default()
        },
    );
    let lines = rpc(
        &env,
        &srv.url(),
        &[json!({"id": "1", "type": "get_available_models"})],
    );
    let models = data(&lines, "1")["models"]
        .as_array()
        .expect("models array")
        .clone();
    let small = models
        .iter()
        .find(|m| m["id"] == json!("small"))
        .unwrap_or_else(|| panic!("model small missing from {models:?}"));
    assert_eq!(small["contextWindow"], json!(16384), "{models:?}");
    assert_eq!(small["maxTokens"], json!(2048), "{models:?}");
}

#[test]
fn eng_006() {
    // invariant: every request names its session (X-Session-Id); the park, the idle prefill and
    // X-Oxe-Snapshot: keep are sent only when the model advertises them, and preserve_thinking is
    // not sent to a model without that capability (Flash).
    let env = TestEnv::new();

    // Rust MoE: preserve_thinking is sent.
    let moe = server(
        vec![ScriptTurn::text("ok")],
        FakeOpts {
            model: "big".to_string(),
            models: vec![FakeModel::moe("big", 131072)],
            ..FakeOpts::default()
        },
    );
    assert_eq!(print_mode(&env, &moe.url(), "hi").status.code(), Some(0));
    let r = sorted(&moe);
    assert!(!r[0].session.is_empty(), "X-Session-Id is sent");
    assert_eq!(
        r[0].preserve_thinking,
        Some(true),
        "MoE advertises preserve_thinking"
    );

    // Flash: no preserve_thinking.
    let flash = server(
        vec![ScriptTurn::text("ok")],
        FakeOpts {
            model: "small".to_string(),
            models: vec![FakeModel::flash("small", 16384, Some(2048))],
            ..FakeOpts::default()
        },
    );
    assert_eq!(print_mode(&env, &flash.url(), "hi").status.code(), Some(0));
    let r = sorted(&flash);
    assert!(!r[0].session.is_empty(), "X-Session-Id is sent");
    assert_eq!(
        r[0].preserve_thinking, None,
        "Flash does not advertise preserve_thinking"
    );

    // An engine that claims nothing: no snapshot keep, no park, still a session name.
    let plain = server(
        vec![ScriptTurn::text("ok")],
        FakeOpts {
            model: "plain".to_string(),
            models: vec![FakeModel {
                alias: "plain".to_string(),
                cap: 131072,
                max_output: None,
                caps: Some(FakeCaps::none()),
            }],
            ..FakeOpts::default()
        },
    );
    assert_eq!(print_mode(&env, &plain.url(), "hi").status.code(), Some(0));
    let r = sorted(&plain);
    assert!(!r[0].session.is_empty(), "X-Session-Id is sent");
    assert!(
        !r[0].snapshot_keep,
        "X-Oxe-Snapshot is not sent without the advertised capability"
    );
    assert_eq!(header(&r[0], "x-oxe-snapshot"), None);
}
