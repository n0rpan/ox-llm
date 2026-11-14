//! E11 end to end: the real `oxa` binary as parent and as child, against the in-process fake
//! engine over HTTP. The parent's scripted reply calls `subagent`; the assertions are on the
//! requests the fake recorded: order (parent chat -> park -> child requests -> the parent's next
//! request), the child's session hint, priority and turn kind, and the parent's cache after the
//! children ran. Isolation: `oxa_fake::TestEnv`; never the real home.

use oxa_fake::{FakeOpts, FakeServer, Recorded, ScriptTurn, TestEnv};
use std::process::{Command, Output};
use std::time::Duration;

const LIMIT: Duration = Duration::from_secs(90);

fn setup(env: &TestEnv) {
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"oxa": {"subagent": {"enabled": true}}}"#,
    );
    env.write(
        "home/.oxa/agent/agents/scout.md",
        "---\nname: scout\ndescription: Looks around\ntools: read\n---\nYou are a scout.\n",
    );
}

/// Run the real binary as the parent (print mode) within [`LIMIT`].
fn oxa(env: &TestEnv, url: &str, words: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_oxa"));
    cmd.args(words)
        .env_clear()
        .envs(env.envs(url))
        .current_dir(env.work());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(cmd.output().expect("run oxa"));
    });
    rx.recv_timeout(LIMIT)
        .unwrap_or_else(|_| panic!("oxa {words:?} did not finish in {LIMIT:?}"))
}

fn header<'a>(r: &'a Recorded, name: &str) -> Option<&'a str> {
    r.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

fn declares_subagent(r: &Recorded) -> bool {
    r.tools_json.contains("\"name\":\"subagent\"")
}

fn requests(server: &FakeServer) -> Vec<Recorded> {
    let mut reqs = server.requests();
    reqs.sort_by_key(|r| r.seq);
    reqs
}

#[test]
fn single_child_parks_parent_and_runs_in_background() {
    let env = TestEnv::new();
    setup(&env);
    let server = FakeServer::start(
        vec![
            ScriptTurn::tool("c1", "subagent", r#"{"agent":"scout","task":"look around"}"#),
            ScriptTurn::text("the child found three files"),
            ScriptTurn::text("parent done"),
        ],
        FakeOpts::default(),
    )
    .expect("fake engine");
    let out = oxa(&env, &server.url(), &["-p", "delegate this"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("parent done"), "{stdout}");

    let reqs = requests(&server);
    assert_eq!(
        reqs.len(),
        4,
        "{:#?}",
        reqs.iter()
            .map(|r| (&r.session, &r.kind, r.max_tokens))
            .collect::<Vec<_>>()
    );
    let parent = reqs[0].session.clone();
    assert!(!parent.is_empty());
    let hint = format!("{parent}.s1");

    // 1. The parent's chat that calls the tool; the tool is declared.
    assert_eq!(reqs[0].kind, "chat");
    assert!(!reqs[0].background);
    assert!(declares_subagent(&reqs[0]));
    // 2. The park: H6 + H4 over the parent's wire, in the foreground.
    let park = &reqs[1];
    assert_eq!(park.session, parent);
    assert_eq!(park.max_tokens, 0);
    assert!(park.snapshot_keep, "the park pins the head (H4)");
    assert_eq!(park.kind, "prefill");
    assert!(!park.background);
    // 3. The child: its own session hint, background, turn kind `subagent`, no nested tool.
    let child = &reqs[2];
    assert_eq!(child.session, hint);
    assert_eq!(header(child, "x-session-id"), Some(hint.as_str()));
    assert_eq!(header(child, "x-oxe-session"), Some(hint.as_str()));
    assert!(child.background);
    assert_eq!(header(child, "x-oxe-priority"), Some("background"));
    assert_eq!(child.kind, "subagent");
    assert!(
        !declares_subagent(child),
        "depth 1: the child has no subagent tool"
    );
    assert!(child.system.contains("You are a scout."));
    assert!(
        child.tools_json.contains("\"name\":\"read\"") && !child.tools_json.contains("\"name\":\"bash\"")
    );
    assert_eq!(child.model, reqs[0].model, "the session's model");
    // 4. The parent's next request: its cache survived the child (cached = the park's prompt).
    let next = &reqs[3];
    assert_eq!(next.session, parent);
    let park_prompt = park.usage.expect("park usage").prompt;
    assert_eq!(next.usage.expect("usage").cached, park_prompt);
    let result = next.messages.last().unwrap();
    assert_eq!(result.content.as_deref(), Some("the child found three files"));
    // EG5: the child's engine state is cleared by its hint; the parent's is not.
    assert_eq!(server.clears(), vec![hint.clone()]);
}

#[test]
fn two_tasks_run_one_after_another() {
    let env = TestEnv::new();
    setup(&env);
    let server = FakeServer::start(
        vec![
            ScriptTurn::tool(
                "c1",
                "subagent",
                r#"{"tasks":[{"agent":"scout","task":"first"},{"agent":"scout","task":"second"}]}"#,
            ),
            ScriptTurn::text("one"),
            ScriptTurn::text("two"),
            ScriptTurn::text("parent done"),
        ],
        FakeOpts::default(),
    )
    .expect("fake engine");
    let out = oxa(&env, &server.url(), &["-p", "delegate twice"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let reqs = requests(&server);
    let parent = reqs[0].session.clone();
    let order: Vec<String> = reqs
        .iter()
        .map(|r| {
            if r.session == parent {
                if r.max_tokens == 0 {
                    "park".to_string()
                } else {
                    "parent".to_string()
                }
            } else {
                r.session.strip_prefix(&parent).unwrap_or(&r.session).to_string()
            }
        })
        .collect();
    assert_eq!(
        order,
        ["parent", "park", ".s1", ".s2", "parent"],
        "one park, children strictly in turn"
    );
    let last = reqs.last().unwrap();
    assert_eq!(
        last.messages.last().unwrap().content.as_deref(),
        Some(
            "Parallel: 2/2 succeeded\n\n### [scout] completed\n\none\n\n---\n\n### [scout] completed\n\ntwo"
        )
    );
    assert_eq!(last.usage.unwrap().cached, reqs[1].usage.unwrap().prompt);
}
