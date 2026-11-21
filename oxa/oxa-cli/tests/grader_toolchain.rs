//! T6: a task that names the tool its own acceptance runs through, and a run that never ran it,
//! ends with an honest question and the `verifier_unused` record. The model server is a fake; the
//! task tools are inert shell functions, so nothing is installed and no real tool is invoked.
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
use std::process::Command;

fn task() -> ScriptTurn {
    ScriptTurn::text("Done: the pattern checks out.")
}

fn server() -> FakeServer {
    FakeServer::start(vec![task()], FakeOpts::default()).unwrap()
}

fn run(env: &TestEnv, server: &FakeServer, mode: &str, task: &str) -> serde_json::Value {
    let mut command = Command::new(env!("CARGO_BIN_EXE_oxa"));
    let output = command
        .current_dir(env.work())
        .env_clear()
        .envs(env.envs(&server.url()))
        .args(["--yolo", "--mode", mode, task])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let record = stderr
        .lines()
        .chain(stdout.lines())
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|value| value["type"] == "oxa_result")
        .unwrap_or_else(|| panic!("no terminal record\nstdout:{stdout}\nstderr:{stderr}"));
    record
}

#[test]
fn a_named_check_tool_that_never_ran_is_recorded_as_a_substitute() {
    let env = TestEnv::new();
    let server = server();
    // The task's own tool is `python99`, which exists nowhere.
    let record = run(
        &env,
        &server,
        "json",
        "Write the regex. The grader applies it with Python 99.",
    );
    assert_eq!(record["verifier_unused"], "python99");
    // Ordinary prose names no tool: the record says so and the run ends in its own turn.
    let plain = run(&env, &server, "json", "Explain how the prefix cache works.");
    assert!(plain["verifier_unused"].is_null());
    assert_eq!(server.requests().len(), 2);
}
