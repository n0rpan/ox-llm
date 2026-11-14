//! Regression rows of the interactive command paths that run requests outside a normal run
//! (lane compact-stuck): a manual `/compact` (with or without instructions) and a `/tree`
//! navigation must settle the session to idle, so the next prompt starts a run, and a
//! `/compact` with nothing to cut says so.

#[path = "support/tui_rig.rs"]
mod rig;
#[path = "support/tree_pty.rs"]
mod tree_pty;

use oxa_fake::{FakeOpts, ScriptTurn, TestEnv};
use rig::{replies, user_texts, Rig, RigOpts, WAIT};
use serde_json::json;

/// Settings that make a compaction cut the history (every earlier message is summarised).
fn cutting_env() -> TestEnv {
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/settings.json",
        &json!({"compaction": {"keepRecentTokens": 1}}).to_string(),
    );
    env
}

/// After a command that ran requests outside a run: the footer leaves `Prefill`, and a prompt
/// typed now is a NEW chat request whose reply is shown (not queued as steering).
fn prompt_starts_a_run(rig: &Rig, text: &str, reply: &str) {
    rig.pty
        .wait_for_named(
            "the footer to leave the Prefill state",
            |s| !s.contains("Prefill"),
            WAIT,
        )
        .unwrap_or_else(|e| panic!("session stuck after the command: {e}\n{:?}", rig.screen()));
    tree_pty::clear_editor(rig);
    let before = rig.generating().len();
    rig.type_text(text);
    rig.pty.send_key("enter");
    let reqs = rig.wait_requests(before + 1);
    assert!(
        reqs.last().is_some_and(|r| r.kind == "chat"),
        "the prompt after the command is a chat request, got {:?}",
        reqs.last().map(|r| r.kind.clone())
    );
    rig.pty.wait_for_text(reply).unwrap_or_else(|e| {
        panic!(
            "the reply to the prompt after the command is not shown: {e}\n{:?}",
            rig.screen()
        )
    });
}

/// `/compact` with nothing to cut gives one status line (`Nothing to compact`), not silence.
#[test]
fn compact_nothing_to_cut_says_so() {
    // invariant: a command that changes nothing still answers; the transcript is unchanged.
    let rig = Rig::start(Vec::new(), RigOpts::default());
    rig.slash("/compact");
    rig.pty
        .wait_for_text("Nothing to compact")
        .unwrap_or_else(|e| panic!("no feedback for an empty /compact: {e}\n{:?}", rig.screen()));
    assert_eq!(rig.generating().len(), 0, "no summary request for nothing to cut");
}

/// After a manual `/compact` the session settles to idle and the next prompt starts a run.
#[test]
fn compact_then_prompt_starts_a_run() {
    // invariant: `Prefill` is progress of a background request; the session is idle once the
    // command has settled, and a prompt typed then is a prompt, not steering.
    let rig = Rig::start_in(
        cutting_env(),
        vec![
            ScriptTurn::text("one"),
            ScriptTurn::text("two"),
            ScriptTurn::text("summary text"),
            ScriptTurn::text("three"),
        ],
        RigOpts::default(),
    );
    rig.pty.wait_for_text("ctrl+d exit").unwrap();
    rig.prompt("first", "one");
    rig.prompt("second", "two");
    rig.slash("/compact");
    rig.pty
        .wait_for_text("Compacted from")
        .unwrap_or_else(|e| panic!("no compaction block: {e}\n{:?}", rig.screen()));
    prompt_starts_a_run(&rig, "third", "three");
}

/// `/compact <instructions>` settles the same way: the instructions run as the summary request,
/// and the next prompt starts a run.
#[test]
fn compact_with_instructions_then_prompt_starts_a_run() {
    // invariant: as above; the instructions only change the summary request (W5 pure append).
    let rig = Rig::start_in(
        cutting_env(),
        vec![
            ScriptTurn::text("one"),
            ScriptTurn::text("two"),
            ScriptTurn::text("summary text"),
            ScriptTurn::text("three"),
        ],
        RigOpts::default(),
    );
    rig.pty.wait_for_text("ctrl+d exit").unwrap();
    rig.prompt("first", "one");
    rig.prompt("second", "two");
    rig.slash("/compact Focus on code changes");
    rig.pty
        .wait_for_text("Compacted from")
        .unwrap_or_else(|e| panic!("no compaction block: {e}\n{:?}", rig.screen()));
    prompt_starts_a_run(&rig, "third", "three");
}

/// A `/tree` navigation (no summary) runs an idle prefill of the new branch; the session must
/// settle to idle afterwards, so the next prompt starts a run.
#[test]
fn navigate_then_prompt_starts_a_run() {
    // invariant: as above, for the navigation path (`navigate` -> idle prefill -> `run_queued`).
    let rig = Rig::start_in(
        cutting_env(),
        vec![
            ScriptTurn::text("one"),
            ScriptTurn::text("two"),
            ScriptTurn::text("three"),
        ],
        RigOpts::default(),
    );
    rig.pty.wait_for_text("ctrl+d exit").unwrap();
    rig.prompt("first", "one");
    rig.prompt("second", "two");
    tree_pty::open_tree(&rig);
    tree_pty::pick(&rig, "first", Some(0));
    tree_pty::navigated(&rig, |_| true);
    prompt_starts_a_run(&rig, "third", "three");
}

/// A prompt typed while a run is active is steering: it is delivered to the engine in the
/// run's next request (not started as a second run), and its reply is shown. Guards the
/// settle rule against marking a running run idle.
#[test]
fn steer_during_a_run_is_delivered() {
    // invariant: `AgentSettled` ends only the run it belongs to; while a run is active the
    // footer shows the queued steering message, and it reaches the model in that same run.
    let rig = Rig::start(
        replies(4),
        RigOpts {
            fake: FakeOpts {
                done_gap_ms: 2500,
                ..FakeOpts::default()
            },
            ..RigOpts::default()
        },
    );
    rig.type_text("long task");
    rig.pty.send_key("enter");
    rig.pty.wait_for_text("reply 1").unwrap();
    rig.type_text("steer this");
    rig.pty.send_key("enter");
    rig.pty.wait_for_text("Steering: steer this").unwrap();
    let reqs = rig.wait_requests(2);
    assert!(
        reqs.iter()
            .any(|r| user_texts(r).iter().any(|t| t.contains("steer this"))),
        "the steering message is sent to the model"
    );
}
