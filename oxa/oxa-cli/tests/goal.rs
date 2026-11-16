//! Exercise the shipped binary through loopback; no provider keys or real HOME.
use oxa_fake::{FakeOpts, FakeServer, Fault, ScriptTurn, TestEnv};
use serde_json::Value;
use std::process::{Command, Stdio};

fn run(env: &TestEnv, server: &FakeServer, args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_oxa"));
    cmd.current_dir(env.work()).env_clear();
    for (key, value) in env.envs(&server.url()) {
        cmd.env(key, value);
    }
    cmd.args(["-p", "--tools", "goal_done,wait"])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap()
}
fn result(output: &std::process::Output) -> Value {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let records: Vec<Value> = stderr
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|v| v["type"] == "oxa_result")
        .collect();
    assert_eq!(records.len(), 1, "{stderr}");
    records[0].clone()
}
#[test]
fn goal_file_needs_no_positional_prompt_and_reports_terminal_evidence() {
    for (status, outcome, code) in [("complete", "done", 0), ("blocked", "blocked", 8)] {
        let env = TestEnv::new();
        env.write("work/goal.txt", "Finish synthetic work.");
        let server = FakeServer::start(
            vec![ScriptTurn::tool(
                "done",
                "goal_done",
                &format!(r#"{{"status":"{status}","evidence":"Synthetic proof."}}"#),
            )],
            FakeOpts::default(),
        )
        .unwrap();
        let output = run(&env, &server, &["--goal", "goal.txt"]);
        let record = result(&output);
        assert_eq!(
            output.status.code(),
            Some(code),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(record["outcome"], outcome);
        assert_eq!(record["goal"]["evidence"], "Synthetic proof.");
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].system.contains("Finish synthetic work."));
    }
}
#[test]
fn continue_restores_a_pursuing_goal_without_rereading_the_objective_file() {
    let env = TestEnv::new();
    env.write("home/.oxa/agent/settings.json", r#"{"retry":{"enabled":false}}"#);
    env.write("work/goal.txt", "Finish synthetic work after restart.");
    let first = FakeServer::start(
        vec![ScriptTurn::fault(Fault::Http {
            status: 400,
            body: "synthetic failure".into(),
        })],
        FakeOpts::default(),
    )
    .unwrap();
    let output = run(&env, &first, &["--goal", "goal.txt"]);
    let record = result(&output);
    assert_eq!(record["goal"]["status"], "pursuing");
    std::fs::remove_file(env.work().join("goal.txt")).unwrap();
    let second = FakeServer::start(
        vec![ScriptTurn::tool(
            "done",
            "goal_done",
            r#"{"status":"complete","evidence":"Restart verified."}"#,
        )],
        FakeOpts::default(),
    )
    .unwrap();
    let output = run(
        &env,
        &second,
        &["--continue", record["session_id"].as_str().unwrap()],
    );
    let record = result(&output);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(record["goal"]["evidence"], "Restart verified.");
    assert!(second.requests()[0]
        .system
        .contains("Finish synthetic work after restart."));
}

#[test]
fn broken_request_usage_survives_restart_without_granting_another_goal_allowance() {
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"retry":{"enabled":false},"oxa":{"bootPreload":false,"envNote":false},"cacheWarming":"off"}"#,
    );
    env.write("work/goal.txt", "Finish synthetic work.");
    let mut cut = ScriptTurn::text(&"synthetic partial output ".repeat(100));
    cut.chunk = 1;
    cut.fault = Some(Fault::DropAt { bytes: 300 });
    let first = FakeServer::start(vec![cut], FakeOpts::default()).unwrap();
    let output = run(&env, &first, &["--goal", "goal.txt", "--max-tokens", "100000"]);
    let record = result(&output);
    assert_eq!(record["goal"]["unknown_requests"], 1);
    assert_eq!(record["goal"]["status"], "blocked");
    let next = FakeServer::start(
        vec![ScriptTurn::tool(
            "done",
            "goal_done",
            r#"{"status":"complete","evidence":"Must not get a fresh allowance"}"#,
        )],
        FakeOpts::default(),
    )
    .unwrap();
    let output = run(
        &env,
        &next,
        &["--continue", record["session_id"].as_str().unwrap()],
    );
    let resumed = result(&output);
    assert_eq!(resumed["outcome"], "blocked");
    assert_eq!(resumed["goal"]["unknown_requests"], 1);
    assert!(next.requests().is_empty());
}

#[test]
fn pre_admission_refusal_does_not_turn_the_goal_budget_into_unknown_spend() {
    for (status, code) in [(429, "budget_exceeded"), (503, "cloud_credentials_unavailable")] {
        let env = TestEnv::new();
        env.write(
            "home/.oxa/agent/settings.json",
            r#"{"retry":{"enabled":false},"oxa":{"bootPreload":false,"envNote":false},"cacheWarming":"off"}"#,
        );
        env.write("work/goal.txt", "Finish synthetic work.");
        let refused = FakeServer::start(vec![ScriptTurn::fault(Fault::Http { status, body: serde_json::json!({"error":{"type":"cloud_error","code":code,"message":"Synthetic pre-admission refusal"}}).to_string() })], FakeOpts::default()).unwrap();
        let output = run(&env, &refused, &["--goal", "goal.txt", "--max-tokens", "100000"]);
        let record = result(&output);
        assert_eq!(record["goal"]["unknown_requests"], 0);
        assert_eq!(record["goal"]["status"], "pursuing");
        let next = FakeServer::start(
            vec![ScriptTurn::tool(
                "done",
                "goal_done",
                r#"{"status":"complete","evidence":"Known refusal used no generation allowance"}"#,
            )],
            FakeOpts::default(),
        )
        .unwrap();
        let output = run(
            &env,
            &next,
            &["--continue", record["session_id"].as_str().unwrap()],
        );
        assert_eq!(result(&output)["outcome"], "done");
        assert_eq!(next.requests().len(), 1);
    }
}
