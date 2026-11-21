//! #2293 A: the completion record of a file the request asks for. A declaration with the
//! `deliverable` tool is checked once more at the end of the run; the record says
//! `verified-now`, `missing`, or `unverified`, and it never changes the run's outcome.
//! Fake engine only.
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
use serde_json::json;
use std::process::{Command, Output};

fn run(env: &TestEnv, server: &FakeServer) -> Output {
    Command::new(env!("CARGO_BIN_EXE_oxa"))
        .args(["-p", "--no-session", "Produce the answer file named answer.txt"])
        .env_clear()
        .envs(env.envs(&server.url()))
        .current_dir(env.work())
        .output()
        .expect("run oxa")
}

fn write_answer() -> ScriptTurn {
    ScriptTurn::tool(
        "w1",
        "write",
        &json!({ "path": "answer.txt", "content": "e2e4\n" }).to_string(),
    )
}

#[test]
fn a_written_and_declared_file_is_verified_now_at_the_end() {
    let env = TestEnv::new();
    let server = FakeServer::start(
        vec![
            write_answer(),
            ScriptTurn::tool("d1", "deliverable", &json!({ "path": "answer.txt" }).to_string()),
            ScriptTurn::text("Wrote answer.txt."),
        ],
        FakeOpts::default(),
    )
    .unwrap();
    let out = run(&env, &server);
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(out.status.success(), "{stderr}");
    assert!(stdout.contains("Wrote answer.txt."), "{stdout}");
    assert!(
        stderr.contains("Deliverable evidence"),
        "the run ends with its completion record: {stderr}"
    );
    assert!(
        stderr.contains("answer.txt: verified-now, present, 5 bytes"),
        "{stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(env.work().join("answer.txt")).unwrap(),
        "e2e4\n"
    );
}

#[test]
fn a_file_that_is_gone_at_the_end_is_reported_missing_and_the_run_still_succeeds() {
    let env = TestEnv::new();
    let server = FakeServer::start(
        vec![
            write_answer(),
            ScriptTurn::tool("d1", "deliverable", &json!({ "path": "answer.txt" }).to_string()),
            ScriptTurn::tool(
                "b1",
                "bash",
                &json!({ "command": "rm -f answer.txt" }).to_string(),
            ),
            ScriptTurn::text("Done."),
        ],
        FakeOpts::default(),
    )
    .unwrap();
    let out = run(&env, &server);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "no finish gate: {stderr}");
    assert!(
        stderr.contains("answer.txt: missing, not on disk"),
        "the record says the declared file is not there: {stderr}"
    );
    assert!(!env.work().join("answer.txt").exists());
}

#[test]
fn a_private_path_is_refused_like_a_read_and_declares_nothing() {
    let env = TestEnv::new();
    // The agent directory is on the deny-list: the check runs through `read`'s policy, so it
    // never shows that file, and a refused call is not a completed declaration.
    let private = env.agent_dir().join("asks.jsonl");
    let server = FakeServer::start(
        vec![
            ScriptTurn::tool(
                "d1",
                "deliverable",
                &json!({ "path": private.to_str().unwrap() }).to_string(),
            ),
            ScriptTurn::text("Done."),
        ],
        FakeOpts::default(),
    )
    .unwrap();
    let out = run(&env, &server);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let requests = server.requests();
    let result = requests[1]
        .messages
        .last()
        .and_then(|m| m.content.as_deref())
        .unwrap_or_default();
    assert!(result.contains("Permission denied"), "{result}");
    assert!(
        !stderr.contains("Deliverable evidence"),
        "a refused check is never verified: {stderr}"
    );
}
