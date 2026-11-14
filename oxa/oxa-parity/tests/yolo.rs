//! O-182: `oxa --yolo`, end to end with the real binary against the fake engine. YOLO removes
//! the permission prompts; the deny-list and the sandbox are still enforced.

use oxa_fake::{FakeOpts, FakeServer, Recorded, ScriptTurn, TestEnv};
use oxa_parity::run_oxa;
use oxa_proto::Role;
use serde_json::json;
use std::path::Path;

fn write_call(path: &Path) -> ScriptTurn {
    ScriptTurn::tool(
        "c1",
        "write",
        &format!("{{\"path\": \"{}\", \"content\": \"x\"}}", path.display()),
    )
}

fn tool_result(reqs: &[Recorded]) -> String {
    reqs.last()
        .and_then(|r| {
            r.messages
                .iter()
                .find(|m| m.role == Role::Tool)
                .and_then(|m| m.content.clone())
        })
        .unwrap_or_default()
}

fn run(env: &TestEnv, turn: ScriptTurn, args: &[&str]) -> (std::process::Output, Vec<Recorded>) {
    let srv = FakeServer::start(vec![turn, ScriptTurn::text("ok")], FakeOpts::default())
        .expect("start fake engine");
    let out = run_oxa(env, &srv.url(), args, "");
    let reqs = srv.requests();
    (out, reqs)
}

/// A write outside the work tree asks in `workspace` mode. Print mode has nobody to ask, so
/// it is refused; with `--yolo` it is auto-approved.
#[test]
fn yolo_auto_approves_what_would_have_asked() {
    let env = TestEnv::new();
    let outside = env.root().join("elsewhere");
    std::fs::create_dir_all(&outside).expect("dir");
    let target = outside.join("out.txt");

    let (out, reqs) = run(&env, write_call(&target), &["-p", "write it"]);
    assert!(out.status.success());
    assert!(
        !target.exists(),
        "without --yolo the write outside the work tree is not approved"
    );
    assert!(!tool_result(&reqs).is_empty());

    let (out, _) = run(&env, write_call(&target), &["-p", "--yolo", "write it"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        std::fs::read_to_string(&target).ok().as_deref(),
        Some("x"),
        "--yolo approves the write without a prompt"
    );
}

/// The negative: under `--yolo` a write to `~/.ssh` is still blocked by the deny-list.
#[test]
fn yolo_does_not_open_the_deny_list() {
    let env = TestEnv::new();
    let ssh = env.home().join(".ssh");
    std::fs::create_dir_all(&ssh).expect("dir");
    for target in [ssh.join("authorized_keys"), env.home().join(".config/oxc/token")] {
        let (out, reqs) = run(&env, write_call(&target), &["-p", "--yolo", "write it"]);
        assert!(out.status.success());
        assert!(!target.exists(), "{target:?} must not be written under --yolo");
        let result = tool_result(&reqs);
        assert!(
            result.contains("keys, tokens or agent state"),
            "the refusal names the deny-list: {result:?}"
        );
    }
}

/// The sandbox stays on: bash under `--yolo` still cannot write outside the work tree.
#[test]
fn yolo_keeps_bash_in_the_sandbox() {
    let env = TestEnv::new();
    let escaped = env.root().join("elsewhere-escaped.txt");
    let turn = ScriptTurn::tool(
        "c1",
        "bash",
        &format!("{{\"command\": \"echo leaked > '{}'\"}}", escaped.display()),
    );
    let (out, _) = run(&env, turn, &["-p", "--yolo", "run it"]);
    assert!(out.status.success());
    assert!(!escaped.exists(), "bash wrote outside the work tree under --yolo");
}

#[test]
fn help_lists_yolo_and_there_is_no_sandbox_off_switch() {
    let env = TestEnv::new();
    let out = run_oxa(&env, "http://127.0.0.1:9", &["--help"], "");
    let help = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(help.contains("--yolo"));
    assert!(help.contains("the sandbox and the deny-list stay on"));
    assert!(!help.contains("--no-sandbox"));
    let out = run_oxa(
        &env,
        "http://127.0.0.1:9",
        &["--yolo", "--no-sandbox", "-p", "x"],
        "",
    );
    assert!(!out.status.success(), "--no-sandbox is not an option");
}

/// The agent a child runs: `write` only (`bash` for the sandbox case).
fn writer(tools: &str) -> String {
    format!("---\nname: writer\ndescription: Writes files\ntools: {tools}\n---\nYou write.\n")
}

/// A parent that calls the `writer` subagent once, then answers. The script serves the parent
/// and its child in arrival order (the parent waits inside the tool call).
fn subagent_run(
    env: &TestEnv,
    tools: &str,
    child_turn: ScriptTurn,
    args: &[&str],
) -> (std::process::Output, Vec<Recorded>) {
    env.write("home/.oxa/agent/agents/writer.md", &writer(tools));
    let call = ScriptTurn::tool(
        "sub_1",
        "subagent",
        &json!({"agent": "writer", "task": "write it"}).to_string(),
    );
    let srv = FakeServer::start(
        vec![
            call,
            child_turn,
            ScriptTurn::text("child done"),
            ScriptTurn::text("parent done"),
        ],
        FakeOpts::default(),
    )
    .expect("start fake engine");
    let out = run_oxa(env, &srv.url(), args, "");
    let reqs = srv.requests();
    (out, reqs)
}

/// The tool result the child saw last (its requests have kind `subagent`).
fn child_tool_result(reqs: &[Recorded]) -> String {
    reqs.iter()
        .rev()
        .find(|r| r.kind == "subagent")
        .and_then(|r| {
            r.messages
                .iter()
                .rev()
                .find(|m| m.role == Role::Tool)
                .and_then(|m| m.content.clone())
        })
        .unwrap_or_default()
}

/// O-182, subagents: a child started by a `--yolo` parent writes outside the project without a
/// prompt. Without `--yolo` the same child is refused (print mode has nobody to ask).
#[test]
fn yolo_child_writes_without_a_prompt() {
    let env = TestEnv::new();
    let outside = env.root().join("elsewhere");
    std::fs::create_dir_all(&outside).expect("dir");
    let target = outside.join("child.txt");

    let (out, _) = subagent_run(
        &env,
        "write",
        write_call(&target),
        &["-p", "--yolo", "delegate it"],
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        std::fs::read_to_string(&target).ok().as_deref(),
        Some("x"),
        "the child writes under --yolo"
    );

    let env = TestEnv::new();
    let outside = env.root().join("elsewhere");
    std::fs::create_dir_all(&outside).expect("dir");
    let target = outside.join("child.txt");
    let (out, reqs) = subagent_run(&env, "write", write_call(&target), &["-p", "delegate it"]);
    assert!(out.status.success());
    assert!(
        !target.exists(),
        "without --yolo the child's write is not approved"
    );
    assert!(child_tool_result(&reqs).contains("Permission denied"));
}

/// O-182, subagents: under `--yolo` the child's write to the project's `.oxa` configuration
/// still asks, and a child cannot answer a question, so it is refused.
#[test]
fn yolo_child_still_asks_for_the_project_oxa_config() {
    let env = TestEnv::new();
    let target = env.work().join(".oxa/settings.json");
    let (out, reqs) = subagent_run(
        &env,
        "write",
        write_call(&target),
        &["-p", "--yolo", "delegate it"],
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(
        !target.exists(),
        "the child's write to .oxa is not approved under --yolo"
    );
    let result = child_tool_result(&reqs);
    assert!(result.contains("Permission denied"), "{result:?}");
}

/// O-182, subagents: a child started by a `--yolo` parent runs bash in the sandbox, so a write
/// outside the project does not land.
#[test]
fn yolo_child_bash_stays_in_the_sandbox() {
    let env = TestEnv::new();
    let escaped = env.root().join("elsewhere-child-escaped.txt");
    let turn = ScriptTurn::tool(
        "c1",
        "bash",
        &format!("{{\"command\": \"echo leaked > '{}'\"}}", escaped.display()),
    );
    let (out, reqs) = subagent_run(&env, "bash", turn, &["-p", "--yolo", "delegate it"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(
        !child_tool_result(&reqs).is_empty(),
        "the child ran its bash call"
    );
    assert!(
        !escaped.exists(),
        "a child's bash wrote outside the work tree under --yolo"
    );
}
