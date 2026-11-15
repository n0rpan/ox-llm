//! Continued branches merge the base before the real child agent starts.
#[path = "common/work.rs"]
mod work;
use oxa_fake::{ScriptTurn, TestEnv};
use std::path::{Path, PathBuf};
use work::{git, run, run_observed, setup};

fn project(env: &TestEnv) -> PathBuf {
    env.work().join("project")
}
fn commit_file(env: &TestEnv, file: &str, content: &str) {
    env.write(&format!("work/project/{file}"), content);
    git(env, &project(env), &["add", file]);
    git(env, &project(env), &["commit", "-m", "fixture update"]);
}
fn diverge(env: &TestEnv, conflict: bool) {
    setup(env, "[]");
    commit_file(env, "shared.txt", "original\n");
    git(env, &project(env), &["checkout", "-b", "agent/test"]);
    commit_file(env, "shared.txt", "task version\n");
    git(env, &project(env), &["checkout", "main"]);
    commit_file(
        env,
        if conflict { "shared.txt" } else { "base.txt" },
        "base version\n",
    );
}
fn tree(record: &serde_json::Value) -> &Path {
    Path::new(record["worktree"].as_str().unwrap())
}
fn merge_head(env: &TestEnv, root: &Path) -> PathBuf {
    PathBuf::from(git(
        env,
        root,
        &["rev-parse", "--path-format=absolute", "--git-path", "MERGE_HEAD"],
    ))
}
fn resolve_turns() -> Vec<ScriptTurn> {
    vec![
        ScriptTurn::tool(
            "resolve",
            "write",
            r#"{"path":"shared.txt","content":"resolved\n"}"#,
        ),
        ScriptTurn::tool(
            "commit",
            "bash",
            r#"{"command":"git add shared.txt && git commit --no-edit"}"#,
        ),
        ScriptTurn::text("merge finished"),
    ]
}
fn trust(env: &TestEnv) {
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"defaultProjectTrust":"always"}"#,
    );
}
#[test]
fn existing_branch_fast_forwards_to_base_before_agent() {
    let env = TestEnv::new();
    setup(&env, "[]");
    git(&env, &project(&env), &["branch", "agent/test"]);
    commit_file(&env, "base.txt", "from main\n");
    let record = run(&env, vec![ScriptTurn::text("nothing")]);
    assert_eq!(record["outcome"], "nothing_to_do", "{record}");
    assert_eq!(
        std::fs::read_to_string(tree(&record).join("base.txt")).unwrap(),
        "from main\n"
    );
    git(
        &env,
        &project(&env),
        &["merge-base", "--is-ancestor", "main", "agent/test"],
    );
}
#[test]
fn divergent_branch_gets_normal_merge_without_losing_task_commit() {
    let env = TestEnv::new();
    diverge(&env, false);
    let record = run(&env, vec![ScriptTurn::text("nothing")]);
    assert_eq!(record["outcome"], "nothing_to_do", "{record}");
    assert_eq!(
        git(&env, tree(&record), &["rev-list", "--parents", "-n", "1", "HEAD"])
            .split_whitespace()
            .count(),
        3
    );
    assert_eq!(
        std::fs::read_to_string(tree(&record).join("shared.txt")).unwrap(),
        "task version\n"
    );
    assert!(tree(&record).join("base.txt").exists());
}
#[test]
fn conflicts_reach_agent_with_merge_in_progress_and_stay_blocked_if_unresolved() {
    let env = TestEnv::new();
    diverge(&env, true);
    let (record, requests) = run_observed(
        &env,
        vec![ScriptTurn::text("cannot resolve")],
        &["work", "../tasks.json"],
        None,
    );
    assert_eq!(record["outcome"], "blocked", "{record}");
    assert!(record["detail"].as_str().unwrap().contains("merge"));
    assert!(merge_head(&env, tree(&record)).exists());
    assert!(std::fs::read_to_string(tree(&record).join("shared.txt"))
        .unwrap()
        .contains("<<<<<<<"));
    let user = requests
        .iter()
        .flat_map(|r| &r.messages)
        .filter_map(|m| m.content.as_deref())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        user.contains("Finish task") && user.contains("merge in progress") && user.contains("shared.txt"),
        "{user}"
    );
}
#[test]
fn agent_can_resolve_and_commit_merge_inside_real_sandbox() {
    let env = TestEnv::new();
    diverge(&env, true);
    trust(&env);
    let record = run(&env, resolve_turns());
    assert_eq!(record["outcome"], "done", "{record}");
    assert!(!merge_head(&env, tree(&record)).exists());
    assert_eq!(
        std::fs::read_to_string(tree(&record).join("shared.txt")).unwrap(),
        "resolved\n"
    );
    assert_eq!(
        std::fs::read_to_string(project(&env).join("shared.txt")).unwrap(),
        "base version\n"
    );
}
#[test]
fn unresolved_merge_can_be_continued_without_resetting_it() {
    let env = TestEnv::new();
    diverge(&env, true);
    trust(&env);
    let first = run(&env, vec![ScriptTurn::text("blocked")]);
    assert_eq!(first["outcome"], "blocked", "{first}");
    let second = run(&env, resolve_turns());
    assert_eq!(second["outcome"], "done", "{second}");
    assert!(!merge_head(&env, tree(&second)).exists());
}
#[test]
fn completed_work_is_saved_and_not_replayed() {
    let env = TestEnv::new();
    setup(&env, "[]");
    trust(&env);
    let first = run(
        &env,
        vec![
            ScriptTurn::tool("write", "write", r#"{"path":"notes.txt","content":"keep me"}"#),
            ScriptTurn::text("done"),
        ],
    );
    assert_eq!(first["outcome"], "done", "{first}");
    let (record, requests) = run_observed(&env, vec![], &["work", "../tasks.json"], None);
    assert_eq!(record["outcome"], "done", "{record}");
    assert!(requests.is_empty());
    assert!(git(&env, tree(&record), &["status", "--porcelain"]).is_empty());
    assert_eq!(
        std::fs::read_to_string(tree(&record).join("notes.txt")).unwrap(),
        "keep me"
    );
}
#[test]
fn continued_branch_cannot_replace_base_acceptance_commands() {
    let env = TestEnv::new();
    setup(&env, r#"["echo base-acceptance; exit 1"]"#);
    git(&env, &project(&env), &["checkout", "-b", "agent/test"]);
    commit_file(&env, ".oxa/settings.json", r#"{"done":[]}"#);
    git(&env, &project(&env), &["checkout", "main"]);
    let record = run(
        &env,
        vec![ScriptTurn::text("done"), ScriptTurn::text("still done")],
    );
    assert_eq!(record["outcome"], "red", "{record}");
    assert!(record["detail"].as_str().unwrap().contains("base-acceptance"));
}

#[test]
fn aborting_merge_cannot_report_green() {
    let env = TestEnv::new();
    diverge(&env, true);
    trust(&env);
    let record = run(
        &env,
        vec![
            ScriptTurn::tool("abort", "bash", r#"{"command":"git merge --abort"}"#),
            ScriptTurn::text("done"),
        ],
    );
    assert_eq!(record["outcome"], "blocked", "{record}");
    assert!(record["detail"].as_str().unwrap().contains("base is not merged"));
}
#[test]
fn changing_branch_cannot_report_green_for_task_branch() {
    let env = TestEnv::new();
    setup(&env, "[]");
    trust(&env);
    let record = run(
        &env,
        vec![
            ScriptTurn::tool("switch", "bash", r#"{"command":"git checkout -b alternate"}"#),
            ScriptTurn::text("done"),
        ],
    );
    assert_eq!(record["outcome"], "blocked", "{record}");
    assert!(record["detail"].as_str().unwrap().contains("different branch"));
}

#[test]
fn resumed_old_merge_is_told_to_integrate_the_newly_captured_base() {
    let env = TestEnv::new();
    diverge(&env, true);
    trust(&env);
    let first = run(&env, vec![ScriptTurn::text("blocked")]);
    assert_eq!(first["outcome"], "blocked", "{first}");
    let old_base = git(&env, &project(&env), &["rev-parse", "main"]);
    commit_file(&env, "new-base.txt", "new base commit\n");
    let new_base = git(&env, &project(&env), &["rev-parse", "main"]);
    let mut turns = resolve_turns();
    turns.insert(
        2,
        ScriptTurn::tool(
            "latest",
            "bash",
            &serde_json::json!({"command": format!("git merge --no-edit {new_base}")}).to_string(),
        ),
    );
    let (record, requests) = run_observed(&env, turns, &["work", "../tasks.json"], None);
    assert_eq!(record["outcome"], "done", "{record}");
    let messages = requests
        .iter()
        .flat_map(|r| &r.messages)
        .filter_map(|m| m.content.as_deref())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        messages.contains(&old_base) && messages.contains(&format!("git merge --no-edit {new_base}")),
        "{messages}"
    );
    assert!(tree(&record).join("new-base.txt").exists());
    git(
        &env,
        &project(&env),
        &["merge-base", "--is-ancestor", &new_base, "agent/test"],
    );
}
