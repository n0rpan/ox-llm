//! Terminal facts describe this invocation, without guessing workspace effects from prose.
use oxa_fake::{FakeOpts, FakeServer, Fault, ScriptTurn, TestEnv};
use serde_json::Value;
use std::io::Write;
use std::process::{Command, Stdio};

fn command(env: &TestEnv, url: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_oxa"));
    command.env_clear().envs(env.envs(url)).current_dir(env.work());
    command
}
fn terminal(bytes: &[u8]) -> Value {
    let records: Vec<Value> = bytes
        .split(|byte| *byte == b'\n')
        .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
        .filter(|record| record["type"] == "oxa_result")
        .collect();
    assert_eq!(records.len(), 1);
    records.into_iter().next().unwrap()
}

#[test]
fn real_print_and_json_report_session_requests_tokens_and_explicit_unknown_file_changes() {
    for mode in ["print", "json"] {
        for saved in [false, true] {
            let env = TestEnv::new();
            env.write(
                "home/.oxa/agent/settings.json",
                r#"{"cacheWarming":"off","oxa":{"envNote":false,"bootPreload":false}}"#,
            );
            let server = FakeServer::start(
                vec![
                    ScriptTurn::tool(
                        "save",
                        "write",
                        r#"{"path":"result.txt","content":"actual file"}"#,
                    ),
                    ScriptTurn::text("I claim 999 files changed."),
                ],
                FakeOpts::default(),
            )
            .unwrap();
            let mut command = command(&env, &server.url());
            if mode == "print" {
                command.arg("-p");
            } else {
                command.args(["--mode", "json"]);
            }
            if !saved {
                command.arg("--no-session");
            }
            let output = command
                .args([
                    "--no-mcp",
                    "--no-extensions",
                    "--no-context-files",
                    "--tools",
                    "write",
                    "make a file",
                ])
                .stdin(Stdio::null())
                .output()
                .unwrap();
            assert_eq!(
                output.status.code(),
                Some(0),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                std::fs::read_to_string(env.work().join("result.txt")).unwrap(),
                "actual file"
            );
            let result = terminal(&output.stderr);
            assert_eq!(result["outcome"], "done");
            assert_eq!(result["requests"], 2);
            assert_eq!(result["turns"], 2);
            assert_eq!(result["usage_scope"], "process");
            assert!(result["tokens"]["prompt"].as_u64().unwrap() > 0);
            assert!(result["tokens"]["completion"].as_u64().unwrap() > 0);
            assert_eq!(result["tokens"]["complete"], true);
            if saved {
                assert!(!result["session_id"].as_str().unwrap().is_empty());
            } else {
                assert!(result["session_id"].is_null());
            }
            assert!(result.as_object().unwrap().contains_key("files_changed"));
            assert!(result["files_changed"].is_null());
            assert_eq!(result["files_changed_evidence"], "unavailable");
            assert_eq!(server.requests().len(), 2);
        }
    }
}

#[test]
fn argument_failure_reports_unknown_facts_instead_of_zero_work() {
    let env = TestEnv::new();
    let output = command(&env, "http://127.0.0.1:1")
        .args(["-p", "--max-tokens", "0", "hello"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let result = terminal(&output.stderr);
    for key in ["session_id", "requests", "turns", "tokens", "files_changed"] {
        assert!(result.as_object().unwrap().contains_key(key), "{key}");
        assert!(result[key].is_null(), "{key}");
    }
}

#[test]
fn rpc_reports_the_same_process_facts_with_or_without_a_saved_session() {
    for saved in [false, true] {
        let env = TestEnv::new();
        env.write(
            "home/.oxa/agent/settings.json",
            r#"{"cacheWarming":"off","oxa":{"envNote":false,"bootPreload":false}}"#,
        );
        let server = FakeServer::start(vec![ScriptTurn::text("done")], FakeOpts::default()).unwrap();
        let mut command = command(&env, &server.url());
        command.args(["--mode", "rpc", "--no-tools", "--no-mcp", "--no-extensions"]);
        if !saved {
            command.arg("--no-session");
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        writeln!(
            child.stdin.take().unwrap(),
            "{}",
            serde_json::json!({"type":"prompt", "message":"hello"})
        )
        .unwrap();
        let output = child.wait_with_output().unwrap();
        assert_eq!(
            output.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result = terminal(&output.stderr);
        assert_eq!(result["requests"], 1);
        assert_eq!(result["turns"], 1);
        assert_eq!(result["tokens"]["complete"], true);
        assert_eq!(result["session_id"].is_string(), saved);
    }
}

#[test]
fn a_resumed_invocation_does_not_recount_the_saved_sessions_usage() {
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"cacheWarming":"off","oxa":{"envNote":false,"bootPreload":false}}"#,
    );
    let server = FakeServer::start(
        vec![ScriptTurn::text("first"), ScriptTurn::text("second")],
        FakeOpts::default(),
    )
    .unwrap();
    let first = command(&env, &server.url())
        .args(["-p", "--no-tools", "--no-mcp", "--no-extensions", "hello"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(first.status.code(), Some(0));
    let first = terminal(&first.stderr);
    let id = first["session_id"].as_str().unwrap();
    let second = command(&env, &server.url())
        .args([
            "-p",
            "--no-tools",
            "--no-mcp",
            "--no-extensions",
            &format!("--continue={id}"),
            "continue",
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(second.status.code(), Some(0));
    let second = terminal(&second.stderr);
    assert_eq!(second["session_id"], id);
    assert_eq!(second["requests"], 1);
    assert_eq!(second["turns"], 1);
    assert_eq!(server.requests().len(), 2);
}

#[test]
fn a_failed_request_with_no_usage_is_an_explicit_incomplete_lower_bound() {
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"cacheWarming":"off","retry":{"enabled":false},"oxa":{"envNote":false,"bootPreload":false}}"#,
    );
    let mut cut = ScriptTurn::text("partial response");
    cut.fault = Some(Fault::DropAt { bytes: 5 });
    let server = FakeServer::start(vec![cut], FakeOpts::default()).unwrap();
    let output = command(&env, &server.url())
        .args(["-p", "--no-tools", "--no-mcp", "--no-extensions", "hello"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let result = terminal(&output.stderr);
    assert_eq!(result["requests"], 1);
    assert_eq!(result["turns"], 1);
    assert_eq!(result["tokens"]["complete"], false);
    assert_eq!(result["tokens"]["unreported_requests"], 1);
}

#[test]
fn an_explicit_allowed_set_reports_no_extra_for_a_named_write() {
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"cacheWarming":"off","oxa":{"envNote":false,"bootPreload":false}}"#,
    );
    let server = FakeServer::start(
        vec![
            ScriptTurn::tool(
                "save",
                "write",
                r#"{"path":"result.txt","content":"actual file"}"#,
            ),
            ScriptTurn::text("done"),
        ],
        FakeOpts::default(),
    )
    .unwrap();
    let output = command(&env, &server.url())
        .args([
            "-p",
            "--no-session",
            "--no-mcp",
            "--no-extensions",
            "--no-context-files",
            "--tools",
            "write",
            "--allowed-changed-files",
            "result.txt",
            "make a file",
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result = terminal(&output.stderr);
    assert_eq!(result["files_changed_evidence"], "tool-observed");
    assert_eq!(result["files_changed"], serde_json::json!(["result.txt"]));
    assert_eq!(result["files_changed_allowed"], serde_json::json!(["result.txt"]));
    assert_eq!(result["files_changed_extra"], serde_json::json!([]));
}

#[test]
fn an_explicit_allowed_set_reports_a_named_write_outside_it_as_extra() {
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"cacheWarming":"off","oxa":{"envNote":false,"bootPreload":false}}"#,
    );
    let server = FakeServer::start(
        vec![
            ScriptTurn::tool(
                "save",
                "write",
                r#"{"path":"result.txt","content":"actual file"}"#,
            ),
            ScriptTurn::text("done"),
        ],
        FakeOpts::default(),
    )
    .unwrap();
    let output = command(&env, &server.url())
        .args([
            "-p",
            "--no-session",
            "--no-mcp",
            "--no-extensions",
            "--no-context-files",
            "--tools",
            "write",
            "--allowed-changed-files",
            "other.txt",
            "make a file",
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result = terminal(&output.stderr);
    assert_eq!(result["files_changed_evidence"], "tool-observed");
    assert_eq!(result["files_changed"], serde_json::json!(["result.txt"]));
    assert_eq!(result["files_changed_allowed"], serde_json::json!(["other.txt"]));
    assert_eq!(result["files_changed_extra"], serde_json::json!(["result.txt"]));
}

#[test]
fn without_an_allowed_set_the_new_fields_stay_null() {
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"cacheWarming":"off","oxa":{"envNote":false,"bootPreload":false}}"#,
    );
    let server = FakeServer::start(
        vec![
            ScriptTurn::tool(
                "save",
                "write",
                r#"{"path":"result.txt","content":"actual file"}"#,
            ),
            ScriptTurn::text("done"),
        ],
        FakeOpts::default(),
    )
    .unwrap();
    let output = command(&env, &server.url())
        .args([
            "-p",
            "--no-session",
            "--no-mcp",
            "--no-extensions",
            "--no-context-files",
            "--tools",
            "write",
            "make a file",
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result = terminal(&output.stderr);
    assert!(result["files_changed"].is_null());
    assert_eq!(result["files_changed_evidence"], "unavailable");
    assert!(result["files_changed_extra"].is_null());
    assert!(result["files_changed_allowed"].is_null());
}
