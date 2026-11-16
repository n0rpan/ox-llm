//! Terminal facts describe this invocation, without guessing workspace effects from prose.
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
use serde_json::Value;
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
            assert_eq!(std::fs::read_to_string(env.work().join("result.txt")).unwrap(), "actual file");
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
