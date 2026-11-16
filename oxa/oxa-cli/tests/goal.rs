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
