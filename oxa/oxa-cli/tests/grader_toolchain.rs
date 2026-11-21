//! T6: a task that names the tool its own acceptance runs through, and a run that never ran it,
//! ends with one honest question and the `verifier_unused` record. The model server is a fake, so
//! nothing is installed and no real tool is invoked.
use oxa_fake::{FakeModel, FakeOpts, FakeServer, ScriptTurn, TestEnv};
use serde_json::json;
use std::process::{Command, Stdio};

/// The model's script: one file edit, its final answer, and (after the question) a second answer.
fn server() -> FakeServer {
    FakeServer::start(
        vec![
            ScriptTurn::tool(
                "w",
                "write",
                &json!({"path": "probe.txt", "content": "x"}).to_string(),
            ),
            ScriptTurn::text("Done: the pattern checks out."),
            ScriptTurn::text("Done: the substituted check passed; the task's own tool stays unverified."),
        ],
        FakeOpts {
            model: "fake-model".into(),
            models: vec![FakeModel {
                alias: "fake-model".into(),
                cap: 400_000,
                max_output: Some(1024),
                caps: None,
            }],
            ..FakeOpts::default()
        },
    )
    .unwrap()
}

/// One `-p` run with `task` as its prompt: the terminal record from its output.
fn run(env: &TestEnv, server: &FakeServer, task: &str) -> serde_json::Value {
    let child = Command::new(env!("CARGO_BIN_EXE_oxa"))
        .current_dir(env.work())
        .env_clear()
        .envs(env.envs(&server.url()))
        .args(["--yolo", "-p", task])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    stdout
        .lines()
        .chain(stderr.lines())
        .filter_map(|line| {
            let line = match line.find(r#"{"type":"#) {
                Some(at) => &line[at..],
                None => line,
            };
            serde_json::from_str::<serde_json::Value>(line.trim()).ok()
        })
        .find(|value| value["type"] == "oxa_result")
        .unwrap_or_else(|| panic!("no terminal record\nstdout:{stdout}\nstderr:{stderr}"))
}

/// The user messages of the run that asked about the task's own acceptance tool.
fn toolchain_questions(server: &FakeServer) -> Vec<String> {
    server
        .requests()
        .iter()
        .map(|request| {
            request
                .messages
                .iter()
                .rev()
                .find(|m| m.role == oxa_proto::Role::User)
                .and_then(|m| m.content.clone())
                .unwrap_or_default()
        })
        .filter(|text| text.starts_with("This task's acceptance names"))
        .collect()
}

#[test]
fn a_task_tool_that_never_ran_is_asked_about_and_recorded() {
    let env = TestEnv::new();
    let named = server();
    // The task's own tool is `python99`, which exists nowhere on any PATH.
    let record = run(
        &env,
        &named,
        "Write the regex. The grader applies it with Python 99.",
    );
    assert_eq!(record["verifier_unused"], "python99");
    let questions = toolchain_questions(&named);
    assert_eq!(questions.len(), 1, "{questions:?}");
    assert!(questions[0].contains("python99"), "{}", questions[0]);

    // A task that names no tool: the record says so and no question is asked.
    let plain = server();
    let record = run(&env, &plain, "Explain how the prefix cache works, in one line.");
    assert!(record["verifier_unused"].is_null());
    assert!(toolchain_questions(&plain).is_empty());
}
