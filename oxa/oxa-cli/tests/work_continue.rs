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
fn fresh_worktree_starts_at_the_published_task_branch_head() {
    let env = TestEnv::new();
    setup(&env, "[]");
    git(&env, &project(&env), &["checkout", "-b", "agent/test"]);
    commit_file(&env, "pr.txt", "from the pull request\n");
    git(&env, &project(&env), &["push", "origin", "agent/test"]);
    git(&env, &project(&env), &["checkout", "main"]);
    git(&env, &project(&env), &["branch", "-D", "agent/test"]);
    git(&env, &project(&env), &["fetch", "origin"]);
    let pushed = git(&env, &project(&env), &["rev-parse", "origin/agent/test"]);
    let record = run(&env, vec![ScriptTurn::text("nothing")]);
    assert_eq!(record["outcome"], "nothing_to_do", "{record}");
    assert_eq!(
        std::fs::read_to_string(tree(&record).join("pr.txt")).unwrap(),
        "from the pull request\n"
    );
    assert_eq!(git(&env, &project(&env), &["rev-parse", "agent/test"]), pushed);
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
fn dirty_retained_worktree_is_blocked_without_dropping_edits() {
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
    std::fs::write(tree(&first).join("notes.txt"), "new retained edits").unwrap();
    let (record, requests) = run_observed(&env, vec![], &["work", "../tasks.json", "--rerun"], None);
    assert_eq!(record["outcome"], "blocked", "{record}");
    assert!(requests.is_empty());
    assert_eq!(
        std::fs::read_to_string(tree(&record).join("notes.txt")).unwrap(),
        "new retained edits"
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

#[test]
fn failed_worker_and_unfinished_merge_keep_both_causes() {
    let env = TestEnv::new();
    diverge(&env, true);
    env.write(
        "work/tasks.json",
        r#"[{"id":"test","branch":"agent/test","model":"missing-model","prompt":"Finish task"}]"#,
    );
    let record = run(&env, vec![]);
    assert_eq!(record["outcome"], "blocked", "{record}");
    let detail = record["detail"].as_str().unwrap();
    assert!(detail.contains("missing-model"), "{detail}");
    assert!(detail.contains("merge remains in progress"), "{detail}");
    assert_eq!(record["worker"]["outcome"], "failed", "{record}");
}

#[test]
fn retained_task_alias_cannot_reuse_the_source_checkout() {
    use std::os::unix::fs::symlink;
    let env = TestEnv::new();
    setup(&env, "[]");
    git(&env, &project(&env), &["checkout", "-b", "agent/test"]);
    let trees = env.work().join(".oxa-worktrees/project");
    std::fs::create_dir_all(&trees).unwrap();
    symlink(project(&env), trees.join("test")).unwrap();
    let (record, requests) = run_observed(
        &env,
        vec![
            ScriptTurn::tool(
                "write",
                "write",
                r#"{"path":"source-escape","content":"escaped"}"#,
            ),
            ScriptTurn::text("done"),
        ],
        &["work", "../tasks.json"],
        None,
    );
    assert_eq!(record["outcome"], "blocked", "{record}");
    assert!(requests.is_empty());
    assert!(!project(&env).join("source-escape").exists());
}

#[test]
fn automatic_merge_hooks_stay_inside_the_sandbox() {
    use std::os::unix::fs::PermissionsExt;
    let env = TestEnv::new();
    diverge(&env, false);
    let hook = env.write(
        "work/project/.git/hooks/post-merge",
        "#!/bin/sh\nprintf escaped > \"$HOME/continuation-hook-escape\" 2>/dev/null || true\nprintf ran > post-merge-ran\n",
    );
    std::fs::set_permissions(hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    let record = run(&env, vec![ScriptTurn::text("nothing")]);
    assert!(!env.home().join("continuation-hook-escape").exists(), "{record}");
    assert_eq!(record["outcome"], "done", "{record}");
    assert_eq!(
        std::fs::read_to_string(tree(&record).join("post-merge-ran")).unwrap(),
        "ran"
    );
    assert!(!project(&env).join("post-merge-ran").exists());
}

#[test]
fn post_worker_merge_verification_does_not_run_clean_filters_on_the_host() {
    let env = TestEnv::new();
    setup(&env, "[]");
    trust(&env);
    env.write("work/project/.gitattributes", "*.data filter=probe\n");
    env.write("work/project/fixture.data", "before\n");
    env.write("work/project/filter.sh", "cat\n");
    git(&env, &project(&env), &["add", "."]);
    git(&env, &project(&env), &["commit", "-m", "filter fixture"]);
    git(
        &env,
        &project(&env),
        &["config", "filter.probe.clean", "sh filter.sh"],
    );
    git(&env, &project(&env), &["config", "filter.probe.smudge", "cat"]);
    git(&env, &project(&env), &["config", "filter.probe.required", "true"]);
    let record = run(&env, vec![
        ScriptTurn::tool("filter", "write", &serde_json::json!({"path":"filter.sh", "content":"printf escaped > \"$HOME/continuation-filter-escape\" 2>/dev/null || true\ncat\n"}).to_string()),
        ScriptTurn::tool("payload", "write", r#"{"path":"fixture.data","content":"after\n"}"#),
        ScriptTurn::text("done"),
    ]);
    assert!(
        !env.home().join("continuation-filter-escape").exists(),
        "{record}"
    );
    assert_eq!(record["outcome"], "done", "{record}");
}

#[test]
fn large_conflicted_merge_has_a_bounded_worker_handoff() {
    let env = TestEnv::new();
    setup(&env, "[]");
    let names: Vec<_> = (0..300)
        .map(|i| format!("conflict-{i:03}-{}-file.txt", "x".repeat(80)))
        .collect();
    for name in &names {
        env.write(&format!("work/project/{name}"), "original\n");
    }
    git(&env, &project(&env), &["add", "."]);
    git(&env, &project(&env), &["commit", "-m", "many shared paths"]);
    git(&env, &project(&env), &["checkout", "-b", "agent/test"]);
    for name in &names {
        env.write(&format!("work/project/{name}"), "task\n");
    }
    git(&env, &project(&env), &["add", "."]);
    git(&env, &project(&env), &["commit", "-m", "task versions"]);
    git(&env, &project(&env), &["checkout", "main"]);
    for name in &names {
        env.write(&format!("work/project/{name}"), "base\n");
    }
    git(&env, &project(&env), &["add", "."]);
    git(&env, &project(&env), &["commit", "-m", "base versions"]);
    let (record, requests) = run_observed(
        &env,
        vec![ScriptTurn::text("blocked")],
        &["work", "../tasks.json"],
        None,
    );
    assert_eq!(record["outcome"], "blocked", "{record}");
    let prompt = requests
        .iter()
        .flat_map(|r| &r.messages)
        .filter_map(|m| m.content.as_deref())
        .find(|text| text.contains("There is a merge in progress"))
        .expect("merge handoff reached worker");
    assert!(
        prompt.len() <= 16 * 1024,
        "merge handoff retained {} bytes",
        prompt.len()
    );
}
