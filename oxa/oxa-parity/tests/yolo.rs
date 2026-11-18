//! O-182, #202: the permission modes end to end with the real binary against the fake engine.
//! `auto` (the default) removes the prompts and keeps the deny-list and the sandbox; `yolo` is
//! full access: no prompts, bash without the sandbox, no guard for the file tools;
//! `standard` asks (and print mode, with nobody to ask, refuses).

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

/// A write outside the work tree asks in `standard`; print mode has nobody to ask, so it is
/// refused. The default (`auto`), `--auto` and `--yolo` approve it without a prompt.
#[test]
fn standard_refuses_auto_and_yolo_approve_what_would_have_asked() {
    let env = TestEnv::new();
    let outside = env.root().join("elsewhere");
    std::fs::create_dir_all(&outside).expect("dir");
    let target = outside.join("out.txt");
    // Explicit absolute /tmp paths now name session scratch (#597). A cwd-relative path
    // still names this host file outside the workspace, which is what this test exercises.
    let tool_path = Path::new("../elsewhere/out.txt");

    let (out, reqs) = run(&env, write_call(tool_path), &["-p", "--standard", "write it"]);
    assert!(out.status.success());
    assert!(
        !target.exists(),
        "--standard does not approve the write outside the work tree"
    );
    assert!(tool_result(&reqs).contains("Permission denied"));

    for args in [
        &["-p", "write it"][..],
        &["-p", "--auto", "write it"],
        &["-p", "--yolo", "write it"],
    ] {
        let _ = std::fs::remove_file(&target);
        let (out, _) = run(&env, write_call(tool_path), args);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(
            std::fs::read_to_string(&target).ok().as_deref(),
            Some("x"),
            "{args:?} approves the write without a prompt"
        );
    }
}

/// `permissions.defaultMode` in the user's settings sets the mode at start; a flag wins.
#[test]
fn the_saved_default_mode_applies_and_a_flag_wins() {
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"permissions": {"defaultMode": "standard"}}"#,
    );
    let outside = env.root().join("elsewhere");
    std::fs::create_dir_all(&outside).expect("dir");
    let target = outside.join("out.txt");
    // Exercise the host write permission, not an absolute /tmp session-scratch path.
    let tool_path = Path::new("../elsewhere/out.txt");

    let (out, _) = run(&env, write_call(tool_path), &["-p", "write it"]);
    assert!(out.status.success());
    assert!(
        !target.exists(),
        "the saved standard mode asks, so print mode refuses"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("permission mode standard"),
        "the mode is logged on stderr: {err}"
    );

    let (out, _) = run(&env, write_call(tool_path), &["-p", "--auto", "write it"]);
    assert!(out.status.success());
    assert!(target.exists(), "--auto wins over the saved default for one run");
    assert!(String::from_utf8_lossy(&out.stderr).contains("permission mode auto"));
}

/// The deny-list holds at `auto`; `yolo` removes it (owner's decision on #202): the same write
/// to `~/.ssh` goes through.
#[test]
fn auto_keeps_the_deny_list_and_yolo_removes_it() {
    let env = TestEnv::new();
    let ssh = env.home().join(".ssh");
    std::fs::create_dir_all(&ssh).expect("dir");
    for target in [ssh.join("authorized_keys"), env.home().join(".config/oxc/token")] {
        let (out, reqs) = run(&env, write_call(&target), &["-p", "--auto", "write it"]);
        assert!(out.status.success());
        assert!(!target.exists(), "{target:?} must not be written under --auto");
        let result = tool_result(&reqs);
        assert!(
            result.contains("keys, tokens or agent state"),
            "the refusal names the deny-list: {result:?}"
        );
    }
    let target = ssh.join("authorized_keys");
    let (out, _) = run(&env, write_call(&target), &["-p", "--yolo", "write it"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(target.exists(), "--yolo removes the deny-list of the file tools");
}

/// `auto` keeps bash in the sandbox; `yolo` runs it without the sandbox.
#[test]
fn auto_keeps_bash_in_the_sandbox_and_yolo_does_not() {
    let env = TestEnv::new();
    let escaped = env.root().join("elsewhere-escaped.txt");
    let turn = || {
        ScriptTurn::tool(
            "c1",
            "bash",
            &format!("{{\"command\": \"echo leaked > '{}'\"}}", escaped.display()),
        )
    };
    // #1512 S3: an attended --auto run has no OS sandbox unless asked for it.
    let (out, _) = run(&env, turn(), &["-p", "--auto", "--sandbox", "run it"]);
    assert!(out.status.success());
    assert!(!escaped.exists(), "bash wrote outside the work tree under --auto");

    let (out, _) = run(&env, turn(), &["-p", "--yolo", "run it"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        std::fs::read_to_string(&escaped).ok().as_deref(),
        Some("leaked\n"),
        "--yolo runs bash without the sandbox"
    );
}

/// #602: absolute /tmp names use bash's active filesystem view, not always scratch.
#[test]
fn bash_and_file_tools_share_tmp_paths_in_auto_and_yolo() {
    for flag in ["--auto", "--yolo"] {
        let env = TestEnv::new();
        let target = env.root().join("outside/result.txt");
        assert!(target.starts_with("/tmp"));
        let turns = vec![
            ScriptTurn::tool(
                "bash",
                "bash",
                &json!({"command": format!(
                    "mkdir -p '{}' && printf 'before\\n' > '{}'",
                    target.parent().unwrap().display(), target.display()
                )})
                .to_string(),
            ),
            ScriptTurn::tool("read1", "read", &json!({"path": target}).to_string()),
            ScriptTurn::tool(
                "edit",
                "edit",
                &json!({"path": target, "oldText": "before", "newText": "edited"}).to_string(),
            ),
            ScriptTurn::tool(
                "bash2",
                "bash",
                &json!({"command": format!("cat '{}'", target.display())}).to_string(),
            ),
            ScriptTurn::tool(
                "write",
                "write",
                &json!({"path": target, "content": "written\n"}).to_string(),
            ),
            ScriptTurn::tool(
                "bash3",
                "bash",
                &json!({"command": format!("cat '{}'", target.display())}).to_string(),
            ),
            ScriptTurn::text("ok"),
        ];
        let srv = FakeServer::start(turns, FakeOpts::default()).unwrap();
        let out = run_oxa(&env, &srv.url(), &["-p", flag, "--sandbox", "use the tools"], "");
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let reqs = srv.requests();
        assert_eq!(reqs.len(), 7, "{flag}: the complete tool loop must run");
        let latest_result = |index: usize| {
            reqs[index]
                .messages
                .iter()
                .rev()
                .find(|m| m.role == Role::Tool)
                .and_then(|m| m.content.as_deref())
                .unwrap_or_default()
        };
        assert!(
            latest_result(2).contains("before\n"),
            "{flag}: read bash output: {}",
            latest_result(2)
        );
        assert!(
            latest_result(4).contains("edited\n"),
            "{flag}: bash sees edit: {}",
            latest_result(4)
        );
        assert!(
            latest_result(6).contains("written\n"),
            "{flag}: bash sees write: {}",
            latest_result(6)
        );
        if flag == "--yolo" {
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "written\n");
        } else {
            assert!(!target.exists(), "auto must leave the host namesake untouched");
        }
    }
}

#[test]
fn help_lists_the_modes_and_there_is_no_separate_sandbox_off_switch() {
    let env = TestEnv::new();
    let out = run_oxa(&env, "http://127.0.0.1:9", &["--help"], "");
    let help = String::from_utf8_lossy(&out.stdout).into_owned();
    for flag in ["--standard", "--auto", "--yolo"] {
        assert!(help.contains(flag), "{flag}");
    }
    assert!(help.contains("the bash sandbox and the deny-list stay on"));
    assert!(help.contains("bash runs without the sandbox"));
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

/// Subagents inherit the mode: a child of a `--yolo` parent writes outside the project; a
/// child of a `--standard` parent is refused (print mode has nobody to ask), even though the
/// child's own default would be `auto`.
#[test]
fn child_inherits_the_parents_mode() {
    let env = TestEnv::new();
    let outside = env.root().join("elsewhere");
    std::fs::create_dir_all(&outside).expect("dir");
    let target = outside.join("child.txt");
    // As above, name the outside host file relative to cwd, not through session /tmp.
    let tool_path = Path::new("../elsewhere/child.txt");

    let (out, _) = subagent_run(
        &env,
        "write",
        write_call(tool_path),
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
    let (out, reqs) = subagent_run(
        &env,
        "write",
        write_call(tool_path),
        &["-p", "--standard", "delegate it"],
    );
    assert!(out.status.success());
    assert!(
        !target.exists(),
        "under --standard the child's write is not approved"
    );
    assert!(child_tool_result(&reqs).contains("Permission denied"));
}

/// Under `auto` the child's write to the project's `.oxa` configuration still asks, and a
/// child cannot answer a question, so it is refused.
#[test]
fn auto_child_still_asks_for_the_project_oxa_config() {
    let env = TestEnv::new();
    let target = env.work().join(".oxa/settings.json");
    let (out, reqs) = subagent_run(
        &env,
        "write",
        write_call(&target),
        &["-p", "--auto", "delegate it"],
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(
        !target.exists(),
        "the child's write to .oxa is not approved under --auto"
    );
    let result = child_tool_result(&reqs);
    assert!(result.contains("Permission denied"), "{result:?}");
}

/// A child of an `--auto` parent runs bash in the sandbox, so a write outside the project does
/// not land.
#[test]
fn auto_child_bash_stays_in_the_sandbox() {
    let env = TestEnv::new();
    let escaped = env.root().join("elsewhere-child-escaped.txt");
    let turn = ScriptTurn::tool(
        "c1",
        "bash",
        &format!("{{\"command\": \"echo leaked > '{}'\"}}", escaped.display()),
    );
    let (out, reqs) = subagent_run(&env, "bash", turn, &["-p", "--auto", "delegate it"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(
        !child_tool_result(&reqs).is_empty(),
        "the child ran its bash call"
    );
    assert!(
        !escaped.exists(),
        "a child's bash wrote outside the work tree under --auto"
    );
}

/// The full-access wrapper must use the session's sandbox, not its pre-session template.
#[test]
fn auto_bash_keeps_session_scratch_between_commands() {
    let env = TestEnv::new();
    let srv = FakeServer::start(
        vec![
            ScriptTurn::tool(
                "save",
                "bash",
                r#"{"command":"printf scratch-marker > /tmp/oxa-yolo-scratch"}"#,
            ),
            ScriptTurn::tool("load", "bash", r#"{"command":"cat /tmp/oxa-yolo-scratch"}"#),
            ScriptTurn::text("done"),
        ],
        FakeOpts::default(),
    )
    .unwrap();
    let out = run_oxa(&env, &srv.url(), &["-p", "--auto", "--sandbox", "save then load"], "");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let reqs = srv.requests();
    let result = reqs
        .last()
        .unwrap()
        .messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Tool)
        .and_then(|m| m.content.as_deref())
        .unwrap_or_default();
    assert!(
        result.contains("scratch-marker"),
        "session scratch lost: {result}"
    );
}

/// Real file-tool opens, not only policy answers: yolo lifts the credential and git/config
/// floor; auto keeps its denials and the protected project-config question.
#[test]
fn yolo_file_tools_read_credentials_and_write_git_and_project_config() {
    for rel in [".git/hooks/pre-commit", ".git/config", ".oxa/settings.json"] {
        let env = TestEnv::new();
        let target = env.work().join(rel);
        let (out, _) = run(&env, write_call(&target), &["-p", "--auto", "write it"]);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert!(!target.exists(), "auto must keep the floor for {rel}");
        let (out, _) = run(&env, write_call(&target), &["-p", "--yolo", "write it"]);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "x", "{rel}");
    }
    let env = TestEnv::new();
    let secret = env.write("home/.ssh/id_test", "synthetic-sensitive-marker\n");
    let read = || ScriptTurn::tool("c1", "read", &json!({"path": secret}).to_string());
    let (out, reqs) = run(&env, read(), &["-p", "--auto", "read it"]);
    assert!(out.status.success());
    assert!(tool_result(&reqs).contains("keys, tokens or agent state"));
    let (out, reqs) = run(&env, read(), &["-p", "--yolo", "read it"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(tool_result(&reqs).contains("synthetic-sensitive-marker"));
}
