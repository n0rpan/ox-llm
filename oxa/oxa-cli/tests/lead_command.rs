//! The SHADOW entry point must use the ordinary print loop without ambient tools.
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
use oxa_proto::Value;
use std::process::Command;

#[test]
fn lead_shadow_reads_evidence_and_repairs_proposals_without_ambient_hooks() {
    let env = TestEnv::new();
    env.write("work/state/digest.txt", "as_of_seq=18\n");
    env.write("work/state/board.txt", "PR #7 awaits adjudication\n");
    env.write("work/state/verdicts.txt", "PR #7 review clear\n");
    // A configured exec hook must never run, even in an otherwise trusted HOME.
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"oxa":{"lead":{"shadow":true}},"codemode":{"enabled":true},"hooks":{"session_start":[{"command":"touch ambient-hook-ran"}]}}"#,
    );
    env.write("home/.oxa/agent/SYSTEM.md", "AMBIENT_SYSTEM_SENTINEL");
    let (hooks, warnings) = oxa_ext::parse_exec_hooks(&serde_json::json!({
        "session_start": [{"command":"touch ambient-hook-ran"}]
    }));
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(hooks.len(), 1);
    let answer = r#"{"mode":"shadow","as_of_seq":18,"proposals":[]}"#;
    let server = FakeServer::start(
        vec![
            ScriptTurn::tool("forbidden", "bash", r#"{"command":"touch forbidden-tool-ran"}"#),
            ScriptTurn::tool("digest", "read", r#"{"path":"state/digest.txt"}"#),
            ScriptTurn::tool("board", "read", r#"{"path":"state/board.txt"}"#),
            ScriptTurn::tool("evidence", "read", r#"{"path":"state/verdicts.txt"}"#),
            ScriptTurn::text(r#"{"mode":"execute","as_of_seq":18,"proposals":[]}"#),
            ScriptTurn::text(answer),
        ],
        FakeOpts::default(),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_oxa"))
        .args(["lead", "--shadow", "--model", "fake-model", "--max-time", "30"])
        .env_clear()
        .envs(env.envs(&server.url()))
        .current_dir(env.work())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        serde_json::from_str::<Value>(answer).unwrap()
    );
    assert!(!env.work().join("ambient-hook-ran").exists());
    assert!(!env.work().join("forbidden-tool-ran").exists());
    let requests: Vec<_> = server
        .requests()
        .into_iter()
        .filter(|r| r.max_tokens > 0)
        .collect();
    assert_eq!(requests.len(), 6);
    for request in &requests {
        let tools: Value = serde_json::from_str(&request.tools_json).unwrap();
        let mut names: Vec<_> = tools
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        names.sort_unstable();
        assert_eq!(names, ["find", "grep", "ls", "read"]);
        assert!(request.system.contains("## Shadow mode"));
        assert!(!request.system.contains("AMBIENT_SYSTEM_SENTINEL"));
        assert!(request.system.contains("Never execute proposals"));
        assert!(request.messages.iter().any(|m| m
            .content
            .as_deref()
            .is_some_and(|s| s.contains("No consumer may execute"))));
    }
    for observed in [
        "as_of_seq=18",
        "PR #7 awaits adjudication",
        "PR #7 review clear",
        "Tool bash not found",
    ] {
        assert!(
            requests[4]
                .messages
                .iter()
                .any(|m| m.content.as_deref().is_some_and(|s| s.contains(observed))),
            "missing observation: {observed}"
        );
    }
}

#[test]
fn lead_shadow_is_off_until_the_user_setting_enables_it() {
    let env = TestEnv::new();
    env.write("work/state/digest.txt", "as_of_seq=18\n");
    let server = FakeServer::start(vec![ScriptTurn::text("{}")], FakeOpts::default()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_oxa"))
        .args(["lead", "--shadow", "--model", "fake-model"])
        .env_clear()
        .envs(env.envs(&server.url()))
        .current_dir(env.work())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("oxa.lead.shadow"), "{stderr}");
    assert!(server.requests().is_empty(), "no engine request before the gate");
}

#[test]
fn lead_refuses_live_mode_and_tool_or_instruction_overrides() {
    for words in [
        vec!["lead"],
        vec!["lead", "--shadow", "--tools", "bash"],
        vec!["lead", "--shadow", "--extension", "untrusted.js"],
        vec!["lead", "--shadow", "--system-prompt", "override"],
    ] {
        let env = TestEnv::new();
        let output = Command::new(env!("CARGO_BIN_EXE_oxa"))
            .args(words)
            .env_clear()
            .envs(env.envs("http://127.0.0.1:1"))
            .current_dir(env.work())
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("lead --shadow"));
    }
}
