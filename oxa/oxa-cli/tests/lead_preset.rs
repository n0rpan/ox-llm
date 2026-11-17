use oxa_cli::parse_args;
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
use oxa_proto::Value;
use std::io::Write;
use std::process::Command;
use std::process::Stdio;

const EMPTY: &str = r#"{"mode":"shadow","as_of_seq":0,"proposals":[]}"#;

#[test]
fn shadow_setting_is_off_and_project_settings_cannot_enable_it() {
    for project_enable in [false, true] {
        let env = TestEnv::new();
        if project_enable {
            env.write("work/.oxa/settings.json", r#"{"oxa":{"lead":{"shadow":true}}}"#);
            env.write(
                "home/.oxa/agent/settings.json",
                r#"{"defaultProjectTrust":"always","oxa":{"lead":{"shadow":false}}}"#,
            );
        }
        let server = FakeServer::start(vec![ScriptTurn::text(EMPTY)], FakeOpts::default()).unwrap();
        let out = Command::new(env!("CARGO_BIN_EXE_oxa"))
            .args(["lead", "--shadow", "-p", "--model", "fake-model", "check"])
            .env_clear()
            .envs(env.envs(&server.url()))
            .current_dir(env.work())
            .output()
            .unwrap();
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("oxa.lead.shadow"));
        assert!(server.requests().is_empty(), "gate precedes provider discovery");
    }
}

#[test]
fn shadow_is_explicit_and_cannot_add_executable_resources() {
    let parse = |args: &[&str]| parse_args(&args.iter().map(|s| (*s).into()).collect::<Vec<_>>(), &[]);
    assert!(!parse(&["-p", "ordinary"]).unwrap().lead);
    assert!(parse(&["lead"]).unwrap_err().message.contains("unavailable"));
    assert!(parse(&["--shadow"]).is_err());
    for args in [
        vec!["lead", "--shadow", "--tools", "bash"],
        vec!["lead", "--shadow", "--extension", "evil.js"],
        vec!["lead", "--shadow", "--skill", "other.md"],
        vec!["lead", "--shadow", "--output-schema", "other.json"],
    ] {
        assert!(parse(&args).is_err(), "{args:?}");
    }
    let a = parse(&["lead", "--shadow", "-p", "check"]).unwrap();
    assert_eq!(a.messages, ["check"]);
    assert!(a.no_mcp && a.no_extensions && a.no_skills && a.no_tools);
    assert_eq!(a.approve, Some(false));
}

#[test]
fn real_print_preset_refuses_write_and_keeps_read_tools_on_continue() {
    let env = TestEnv::new();
    env.write("work/evidence.txt", "synthetic bounded observation");
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"codemode":{"enabled":true},"oxa":{"subagent":{"enabled":true}}}"#,
    );
    let server = FakeServer::start(
        vec![
            ScriptTurn::tool("forbidden", "write", r#"{"path":"marker","content":"bad"}"#),
            ScriptTurn::tool("evidence", "read", r#"{"path":"evidence.txt"}"#),
            ScriptTurn::text(EMPTY),
            ScriptTurn::text(EMPTY),
        ],
        FakeOpts::default(),
    )
    .unwrap();
    for words in [
        vec!["lead", "--shadow", "-p", "--session-id", "lead-test", "check"],
        vec!["lead", "--shadow", "-p", "--continue=lead-test", "again"],
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_oxa"))
            .args(words)
            .env_clear()
            .envs(env.envs(&server.url()))
            .current_dir(env.work())
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(
            serde_json::from_slice::<Value>(&out.stdout).unwrap(),
            serde_json::from_str::<Value>(EMPTY).unwrap()
        );
    }
    assert!(!env.work().join("marker").exists());
    let requests: Vec<_> = server
        .requests()
        .into_iter()
        .filter(|r| r.max_tokens > 0)
        .collect();
    assert_eq!(requests.len(), 4);
    for request in &requests {
        let tools: Value = serde_json::from_str(&request.tools_json).unwrap();
        let mut names: Vec<_> = tools
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["function"]["name"].as_str().unwrap())
            .collect();
        names.sort_unstable();
        assert_eq!(names, ["find", "grep", "ls", "read"]);
        assert!(request.system.contains("# Lead session"));
        assert!(request.system.contains("Shadow preset: proposals only"));
    }
    assert!(requests[2].messages.iter().any(|m| m
        .content
        .as_deref()
        .is_some_and(|s| s.contains("synthetic bounded observation"))));
}

#[test]
fn rpc_shell_escape_is_blocked_without_starting_a_process() {
    let env = TestEnv::new();
    let server = FakeServer::start(Vec::new(), FakeOpts::default()).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_oxa"))
        .args(["lead", "--shadow", "--mode", "rpc", "--no-session"])
        .env_clear()
        .envs(env.envs(&server.url()))
        .current_dir(env.work())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"{\"id\":\"shell\",\"type\":\"bash\",\"command\":\"touch marker\"}\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!env.work().join("marker").exists());
    assert!(String::from_utf8_lossy(&out.stdout).contains("Shell commands are unavailable"));
}

#[test]
fn embedded_shadow_schema_repairs_an_unbound_action() {
    let env = TestEnv::new();
    let server = FakeServer::start(
        vec![
            ScriptTurn::text(r#"{"mode":"shadow","as_of_seq":0,"proposals":[{"action":"merge"}]}"#),
            ScriptTurn::text(EMPTY),
        ],
        FakeOpts::default(),
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_oxa"))
        .args(["lead", "--shadow", "-p", "--no-session", "propose"])
        .env_clear()
        .envs(env.envs(&server.url()))
        .current_dir(env.work())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), EMPTY);
    assert_eq!(
        server.requests().into_iter().filter(|r| r.max_tokens > 0).count(),
        2
    );
}
