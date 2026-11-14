//! Wave D D4/D5: oxa sees a remote model's capabilities before the first request, skips the
//! local-only wire features, runs a tool loop through the controller mock, and refuses the
//! model under `OXA_OFFLINE`.
use oxa_fake::{FakeModel, FakeOpts, FakeServer, ScriptTurn, TestEnv};
use std::process::{Command, Output};

fn run(env: &TestEnv, engine_url: &str, args: &[&str], extra: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_oxa"));
    command.args(args).current_dir(env.work()).env_clear();
    for (name, value) in env.envs(engine_url) {
        command.env(name, value);
    }
    for (name, value) in extra {
        command.env(name, value);
    }
    command.output().expect("spawn oxa")
}

fn cloud_engine(script: Vec<ScriptTurn>) -> FakeServer {
    FakeServer::start(
        script,
        FakeOpts {
            n_ctx: 8_000,
            model: "cloud-model".to_string(),
            remote: Some("openrouter"),
            models: vec![FakeModel {
                alias: "cloud-model".to_string(),
                cap: 64_000,
                max_output: Some(4096),
                caps: None,
            }],
            ..FakeOpts::default()
        },
    )
    .expect("fake controller")
}

#[test]
fn a_remote_model_runs_a_tool_loop_without_local_only_wire_features() {
    let env = TestEnv::new();
    env.write("work/note.txt", "NOTE\n");
    let server = cloud_engine(vec![
        ScriptTurn::tool("call_1", "read", r#"{"path": "note.txt"}"#),
        ScriptTurn::text("done"),
    ]);
    let out = run(&env, &server.url(), &["--no-session", "-p", "read note.txt"], &[]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("done"), "{stdout}");

    let requests = server.requests();
    assert_eq!(requests.len(), 2, "one tool turn and one final chat turn");
    let tool = requests[1].messages.last().expect("tool result");
    assert_eq!(tool.role, oxa_proto::Role::Tool);
    assert_eq!(tool.content.as_deref(), Some("NOTE\n"));

    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).expect("request JSON");
    assert!(!body.as_object().unwrap().contains_key("cache_prompt"), "{body}");
    assert!(
        !body.as_object().unwrap().contains_key("chat_template_kwargs"),
        "{body}"
    );
    assert!(body.get("reasoning_effort").is_some(), "{body}");
    let headers = &requests[0].headers;
    assert!(
        !headers.iter().any(|(name, _)| name.starts_with("x-oxe-")),
        "no local hints reach a remote model: {headers:?}"
    );
}

#[test]
fn the_context_size_comes_from_the_remote_capability_row() {
    let env = TestEnv::new();
    let server = cloud_engine(vec![ScriptTurn::text("ok")]);
    let out = run(&env, &server.url(), &["--list-models"], &[]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("cloud-model"), "{stdout}");
    // FakeOpts has n_ctx 8000, but the model's capability row says 64000.
    assert!(stdout.contains("64K"), "the row's context wins: {stdout}");
}

#[test]
fn offline_refuses_a_remote_model() {
    let env = TestEnv::new();
    let server = cloud_engine(vec![ScriptTurn::text("ok")]);
    let out = run(
        &env,
        &server.url(),
        &["--no-session", "-p", "hello"],
        &[("OXA_OFFLINE", "1")],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_ne!(out.status.code(), Some(0), "stderr: {stderr}");
    assert!(stderr.contains("cloud model cloud-model"), "{stderr}");
    assert!(stderr.contains("offline mode is on"), "{stderr}");
    assert!(server.requests().is_empty(), "nothing reached the mock");
}
