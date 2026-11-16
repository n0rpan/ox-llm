//! #332 step 5 / #184: one thinking fallback, then an explicit terminal failure class.
#[path = "common/http.rs"]
mod http;
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
use oxa_proto::Finish;
use serde_json::Value;
use std::io::Write;
use std::process::{Command, Stdio};

fn overrun() -> ScriptTurn {
    let mut turn = ScriptTurn::text("");
    turn.text = None;
    turn.reasoning = Some("synthetic reasoning ".repeat(20));
    turn.finish = Finish::Length;
    turn
}

fn run(mode: &str, second: ScriptTurn) -> (std::process::Output, FakeServer) {
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"retry":{"enabled":false},"oxa":{"envNote":false}}"#,
    );
    let server = FakeServer::start(
        vec![overrun(), second, ScriptTurn::text("must not run")],
        FakeOpts::default(),
    )
    .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_oxa"));
    command
        .current_dir(env.work())
        .env_clear()
        .envs(http::envs(&env, &server.url()))
        .args(["--no-session", "--no-tools", "--thinking", "medium"]);
    if mode == "print" {
        command.args(["-p", "explain the task"]);
    } else if mode == "json" {
        command.args(["--mode", "json", "explain the task"]);
    } else {
        command.args(["--mode", "rpc"]);
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
            .write_all(b"{\"type\":\"prompt\",\"message\":\"explain the task\"}\n")
            .unwrap();
    }
    child.stdin.take();
    let output = child.wait_with_output().unwrap();
    (output, server)
}

fn failed(mode: &str) {
    for content in ["", "  \n"] {
        let mut second = ScriptTurn::text(content);
        second.finish = Finish::Length;
        let (output, server) = run(mode, second);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{stderr}");
        let results: Vec<Value> = stderr
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|value| value["type"] == "oxa_result")
            .collect();
        assert_eq!(results.len(), 1, "{stderr}");
        assert_eq!(results[0]["outcome"], "failed");
        assert_eq!(results[0]["error_class"], "thinking_overrun");
        assert!(
            stderr.contains("even without thinking")
                || String::from_utf8_lossy(&output.stdout).contains("even without thinking")
        );
        let requests = server.requests();
        assert_eq!(requests.len(), 2, "one fallback only");
        assert!(requests[0].thinking && !requests[1].thinking);
        assert_eq!(
            requests[0].messages, requests[1].messages,
            "discarded reasoning never becomes history"
        );
        if mode == "print" {
            assert!(output.stdout.is_empty(), "no empty success answer");
        }
    }
}

#[test]
fn print_reports_exhausted_thinking_fallback() {
    failed("print");
}
#[test]
fn json_reports_exhausted_thinking_fallback() {
    failed("json");
}
#[test]
fn rpc_reports_exhausted_thinking_fallback() {
    failed("rpc");
}

#[test]
fn successful_thinking_fallback_stays_done() {
    for mode in ["print", "json", "rpc"] {
        let (output, server) = run(mode, ScriptTurn::text("the answer"));
        assert_eq!(
            output.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result: Value = String::from_utf8_lossy(&output.stderr)
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|value| value["type"] == "oxa_result")
            .unwrap();
        assert_eq!(result["outcome"], "done");
        assert!(result["error_class"].is_null());
        assert_eq!(server.requests().len(), 2);
        assert!(String::from_utf8_lossy(&output.stdout).contains("the answer"));
    }
}
