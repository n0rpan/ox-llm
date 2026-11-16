//! P6 (privacy): `oxa wipe --session <id>` and `oxa wipe --all`. Three sessions with their spill
//! files sit in a temporary home next to settings, trust, models, MCP and identity files. Each
//! test snapshots the whole temporary tree before and after the command and checks what changed.
//! Offline and synthetic only: no engine is reached (the engine tests use refusals).

use oxa_cli::{run, Stdio};
use oxa_config::Env;
use oxa_fake::{parse_script, FakeOpts, FakeServer, TestEnv};
use std::collections::BTreeMap;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// The process network policy and the environment are per process; these tests run one at a time.
static LOCK: Mutex<()> = Mutex::new(());

const A: &str = "d0c1-first";
const B: &str = "d0c2-second";
const C: &str = "77aa-third";
/// Two of the three ids start with `d0c`: that prefix is ambiguous.
const AMBIGUOUS: &str = "d0c";

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct Outcome {
    code: i32,
    out: String,
    err: String,
}

/// Run `oxa <words>` in-process. `stdin_text` is the answer piped to stdin; `tty` says whether
/// stdin counts as a terminal (the confirmation then reads it).
fn wipe(env: &TestEnv, extra: &[(&str, &str)], tty: bool, stdin_text: &str, words: &[&str]) -> Outcome {
    wipe_with_engine(env, "http://127.0.0.1:9", extra, tty, stdin_text, words)
}

fn wipe_with_engine(
    env: &TestEnv,
    engine_url: &str,
    extra: &[(&str, &str)],
    tty: bool,
    stdin_text: &str,
    words: &[&str],
) -> Outcome {
    let mut owned = env.envs(engine_url);
    owned.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    let pairs: Vec<(&str, &str)> = owned.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let (out, err) = (Sink::default(), Sink::default());
    let io = Stdio {
        stdin: Box::new(Cursor::new(stdin_text.as_bytes().to_vec())),
        stdout: Box::new(out.clone()),
        stderr: Box::new(err.clone()),
        stdin_is_tty: tty,
        stdout_is_tty: false,
        cwd: env.work(),
    };
    let mut argv = vec!["wipe".to_string()];
    argv.extend(words.iter().map(|s| s.to_string()));
    let code = run(&argv, &Env::from_pairs(&pairs), io);
    let text = |s: &Sink| String::from_utf8(s.0.lock().unwrap().clone()).unwrap();
    Outcome {
        code,
        out: text(&out),
        err: text(&err),
    }
}

/// One session file (header line and one message) in `dir/<cwd dir>/`.
fn write_session(env: &TestEnv, rel_dir: &str, id: &str) -> PathBuf {
    let header = format!(
        r#"{{"type":"session","version":3,"id":"{id}","timestamp":"2026-11-01T10:00:00.000Z","cwd":"/work/proj"}}"#
    );
    let message = format!(
        r#"{{"type":"message","id":"m{id}","parentId":null,"timestamp":"2026-11-01T10:00:01.000Z","message":{{"role":"user","content":"private words of {id}"}}}}"#
    );
    env.write(
        &format!("{rel_dir}/--work-proj--/2026-11-01T10-00-00-000Z_{id}.jsonl"),
        &format!("{header}\n{message}\n"),
    )
}

/// The three sessions, each with two spill files under `tmp/<id>/`, plus the settings and
/// identity files that must survive every wipe. Returns the sessions directory.
fn fixture() -> TestEnv {
    let env = TestEnv::new();
    let sessions = "home/.oxa/agent/sessions";
    for id in [A, B, C] {
        write_session(&env, sessions, id);
        oxa_session::send_inbox(&env.agent_dir(), id, &format!("pending private words of {id}")).unwrap();
        env.write(
            &format!("home/.oxa/agent/tmp/{id}/bash-1.log"),
            &format!("output of {id}"),
        );
        env.write(
            &format!("home/.oxa/agent/tmp/{id}/subagent-abc.txt"),
            &format!("child result of {id}"),
        );
    }
    // Scratch that belongs to no session (codemode writes here); only `--all` removes it.
    env.write("home/.oxa/agent/tmp/codemode/codemode-1.txt", "shared scratch");
    env.write("home/.oxa/agent/settings.json", r#"{"oxa":{"offline":false}}"#);
    env.write("home/.oxa/agent/trust.json", r#"{"trusted":["/work/proj"]}"#);
    env.write("home/.oxa/agent/models.json", r#"{"endpoints":[]}"#);
    env.write("home/.oxa/agent/mcp.json", r#"{"mcpServers":{}}"#);
    env.write("home/.oxa/agent/mcp-auth.json", r#"{"token":"keep-me"}"#);
    env.write("home/.oxa/agent/identity.md", "identity text");
    env
}

/// Every file under `root` with its bytes, keyed by path.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(m) = std::fs::symlink_metadata(&p) else {
                continue;
            };
            if m.is_dir() {
                stack.push(p);
            } else if m.is_file() {
                out.insert(p.clone(), std::fs::read(&p).unwrap_or_default());
            }
        }
    }
    out
}

/// Assert that no path or file content under `root` mentions `id`.
fn assert_no_trace(root: &Path, id: &str) {
    for (path, bytes) in snapshot(root) {
        let name = path.to_string_lossy();
        assert!(!name.contains(id), "a path still names {id}: {name}");
        let text = String::from_utf8_lossy(&bytes);
        assert!(!text.contains(id), "{name} still holds {id}");
    }
}

fn keys_gone(before: &BTreeMap<PathBuf, Vec<u8>>, after: &BTreeMap<PathBuf, Vec<u8>>) -> Vec<PathBuf> {
    before
        .keys()
        .filter(|k| !after.contains_key(*k))
        .cloned()
        .collect()
}

/// Every file that is still there is byte-for-byte what it was, and nothing was added.
fn assert_survivors_intact(before: &BTreeMap<PathBuf, Vec<u8>>, after: &BTreeMap<PathBuf, Vec<u8>>) {
    for (path, bytes) in after {
        assert_eq!(
            before.get(path),
            Some(bytes),
            "changed or new: {}",
            path.display()
        );
    }
}

#[test]
fn session_removes_exactly_one_sessions_files() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let env = fixture();
    let before = snapshot(env.root());
    let r = wipe(&env, &[], false, "", &["--session", A]);
    assert_eq!(r.code, 0, "stderr: {}", r.err);
    let after = snapshot(env.root());
    assert_survivors_intact(&before, &after);
    let gone = keys_gone(&before, &after);
    assert!(!gone.is_empty());
    for p in &gone {
        let s = p.to_string_lossy();
        assert!(s.contains(A), "removed something that is not session {A}: {s}");
    }
    assert_no_trace(env.root(), A);
    // The other two sessions and their spill files are intact.
    for id in [B, C] {
        assert!(
            after.keys().any(|k| k.to_string_lossy().contains(id)),
            "session {id} is gone"
        );
    }
    assert!(r.out.contains("removed session file"), "{}", r.out);
    assert!(r.out.contains("removed spill directory"), "{}", r.out);
    assert!(r.out.contains("(2 files)"), "{}", r.out);
}

#[test]
fn session_removes_the_spill_directory_and_names_it() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let env = fixture();
    let spill = env.agent_dir().join("tmp").join(A);
    assert!(spill.is_dir());
    let r = wipe(&env, &[], false, "", &["--session", A]);
    assert_eq!(r.code, 0, "stderr: {}", r.err);
    assert!(!spill.exists(), "spill directory survived");
    assert!(r.out.contains(&spill.display().to_string()), "{}", r.out);
    // The codemode scratch is not this session's and stays.
    assert!(env.agent_dir().join("tmp/codemode/codemode-1.txt").is_file());
}

#[test]
fn session_accepts_a_unique_prefix() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let env = fixture();
    let r = wipe(&env, &[], false, "", &["--session", "77a"]);
    assert_eq!(r.code, 0, "stderr: {}", r.err);
    assert_no_trace(env.root(), C);
    let after = snapshot(env.root());
    assert!(after.keys().any(|k| k.to_string_lossy().contains(A)));
}

#[test]
fn ambiguous_prefix_is_an_error_that_lists_the_matches_and_removes_nothing() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let env = fixture();
    let before = snapshot(env.root());
    let r = wipe(&env, &[], false, "", &["--session", AMBIGUOUS]);
    assert_eq!(r.code, 1, "stdout: {} stderr: {}", r.out, r.err);
    assert!(
        r.err.contains("ambiguous") && r.err.contains("2 sessions match"),
        "{}",
        r.err
    );
    assert!(
        r.err.contains(A) && r.err.contains(B),
        "matches not listed: {}",
        r.err
    );
    assert_eq!(snapshot(env.root()), before);
}

#[test]
fn unknown_id_is_exit_1_with_a_message_and_removes_nothing() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let env = fixture();
    let before = snapshot(env.root());
    let r = wipe(&env, &[], false, "", &["--session", "zzzz"]);
    assert_eq!(r.code, 1);
    assert!(r.err.contains("no session matches `zzzz`"), "{}", r.err);
    assert_eq!(snapshot(env.root()), before);
}

#[test]
fn wipe_needs_exactly_one_selector() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let env = fixture();
    let before = snapshot(env.root());
    assert_eq!(wipe(&env, &[], false, "", &[]).code, 2);
    assert_eq!(wipe(&env, &[], false, "", &["--session", A, "--all"]).code, 2);
    assert_eq!(wipe(&env, &[], false, "", &["--bogus"]).code, 2);
    assert_eq!(snapshot(env.root()), before);
}

#[test]
fn all_without_yes_from_a_pipe_is_refused() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let env = fixture();
    let before = snapshot(env.root());
    // Even a piped `yes` does not confirm: only a terminal is asked.
    let r = wipe(&env, &[], false, "yes\n", &["--all"]);
    assert_eq!(r.code, 2, "stdout: {} stderr: {}", r.out, r.err);
    assert!(r.err.contains("--yes"), "{}", r.err);
    assert_eq!(snapshot(env.root()), before);
}

#[test]
fn all_in_a_terminal_needs_the_word_yes() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let env = fixture();
    let before = snapshot(env.root());
    let r = wipe(&env, &[], true, "no\n", &["--all"]);
    assert_eq!(r.code, 1);
    assert!(r.err.contains("Cancelled. Nothing was removed."), "{}", r.err);
    assert_eq!(snapshot(env.root()), before);
}

#[test]
fn all_with_yes_removes_every_session_and_spill_and_keeps_settings() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let env = fixture();
    let before = snapshot(env.root());
    let keep = [
        "home/.oxa/agent/settings.json",
        "home/.oxa/agent/trust.json",
        "home/.oxa/agent/models.json",
        "home/.oxa/agent/mcp.json",
        "home/.oxa/agent/mcp-auth.json",
        "home/.oxa/agent/identity.md",
    ];
    let r = wipe(&env, &[], false, "", &["--all", "--yes"]);
    assert_eq!(r.code, 0, "stderr: {}", r.err);
    let after = snapshot(env.root());
    assert_survivors_intact(&before, &after);
    for id in [A, B, C] {
        assert_no_trace(env.root(), id);
    }
    assert!(
        !env.agent_dir().join("tmp").exists(),
        "spill directory survived --all"
    );
    for rel in keep {
        let p = env.root().join(rel);
        assert_eq!(after.get(&p), before.get(&p), "{rel} changed");
        assert!(p.is_file(), "{rel} is gone");
    }
    assert!(r.out.contains("removed 3 session file(s)"), "{}", r.out);
    assert!(r.out.contains("removed spill directory"), "{}", r.out);
}

#[test]
fn all_in_a_terminal_removes_after_the_word_yes() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let env = fixture();
    let r = wipe(&env, &[], true, "yes\n", &["--all"]);
    assert_eq!(r.code, 0, "stderr: {}", r.err);
    for id in [A, B, C] {
        assert_no_trace(env.root(), id);
    }
    assert!(env.agent_dir().join("settings.json").is_file());
    assert!(!env.agent_dir().join("tmp").exists());
}

#[test]
fn session_dir_flag_points_the_wipe_at_another_directory() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let env = fixture();
    let custom = env.root().join("work/custom");
    write_session(&env, "work/custom", "5e55-elsewhere");
    let before = snapshot(env.root());
    let r = wipe(
        &env,
        &[],
        false,
        "",
        &[
            "--session",
            "5e55-elsewhere",
            "--session-dir",
            &custom.to_string_lossy(),
        ],
    );
    assert_eq!(r.code, 0, "stderr: {}", r.err);
    assert_no_trace(env.root(), "5e55-elsewhere");
    assert_eq!(snapshot(env.root()).len() + 1, before.len());
}

#[test]
fn engine_flag_offline_still_wipes_locally_and_says_the_engine_kept_its_state() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let env = fixture();
    let r = wipe(
        &env,
        &[("OXA_OFFLINE", "1")],
        false,
        "",
        &["--session", A, "--engine"],
    );
    assert_eq!(r.code, 1, "stdout: {} stderr: {}", r.out, r.err);
    assert!(
        r.err
            .contains("engine-side state not cleared: offline mode is on"),
        "{}",
        r.err
    );
    assert_no_trace(env.root(), A);
}

#[test]
fn engine_flag_refuses_a_remote_engine_before_connecting() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let env = fixture();
    let r = wipe(
        &env,
        &[("OXA_ENGINE_URL", "http://10.255.0.2:8092")],
        false,
        "",
        &["--session", B, "--engine"],
    );
    assert_eq!(r.code, 1, "stdout: {} stderr: {}", r.out, r.err);
    assert!(r.err.contains("engine-side state not cleared"), "{}", r.err);
    assert!(r.err.contains("10.255.0.2"), "{}", r.err);
    assert_no_trace(env.root(), B);
}

#[test]
fn engine_flag_clears_the_session_hint_on_the_engine_and_wipes_locally() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let env = fixture();
    let server =
        FakeServer::start(parse_script("").expect("script"), FakeOpts::default()).expect("start fake engine");
    let r = wipe_with_engine(&env, &server.url(), &[], false, "", &["--session", A, "--engine"]);
    assert_eq!(r.code, 0, "stdout: {} stderr: {}", r.out, r.err);
    assert_eq!(
        server.clears(),
        vec![A.to_string()],
        "the engine was asked for the session hint"
    );
    assert!(
        r.out
            .contains(&format!("engine: cleared the state of session {A}")),
        "{}",
        r.out
    );
    assert_no_trace(env.root(), A);
}

#[test]
fn engine_flag_reports_a_busy_engine_and_still_removed_the_files() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let env = fixture();
    let server =
        FakeServer::start(parse_script("").expect("script"), FakeOpts::default()).expect("start fake engine");
    server.set_clear_busy(true);
    let r = wipe_with_engine(&env, &server.url(), &[], false, "", &["--session", B, "--engine"]);
    assert_eq!(r.code, 1, "stdout: {} stderr: {}", r.out, r.err);
    assert!(
        r.err.contains(&format!("state of session {B} not cleared")),
        "{}",
        r.err
    );
    assert_no_trace(env.root(), B);
}
