//! Runner contract: a broken stream and a length cut can still finish useful work.
#[path = "common/http.rs"]
mod http;
use oxa_fake::{FakeOpts, FakeServer, Fault, ScriptTurn, TestEnv};
use oxa_proto::Finish;
use serde_json::Value;
use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn print_preserves_the_complete_local_answer_across_length_cuts() {
    for (remote, broken) in [(false, false), (false, true), (true, false), (true, true)] {
        let env = TestEnv::new();
        env.write(
            "home/.oxa/agent/settings.json",
            r#"{"retry":{"enabled":true,"maxRetries":2},"cacheWarming":"off","oxa":{"envNote":false}}"#,
        );
        let mut first = ScriptTurn::text("α first ");
        first.finish = Finish::Length;
        let mut second = ScriptTurn::text("middle ");
        second.finish = Finish::Length;
        let mut turns = vec![first];
        if broken {
            let mut partial = ScriptTurn::text("SPOILED_STREAM");
            partial.chunk = 1;
            partial.fault = Some(Fault::DropAt { bytes: 500 });
            turns.push(partial);
        }
        turns.extend([second, ScriptTurn::text("tail")]);
        let srv = FakeServer::start(
            turns,
            FakeOpts {
                remote: remote.then_some("openrouter"),
                ..FakeOpts::default()
            },
        )
        .unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_oxa"))
            .args([
                "-p",
                "--no-session",
                "--no-tools",
                "--no-extensions",
                "--no-mcp",
                "answer",
            ])
            .env_clear()
            .envs(http::envs(&env, &srv.url()))
            .current_dir(env.work())
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        if broken {
            assert!(String::from_utf8_lossy(&output.stderr).contains("the reply stream broke"));
            let requests = srv.requests();
            assert_eq!(requests[1].idem_key, requests[2].idem_key);
        }
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            if remote {
                "tail\n"
            } else {
                "α first middle tail\n"
            }
        );
    }
}

#[test]
fn all_headless_modes_complete_after_byte_and_length_cuts() {
    for mode in ["print", "json", "rpc"] {
        for remote in [false, true] {
            let env = TestEnv::new();
            env.write("home/.oxa/agent/settings.json", r#"{"retry":{"enabled":true,"maxRetries":1,"baseDelayMs":1,"maxAgentDelayMs":5},"oxa":{"envNote":false}}"#);
            let mut broken = ScriptTurn::text("UNFINISHED_STREAM");
            broken.chunk = 1;
            broken.fault = Some(Fault::DropAt { bytes: 5 });
            let mut cut = ScriptTurn::text("UNFINISHED_LENGTH");
            cut.finish = Finish::Length;
            let server = FakeServer::start(
                vec![
                    broken,
                    cut,
                    ScriptTurn::tool(
                        "save",
                        "write",
                        r#"{"path":"completed.txt","content":"complete"}"#,
                    ),
                    ScriptTurn::text("TASK_COMPLETE"),
                ],
                FakeOpts {
                    remote: remote.then_some("openrouter"),
                    ..FakeOpts::default()
                },
            )
            .unwrap();
            let mut command = Command::new(env!("CARGO_BIN_EXE_oxa"));
            command
                .current_dir(env.work())
                .env_clear()
                .envs(http::envs(&env, &server.url()))
                .args(["--no-session", "--permission-mode", "workspace", "--yolo"]);
            match mode {
                "print" => {
                    command.args(["-p", "finish the task"]);
                }
                "json" => {
                    command.args(["--mode", "json", "finish the task"]);
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
                    .write_all(b"{\"type\":\"prompt\",\"message\":\"finish the task\"}\n")
                    .unwrap();
            }
            drop(child.stdin.take());
            let output = child.wait_with_output().unwrap();
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert_eq!(output.status.code(), Some(0), "{mode}/{remote}: {stderr}");
            let terminal: Vec<Value> = stderr
                .lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .filter(|v| v["type"] == "oxa_result")
                .collect();
            assert_eq!(terminal.len(), 1);
            assert_eq!(terminal[0]["outcome"], "done");
            assert_eq!(
                std::fs::read_to_string(env.work().join("completed.txt")).unwrap(),
                "complete"
            );
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(stdout.contains("TASK_COMPLETE"));
            if mode == "print" {
                assert_eq!(stdout.trim(), "TASK_COMPLETE");
            } else {
                let records: Vec<Value> = stdout
                    .lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect();
                assert_eq!(records.iter().filter(|v| v["type"] == "agent_settled").count(), 1);
            }
            let requests = server.requests();
            assert_eq!(requests.len(), if remote { 4 } else { 5 }, "{mode}/{remote}");
            assert_eq!(requests[0].idem_key, requests[1].idem_key);
            assert_eq!(
                requests.iter().filter(|r| r.max_tokens == 0).count(),
                usize::from(!remote)
            );
            assert!(requests.iter().all(|r| r.messages.iter().all(|m| !m
                .content
                .as_deref()
                .unwrap_or("")
                .contains("UNFINISHED_STREAM"))));
            if remote {
                assert!(requests.iter().all(|r| r.messages.iter().all(|m| !m
                    .content
                    .as_deref()
                    .unwrap_or("")
                    .contains("UNFINISHED_LENGTH"))));
            }
        }
    }
}
