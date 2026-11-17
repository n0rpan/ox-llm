//! Offline mode does not run the `done` list: a project's checks may need the network (a module
//! test that fetches its dependencies cannot pass offline), so the failure is not the model's to
//! fix and must not cost a repair round or fail an otherwise completed run (issue #1054).
//! Through the real program path (`oxa -p`); the network policy is process-global, so the tests
//! in this file hold one lock each. Isolation: `oxa_fake::TestEnv` for every file and directory.

use oxa_cli::{run, Stdio};
use oxa_config::Env;
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
use std::io::{Cursor, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

static POLICY: Mutex<()> = Mutex::new(());

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

const NOTICE: &str = "done checks were not run: offline mode is on";
const FAILED: &str = "done checks failed";

/// Run `oxa <args>` in-process against a fake engine with `turns`; returns (code, stdout, stderr).
fn go(env: &TestEnv, turns: Vec<ScriptTurn>, args: &[&str]) -> (i32, String, String) {
    let server = FakeServer::start(turns, FakeOpts::default()).expect("fake engine");
    let owned = env.envs(&server.url());
    let cwd = env.work();
    let argv: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let pairs: Vec<(&str, &str)> = owned.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let env = Env::from_pairs(&pairs);
        let (out, err) = (Sink::default(), Sink::default());
        let io = Stdio {
            stdin: Box::new(Cursor::new(Vec::new())),
            stdout: Box::new(out.clone()),
            stderr: Box::new(err.clone()),
            stdin_is_tty: false,
            stdout_is_tty: false,
            cwd,
        };
        let code = run(&argv, &env, io);
        let text = |s: &Sink| String::from_utf8_lossy(&s.0.lock().unwrap()).into_owned();
        let _ = tx.send((code, text(&out), text(&err)));
    });
    rx.recv_timeout(Duration::from_secs(120)).expect("oxa finished")
}

fn trust_always(env: &TestEnv) {
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"defaultProjectTrust": "always"}"#,
    );
}

fn project_done(env: &TestEnv, list: &str) {
    env.write("work/.oxa/settings.json", &format!(r#"{{"done": {list}}}"#));
}

/// `rounds` turns of one `write` (the edit that makes the done checks due) and a final text.
fn edits_then_stops(rounds: usize) -> Vec<ScriptTurn> {
    let mut turns = Vec::new();
    for i in 0..rounds {
        turns.push(ScriptTurn::tool(
            &format!("c{i}"),
            "write",
            &serde_json::json!({"path": format!("f{i}.txt"), "content": "x"}).to_string(),
        ));
        turns.push(ScriptTurn::text("finished"));
    }
    turns
}

#[test]
fn offline_does_not_run_the_projects_done_checks() {
    let _g = POLICY.lock().unwrap_or_else(|p| p.into_inner());
    let env = TestEnv::new();
    trust_always(&env);
    project_done(&env, r#"["exit 1"]"#);
    let (code, out, err) = go(&env, edits_then_stops(1), &["--offline", "-p", "go"]);
    assert_eq!(code, 0, "checks did not run, so nothing failed: {err}");
    assert_eq!(err.matches(NOTICE).count(), 1, "one notice: {err}");
    assert!(!err.contains("done check:") && !err.contains(FAILED), "{err}");
    assert!(out.contains("finished"), "the reply is still printed: {out}");
}

#[test]
fn online_still_runs_the_same_list() {
    let _g = POLICY.lock().unwrap_or_else(|p| p.into_inner());
    let env = TestEnv::new();
    trust_always(&env);
    project_done(&env, r#"["exit 1"]"#);
    let (code, _, err) = go(&env, edits_then_stops(3), &["-p", "go"]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains(FAILED) && err.contains("FAILED (exit 1)"), "{err}");
    assert!(!err.contains(NOTICE), "offline notice online: {err}");
}

#[test]
fn offline_without_a_done_list_prints_no_notice() {
    let _g = POLICY.lock().unwrap_or_else(|p| p.into_inner());
    let env = TestEnv::new();
    trust_always(&env);
    let (code, _, err) = go(&env, vec![ScriptTurn::text("hi")], &["--offline", "-p", "go"]);
    assert_eq!(code, 0, "{err}");
    assert!(!err.contains(NOTICE), "{err}");
}
