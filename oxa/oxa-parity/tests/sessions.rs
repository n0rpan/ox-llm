//! G3 PARITY tests (sessions). Owner: task G3.
//! One test per PARITY.md row of this area, named after the row id (`SES-012` -> `fn ses_012()`).
//!
//! Every row test drives the real `oxa` binary (print mode, `--mode rpc`, flags) against the
//! in-process fake engine, inside an isolated `TestEnv`, or reads the session files it wrote.
//! Conventions (see `oxa-parity/src/lib.rs`):
//! * `// pi docs: <page>` names the pi 1.5.2 documented behaviour a P row compares with.
//! * `// invariant: <name>` names the W-invariant or file-safety rule an A row adapts to.
//! * O rows assert the `unsupported_in_oxa` error.
//! * A row that cannot be exercised, or that fails, is a RED test: it panics with
//!   `RED (<ROW>): <precise reason>`. RED rows are the H-wave input; they are not ignored.
//!
//! The parity crate has no `oxa-session` dependency (architect-owned Cargo.toml), so session
//! files are read as JSON lines here, in the documented pi format.

use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
use oxa_parity::oxa_bin;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::time::Duration;

#[path = "support/tui_rig.rs"]
mod rig;
#[path = "support/tree_pty.rs"]
mod tree_pty;
use rig::{session_lines, user_texts, Rig, RigOpts, Screen, WAIT};
use tree_pty::{open_tree, select_listed, select_row, selected};

// ---------------------------------------------------------------- helpers

/// Panic with the RED marker of a row: the behaviour is missing or wrong, and the reason is the
/// H-wave input.
fn red(row: &str, reason: &str) -> ! {
    panic!("RED ({row}): {reason}");
}

/// `oxa <args>` run in `cwd` with `env.envs(url)` plus `extra` variables, stdin closed.
fn oxa_in(env: &TestEnv, cwd: &Path, url: &str, extra: &[(&str, &str)], args: &[&str]) -> Output {
    let mut envs = env.envs(url);
    for (k, v) in extra {
        envs.push(((*k).to_string(), (*v).to_string()));
    }
    Command::new(oxa_bin())
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .envs(envs)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn oxa (build it first: cargo build -p oxa-cli -p oxa-fake)")
}

/// `oxa <args>` in the test's work directory.
fn oxa(env: &TestEnv, url: &str, args: &[&str]) -> Output {
    oxa_in(env, &env.work(), url, &[], args)
}

fn stdout_of(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr_of(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// `<agent_dir>/sessions`, the default session root.
fn sessions_root(env: &TestEnv) -> PathBuf {
    env.agent_dir().join("sessions")
}

/// pi's directory name for a cwd (`/a/b` -> `--a-b--`), as the file store names it.
fn session_dir_name(cwd: &Path) -> String {
    let s = cwd.to_string_lossy();
    let rest = s.strip_prefix('/').unwrap_or(&s);
    format!("--{}--", rest.replace('/', "-"))
}

/// Every `.jsonl` file below `dir`, recursively, sorted.
fn session_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect_jsonl(dir, &mut out);
    out.sort();
    out
}

fn collect_jsonl(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_jsonl(&p, out);
        } else if p.extension().is_some_and(|x| x == "jsonl") {
            out.push(p);
        }
    }
}

/// Every regular file below `dir`, recursively.
fn all_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            all_files(&p, out);
        } else {
            out.push(p);
        }
    }
}

/// `<ts>_<id>.jsonl` where `<ts>` = `YYYY-MM-DDTHH-MM-SS-mmmZ` and `<id>` a lowercase uuid v4.
fn is_session_file_name(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".jsonl") else {
        return false;
    };
    let Some((ts, id)) = stem.split_once('_') else {
        return false;
    };
    let tb = ts.as_bytes();
    let ts_ok = tb.len() == 24
        && tb.iter().enumerate().all(|(i, c)| match i {
            4 | 7 | 13 | 16 | 19 => *c == b'-',
            10 => *c == b'T',
            23 => *c == b'Z',
            _ => c.is_ascii_digit(),
        });
    let ib = id.as_bytes();
    let id_ok = ib.len() == 36
        && ib.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_digit() || (b'a'..=b'f').contains(c),
        });
    ts_ok && id_ok
}

fn mode_of(p: &Path) -> u32 {
    std::fs::metadata(p).expect("stat").permissions().mode() & 0o777
}

/// The JSON records of a session file, one per line.
fn records(path: &Path) -> Vec<Value> {
    let text = std::fs::read_to_string(path).expect("read session file");
    text.lines()
        .map(|l| serde_json::from_str::<Value>(l).expect("every line of a session file is JSON"))
        .collect()
}

fn header_of(path: &Path) -> Value {
    records(path).remove(0)
}

/// Entry ids of a session file (every record but the header), in file order.
fn entry_ids(path: &Path) -> Vec<String> {
    records(path)
        .iter()
        .filter(|r| r["type"] != "session")
        .map(|r| r["id"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// The user messages the engine received in one request, as plain text.
fn request_user_texts(server: &FakeServer, index: usize) -> Vec<String> {
    let reqs = server.requests();
    reqs[index]
        .messages
        .iter()
        .filter(|m| m.role == oxa_proto::Role::User)
        .filter_map(|m| m.content.clone())
        .collect()
}

/// A line-oriented driver of `oxa --mode rpc`: commands are written one at a time and the
/// responses and events are read back.
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
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn oxa --mode rpc (build it first)");
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
        self.rx
            .recv_timeout(Duration::from_secs(30))
            .expect("an rpc record within 30 s")
    }

    /// Send one command and return its response; events read meanwhile are buffered. No retry:
    /// a command sent after the previous response or `agent_settled` must be accepted (rpc.md).
    fn call(&mut self, mut cmd: Value) -> Value {
        self.next += 1;
        let id = format!("g3-{}", self.next);
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

    /// The data of a successful command; panics with the response otherwise.
    fn ok(&mut self, cmd: Value) -> Value {
        let r = self.call(cmd.clone());
        assert_eq!(r["success"], json!(true), "{cmd} failed: {r}");
        r["data"].clone()
    }

    /// Wait for the `agent_settled` event that ends a prompt.
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

    fn prompt(&mut self, text: &str) {
        let r = self.call(json!({"type": "prompt", "message": text}));
        assert_eq!(r["success"], json!(true), "prompt failed: {r}");
        self.settle();
    }

    /// Close stdin (shutdown) and return the exit code.
    fn finish(mut self) -> i32 {
        drop(self.child.stdin.take());
        self.child.wait().expect("wait for oxa").code().unwrap_or(-1)
    }
}

/// The file path of the active session, from `get_state` (which must reflect every command
/// answered before it), with the whole state.
fn state_file(rpc: &mut Rpc) -> (String, Value) {
    let st = rpc.ok(json!({"type": "get_state"}));
    let file = st["sessionFile"]
        .as_str()
        .expect("sessionFile in get_state")
        .to_string();
    (file, st)
}

/// Depth-first search of a `get_tree` node list for the node whose entry has `id`.
fn find_node<'a>(nodes: &'a [Value], id: &str) -> Option<&'a Value> {
    for n in nodes {
        if n["entry"]["id"] == json!(id) {
            return Some(n);
        }
        if let Some(children) = n["children"].as_array() {
            if let Some(hit) = find_node(children, id) {
                return Some(hit);
            }
        }
    }
    None
}

// ---------------------------------------------------------------- rows

/// pi docs: sessions.md "Sessions are stored per working directory" (layout), file safety
/// (0600 files in 0700 directories, O_APPEND, O_NOFOLLOW).
/// invariant: file safety (SES-001 in PARITY.md: 0600 files in 0700 dirs, O_APPEND, O_NOFOLLOW).
#[test]
fn ses_001() {
    let env = TestEnv::new();
    let server = FakeServer::start(
        vec![
            ScriptTurn::text("one"),
            ScriptTurn::text("two"),
            ScriptTurn::text("three"),
        ],
        FakeOpts::default(),
    )
    .expect("fake engine");
    let url = server.url();
    let run = oxa(&env, &url, &["-p", "hello"]);
    assert_eq!(run.status.code(), Some(0), "{}", stderr_of(&run));

    let root = sessions_root(&env);
    let dir = root.join(session_dir_name(&env.work()));
    assert!(dir.is_dir(), "per-cwd directory {} exists", dir.display());
    let files = session_files(&dir);
    assert_eq!(files.len(), 1, "one session file in the cwd directory: {files:?}");
    let name = files[0].file_name().expect("name").to_string_lossy().into_owned();
    assert!(is_session_file_name(&name), "`<ts>_<id>.jsonl` name: {name}");
    assert_eq!(mode_of(&files[0]), 0o600, "session file is 0600");
    assert_eq!(mode_of(&dir), 0o700, "cwd directory is 0700");
    assert_eq!(mode_of(&root), 0o700, "sessions root is 0700");

    // O_APPEND: a later run only adds lines; the bytes written before it are unchanged.
    let first_bytes = std::fs::read(&files[0]).expect("read session");
    let again = oxa(&env, &url, &["-c", "-p", "again"]);
    assert_eq!(again.status.code(), Some(0), "{}", stderr_of(&again));
    let grown = std::fs::read(&files[0]).expect("read session");
    assert!(
        grown.starts_with(&first_bytes),
        "the session file is only appended to"
    );
    assert_eq!(session_files(&dir).len(), 1, "-c appends to the same file");

    // O_NOFOLLOW: a session path that is a symlink is never appended through.
    let elsewhere = env.root().join("elsewhere.jsonl");
    std::fs::rename(&files[0], &elsewhere).expect("move the session aside");
    symlink(&elsewhere, &files[0]).expect("symlink in place of the session");
    let before = std::fs::read(&elsewhere).expect("read target");
    let _ = oxa(&env, &url, &["-c", "-p", "through the link"]);
    let after = std::fs::read(&elsewhere).expect("read target");
    assert_eq!(before, after, "a symlinked session file is not written through");
}

/// pi docs: sessions.md "`--continue` opens the most recent session for the current directory".
/// invariant: W3 (the stored prefix bytes are replayed on continue; no new session is made).
#[test]
fn ses_002() {
    let env = TestEnv::new();
    let server = FakeServer::start(
        vec![
            ScriptTurn::text("first answer"),
            ScriptTurn::text("second answer"),
        ],
        FakeOpts::default(),
    )
    .expect("fake engine");
    let url = server.url();
    let one = oxa(&env, &url, &["-p", "first question"]);
    assert_eq!(one.status.code(), Some(0), "{}", stderr_of(&one));
    let two = oxa(&env, &url, &["-c", "-p", "second question"]);
    assert_eq!(two.status.code(), Some(0), "{}", stderr_of(&two));
    let files = session_files(&sessions_root(&env));
    assert_eq!(
        files.len(),
        1,
        "-c continues the latest session, it does not create one: {files:?}"
    );
    let sent = request_user_texts(&server, 1);
    assert!(
        sent.iter().any(|t| t == "first question"),
        "the continued request carries the earlier turn: {sent:?}"
    );
    // The `-r` picker: search, sort, named filter, rename and delete (pi
    // session-selector.ts; sessions.md "`--resume` opens the session picker").
    let picker = oxa(&env, &url, &["-r", "-p", "x"]);
    assert_ne!(
        picker.status.code(),
        Some(0),
        "-r without a terminal does not open a picker"
    );
    drop(server);
    ses_002_picker();
}

/// SES-002, the interactive part: the `-r` picker through the pty.
fn ses_002_picker() {
    let env = TestEnv::new();
    three_sessions(&env);
    let rig = picker_rig(env, RigOpts::default());
    let s = rig.screen();
    assert!(s.contains("Resume Session (Current Folder)"), "{s:?}");
    assert!(
        s.contains("◉ Current Folder | ○ All") && s.contains("Name: All") && s.contains("Sort: Threaded")
    );
    let rows = picker_rows(&s);
    // Newest first; the named session shows its name; `<count> <age>` on the right.
    assert!(
        rows[0].starts_with("› fruit bowl") && rows[0].trim_end().ends_with("2 now"),
        "{rows:#?}"
    );
    assert!(
        rows[1].contains("banana question") && rows[2].contains("apple question"),
        "{rows:#?}"
    );
    // Search.
    rig.pty.send_text("banana");
    let s = rig
        .pty
        .wait_for_named("search", |s| picker_rows(s).len() == 1, WAIT)
        .unwrap();
    assert!(picker_rows(&s)[0].contains("banana question"), "{s:?}");
    assert!(s.contains("> banana"));
    for _ in 0.."banana".len() {
        rig.pty.send_key("backspace");
    }
    rig.pty
        .wait_for_named("search cleared", |s| picker_rows(s).len() == 3, WAIT)
        .unwrap();
    // Sort modes: Threaded -> Recent -> Fuzzy -> Threaded.
    for label in ["Recent", "Fuzzy", "Threaded"] {
        rig.pty.send_key("ctrl+s");
        rig.pty.wait_for_text(&format!("Sort: {label}")).unwrap();
    }
    // Named filter.
    rig.pty.send_key("ctrl+n");
    let s = rig
        .pty
        .wait_for_named(
            "named only",
            |s| s.contains("Name: Named") && picker_rows(s).len() == 1,
            WAIT,
        )
        .unwrap();
    assert!(picker_rows(&s)[0].contains("fruit bowl"));
    rig.pty.send_key("ctrl+n");
    rig.pty
        .wait_for_named("all names", |s| picker_rows(s).len() == 3, WAIT)
        .unwrap();
    // Rename the apple session.
    select_listed(&rig, "apple question");
    rig.pty.send_key("ctrl+r");
    rig.pty.wait_for_text("Rename Session").unwrap();
    rig.pty.send_text("red apple");
    rig.pty.wait_for_text("> red apple").unwrap();
    rig.pty.send_key("enter");
    let s = rig
        .pty
        .wait_for_named(
            "renamed",
            |s| picker_rows(s).iter().any(|r| r.contains("red apple")),
            WAIT,
        )
        .unwrap();
    assert!(!s.contains("Rename Session"));
    let apple = file_of(&rig.env, "apple question").expect("apple file");
    assert!(
        session_lines(&apple)
            .iter()
            .any(|l| l["type"] == "session_info" && l["name"] == "red apple"),
        "the name is a session_info entry"
    );
    // Delete the banana session (no `trash` on this PATH: unlinked).
    select_listed(&rig, "banana question");
    rig.pty.send_key("ctrl+d");
    rig.pty.wait_for_text("Delete session?").unwrap();
    rig.pty.send_key("enter");
    let s = rig
        .pty
        .wait_for_named("deleted", |s| picker_rows(s).len() == 2, WAIT)
        .unwrap();
    assert!(
        s.contains("Session deleted") || s.contains("Session moved to trash"),
        "{s:?}"
    );
    assert!(file_of(&rig.env, "banana question").is_none(), "the file is gone");
    // Open the named session: its transcript is back and the next prompt continues it.
    select_listed(&rig, "fruit bowl");
    rig.pty.send_key("enter");
    rig.pty.wait_for_text("cherry question").unwrap();
    rig.pty.wait_for_text("ctrl+d exit").unwrap();
    rig.prompt("more cherries", "reply 1");
    let last = rig.generating().last().cloned().unwrap();
    assert_eq!(user_texts(&last), vec!["cherry question", "more cherries"]);
}

/// pi docs: sessions.md "Deleting a session uses `trash` when available" (picker delete).
#[test]
fn ses_003() {
    // pi docs: sessions.md (the picker's delete); pi session-selector.ts `deleteSessionFile`:
    // the `trash` command first ("Session moved to trash"), unlink when it fails ("Session
    // deleted"); the active session cannot be deleted.
    let env = TestEnv::new();
    three_sessions(&env);
    let bin = env.root().join("bin");
    let trash_dir = env.root().join("trash-can");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&trash_dir).unwrap();
    let trash = bin.join("trash");
    std::fs::write(
        &trash,
        format!(
            "#!/bin/sh\necho moving \"$1\"\nmv \"$1\" {}/\n",
            trash_dir.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&trash, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:/usr/bin:/bin", bin.display());
    let rig = picker_rig(
        env,
        RigOpts {
            env: vec![("PATH".to_string(), path)],
            ..RigOpts::default()
        },
    );
    let banana = file_of(&rig.env, "banana question").unwrap();
    select_listed(&rig, "banana question");
    rig.pty.send_key("ctrl+d");
    rig.pty.wait_for_text("Delete session?").unwrap();
    rig.pty.send_key("enter");
    let s = rig.pty.wait_for_text("Session moved to trash").unwrap();
    assert!(
        !s.contains("moving"),
        "trash's output never reaches the screen\n{s:?}"
    );
    assert!(!banana.exists());
    let name = banana.file_name().unwrap();
    assert!(trash_dir.join(name).exists(), "the file is in the trash");
    // A failing trash: unlinked.
    std::fs::write(&trash, "#!/bin/sh\nexit 1\n").unwrap();
    let apple = file_of(&rig.env, "apple question").unwrap();
    select_listed(&rig, "apple question");
    rig.pty.send_key("ctrl+d");
    rig.pty.wait_for_text("Delete session?").unwrap();
    rig.pty.send_key("enter");
    rig.pty.wait_for_text("Session deleted").unwrap();
    assert!(!apple.exists());
    // Open the last one; from /resume its own session cannot be deleted.
    rig.pty.send_key("enter");
    rig.pty.wait_for_text("cherry question").unwrap();
    rig.pty.wait_for_text("ctrl+d exit").unwrap();
    rig.slash("/resume");
    rig.pty.wait_for_text("Resume Session").unwrap();
    rig.pty.send_key("ctrl+d");
    rig.pty
        .wait_for_text("Cannot delete the currently active session")
        .unwrap();
    assert!(file_of(&rig.env, "cherry question").is_some());
}

/// pi docs: sessions.md "Missing cwd" (interactive asks Continue / Fork / Cancel; other modes
/// exit with an error).
#[test]
fn ses_004() {
    let env = TestEnv::new();
    let server = FakeServer::start(
        vec![ScriptTurn::text("one"), ScriptTurn::text("two")],
        FakeOpts::default(),
    )
    .expect("fake engine");
    let url = server.url();
    let proj = env.root().join("proj");
    std::fs::create_dir_all(&proj).expect("proj dir");
    let made = oxa_in(&env, &proj, &url, &[], &["-p", "from proj"]);
    assert_eq!(made.status.code(), Some(0), "{}", stderr_of(&made));
    let file = session_files(&sessions_root(&env))
        .into_iter()
        .next()
        .expect("the session of proj");
    std::fs::remove_dir_all(&proj).expect("remove the session's directory");

    // Print mode: the session's directory is gone, so the run is an error and no request is sent.
    let sent_before = server.requests().len();
    let resumed = oxa(
        &env,
        &url,
        &["-p", "--session", file.to_str().expect("utf-8"), "again"],
    );
    assert_ne!(
        resumed.status.code(),
        Some(0),
        "a missing cwd is an error in print mode"
    );
    assert!(
        stderr_of(&resumed).contains("does not exist"),
        "the error names the missing directory: {}",
        stderr_of(&resumed)
    );
    assert_eq!(server.requests().len(), sent_before, "no request is sent");

    // Fork: a copy in the current directory with a new id and a parentSession header.
    let forked = oxa(
        &env,
        &url,
        &["-p", "--fork", file.to_str().expect("utf-8"), "fork turn"],
    );
    assert_eq!(forked.status.code(), Some(0), "{}", stderr_of(&forked));
    let copies: Vec<PathBuf> = session_files(&sessions_root(&env))
        .into_iter()
        .filter(|p| *p != file)
        .collect();
    assert_eq!(copies.len(), 1, "one new session for the fork: {copies:?}");
    let h = header_of(&copies[0]);
    assert_eq!(
        h["parentSession"],
        json!(file.to_string_lossy()),
        "fork records its parent"
    );
    assert_eq!(
        h["cwd"],
        json!(env.work().to_string_lossy()),
        "the fork runs in the current directory"
    );
}

/// pi docs: sessions.md "/new", "/resume", "/name" and "/session" (slash-commands.md). Headless
/// equivalents are the RPC commands new_session, switch_session, set_session_name, get_session_stats.
#[test]
fn ses_005() {
    let env = TestEnv::new();
    let server =
        FakeServer::start(vec![ScriptTurn::text("first answer")], FakeOpts::default()).expect("fake engine");
    let url = server.url();
    // Built-in TUI commands are not listed by get_commands (pi's rule), so the headless
    // equivalents of /new, /resume, /name and /session are exercised directly.
    let mut rpc = Rpc::start(&env, &url);
    rpc.prompt("first question");
    let (file1, _) = state_file(&mut rpc);
    let id1 = state_file(&mut rpc).1["sessionId"].clone();

    // /name: the session_info entry is written before the response, so the file is the check.
    rpc.ok(json!({"type": "set_session_name", "name": "demo"}));
    let named = records(Path::new(&file1))
        .iter()
        .any(|r| r["type"] == json!("session_info") && r["name"] == json!("demo"));
    assert!(named, "/name appends a session_info entry named demo");

    // /session: the file, id and message counts.
    let stats = rpc.ok(json!({"type": "get_session_stats"}));
    assert_eq!(
        stats["sessionFile"],
        json!(file1),
        "/session names the session file"
    );
    assert_eq!(stats["sessionId"], id1, "/session shows the id");
    assert_eq!(
        stats["userMessages"],
        json!(1),
        "/session counts the user message"
    );

    // /new starts a new session file with a new id.
    rpc.ok(json!({"type": "new_session"}));
    let (file2, st2) = state_file(&mut rpc);
    assert_ne!(file2, file1, "/new starts another file");
    assert_ne!(st2["sessionId"], id1, "/new starts another id");

    // /resume the first session by its path.
    rpc.ok(json!({"type": "switch_session", "sessionPath": file1}));
    let (back, st3) = state_file(&mut rpc);
    assert_eq!(back, file1, "/resume returns to the chosen file");
    assert_eq!(st3["sessionId"], id1, "/resume restores the id");
    let msgs = rpc.ok(json!({"type": "get_messages"}));
    assert!(
        msgs.to_string().contains("first question"),
        "/resume shows the saved messages: {msgs}"
    );
    assert_eq!(rpc.finish(), 0);
}

/// pi docs: sessions.md "`/fork` creates a new session" with a `parentSession` header.
#[test]
fn ses_006() {
    let env = TestEnv::new();
    let server =
        FakeServer::start(vec![ScriptTurn::text("first answer")], FakeOpts::default()).expect("fake engine");
    let url = server.url();
    let mut rpc = Rpc::start(&env, &url);
    rpc.prompt("first question");
    let (file1, _) = state_file(&mut rpc);
    let points = rpc.ok(json!({"type": "get_fork_messages"}));
    let entry = points["messages"][0]["entryId"].clone();
    assert!(
        entry.is_string(),
        "a fork point is offered for the user message: {points}"
    );
    let forked = rpc.ok(json!({"type": "fork", "entryId": entry}));
    assert_eq!(
        forked["text"],
        json!("first question"),
        "the fork reports its user text"
    );
    let (file2, _) = state_file(&mut rpc);
    assert_ne!(file2, file1, "/fork creates a new session file");
    assert_eq!(rpc.finish(), 0);
    let h = header_of(Path::new(&file2));
    assert_eq!(
        h["parentSession"],
        json!(file1),
        "the header names the parent session file"
    );
    assert_ne!(h["id"], json!(""), "the new session has an id");
}

/// pi docs: sessions.md "`/clone` copies the active branch into a new session" (at its position).
#[test]
fn ses_007() {
    let env = TestEnv::new();
    let server = FakeServer::start(
        vec![
            ScriptTurn::text("first answer"),
            ScriptTurn::text("second answer"),
        ],
        FakeOpts::default(),
    )
    .expect("fake engine");
    let url = server.url();
    let mut rpc = Rpc::start(&env, &url);
    rpc.prompt("first question");
    rpc.prompt("second question");
    let (file1, _) = state_file(&mut rpc);
    let original = entry_ids(Path::new(&file1));
    rpc.ok(json!({"type": "clone"}));
    let (file2, _) = state_file(&mut rpc);
    assert_ne!(file2, file1, "/clone creates a new session file");
    assert_eq!(rpc.finish(), 0);
    let copy = Path::new(&file2);
    assert_eq!(
        header_of(copy)["parentSession"],
        json!(file1),
        "the clone names the session it copies"
    );
    assert_eq!(
        entry_ids(copy),
        original,
        "the clone holds the active branch at the same position, with the same entry ids"
    );
}

// ---------------------------------------------------------------------------------------------
// The session picker through the pty (lane tui, wave H)
// ---------------------------------------------------------------------------------------------

/// Three sessions of the work directory (apple, banana, cherry; cherry named `fruit bowl`),
/// written by the binary in print mode and RPC, oldest first.
fn three_sessions(env: &TestEnv) {
    let server = FakeServer::start(
        (1..=3)
            .map(|i| ScriptTurn::text(&format!("answer {i}")))
            .collect(),
        FakeOpts::default(),
    )
    .expect("fake engine");
    for q in ["apple question", "banana question", "cherry question"] {
        let o = oxa(env, &server.url(), &["-p", q]);
        assert_eq!(o.status.code(), Some(0), "{}", stderr_of(&o));
        // Distinct modification times (the picker sorts by them).
        std::thread::sleep(Duration::from_millis(15));
    }
    let o = oxa_parity::run_oxa(
        env,
        &server.url(),
        &["-c", "--mode", "rpc"],
        "{\"id\":\"1\",\"type\":\"set_session_name\",\"name\":\"fruit bowl\"}\n",
    );
    assert!(stdout_of(&o).contains("\"success\":true"), "{}", stdout_of(&o));
}

/// The session file whose first user message is `first`.
fn file_of(env: &TestEnv, first: &str) -> Option<PathBuf> {
    session_files(&sessions_root(env)).into_iter().find(|f| {
        session_lines(f).iter().any(|l| {
            l["type"] == "message" && l["message"]["role"] == "user" && l["message"]["content"] == first
        })
    })
}

/// The picker's list rows (cursor column and text), between the search line and the bottom border.
fn picker_rows(s: &Screen) -> Vec<String> {
    let rows = s.rows();
    let Some(search) = rows.iter().position(|r| r.starts_with("> ")) else {
        return Vec::new();
    };
    rows[search + 2..]
        .iter()
        .take_while(|r| !r.is_empty() && !r.starts_with('─'))
        .cloned()
        .collect()
}

/// `oxa -r` in the work directory with the three sessions; waits for the picker.
fn picker_rig(env: TestEnv, opts: RigOpts) -> Rig {
    let rig = Rig::start_in(
        env,
        rig::replies(3),
        RigOpts {
            args: vec!["-r".to_string()],
            ..opts
        },
    );
    rig.pty
        .wait_for_named("the picker", |s| picker_rows(s).len() == 3, WAIT)
        .unwrap();
    rig
}

/// pi docs: sessions.md "`--no-session`": an ephemeral run keeps nothing on disk.
#[test]
fn ses_009() {
    let env = TestEnv::new();
    let server =
        FakeServer::start(vec![ScriptTurn::text("ephemeral")], FakeOpts::default()).expect("fake engine");
    let run = oxa(&env, &server.url(), &["-p", "--no-session", "hello"]);
    assert_eq!(run.status.code(), Some(0), "{}", stderr_of(&run));
    assert_eq!(stdout_of(&run), "ephemeral\n", "the reply is still printed");
    let left = session_files(env.root());
    assert!(left.is_empty(), "no session file is written: {left:?}");
}

/// pi docs: sessions.md and settings.md "sessionDir": `--session-dir` > `OXA_SESSION_DIR`
/// (`PI_CODING_AGENT_SESSION_DIR` in pi) > the `sessionDir` setting > the default.
#[test]
fn ses_010() {
    let env = TestEnv::new();
    let server = FakeServer::start(
        vec![
            ScriptTurn::text("a"),
            ScriptTurn::text("b"),
            ScriptTurn::text("c"),
            ScriptTurn::text("d"),
        ],
        FakeOpts::default(),
    )
    .expect("fake engine");
    let url = server.url();
    let flag_dir = env.root().join("flag-dir");
    let env_dir = env.root().join("env-dir");
    let flag_s = flag_dir.to_string_lossy().into_owned();
    let env_s = env_dir.to_string_lossy().into_owned();

    // The flag wins over the environment variable.
    let both = oxa_in(
        &env,
        &env.work(),
        &url,
        &[("OXA_SESSION_DIR", env_s.as_str())],
        &["-p", "--session-dir", flag_s.as_str(), "one"],
    );
    assert_eq!(both.status.code(), Some(0), "{}", stderr_of(&both));
    assert_eq!(
        session_files(&flag_dir).len(),
        1,
        "the flag directory holds the session"
    );
    assert!(
        session_files(&env_dir).is_empty(),
        "the environment directory is not used"
    );

    // The environment variable is used when there is no flag.
    let via_env = oxa_in(
        &env,
        &env.work(),
        &url,
        &[("OXA_SESSION_DIR", env_s.as_str())],
        &["-p", "two"],
    );
    assert_eq!(via_env.status.code(), Some(0), "{}", stderr_of(&via_env));
    assert_eq!(
        session_files(&env_dir).len(),
        1,
        "OXA_SESSION_DIR holds the session"
    );

    // The `sessionDir` setting applies when neither the flag nor the variable is given.
    let setting_dir = env.root().join("setting-dir");
    let setting_s = setting_dir.to_string_lossy().into_owned();
    env.write(
        "home/.oxa/agent/settings.json",
        &json!({"sessionDir": setting_s}).to_string(),
    );
    let via_setting = oxa_in(&env, &env.work(), &url, &[], &["-p", "three"]);
    assert_eq!(via_setting.status.code(), Some(0), "{}", stderr_of(&via_setting));
    assert_eq!(
        session_files(&setting_dir).len(),
        1,
        "the `sessionDir` setting holds the session when neither the flag nor the variable is given"
    );
    // The variable still wins over the setting.
    let env_over_setting = oxa_in(
        &env,
        &env.work(),
        &url,
        &[("OXA_SESSION_DIR", env_s.as_str())],
        &["-p", "four"],
    );
    assert_eq!(
        env_over_setting.status.code(),
        Some(0),
        "{}",
        stderr_of(&env_over_setting)
    );
    assert_eq!(
        session_files(&env_dir).len(),
        2,
        "OXA_SESSION_DIR wins over sessionDir"
    );
    assert_eq!(session_files(&setting_dir).len(), 1);
}

/// pi docs: sessions.md "Labels on entries" (`/tree`, Shift+L). Headless: a label is read and
/// shown by the tree; the write is interactive-only.
#[test]
fn ses_011() {
    // pi docs: sessions.md (`/tree` Shift+L labels an entry; session-format.md LabelEntry);
    // get_tree shows it (rpc.md).
    let env = TestEnv::new();
    let rig = Rig::start_in(env, rig::replies(2), RigOpts::default());
    rig.pty.wait_for_text("ctrl+d exit").unwrap();
    rig.prompt("first question", "reply 1");
    open_tree(&rig);
    select_row(&rig, "user: first question", "up");
    rig.pty.send_key("shift+l");
    rig.pty.wait_for_text("Label (empty to remove)").unwrap();
    rig.pty.send_text("checkpoint-1");
    rig.pty.send_key("enter");
    rig.pty
        .wait_for_named("labelled", |s| selected(s).contains("[checkpoint-1]"), WAIT)
        .unwrap();
    rig.pty.send_key("escape");
    rig.pty.wait_for_text_gone("Session Tree").unwrap();
    let file = session_files(&sessions_root(&rig.env))
        .pop()
        .expect("the session file");
    let lines = session_lines(&file);
    let target = lines
        .iter()
        .find(|l| l["type"] == "message" && l["message"]["role"] == "user")
        .and_then(|l| l["id"].as_str())
        .unwrap()
        .to_string();
    let label = lines
        .iter()
        .find(|l| l["type"] == "label")
        .cloned()
        .expect("a label entry");
    assert_eq!(label["targetId"], json!(target));
    assert_eq!(label["label"], json!("checkpoint-1"));
    let url = rig.server.url();
    let path = file.display().to_string();
    let tree_label = |rig: &Rig| {
        let mut reader = Rpc::start(&rig.env, &url);
        reader.ok(json!({"type": "switch_session", "sessionPath": path}));
        let tree = reader.ok(json!({"type": "get_tree"}));
        let node = find_node(tree["tree"].as_array().expect("tree"), &target)
            .expect("the entry is in the tree")
            .clone();
        assert_eq!(reader.finish(), 0);
        node.get("label").cloned().unwrap_or(Value::Null)
    };
    assert_eq!(
        tree_label(&rig),
        json!("checkpoint-1"),
        "get_tree shows the label"
    );
    // Clearing: an empty label removes it (a label entry without `label`).
    open_tree(&rig);
    select_row(&rig, "user: first question", "up");
    rig.pty.send_key("shift+l");
    rig.pty.wait_for_text("Label (empty to remove)").unwrap();
    for _ in 0.."checkpoint-1".len() {
        rig.pty.send_key("backspace");
    }
    rig.pty.send_key("enter");
    rig.pty
        .wait_for_named(
            "unlabelled",
            |s| selected(s).contains("user: first question") && !selected(s).contains('['),
            WAIT,
        )
        .unwrap();
    rig.pty.send_key("escape");
    rig.pty.wait_for_text_gone("Session Tree").unwrap();
    assert_eq!(tree_label(&rig), Value::Null, "cleared");
    let labels = session_lines(&file)
        .iter()
        .filter(|l| l["type"] == "label")
        .count();
    assert_eq!(labels, 2, "set, then cleared");
}

/// pi docs: sessions.md "Automatic compaction" keeps long sessions usable. invariant: W5 (the
/// compaction is an append; earlier entries are not rewritten, and the next request fits).
#[test]
fn ses_012() {
    let env = TestEnv::new();
    // The system prompt alone is about 7k tokens, so the window is 30k with a reserve of 29k:
    // the free space (1k) is below the threshold once the first turn is in the context.
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"compaction":{"enabled":true,"reserveTokens":29000,"keepRecentTokens":200}}"#,
    );
    let big = "x".repeat(1500);
    let server = FakeServer::start(
        vec![
            ScriptTurn::text(&big),
            ScriptTurn::text("summary of the first turn"),
            ScriptTurn::text("answer after compaction"),
        ],
        FakeOpts {
            n_ctx: 30_000,
            ..FakeOpts::default()
        },
    )
    .expect("fake engine");
    let url = server.url();
    let mut rpc = Rpc::start(&env, &url);
    rpc.prompt("a long first question");
    let (file, _) = state_file(&mut rpc);
    let before = std::fs::read(&file).expect("read session before compaction");
    rpc.prompt("a second question");
    assert_eq!(rpc.finish(), 0);
    let after = std::fs::read(&file).expect("read session after compaction");
    assert!(
        after.starts_with(&before),
        "compaction appends: the earlier lines are unchanged"
    );
    let kinds: Vec<String> = records(Path::new(&file))
        .iter()
        .filter_map(|r| r["type"].as_str().map(str::to_string))
        .collect();
    assert!(
        kinds.iter().any(|k| k == "compaction"),
        "a compaction entry was appended: {kinds:?}"
    );
    let overflowed = records(Path::new(&file))
        .iter()
        .any(|r| r["type"] == "shown" && r["text"].as_str().is_some_and(|t| t.contains("context is full")));
    assert!(!overflowed, "no turn was cut by a full context");
}

/// Standard base64 decoding (no dependency): the bytes of the valid base64 characters in `text`.
fn base64_decode(text: &str) -> String {
    let val = |c: u8| -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some(u32::from(c - b'A')),
            b'a'..=b'z' => Some(u32::from(c - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(c - b'0') + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    };
    let mut out = Vec::new();
    let mut acc = 0u32;
    let mut bits = 0;
    for c in text.bytes() {
        let Some(v) = val(c) else { continue };
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The decoded longest base64 run of an exported page (the embedded session data).
fn base64_payload(html: &str) -> String {
    let longest = html
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '='))
        .max_by_key(|t| t.len())
        .unwrap_or_default();
    base64_decode(longest)
}

/// pi docs: sessions.md "Export or share a session": `/export` and `--export` write a
/// self-contained HTML file.
#[test]
fn ses_013() {
    let env = TestEnv::new();
    let server =
        FakeServer::start(vec![ScriptTurn::text("Export me.")], FakeOpts::default()).expect("fake engine");
    let url = server.url();
    let mut rpc = Rpc::start(&env, &url);
    rpc.prompt("export this question");
    let out_path = env.work().join("rpc-export.html");
    let exported = rpc.ok(json!({"type": "export_html", "outputPath": out_path.to_string_lossy()}));
    assert!(exported.is_object(), "export_html responds: {exported}");
    assert_eq!(rpc.finish(), 0);
    let html = std::fs::read_to_string(&out_path).expect("the /export file");
    assert!(
        html.to_ascii_lowercase().starts_with("<!doctype html>"),
        "an HTML document"
    );
    let payload = base64_payload(&html);
    assert!(
        payload.contains("export this question"),
        "the transcript is in the page (base64 session data, as pi embeds it)"
    );
    // Inlined assets only: no external script, stylesheet, image or CSS import is referenced.
    for external in [
        "src=\"http",
        "href=\"http",
        "url(http",
        "@import url(http",
        "@import \"http",
    ] {
        assert!(!html.contains(external), "self-contained: no `{external}`");
    }

    // The `--export` flag on the session file.
    let file = session_files(&sessions_root(&env))
        .into_iter()
        .next()
        .expect("session file");
    let cli_out = oxa(
        &env,
        &url,
        &["--export", file.to_str().expect("utf-8"), "cli-export.html"],
    );
    assert_eq!(cli_out.status.code(), Some(0), "{}", stderr_of(&cli_out));
    let html2 = std::fs::read_to_string(env.work().join("cli-export.html")).expect("--export file");
    assert!(
        base64_payload(&html2).contains("export this question"),
        "--export carries the transcript"
    );
}

/// pi docs: sessions.md "Report a bug": `/bug` prepares a report. oxa writes a local redacted
/// bundle. invariant: oxa-only, nothing is uploaded.
#[test]
fn ses_015() {
    let env = TestEnv::new();
    let server = FakeServer::start(Vec::new(), FakeOpts::default()).expect("fake engine");
    let run = oxa(&env, &server.url(), &["-p", "/bug the thing broke"]);
    let mut all = Vec::new();
    all_files(env.root(), &mut all);
    let found = all.iter().any(|p| {
        std::fs::read_to_string(p)
            .map(|t| t.contains("nothing was uploaded"))
            .unwrap_or(false)
    });
    if !found {
        red(
            "SES-015",
            &format!(
                "headless `/bug` writes no local report (exit {:?}): print mode answers with `invalid` \
                 (\"slash commands and ! lines go through run_command or user_bash\"); the bundle \
                 (oxa-interactive driver bug_report) exists only in the interactive driver",
                run.status.code()
            ),
        );
    }
}

/// pi docs: session-format.md. invariant: file safety (a torn last line is dropped on load,
/// never repaired in place; the next append starts a new line).
#[test]
fn ses_016() {
    let env = TestEnv::new();
    let server = FakeServer::start(
        vec![
            ScriptTurn::text("first answer"),
            ScriptTurn::text("after the torn line"),
        ],
        FakeOpts::default(),
    )
    .expect("fake engine");
    let url = server.url();
    let one = oxa(&env, &url, &["-p", "first question"]);
    assert_eq!(one.status.code(), Some(0), "{}", stderr_of(&one));
    let file = session_files(&sessions_root(&env))
        .into_iter()
        .next()
        .expect("session file");
    let intact = std::fs::read(&file).expect("read");
    // A crash mid-write: a fragment with no trailing newline.
    let torn = b"{\"type\":\"message\",\"id\":\"torn0001\",\"parentId\":\"";
    let mut bytes = intact.clone();
    bytes.extend_from_slice(torn);
    std::fs::write(&file, &bytes).expect("write torn line");

    let two = oxa(&env, &url, &["-c", "-p", "second question"]);
    assert_eq!(two.status.code(), Some(0), "{}", stderr_of(&two));
    let sent = request_user_texts(&server, 1);
    assert!(
        sent.iter().any(|t| t == "first question"),
        "the intact history is loaded: {sent:?}"
    );
    let after = std::fs::read(&file).expect("read");
    assert!(
        after.starts_with(&bytes),
        "the torn fragment is never repaired in place"
    );
    let text = String::from_utf8(after).expect("utf-8");
    let good_lines = text
        .lines()
        .filter(|l| serde_json::from_str::<Value>(l).is_ok())
        .count();
    assert!(good_lines >= 4, "the new entries are appended as whole lines");
    assert!(
        text.lines().filter(|l| l.contains("second question")).count() >= 1,
        "the new turn is in the file"
    );
    assert!(
        text.contains(&format!("{}\n", String::from_utf8_lossy(torn))),
        "the fragment is followed by a newline, so the new entry is not glued to it"
    );
}

// ---------------------------------------------------------------- meta checks

/// The PARITY.md file of the workspace root.
fn parity_md() -> String {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("PARITY.md");
    std::fs::read_to_string(&root).unwrap_or_else(|e| panic!("read {}: {e}", root.display()))
}

/// The SES rows of PARITY.md: (id, verdict, test column).
fn ses_rows() -> Vec<(String, String, String)> {
    let mut rows = Vec::new();
    for line in parity_md().lines() {
        let cells: Vec<&str> = line.split('|').map(str::trim).collect();
        if cells.len() < 6 || !cells[1].starts_with("SES-") {
            continue;
        }
        rows.push((cells[1].to_string(), cells[3].to_string(), cells[5].to_string()));
    }
    rows
}

/// The source of this file.
const SELF: &str = include_str!("sessions.rs");

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
    let rows = ses_rows();
    assert_eq!(rows.len(), 14, "the SES area has 14 rows: {rows:?}");
    for (id, verdict, test) in &rows {
        if test == "omission" {
            assert_eq!(verdict, "O", "{id}: only O rows may be omitted");
            continue;
        }
        let fn_name = id.to_ascii_lowercase().replace('-', "_");
        assert!(
            test.contains(&format!("sessions.rs::{fn_name}")),
            "{id} maps to sessions.rs::{fn_name} in PARITY.md (test column `{test}`)"
        );
        assert!(
            SELF.contains(&format!("fn {fn_name}()")),
            "{id}: test fn {fn_name} exists in sessions.rs"
        );
        assert!(
            matches!(verdict.as_str(), "P" | "A" | "O"),
            "{id} has a verdict P, A or O: {verdict}"
        );
    }
}

/// documented output or recorded fixture
#[test]
fn documented_rows_have_tests() {
    for (id, verdict, _) in ses_rows() {
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
    for (id, verdict, _) in ses_rows() {
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
