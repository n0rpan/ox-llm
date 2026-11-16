//! Worker status is a bounded snapshot, usable without reading the event stream.
use oxa_fake::{FakeOpts, FakeServer, Fault, ScriptTurn, TestEnv};
use oxa_proto::Finish;
use oxa_session::{FsStore, SessionStore};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn command(env: &TestEnv, url: &str) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_oxa"));
    cmd.env_clear().envs(env.envs(url)).current_dir(env.work());
    cmd
}

fn session(env: &TestEnv) -> oxa_session::SessionRef {
    FsStore::new(env.agent_dir().join("sessions"))
        .list(None)
        .into_iter()
        .next()
        .expect("a saved session")
        .sref
}

fn status(env: &TestEnv, id: &str) -> Value {
    // A dead loopback port proves this subcommand never needs engine discovery.
    let output = command(env, "http://127.0.0.1:1")
        .args(["status", id])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn saved_runs_have_bounded_status_and_terminal_outcomes() {
    for mode in ["print", "json", "rpc"] {
        for remote in [false, true] {
            let env = TestEnv::new();
            env.write("home/.oxa/agent/settings.json", r#"{"oxa":{"envNote":false,"bootPreload":false},"cacheWarming":"off"}"#);
            let srv = FakeServer::start(
                vec![
                    ScriptTurn::tool("save", "write", r#"{"path":"result.txt","content":"TOOL_CONTENT_SENTINEL"}"#),
                    ScriptTurn::text("ANSWER_SENTINEL"),
                ],
                FakeOpts { remote: remote.then_some("openrouter"), ..FakeOpts::default() },
            ).unwrap();
            let mut cmd = command(&env, &srv.url());
            if mode == "print" { cmd.arg("-p"); } else { cmd.args(["--mode", mode]); }
            cmd.args(["--no-mcp", "--no-extensions", "--no-skills", "--no-context-files", "--tools", "write"])
                .stdout(Stdio::piped()).stderr(Stdio::piped());
            if mode == "rpc" { cmd.stdin(Stdio::piped()); } else { cmd.arg("PROMPT_SENTINEL").stdin(Stdio::null()); }
            let mut child = cmd.spawn().unwrap();
            if let Some(mut stdin) = child.stdin.take() {
                writeln!(stdin, "{}", json!({"type":"prompt", "message":"PROMPT_SENTINEL"})).unwrap();
            }
            let output = child.wait_with_output().unwrap();
            assert_eq!(output.status.code(), Some(0), "{}", String::from_utf8_lossy(&output.stderr));
            let sref = session(&env);
            let path = sref.path.with_extension("status.json");
            let bytes = std::fs::read(&path).expect("the atomic status sidecar");
            assert!(bytes.len() <= 4096);
            let text = String::from_utf8(bytes).unwrap();
            for secret in ["PROMPT_SENTINEL", "TOOL_CONTENT_SENTINEL", "ANSWER_SENTINEL"] { assert!(!text.contains(secret)); }
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
            let value = status(&env, &sref.id);
            assert_eq!(value["type"], "oxa_status");
            assert_eq!(value["version"], 1);
            assert_eq!(value["session_id"], sref.id);
            assert_eq!(value["phase"], "finished");
            assert_eq!(value["last_tool"], "write");
            assert!(value["step"].as_u64().unwrap() >= 2);
            assert!(value["tokens"]["completion"].as_u64().unwrap() > 0);
            assert_eq!(value["outcome"]["outcome"], "done");
            assert_eq!(value["outcome"]["exit_code"], 0);
            assert_eq!(value["stalled"], false);
            assert!(value["stall_deadline_ms"].is_null());
            assert_eq!(value["cost_usd"], if remote { Value::Null } else { json!(0) });
            if mode == "json" {
                let header: Value = serde_json::from_slice(output.stdout.split(|b| *b == b'\n').next().unwrap()).unwrap();
                assert_eq!(header["statusFile"], path.to_string_lossy().as_ref());
            }
            assert_eq!(srv.requests().len(), 2, "status lookup makes no model request");
        }
    }
}

#[test]
fn stalled_model_is_detected_without_changing_progress() {
    let env = TestEnv::new();
    env.write("home/.oxa/agent/settings.json", r#"{"oxa":{"envNote":false,"bootPreload":false},"cacheWarming":"off"}"#);
    let mut turn = ScriptTurn::text(&"x".repeat(1024));
    turn.chunk = 16;
    turn.fault = Some(Fault::Stall { bytes: 512, ms: 1000 });
    let srv = FakeServer::start(vec![turn], FakeOpts::default()).unwrap();
    let mut child = command(&env, &srv.url())
        .args(["--mode", "rpc", "--no-tools", "--stall-timeout", "0.1"])
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    writeln!(stdin, "{}", json!({"type":"prompt","message":"say hello"})).unwrap();
    stdin.flush().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    loop {
        line.clear();
        assert!(lines.read_line(&mut line).unwrap() > 0);
        if serde_json::from_str::<Value>(&line).unwrap()["type"] == "message_update" { break; }
    }
    let sref = session(&env);
    let path = sref.path.with_extension("status.json");
    let deadline = Instant::now() + Duration::from_millis(700);
    let stalled = loop {
        let value = status(&env, &sref.id);
        if value["stalled"] == true { break value; }
        assert!(Instant::now() < deadline, "stream never signalled its configured stall: {value}");
        std::thread::sleep(Duration::from_millis(10));
    };
    let before = std::fs::read(&path).unwrap();
    assert!(stalled["outcome"].is_null());
    assert_eq!(stalled["phase"], "model");
    assert!(stalled["stall_deadline_ms"].as_u64().unwrap() > stalled["last_progress_ms"].as_u64().unwrap());
    let again = status(&env, &sref.id);
    assert_eq!(again["last_progress_ms"], stalled["last_progress_ms"]);
    assert_eq!(std::fs::read(&path).unwrap(), before, "reading status must not manufacture progress");
    drop(stdin);
    // Drain the protocol while waiting, so pipe back-pressure cannot become a fake stall.
    for line in lines.lines() { serde_json::from_str::<Value>(&line.unwrap()).unwrap(); }
    assert_eq!(child.wait_with_output().unwrap().status.code(), Some(0));
    let done = status(&env, &sref.id);
    assert_eq!(done["outcome"]["outcome"], "done");
    assert_eq!(done["stalled"], false);
}

#[test]
fn status_keeps_an_earlier_prompt_failure() {
    for mode in ["print", "json"] {
        let env = TestEnv::new();
        env.write("home/.oxa/agent/settings.json", r#"{"retry":{"enabled":false},"cacheWarming":"off","oxa":{"envNote":false}}"#);
        let mut cut = ScriptTurn::text("partial");
        cut.finish = Finish::Length;
        let srv = FakeServer::start(vec![cut, ScriptTurn::text("fine")], FakeOpts::default()).unwrap();
        let mut cmd = command(&env, &srv.url());
        if mode == "print" { cmd.arg("-p"); } else { cmd.args(["--mode", mode]); }
        let output = cmd.args(["--no-tools", "one", "two"]).stdin(Stdio::null()).output().unwrap();
        assert_eq!(output.status.code(), Some(1));
        let value = status(&env, &session(&env).id);
        assert_eq!(value["outcome"]["outcome"], "failed");
        assert_eq!(value["outcome"]["error_class"], "output_limit");
    }
}

#[test]
fn ephemeral_runs_create_no_status() {
    let env = TestEnv::new();
    let srv = FakeServer::start(vec![ScriptTurn::text("done")], FakeOpts::default()).unwrap();
    let output = command(&env, &srv.url()).args(["-p", "--no-session", "--no-tools", "hello"]).stdin(Stdio::null()).output().unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert!(FsStore::new(env.agent_dir().join("sessions")).list(None).is_empty());
    assert!(!env.agent_dir().join("sessions").exists());
}
