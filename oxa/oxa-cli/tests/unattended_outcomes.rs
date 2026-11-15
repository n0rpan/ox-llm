//! #332: unattended callers get a stable result without interpreting human diagnostics.
use oxa_fake::{FakeOpts, FakeServer, Fault, ScriptTurn, TestEnv};
use serde_json::{json, Value};
use std::io::Write;
use std::process::{Command, Stdio};

fn run(env: &TestEnv, server: &FakeServer, args: &[&str], input: &[u8]) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_oxa"));
    command.current_dir(env.work()).env_clear();
    for (key, value) in env.envs(&server.url()) {
        command.env(key, value);
    }
    let mut child = command
        .args(["--no-session", "--no-tools"])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

fn assert_result(output: &std::process::Output, expected_code: i32, outcome: &str, error: Option<&str>) {
    let stderr = String::from_utf8(output.stderr.clone()).unwrap();
    assert_eq!(output.status.code(), Some(expected_code), "{stderr}");
    let results: Vec<Value> = stderr
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|v| v["type"] == "oxa_result")
        .collect();
    assert_eq!(results.len(), 1, "exactly one terminal result: {stderr}");
    assert_eq!(results[0]["outcome"], outcome, "{stderr}");
    assert_eq!(results[0]["error_class"], json!(error), "{stderr}");
    if outcome == "done" {
        assert!(String::from_utf8(output.stdout.clone())
            .unwrap()
            .contains("finished"));
    }
}

fn check(mode: &str, turn: ScriptTurn, expected_code: i32, outcome: &str, error: Option<&str>) {
    let env = TestEnv::new();
    // Isolate outcome classification from the separately tested transient retry policy.
    env.write("home/.oxa/agent/settings.json", r#"{"retry":{"enabled":false}}"#);
    let server = FakeServer::start(vec![turn], FakeOpts::default()).unwrap();
    let output = match mode {
        "print" => run(&env, &server, &["-p", "hello"], b""),
        "json" => run(&env, &server, &["--mode", "json", "hello"], b""),
        _ => run(
            &env,
            &server,
            &["--mode", "rpc"],
            b"{\"type\":\"prompt\",\"message\":\"hello\"}\n",
        ),
    };
    assert_result(&output, expected_code, outcome, error);
    if mode == "rpc" {
        let stdout = String::from_utf8(output.stdout.clone()).unwrap();
        let settled = stdout
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|record| record["type"] == "agent_settled")
            .expect("RPC settlement");
        assert_eq!(settled["result"]["outcome"], outcome);
        assert_eq!(settled["result"]["error_class"], json!(error));
    }

    if outcome == "blocked" || outcome == "failed" {
        assert_eq!(server.requests().len(), 1, "outcome fixtures disable retries");
    }
}

fn done(mode: &str) {
    check(mode, ScriptTurn::text("finished"), 0, "done", None);
}

fn budget(mode: &str) {
    check(
        mode,
        ScriptTurn::fault(Fault::Http {
            status: 429,
            body: json!({"error":{"type":"cloud_error", "code":"budget_exceeded", "message":"Daily budget exhausted"}}).to_string(),
        }),
        oxa_proto::EXIT_BUDGET,
        "blocked",
        Some("budget_exhausted"),
    );
}

#[test]
fn print_emits_done_result() {
    done("print");
}
#[test]
fn json_emits_done_result() {
    done("json");
}
#[test]
fn rpc_emits_done_result() {
    done("rpc");
}
#[test]
fn print_emits_budget_result() {
    budget("print");
}
#[test]
fn json_emits_budget_result() {
    budget("json");
}
#[test]
fn rpc_emits_budget_result() {
    budget("rpc");
}

#[test]
fn gateway_configuration_refusals_keep_their_classes_in_every_mode() {
    for mode in ["print", "json", "rpc"] {
        for (class, code) in [
            ("cloud_credentials_unavailable", 5),
            ("cloud_ledger_unavailable", 6),
        ] {
            check(
                mode,
                ScriptTurn::fault(Fault::Http {
                    status: 503,
                    body: json!({"error":{"type":"cloud_error", "code":class, "message":"Unavailable"}})
                        .to_string(),
                }),
                code,
                "blocked",
                Some(class),
            );
        }
    }
}

#[test]
fn non_retryable_engine_errors_report_the_error_class() {
    for mode in ["print", "json", "rpc"] {
        check(
            mode,
            ScriptTurn::fault(Fault::Http {
                status: 400,
                body: "invalid input".to_string(),
            }),
            1,
            "failed",
            Some("engine_http"),
        );
    }
}

#[test]
fn truncated_replies_are_failed_in_every_mode() {
    for mode in ["print", "json", "rpc"] {
        let mut turn = ScriptTurn::text("partial answer");
        turn.finish = oxa_proto::Finish::Length;
        check(mode, turn, 1, "failed", Some("output_limit"));
    }
}

#[test]
fn rpc_abort_reports_interrupted() {
    let env = TestEnv::new();
    let server = FakeServer::start(
        vec![ScriptTurn::fault(Fault::HoldHeaders { ms: 1000 })],
        FakeOpts::default(),
    )
    .unwrap();
    let output = run(
        &env,
        &server,
        &["--mode", "rpc"],
        b"{\"type\":\"prompt\",\"message\":\"hello\"}\n{\"type\":\"abort\"}\n",
    );
    assert_result(&output, 130, "interrupted", Some("aborted"));
}

#[test]
fn hook_handled_input_reports_nothing_to_do() {
    for mode in ["print", "json", "rpc"] {
        let env = TestEnv::new();
        let extension = env.write(
            "handled.js",
            r#"export default function(oxa) { oxa.on("input", () => ({action:"handled"})); }"#,
        );
        let server = FakeServer::start(vec![], FakeOpts::default()).unwrap();
        let mut args = vec!["-e", extension.to_str().unwrap()];
        if mode == "print" {
            args.extend(["-p", "hello"]);
        } else if mode == "json" {
            args.extend(["--mode", "json", "hello"]);
        } else {
            args.extend(["--mode", "rpc"]);
        }
        let input: &[u8] = if mode == "rpc" {
            b"{\"type\":\"prompt\",\"message\":\"hello\"}\n"
        } else {
            b""
        };
        let output = run(&env, &server, &args, input);
        assert_result(&output, 4, "nothing_to_do", None);
        assert!(server.requests().is_empty());
    }
}

#[test]
fn an_earlier_failure_is_not_hidden_by_a_later_success() {
    for mode in ["print", "json"] {
        let env = TestEnv::new();
        let server = FakeServer::start(
            vec![
                ScriptTurn::fault(Fault::Http {
                    status: 400,
                    body: "invalid".to_string(),
                }),
                ScriptTurn::text("finished"),
            ],
            FakeOpts::default(),
        )
        .unwrap();
        let args = if mode == "print" {
            vec!["-p", "first", "second"]
        } else {
            vec!["--mode", "json", "first", "second"]
        };
        let output = run(&env, &server, &args, b"");
        assert_result(&output, 1, "failed", Some("engine_http"));
        assert_eq!(server.requests().len(), 2);
    }
}

#[test]
fn rpc_eof_without_work_reports_nothing_to_do() {
    let env = TestEnv::new();
    let server = FakeServer::start(vec![], FakeOpts::default()).unwrap();
    let output = run(&env, &server, &["--mode", "rpc"], b"");
    assert_result(&output, 4, "nothing_to_do", None);
}

#[test]
fn offline_startup_refusals_report_blocked_in_every_mode() {
    for mode in ["print", "json", "rpc"] {
        let env = TestEnv::new();
        env.write("home/.oxa/agent/settings.json", r#"{"oxa":{"offline":true}}"#);
        let server = FakeServer::start(
            vec![],
            FakeOpts {
                remote: Some("openrouter"),
                ..FakeOpts::default()
            },
        )
        .unwrap();
        let args = if mode == "print" {
            vec!["-p", "hello"]
        } else {
            vec!["--mode", mode, "hello"]
        };
        let output = run(&env, &server, &args, b"");
        assert_result(&output, 7, "blocked", Some("offline"));
        assert!(server.requests().is_empty());
    }
}

#[test]
fn a_later_command_error_keeps_the_first_unsuccessful_outcome() {
    for mode in ["print", "json"] {
        let env = TestEnv::new();
        let server = FakeServer::start(vec![ScriptTurn::fault(Fault::Http {
            status:429,
            body:json!({"error":{"type":"cloud_error","code":"budget_exceeded","message":"Daily budget exhausted"}}).to_string(),
        })],FakeOpts::default()).unwrap();
        let args = if mode == "print" {
            vec!["-p", "first", "/share"]
        } else {
            vec!["--mode", "json", "first", "/share"]
        };
        let output = run(&env, &server, &args, b"");
        assert_result(&output, 3, "blocked", Some("budget_exhausted"));
        assert_eq!(server.requests().len(), 1);
    }
}

#[test]
fn unknown_rpc_commands_are_failed_outcomes() {
    let env = TestEnv::new();
    let server = FakeServer::start(vec![], FakeOpts::default()).unwrap();
    let output = run(
        &env,
        &server,
        &["--mode", "rpc"],
        b"{\"type\":\"no_such_command\"}\n",
    );
    assert_result(&output, 1, "failed", Some("invalid"));
    assert!(server.requests().is_empty());
}
