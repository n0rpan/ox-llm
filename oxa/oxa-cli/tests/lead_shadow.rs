//! Exercise the documented shadow preset, including its real print-mode process.
use oxa_core::output_schema::OutputSchema;
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
use oxa_proto::{Role, Value};
use serde_json::json;
use std::process::Command;

const SCHEMA: &str = include_str!("../../docs/lead-proposals.schema.json");
const GUIDE: &str = include_str!("../../docs/lead.md");
const HEAD: &str = "1111111111111111111111111111111111111111";

fn proposal() -> Value {
    json!({"mode":"shadow","as_of_seq":18,"proposals":[{
        "action":"approve","subject_kind":"pull_request","number":7,"commit":HEAD,
        "evidence":{"file":"state/verdicts.txt","line":1,"text":format!("PR #7 {HEAD}: gate green; review clear; holds none")},
        "reason":"The supplied observation satisfies the stated review rule."}]})
}

#[test]
fn shadow_schema_requires_bound_proposals_and_rejects_malformed_fields() {
    let schema = OutputSchema::new(serde_json::from_str(SCHEMA).unwrap()).unwrap();
    assert!(schema.matches(r#"{"mode":"shadow","as_of_seq":0,"proposals":[]}"#));
    for action in ["approve", "send_back", "hand_out", "ask_owner", "hold"] {
        let mut valid = proposal();
        valid["proposals"][0]["action"] = json!(action);
        assert!(schema.matches(&valid.to_string()));
    }
    for (pointer, value) in [
        ("/mode", json!("execute")),
        ("/as_of_seq", json!(-1)),
        ("/as_of_seq", json!(1.5)),
        ("/proposals/0/action", json!("merge")),
        ("/proposals/0/subject_kind", json!("branch")),
        ("/proposals/0/number", json!(0)),
        ("/proposals/0/number", json!(1.5)),
        ("/proposals/0/commit", json!("1111111")),
        ("/proposals/0/evidence/file", json!("")),
        ("/proposals/0/evidence/line", json!(0)),
        ("/proposals/0/evidence/text", json!("")),
        ("/proposals/0/reason", json!("")),
    ] {
        let mut invalid = proposal();
        *invalid.pointer_mut(pointer).unwrap() = value;
        assert!(
            !schema.matches(&invalid.to_string()),
            "accepted {pointer}: {invalid}"
        );
    }
    for pointer in ["", "/proposals/0", "/proposals/0/evidence"] {
        let mut invalid = proposal();
        invalid.pointer_mut(pointer).unwrap()["execute"] = json!(true);
        assert!(
            !schema.matches(&invalid.to_string()),
            "accepted unknown field: {invalid}"
        );
    }
}

#[test]
fn documented_shadow_invocation_loads_lead_and_only_read_tools() {
    let env = TestEnv::new();
    env.write("work/skills/lead/SKILL.md", oxa_resources::LEAD_SKILL);
    env.write("work/proposals.schema.json", SCHEMA);
    env.write("work/state/digest.txt", "as_of_seq=18\n");
    env.write("work/state/board.txt", "PR #7 waits for review adjudication\n");
    let evidence = proposal()["proposals"][0]["evidence"]["text"]
        .as_str()
        .unwrap()
        .to_owned();
    env.write("work/state/verdicts.txt", &evidence);
    std::fs::create_dir(env.root().join("agent")).unwrap();
    let answer = proposal().to_string();
    let mut invalid = proposal();
    invalid["proposals"][0]["action"] = json!("merge");
    let server = FakeServer::start(
        vec![
            ScriptTurn::tool("read_evidence", "read", r#"{"path":"state/verdicts.txt"}"#),
            ScriptTurn::text(&invalid.to_string()),
            ScriptTurn::text(&answer),
        ],
        FakeOpts::default(),
    )
    .unwrap();
    let invocation = GUIDE
        .split("```sh lead-shadow\n")
        .nth(1)
        .unwrap()
        .split("```")
        .next()
        .unwrap();
    let binary = std::path::Path::new(env!("CARGO_BIN_EXE_oxa"));
    let path = std::env::join_paths(std::iter::once(binary.parent().unwrap().to_path_buf()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .unwrap();
    let output = Command::new("/bin/sh")
        .args(["-eu", "-c", invocation])
        .env_clear()
        .envs(env.envs(&server.url()))
        .env("PATH", path)
        .env("LEAD_SHADOW_DIR", env.root())
        .env("LEAD_ENGINE_URL", server.url())
        .env("LEAD_MODEL", "fake-model")
        .env("LEAD_OXA_BIN", binary)
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
        proposal()
    );
    let requests: Vec<_> = server
        .requests()
        .into_iter()
        .filter(|r| r.max_tokens > 0)
        .collect();
    assert_eq!(requests.len(), 3, "the invalid action must trigger schema repair");
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
        assert!(request.system.contains("<name>lead</name>"), "lead skill missing");
        assert!(!request.system.contains("<name>pull-request</name>"));
        assert!(
            request.messages.iter().any(|m| m.role == Role::User
                && m.content.as_deref().is_some_and(|s| s.contains("## Shadow mode"))),
            "lead instructions were not expanded"
        );
    }
    assert!(requests[1]
        .messages
        .iter()
        .any(|m| m.role == Role::Tool && m.content.as_deref().is_some_and(|s| s.contains(&evidence))));
}
