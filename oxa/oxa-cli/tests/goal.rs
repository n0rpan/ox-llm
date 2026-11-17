//! Exercise the shipped binary through loopback; no provider keys or real HOME.
use oxa_fake::{FakeOpts, FakeServer, Fault, ScriptTurn, TestEnv};
use serde_json::Value;
use std::io::Write;
use std::process::{Command, Stdio};

fn run(env: &TestEnv, server: &FakeServer, args: &[&str]) -> std::process::Output {
    run_input(env, server, args, None)
}
fn run_input(env: &TestEnv, server: &FakeServer, args: &[&str], input: Option<&str>) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_oxa"));
    cmd.current_dir(env.work()).env_clear();
    for (key, value) in env.envs(&server.url()) {
        cmd.env(key, value);
    }
    cmd.args(["-p", "--tools", "goal_done,wait"])
        .args(args)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    if let Some(input) = input {
        child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
    }
    child.wait_with_output().unwrap()
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
        // This fixture exercises the loopback HTTP controller, not the native factories.
        env.write(
            "home/.oxa/agent/settings.json",
            r#"{"oxa":{"engine":{"inProcessCloud":false,"inProcessLocal":false}}}"#,
        );
        env.write("work/goal.txt", "Finish synthetic work.");
        let mut complete = ScriptTurn::tool(
            "done",
            "goal_done",
            &format!(r#"{{"status":"{status}","evidence":"Synthetic proof."}}"#),
        );
        complete.text = Some("Synthetic final answer".into());
        let server = FakeServer::start(vec![complete], FakeOpts::default()).unwrap();
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
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            if status == "complete" {
                "Synthetic final answer\n"
            } else {
                ""
            },
            "only successful completion exposes the final goal answer"
        );
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].system.contains("Finish synthetic work."));
    }
}
#[test]
fn continue_restores_a_pursuing_goal_without_rereading_the_objective_file() {
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"retry":{"enabled":false},"oxa":{"engine":{"inProcessCloud":false,"inProcessLocal":false}}}"#,
    );
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
        r#"{"retry":{"enabled":false},"oxa":{"engine":{"inProcessCloud":false,"inProcessLocal":false},"bootPreload":false,"envNote":false},"cacheWarming":"off"}"#,
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
            r#"{"retry":{"enabled":false},"oxa":{"engine":{"inProcessCloud":false,"inProcessLocal":false},"bootPreload":false,"envNote":false},"cacheWarming":"off"}"#,
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

fn bounded_goal_env() -> TestEnv {
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"retry":{"enabled":false},"oxa":{"engine":{"inProcessCloud":false,"inProcessLocal":false},"bootPreload":false,"envNote":false},"cacheWarming":"off"}"#,
    );
    env.write("work/goal.txt", "Finish synthetic work.");
    env
}
fn completed_turn() -> ScriptTurn {
    ScriptTurn::tool(
        "done",
        "goal_done",
        r#"{"status":"complete","evidence":"Synthetic first-turn proof."}"#,
    )
}
fn user_text(request: &oxa_fake::Recorded) -> String {
    request
        .messages
        .iter()
        .filter_map(|m| m.content.as_deref())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn goal_first_request_contains_positional_instructions_and_attachment() {
    let env = bounded_goal_env();
    env.write(
        "work/constraints.txt",
        "Attachment restriction: preserve all fixtures.",
    );
    let server = FakeServer::start(vec![completed_turn()], FakeOpts::default()).unwrap();
    let output = run(
        &env,
        &server,
        &[
            "--goal",
            "goal.txt",
            "@constraints.txt",
            "Only modify files under src/.",
        ],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let requests = server.requests();
    let first = user_text(&requests[0]);
    assert!(first.contains("Only modify files under src/."), "{first}");
    assert!(
        first.contains("Attachment restriction: preserve all fixtures."),
        "{first}"
    );
    assert_eq!(requests.len(), 1);
}

#[test]
fn goal_first_request_preserves_piped_constraints() {
    let env = bounded_goal_env();
    let server = FakeServer::start(vec![completed_turn()], FakeOpts::default()).unwrap();
    let output = run_input(
        &env,
        &server,
        &["--goal", "goal.txt"],
        Some("Piped restriction: do not change public APIs.\n"),
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let requests = server.requests();
    let first = user_text(&requests[0]);
    assert!(
        first.contains("Piped restriction: do not change public APIs."),
        "{first}"
    );
    assert_eq!(requests.len(), 1);
}

#[test]
fn ambiguous_ledger_error_preserves_unknown_usage_across_process_restart() {
    let env = bounded_goal_env();
    let first=FakeServer::start(vec![ScriptTurn::fault(Fault::Http {status:503,body:serde_json::json!({"error":{"type":"cloud_error","code":"cloud_ledger_unavailable","message":"Synthetic ambiguous settlement failure"}}).to_string()})],FakeOpts::default()).unwrap();
    let output = run(&env, &first, &["--goal", "goal.txt", "--max-tokens", "100000"]);
    let record = result(&output);
    assert_eq!(
        first.requests().len(),
        1,
        "an actual HTTP request reached the fake engine"
    );
    assert_eq!(record["goal"]["unknown_requests"], 1);
    assert_eq!(record["goal"]["status"], "blocked");
    let next = FakeServer::start(vec![completed_turn()], FakeOpts::default()).unwrap();
    let output = run(
        &env,
        &next,
        &["--continue", record["session_id"].as_str().unwrap()],
    );
    let resumed = result(&output);
    assert_eq!(resumed["goal"]["unknown_requests"], 1);
    assert_eq!(resumed["outcome"], "blocked");
    assert!(next.requests().is_empty());
}

#[test]
fn goal_and_output_schema_require_a_valid_final_answer_after_goal_done() {
    for (initial, answer, success) in [
        (None, Some(r#"{"done":true}"#), true),
        (None, Some("not JSON"), false),
        (Some(r#"{"done":true}"#), None, true),
    ] {
        let env = TestEnv::new();
        // This fixture exercises the loopback HTTP controller, not the native factories.
        env.write(
            "home/.oxa/agent/settings.json",
            r#"{"oxa":{"engine":{"inProcessCloud":false,"inProcessLocal":false}}}"#,
        );
        env.write("work/goal.txt", "Finish synthetic work with a structured answer.");
        env.write(
            "work/answer.schema.json",
            r#"{"type":"object","required":["done"],"properties":{"done":{"enum":[true]}}}"#,
        );
        let mut complete = ScriptTurn::tool(
            "done",
            "goal_done",
            r#"{"status":"complete","evidence":"Synthetic proof."}"#,
        );
        complete.text = initial.map(str::to_owned);
        let mut script = vec![complete];
        if let Some(answer) = answer {
            script.push(ScriptTurn::text(answer));
        }
        let server = FakeServer::start(script, FakeOpts::default()).unwrap();
        let output = run(
            &env,
            &server,
            &["--goal", "goal.txt", "--output-schema", "answer.schema.json"],
        );
        let terminal = result(&output);
        assert_eq!(
            server.requests().iter().filter(|r| r.max_tokens != 0).count(),
            if answer.is_some() { 2 } else { 1 }
        );
        if success {
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(terminal["outcome"], "done");
            assert_eq!(terminal["goal"]["status"], "done");
            assert_eq!(
                serde_json::from_slice::<Value>(&output.stdout).unwrap()["done"],
                true
            );
        } else {
            assert!(!output.status.success());
            assert_eq!(terminal["error_class"], "schema_mismatch");
            assert_ne!(terminal["goal"]["status"], "done");
            assert!(
                output.stdout.is_empty(),
                "a rejected answer must remain unpublished"
            );
        }
    }
}

#[test]
fn explicit_continue_resumes_a_paused_goal_without_resetting_its_budget_or_objective() {
    for named in [false, true] {
        let env = TestEnv::new();
        env.write(
            "home/.oxa/agent/settings.json",
            r#"{"retry":{"enabled":false},"oxa":{"engine":{"inProcessCloud":false,"inProcessLocal":false},"bootPreload":false,"envNote":false},"cacheWarming":"off"}"#,
        );
        env.write(
            "work/goal.txt",
            "Keep reviewing synthetic work and wait between sweeps.",
        );
        let first = FakeServer::start(
            vec![
                ScriptTurn::text("First idle reply"),
                ScriptTurn::text("Second idle reply"),
            ],
            FakeOpts::default(),
        )
        .unwrap();
        let output = run(&env, &first, &["--goal", "goal.txt", "--max-tokens", "1000000"]);
        let paused = result(&output);
        assert_eq!(output.status.code(), Some(9));
        assert_eq!(paused["goal"]["status"], "paused");
        assert_eq!(paused["goal"]["turns"], 2);
        let used = paused["goal"]["tokens_used"].as_u64().unwrap();
        assert!(used > 0);
        std::fs::remove_file(env.work().join("goal.txt")).unwrap();
        let next = FakeServer::start(
            vec![ScriptTurn::tool(
                "finish",
                "goal_done",
                r#"{"status":"complete","evidence":"Explicit continuation retained prior work."}"#,
            )],
            FakeOpts::default(),
        )
        .unwrap();
        let mut args = vec!["--continue"];
        if named {
            args.push(paused["session_id"].as_str().unwrap());
        }
        let output = run(&env, &next, &args);
        let resumed = result(&output);
        assert_eq!(
            output.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(resumed["goal"]["status"], "done");
        assert_eq!(resumed["session_id"], paused["session_id"]);
        assert_eq!(resumed["goal"]["started_ms"], paused["goal"]["started_ms"]);
        assert_eq!(resumed["goal"]["budget"], paused["goal"]["budget"]);
        assert!(resumed["goal"]["tokens_used"].as_u64().unwrap() >= used);
        assert_eq!(resumed["goal"]["turns"], 3);
        assert_eq!(resumed["goal"]["unknown_requests"], 0);
        let requests = next.requests();
        assert_eq!(requests.len(), 1);
        assert!(requests[0]
            .system
            .contains("Keep reviewing synthetic work and wait between sweeps."));
    }
}
