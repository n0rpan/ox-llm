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

#[test]
fn a_prompt_word_after_long_continue_still_continues_the_latest_session() {
    let env = TestEnv::new();
    env.write("home/.oxa/agent/settings.json", r#"{"retry":{"enabled":false}}"#);
    let server = FakeServer::start(
        vec![
            ScriptTurn::text("one"),
            ScriptTurn::text("two"),
            ScriptTurn::text("three"),
        ],
        FakeOpts::default(),
    )
    .unwrap();
    let first = run(&env, &server, &["-p", "--no-tools", "first instruction"]);
    let id = announced_id(&first);
    let one = run(&env, &server, &["-p", "--no-tools", "--continue", "fix"]);
    assert_eq!(announced_id(&one), id);
    let two = run(
        &env,
        &server,
        &["--no-tools", "--continue", "summarize the diff", "-p"],
    );
    assert_eq!(announced_id(&two), id);
    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    assert!(format!("{:?}", requests[1].messages).contains("fix"));
    assert!(format!("{:?}", requests[2].messages).contains("summarize the diff"));
}

#[test]
fn a_prompt_word_matching_an_older_session_prefix_is_kept_as_prompt_text() {
    let env = TestEnv::new();
    let server = FakeServer::start(
        vec![
            ScriptTurn::text("old"),
            ScriptTurn::text("latest"),
            ScriptTurn::text("continued"),
            ScriptTurn::text("tests added"),
        ],
        FakeOpts::default(),
    )
    .unwrap();
    let old = run(
        &env,
        &server,
        &[
            "-p",
            "--no-tools",
            "--session-id",
            "add-older",
            "older instruction",
        ],
    );
    assert_eq!(announced_id(&old), "add-older");
    let latest = run(
        &env,
        &server,
        &[
            "-p",
            "--no-tools",
            "--session-id",
            "latest-session",
            "latest instruction",
        ],
    );
    assert_eq!(announced_id(&latest), "latest-session");
    let continued = run(&env, &server, &["-p", "--no-tools", "--continue", "add", "tests"]);
    assert_eq!(announced_id(&continued), "latest-session");
    let requests = server.requests();
    assert_eq!(requests.len(), 4);
    let messages = format!("{:?}", requests[2].messages);
    assert!(
        messages.contains("latest instruction")
            && requests[2].messages.iter().any(|message| message
                .content
                .as_deref()
                .is_some_and(|text| text == "add" || text.starts_with("add\n"))),
        "{messages}"
    );
    assert!(!messages.contains("older instruction"));
    assert!(format!("{:?}", requests[3].messages).contains("tests"));
}

#[test]
fn explicit_continue_uses_the_configured_session_store_on_both_parse_passes() {
    for project_setting in [false, true] {
        for extension_flag in [false, true] {
            let env = TestEnv::new();
            let settings = r#"{"sessionDir":"custom-sessions","retry":{"enabled":false}}"#;
            if project_setting {
                env.write(
                    "home/.oxa/agent/settings.json",
                    r#"{"defaultProjectTrust":"always"}"#,
                );
                env.write("work/.oxa/settings.json", settings);
            } else {
                env.write("home/.oxa/agent/settings.json", settings);
            }
            let extension = env.write("flags.js", r#"export default function(oxa) { oxa.registerFlag("synthetic-flag", {description:"Fixture flag",type:"boolean",default:false}); }"#);
            let server = FakeServer::start(
                vec![
                    ScriptTurn::text("first"),
                    ScriptTurn::text("other"),
                    ScriptTurn::text("continued"),
                ],
                FakeOpts::default(),
            )
            .unwrap();
            let first = run(&env, &server, &["-p", "--no-tools", "first instruction"]);
            let id = announced_id(&first);
            let other = run(&env, &server, &["-p", "--no-tools", "other instruction"]);
            assert_ne!(announced_id(&other), id);
            let mut args = vec!["-p", "--no-tools", "--continue", &id, "new instruction"];
            if extension_flag {
                args.extend(["-e", extension.to_str().unwrap(), "--synthetic-flag"]);
            }
            let output = run(&env, &server, &args);
            assert_eq!(
                announced_id(&output),
                id,
                "project={project_setting}, reparse={extension_flag}"
            );
            let requests = server.requests();
            assert_eq!(requests.len(), 3, "id must not become an extra prompt");
            let messages = format!("{:?}", requests[2].messages);
            assert!(messages.contains("first instruction") && messages.contains("new instruction"));
            assert!(!messages.contains("other instruction") && !messages.contains(&id));
            assert_eq!(
                FsStore::new(env.work().join("custom-sessions")).list(None).len(),
                2
            );
        }
    }
}
