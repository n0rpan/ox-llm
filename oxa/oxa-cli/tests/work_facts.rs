//! Facts through the real runner CLI, without reading private task-state files.
#[path = "common/work.rs"]
mod work;
use oxa_fake::{ScriptTurn, TestEnv};
use oxa_proto::work::{EventPage, Fact, Snapshot};
use std::process::Command;
use work::{run, run_observed, setup};

fn read(env: &TestEnv, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_oxa"))
        .args(args)
        .current_dir(env.work().join("project"))
        .env_clear()
        .envs(env.envs("http://127.0.0.1:1"))
        .output()
        .unwrap()
}
fn situation(env: &TestEnv) -> Snapshot {
    let output = read(env, &["work", "situation", "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Snapshot::from_json(&output.stdout).unwrap()
}
fn events(env: &TestEnv, after: u64) -> EventPage {
    let output = read(env, &["work", "events", "--after", &after.to_string(), "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    EventPage::from_json(&output.stdout).unwrap()
}

#[test]
fn real_work_records_contiguous_events_but_never_claims_saved_work_was_landed() {
    let env = TestEnv::new();
    setup(&env, "[]");
    let first = run(&env, vec![ScriptTurn::text("done")]);
    assert_eq!(first["outcome"], "nothing_to_do", "{first}");
    let snapshot = situation(&env);
    assert!(snapshot.running.is_empty() && snapshot.blocked.is_empty());
    assert!(snapshot.local_accepted.is_empty() && snapshot.landed.is_empty());
    let page = events(&env, 0);
    assert_eq!(page.next, snapshot.as_of_seq);
    assert_eq!(
        page.events
            .iter()
            .filter(|event| matches!(event.fact, Fact::WorkerFinished(_)))
            .count(),
        1
    );
    assert!(page
        .events
        .iter()
        .any(|event| matches!(event.fact, Fact::TaskStarted(_))));
    for (index, event) in page.events.iter().enumerate() {
        assert_eq!(event.seq, index as u64 + 1);
    }
    let later = events(&env, 1);
    assert_eq!(later.events.first().unwrap().seq, 2);
    let (cached, requests) = run_observed(&env, vec![], &["work", "../tasks.json"], None);
    assert_eq!(cached, first);
    assert!(requests.is_empty());
    assert_eq!(events(&env, page.next).events.len(), 0);
    let text = read(&env, &["work", "situation"]);
    assert!(text.status.success());
    assert!(String::from_utf8_lossy(&text.stdout).contains("as of event"));
}

#[test]
fn red_work_is_visible_as_a_block_and_its_terminal_transition_is_a_separate_event() {
    let env = TestEnv::new();
    setup(&env, r#"["exit 1"]"#);
    let record = run(
        &env,
        vec![ScriptTurn::text("done"), ScriptTurn::text("cannot repair")],
    );
    assert_eq!(record["outcome"], "red", "{record}");
    let snapshot = situation(&env);
    assert!(snapshot.running.is_empty() && snapshot.landed.is_empty());
    assert_eq!(snapshot.blocked.len(), 1);
    assert_eq!(snapshot.blocked[0].code, "checks_failed");
    let page = events(&env, 0);
    let blocked = page
        .events
        .iter()
        .position(|event| matches!(event.fact, Fact::Blocked(_)))
        .unwrap();
    assert!(matches!(page.events[blocked + 1].fact, Fact::TaskTransition(_)));
    assert_ne!(
        page.events[blocked].checkpoint.revision,
        page.events[blocked + 1].checkpoint.revision
    );
}

#[test]
fn facts_reads_refuse_missing_history_without_initializing_an_empty_situation() {
    let env = TestEnv::new();
    setup(&env, "[]");
    for args in [
        vec!["work", "situation", "--json"],
        vec!["work", "events", "--after", "0", "--json"],
    ] {
        let output = read(&env, &args);
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
    }
    assert!(!env.agent_dir().join("work").exists());
}

#[test]
fn owner_storage_cannot_be_placed_in_a_worktree_or_bare_git_metadata() {
    use std::fs;
    let env = TestEnv::new();
    setup(&env, "[]");
    for agent in [
        env.work().join("project/owner-state"),
        env.work().join("project/.git/owner-state"),
        env.work().join("remote.git/owner-state"),
        env.work().join(".oxa-worktrees/project/later/owner-state"),
    ] {
        fs::create_dir_all(&agent).unwrap();
        fs::write(agent.join("settings.json"), r#"{"defaultProjectTrust":"always"}"#).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_oxa"))
            .args(["work", "../tasks.json"])
            .current_dir(env.work().join("project"))
            .env_clear()
            .envs(env.envs("http://127.0.0.1:1"))
            .env("OXA_AGENT_DIR", &agent)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert!(!agent.join("work").exists());
    }
}
