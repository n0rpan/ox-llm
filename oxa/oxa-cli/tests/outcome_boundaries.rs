//! Review regressions: independent prompt outcomes and argument-error mode boundaries.
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
use serde_json::Value;
use std::io::{BufRead, BufReader, Cursor, Read, Write};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);
impl Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn argument_error(words: &[&str], tty: bool) -> String {
    let env = TestEnv::new();
    let owned = env.envs("http://127.0.0.1:9");
    let pairs: Vec<(&str, &str)> = owned
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    let err = Capture::default();
    let code = oxa_cli::run(
        &words.iter().map(|word| word.to_string()).collect::<Vec<_>>(),
        &oxa_config::Env::from_pairs(&pairs),
        oxa_cli::Stdio {
            stdin: Box::new(Cursor::new(Vec::new())),
            stdout: Box::new(std::io::sink()),
            stderr: Box::new(err.clone()),
            stdin_is_tty: tty,
            stdout_is_tty: tty,
            cwd: env.work(),
        },
    );
    assert_eq!(code, 2);
    let text = String::from_utf8(err.0.lock().unwrap().clone()).unwrap();
    text
}

#[test]
fn non_headless_argument_errors_keep_their_output_contract() {
    for (args, tty) in [
        (vec!["--thinking", "invalid", "json"], true),
        (vec!["--thinking", "invalid", "rpc"], true),
        (vec!["--export"], false),
        (vec!["config", "--bogus"], false),
        (vec!["mcp", "--bogus"], false),
    ] {
        let err = argument_error(&args, tty);
        assert!(!err.contains("oxa_result"), "{args:?}: {err}");
    }
}

#[test]
fn explicit_headless_modes_still_report_argument_errors_as_typed_outcomes() {
    for args in [
        vec!["-p", "--thinking", "invalid"],
        vec!["--mode", "rpc", "--thinking", "invalid"],
        vec!["--mode=json", "--thinking", "invalid"],
    ] {
        let err = argument_error(&args, true);
        let result: Value = err
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|value| value["type"] == "oxa_result")
            .unwrap();
        assert_eq!(result["error_class"], "usage");
    }
}

#[test]
fn a_read_only_prompt_after_failed_done_checks_reports_its_own_success() {
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"defaultProjectTrust":"always","retry":{"enabled":false}}"#,
    );
    env.write("work/.oxa/settings.json", r#"{"done":["exit 1"]}"#);
    let mut turns = Vec::new();
    for index in 0..3 {
        turns.push(ScriptTurn::tool(
            &format!("edit-{index}"),
            "write",
            &serde_json::json!({"path":format!("file-{index}.txt"),"content":"synthetic"}).to_string(),
        ));
        turns.push(ScriptTurn::text("first completed"));
    }
    turns.push(ScriptTurn::text("read-only answer"));
    let server = FakeServer::start(turns, FakeOpts::default()).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_oxa"))
        .current_dir(env.work())
        .env_clear()
        .envs(env.envs(&server.url()))
        .args(["--mode", "rpc"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut results = Vec::new();
    for message in ["write a file", "answer without editing"] {
        writeln!(
            input,
            "{}",
            serde_json::json!({"type":"prompt","message":message})
        )
        .unwrap();
        input.flush().unwrap();
        loop {
            let mut line = String::new();
            assert_ne!(
                output.read_line(&mut line).unwrap(),
                0,
                "RPC exited before settlement"
            );
            if let Ok(record) = serde_json::from_str::<Value>(&line) {
                if record["type"] == "agent_settled" {
                    results.push(record["result"].clone());
                    break;
                }
            }
        }
    }
    drop(input);
    let mut tail = String::new();
    output.read_to_string(&mut tail).unwrap();
    let result = child.wait_with_output().unwrap();
    assert_eq!(
        result.status.code(),
        Some(1),
        "the first failure remains the aggregate outcome"
    );
    assert_eq!(results[0]["error_class"], "done_checks_failed");
    assert_eq!(results[1]["outcome"], "done");
    assert!(results[1]["error_class"].is_null());
    assert_eq!(server.requests().len(), 7);
}

#[test]
fn successful_builtins_are_done_in_every_headless_mode() {
    for mode in ["print", "json", "rpc"] {
        let env = TestEnv::new();
        let server = FakeServer::start(vec![], FakeOpts::default()).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_oxa"));
        command
            .current_dir(env.work())
            .env_clear()
            .envs(env.envs(&server.url()));
        match mode {
            "print" => {
                command.args(["-p", "/session"]);
            }
            "json" => {
                command.args(["--mode", "json", "/session"]);
            }
            _ => {
                command.args(["--mode", "rpc"]);
            }
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        if mode == "rpc" {
            child
                .stdin
                .as_mut()
                .unwrap()
                .write_all(b"{\"type\":\"prompt\",\"message\":\"/session\"}\n")
                .unwrap();
        }
        drop(child.stdin.take());
        let output = child.wait_with_output().unwrap();
        let err = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(0), "{mode}: {err}");
        let results: Vec<Value> = err
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|value| value["type"] == "oxa_result")
            .collect();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["outcome"], "done");
        assert!(server.requests().is_empty());
        if mode == "rpc" {
            let records: Vec<Value> = String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect();
            assert!(records.iter().any(|record| record["type"] == "response"
                && record["success"] == true
                && record["data"]["disposition"] == "handled"));
        }
    }
}
