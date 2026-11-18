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

    assert_eq!(
        requests[0].session, requests[1].session,
        "one session across tool turns"
    );
    assert_ne!(
        requests[0].idem_key, requests[1].idem_key,
        "a key per logical request"
    );
    for request in &requests {
        let body: serde_json::Value = serde_json::from_slice(&request.body).expect("request JSON");
        assert!(!body.as_object().unwrap().contains_key("cache_prompt"), "{body}");
        assert!(
            !body.as_object().unwrap().contains_key("chat_template_kwargs"),
            "{body}"
        );
        assert!(body.get("reasoning_effort").is_some(), "{body}");
        assert!(
            !request.session.is_empty(),
            "oxc needs the session for correlation"
        );
        assert!(
            !request.idem_key.is_empty(),
            "oxc needs the request's idempotency key"
        );
        let headers = &request.headers;
        assert!(
            headers
                .iter()
                .any(|(name, value)| name == "x-session-id" && value == &request.session),
            "both session headers reach oxc: {headers:?}"
        );
        assert!(
            !headers
                .iter()
                .any(|(name, _)| name.starts_with("x-oxe-") && name != "x-oxe-session"),
            "only the session correlation header, no local hints, reaches oxc: {headers:?}"
        );
    }
}

#[test]
fn the_context_size_comes_from_the_remote_capability_row() {
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"oxa":{"engine":{"inProcessCloud":false},"cloud":{"models":{"cloud-model":{
            "provider":"openrouter","provider_routing":{"zdr":true}
        }}}}}"#,
    );
    let server = cloud_engine(vec![ScriptTurn::text("ok")]);
    let out = run(&env, &server.url(), &["--list-models"], &[]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("cloud-model"), "{stdout}");
    // FakeOpts has n_ctx 8000, but the model's capability row says 64000.
    assert!(stdout.contains("64K"), "the row's context wins: {stdout}");
    let row = stdout.lines().nth(1).expect("model row");
    assert_eq!(row.split_whitespace().last(), Some("-"), "{row}");
}

/// Subscription models must be discoverable by provider, without guessing capabilities.
#[test]
fn list_models_chatgpt_provider_and_published_modalities() {
    for omit_modality in [false, true] {
        let env = TestEnv::new();
        let server = FakeServer::start(
            Vec::new(),
            FakeOpts {
                model: "chatgpt/subscription-model".to_string(),
                remote: Some("chatgpt"),
                omit_modality,
                ..FakeOpts::default()
            },
        )
        .expect("fake controller");
        let out = run(&env, &server.url(), &["--list-models", "chatgpt"], &[]);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(0), "stdout: {stdout}\nstderr: {stderr}");
        let rows: Vec<_> = stdout.lines().skip(1).collect();
        assert_eq!(rows.len(), 1, "{stdout}");
        let cells: Vec<_> = rows[0].split_whitespace().collect();
        assert_eq!(cells[0], "chatgpt", "subscription provider: {stdout}");
        assert_eq!(cells[1], "chatgpt/subscription-model");
        let expected = if omit_modality { ["?", "?"] } else { ["yes", "no"] };
        assert_eq!(cells[4..6], expected, "published capabilities: {stdout}");
        assert_eq!(cells[6..], ["-"], "subscription routing: {stdout}");
        assert!(server.requests().is_empty(), "listing must not generate");
    }
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

/// Cloud-only oxc (no engine): the status still names the default local alias, `/v1/models`
/// lists only remote rows. oxa must pick a remote model, send no prefix preload and no warning.
#[test]
fn a_cloud_only_controller_gets_a_remote_model_and_no_preload() {
    let env = TestEnv::new();
    let server = FakeServer::start(
        vec![ScriptTurn::text("hello")],
        FakeOpts {
            n_ctx: 8_000,
            model: "qwen-local".to_string(),
            remote: Some("anthropic"),
            no_engine: true,
            models: vec![FakeModel {
                alias: "anthropic/claude-haiku-5-5".to_string(),
                cap: 200_000,
                max_output: Some(32_000),
                caps: None,
            }],
            ..FakeOpts::default()
        },
    )
    .expect("fake controller");
    let out = run(&env, &server.url(), &["--no-session", "-p", "hi"], &[]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        !stderr.contains("warning") && !stderr.contains("preload"),
        "{stderr}"
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 1, "no preload request: {requests:?}");
    assert_eq!(requests[0].model, "anthropic/claude-haiku-5-5");
}
