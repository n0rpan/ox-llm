//! #332 step 3: a print run's announced id identifies the exact session to continue.
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
use oxa_session::{FsStore, SessionStore};
use std::process::{Command, Output};

fn run(env: &TestEnv, server: &FakeServer, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_oxa"));
    command.current_dir(env.work()).env_clear().args(args);
    for (key, value) in env.envs(&server.url()) {
        command.env(key, value);
    }
    command.output().unwrap()
}

fn announced_id(output: &Output) -> String {
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .find_map(|line| line.strip_prefix("oxa: session "))
        .expect("session announced on stderr")
        .split_whitespace()
        .next()
        .unwrap()
        .to_string()
}

#[test]
fn explicit_id_continues_the_requested_session_with_a_new_instruction() {
    let env = TestEnv::new();
    env.write("home/.oxa/agent/settings.json", r#"{"retry":{"enabled":false}}"#);
    let server = FakeServer::start(
        vec![
            ScriptTurn::text("one"),
            ScriptTurn::text("unrelated"),
            ScriptTurn::text("continued"),
        ],
        FakeOpts::default(),
    )
    .unwrap();
    let first = run(&env, &server, &["-p", "--no-tools", "first instruction"]);
    let id = announced_id(&first);
    let other = run(&env, &server, &["-p", "--no-tools", "other instruction"]);
    let other_id = announced_id(&other);
    assert_ne!(id, other_id);
    let continued = run(
        &env,
        &server,
        &["-p", "--no-tools", "--continue", &id, "new instruction"],
    );
    assert_eq!(announced_id(&continued), id);
    assert_eq!(String::from_utf8_lossy(&continued.stdout).trim(), "continued");
    let requests = server.requests();
    assert_eq!(
        requests.len(),
        3,
        "one new model request, not a prompt containing the id"
    );
    let messages = format!("{:?}", requests[2].messages);
    assert!(messages.contains("first instruction") && messages.contains("new instruction"));
    assert!(!messages.contains("other instruction"));
    assert!(!messages.contains(&id));
    let store = FsStore::new(env.agent_dir().join("sessions"));
    assert_eq!(
        store.list(None).len(),
        2,
        "continuing appended to the existing session"
    );
}

#[test]
fn no_session_keeps_print_runs_ephemeral() {
    let env = TestEnv::new();
    let server = FakeServer::start(vec![ScriptTurn::text("done")], FakeOpts::default()).unwrap();
    let output = run(&env, &server, &["-p", "--no-session", "--no-tools", "hello"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("oxa: session "));
    assert!(FsStore::new(env.agent_dir().join("sessions"))
        .list(None)
        .is_empty());
}
