//! Continued branches merge the base before the real child agent starts.
#[path = "common/work.rs"]
mod work;
use oxa_fake::{ScriptTurn, TestEnv};
use std::path::{Path, PathBuf};
use work::{git, run, run_batch, run_observed, setup};

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

fn published_task(env: &TestEnv) -> String {
    setup(env, "[]");
    publish_prepared_task(env)
}

fn publish_prepared_task(env: &TestEnv) -> String {
    git(env, &project(env), &["checkout", "-b", "agent/test"]);
    commit_file(env, "pr.txt", "from the pull request\n");
    git(env, &project(env), &["push", "origin", "agent/test"]);
    git(env, &project(env), &["checkout", "main"]);
    git(env, &project(env), &["branch", "-D", "agent/test"]);
    git(env, &project(env), &["fetch", "origin"]);
    git(env, &project(env), &["rev-parse", "origin/agent/test"])
}

fn synthetic_github_path(env: &TestEnv) -> String {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let tools = env.tmp().join("trusted-tools");
    std::fs::DirBuilder::new().mode(0o700).create(&tools).unwrap();
    let gh = env.write("tmp/trusted-tools/gh", "#!/bin/sh\ntest \"$*\" = 'auth token --hostname github.com' || exit 9\nprintf looked > \"$0.called\"\nprintf '%s\\n' SYNTH_CHECKOUT_CREDENTIAL\n");
    std::fs::set_permissions(gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    format!("{}:{}", tools.display(), std::env::var("PATH").unwrap())
}

#[test]
fn fresh_worktree_starts_at_the_published_task_branch_head() {
    let env = TestEnv::new();
    let pushed = published_task(&env);
    let record = run(&env, vec![ScriptTurn::text("nothing")]);
    assert_eq!(record["outcome"], "nothing_to_do", "{record}");
    assert_eq!(
        std::fs::read_to_string(tree(&record).join("pr.txt")).unwrap(),
        "from the pull request\n"
    );
    assert_eq!(git(&env, &project(&env), &["rev-parse", "agent/test"]), pushed);
    assert_eq!(git(&env, tree(&record), &["rev-parse", "HEAD"]), pushed);
    assert_eq!(
        git(&env, tree(&record), &["symbolic-ref", "--quiet", "HEAD"]),
        "refs/heads/agent/test"
    );
    // The source checkout stays where it was; only the task worktree moves.
    assert_eq!(
        git(&env, &project(&env), &["symbolic-ref", "--quiet", "HEAD"]),
        "refs/heads/main"
    );
}

#[test]
fn ordinary_fresh_branch_reference_hooks_run_only_in_the_task_sandbox() {
    use std::os::unix::fs::PermissionsExt;
    let env = TestEnv::new();
    setup(&env, "[]");
    let pushed = git(&env, &project(&env), &["rev-parse", "main"]);
    let hook = env.write(
        "work/project/.git/hooks/reference-transaction",
        "#!/bin/sh\ncat >/dev/null\nprintf escaped > \"$HOME/published-head-host-hook\" 2>/dev/null || true\nprintf ran > published-head-hook-ran\n",
    );
    std::fs::set_permissions(hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    let record = run(&env, vec![ScriptTurn::text("nothing")]);
    assert!(!env.home().join("published-head-host-hook").exists(), "{record}");
    assert!(
        !project(&env).join("published-head-hook-ran").exists(),
        "{record}"
    );
    assert!(tree(&record).join("published-head-hook-ran").exists(), "{record}");
    assert_eq!(record["outcome"], "done", "{record}");
    git(
        &env,
        tree(&record),
        &["merge-base", "--is-ancestor", &pushed, "HEAD"],
    );
}

#[test]
fn published_checkout_hooks_have_neither_network_nor_github_credentials() {
    published_checkout_policy(false);
}

#[test]
fn normalizing_an_adopted_checkout_preserves_the_restricted_policy() {
    published_checkout_policy(true);
}

fn published_checkout_policy(normalize: bool) {
    use std::os::unix::fs::PermissionsExt;
    let env = TestEnv::new();
    published_task(&env);
    git(
        &env,
        &project(&env),
        &["checkout", "-b", "published", "origin/agent/test"],
    );
    commit_file(&env, "checkout-policy.sh", "printf ran > adopted-hook-ran\nif test -n \"${GH_TOKEN-}\"; then printf exposed > adopted-hook-credentials; fi\nreadlink /proc/self/ns/net > adopted-hook-network\n");
    env.write("work/project/.gitattributes", "*.payload filter=adopted\n");
    env.write("work/project/fixture.payload", "synthetic payload\n");
    git(
        &env,
        &project(&env),
        &["add", ".gitattributes", "fixture.payload"],
    );
    git(&env, &project(&env), &["commit", "-m", "fixture filter input"]);
    git(&env, &project(&env), &["push", "origin", "published:agent/test"]);
    git(&env, &project(&env), &["checkout", "main"]);
    git(&env, &project(&env), &["branch", "-D", "published"]);
    git(&env, &project(&env), &["fetch", "origin"]);
    let normalized_base = normalize.then(|| {
        let (_, fetched) = lag_existing_base(&env);
        git(
            &env,
            &project(&env),
            &["checkout", "-b", "published", "origin/agent/test"],
        );
        git(&env, &project(&env), &["merge", "--no-edit", "origin/main"]);
        git(&env, &project(&env), &["push", "origin", "published:agent/test"]);
        git(&env, &project(&env), &["checkout", "source"]);
        git(&env, &project(&env), &["branch", "-D", "published"]);
        git(&env, &project(&env), &["fetch", "origin"]);
        fetched
    });
    git(&env, &project(&env), &["config", "filter.adopted.smudge", "printf ran > adopted-filter-ran; if test -n \"${GH_TOKEN-}${GITHUB_TOKEN-}\"; then printf exposed > adopted-filter-credentials; fi; if test -r \"$HOME/.config/gh/config.yml\"; then printf exposed > adopted-filter-config; fi; readlink /proc/self/ns/net > adopted-filter-network; cat"]);
    env.write("home/.config/gh/config.yml", "version: 1\n");
    let hook = env.write(
        "work/project/.git/hooks/post-checkout",
        "#!/bin/sh\nif test -f checkout-policy.sh; then sh checkout-policy.sh; fi\n",
    );
    std::fs::set_permissions(hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = synthetic_github_path(&env);
    // Only this fixture gh is reachable first, under a throwaway HOME. No real login is read.
    let (record, _) = work::run_observed_with(
        &env,
        vec![ScriptTurn::text("nothing")],
        &["work", "../tasks.json"],
        Some(path),
        |command| {
            command.envs([
                ("OXA_TEST_ISOLATION", "0"),
                ("GH_TOKEN", "SYNTH_AMBIENT_CREDENTIAL"),
                ("GITHUB_TOKEN", "SYNTH_AMBIENT_CREDENTIAL"),
            ]);
        },
    );
    assert_eq!(record["outcome"], "done", "{record}");
    if let Some(fetched) = normalized_base {
        assert_eq!(record["base_sha"], fetched, "{record}");
        assert_eq!(git(&env, &project(&env), &["rev-parse", "main"]), fetched);
    }
    assert!(!tree(&record).join("adopted-hook-ran").exists(), "{record}");
    assert!(
        !tree(&record).join("adopted-hook-credentials").exists(),
        "checkout exposed credentials: {record}"
    );
    assert!(!tree(&record).join("adopted-hook-network").exists(), "{record}");
    assert!(tree(&record).join("adopted-filter-ran").exists(), "{record}");
    assert!(
        !tree(&record).join("adopted-filter-credentials").exists(),
        "{record}"
    );
    assert!(!tree(&record).join("adopted-filter-config").exists(), "{record}");
    let hook_net = std::fs::read_to_string(tree(&record).join("adopted-filter-network")).unwrap();
    assert_ne!(
        hook_net.trim(),
        std::fs::read_link("/proc/self/ns/net").unwrap().to_str().unwrap(),
        "checkout retained host networking"
    );
}

#[test]
fn adopted_save_callbacks_have_no_ambient_authority() {
    use std::os::unix::fs::PermissionsExt;
    let env = TestEnv::new();
    published_task(&env);
    git(
        &env,
        &project(&env),
        &["checkout", "-b", "published", "origin/agent/test"],
    );
    env.write("work/project/save-callback.sh", "printf ran > adopted-save-ran\nif test -n \"${GH_TOKEN-}${GITHUB_TOKEN-}\"; then printf exposed > adopted-save-credentials; fi\nif test -r \"$HOME/.config/gh/config.yml\"; then printf exposed > adopted-save-config; fi\nreadlink /proc/self/ns/net > adopted-save-network\n");
    env.write("work/project/.gitignore", "/adopted-save-*\n");
    git(&env, &project(&env), &["add", "save-callback.sh", ".gitignore"]);
    git(&env, &project(&env), &["commit", "-m", "fixture saved hook"]);
    git(&env, &project(&env), &["push", "origin", "published:agent/test"]);
    git(&env, &project(&env), &["checkout", "main"]);
    git(&env, &project(&env), &["branch", "-D", "published"]);
    git(&env, &project(&env), &["fetch", "origin"]);
    // A repository-installed callback can invoke files from the adopted head.
    let hook = env.write(
        "work/project/.git/hooks/pre-commit",
        "#!/bin/sh\nexec sh save-callback.sh\n",
    );
    std::fs::set_permissions(hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    env.write("home/.config/gh/config.yml", "version: 1\n");
    let (record, _) = work::run_observed_with(
        &env,
        vec![
            ScriptTurn::tool(
                "edit",
                "write",
                r#"{"path":"changed.txt","content":"synthetic change\n"}"#,
            ),
            ScriptTurn::text("done"),
        ],
        &["work", "../tasks.json"],
        Some(synthetic_github_path(&env)),
        |command| {
            command.envs([
                ("OXA_TEST_ISOLATION", "0"),
                ("GH_TOKEN", "SYNTH_AMBIENT_CREDENTIAL"),
                ("GITHUB_TOKEN", "SYNTH_AMBIENT_CREDENTIAL"),
            ]);
        },
    );
    assert_eq!(record["outcome"], "done", "{record}");
    assert_eq!(
        std::fs::read_to_string(tree(&record).join("changed.txt")).unwrap(),
        "synthetic change\n"
    );
    assert!(tree(&record).join("adopted-save-ran").exists(), "{record}");
    assert!(
        !tree(&record).join("adopted-save-credentials").exists(),
        "save exposed credentials: {record}"
    );
    assert!(!tree(&record).join("adopted-save-config").exists(), "{record}");
    let hook_net = std::fs::read_to_string(tree(&record).join("adopted-save-network")).unwrap();
    assert_ne!(
        hook_net.trim(),
        std::fs::read_link("/proc/self/ns/net").unwrap().to_str().unwrap()
    );
    assert_eq!(git(&env, tree(&record), &["status", "--porcelain"]), "");
}

#[test]
fn adopted_acceptance_has_no_github_authority_and_preserves_network_policy() {
    for offline in [false, true] {
        let env = TestEnv::new();
        setup(
            &env,
            r#"["test -f answer.txt && git diff --check && sh acceptance.sh"]"#,
        );
        env.write("work/project/acceptance.sh", "true\n");
        env.write("work/project/.gitignore", "/adopted-acceptance-*\n");
        git(&env, &project(&env), &["add", "acceptance.sh", ".gitignore"]);
        git(&env, &project(&env), &["commit", "-m", "fixture acceptance"]);
        publish_prepared_task(&env);
        git(
            &env,
            &project(&env),
            &["checkout", "-b", "published", "origin/agent/test"],
        );
        env.write("work/project/acceptance.sh", "printf ran > adopted-acceptance-ran\nif test -n \"${GH_TOKEN-}${GITHUB_TOKEN-}\"; then printf exposed > adopted-acceptance-credentials; fi\nreadlink /proc/self/ns/net > adopted-acceptance-network\n");
        git(&env, &project(&env), &["add", "acceptance.sh"]);
        git(
            &env,
            &project(&env),
            &["commit", "-m", "fixture published acceptance"],
        );
        git(&env, &project(&env), &["push", "origin", "published:agent/test"]);
        git(&env, &project(&env), &["checkout", "main"]);
        git(&env, &project(&env), &["branch", "-D", "published"]);
        git(&env, &project(&env), &["fetch", "origin"]);
        let args = if offline {
            vec!["--offline", "work", "../tasks.json"]
        } else {
            vec!["work", "../tasks.json"]
        };
        let (record, _) = work::run_observed_with(
            &env,
            vec![
                ScriptTurn::tool(
                    "edit",
                    "write",
                    r#"{"path":"answer.txt","content":"synthetic change\n"}"#,
                ),
                ScriptTurn::text("done"),
            ],
            &args,
            Some(synthetic_github_path(&env)),
            |command| {
                command.envs([
                    ("OXA_TEST_ISOLATION", "0"),
                    ("GH_TOKEN", "SYNTH_AMBIENT_CREDENTIAL"),
                    ("GITHUB_TOKEN", "SYNTH_AMBIENT_CREDENTIAL"),
                ]);
            },
        );
        assert_eq!(record["outcome"], "done", "{record}");
        assert!(tree(&record).join("adopted-acceptance-ran").exists(), "{record}");
        assert!(
            !tree(&record).join("adopted-acceptance-credentials").exists(),
            "{record}"
        );
        assert!(!env.tmp().join("trusted-tools/gh.called").exists(), "{record}");
        let network = std::fs::read_to_string(tree(&record).join("adopted-acceptance-network")).unwrap();
        let host = std::fs::read_link("/proc/self/ns/net").unwrap();
        assert_eq!(network.trim() == host.to_str().unwrap(), !offline, "{record}");
    }
}

#[test]
fn a_divergent_matching_remote_tip_prevents_published_head_adoption() {
    let env = TestEnv::new();
    published_task(&env);
    let base = git(&env, &project(&env), &["rev-parse", "main"]);
    git(&env, &project(&env), &["checkout", "--orphan", "other"]);
    commit_file(&env, "unrelated.txt", "another published history\n");
    let other = git(&env, &project(&env), &["rev-parse", "HEAD"]);
    git(&env, &project(&env), &["checkout", "main"]);
    git(&env, &project(&env), &["remote", "add", "other", "../remote.git"]);
    git(
        &env,
        &project(&env),
        &["update-ref", "refs/remotes/other/agent/test", &other],
    );
    let record = run(&env, vec![ScriptTurn::text("nothing")]);
    assert_eq!(record["outcome"], "nothing_to_do", "{record}");
    assert_eq!(git(&env, tree(&record), &["rev-parse", "HEAD"]), base);
    assert!(!tree(&record).join("pr.txt").exists());
}

#[test]
fn identical_matching_remote_tips_keep_the_published_head_eligible() {
    let env = TestEnv::new();
    let pushed = published_task(&env);
    git(
        &env,
        &project(&env),
        &["update-ref", "refs/remotes/mirror/agent/test", &pushed],
    );
    let record = run(&env, vec![ScriptTurn::text("nothing")]);
    assert_eq!(record["outcome"], "nothing_to_do", "{record}");
    assert_eq!(git(&env, tree(&record), &["rev-parse", "HEAD"]), pushed);
}

#[test]
fn ordinary_branch_creation_keeps_required_reference_hook_policy() {
    use std::os::unix::fs::PermissionsExt;
    let env = TestEnv::new();
    setup(&env, "[]");
    let hook = env.write("work/project/.git/hooks/reference-transaction", "#!/bin/sh\nwhile read old new reference; do if test \"$reference\" = refs/heads/agent/test; then echo required-published-reference-policy >&2; exit 1; fi; done\n");
    std::fs::set_permissions(hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    let (record, requests) = run_observed(&env, vec![], &["work", "../tasks.json"], None);
    assert_eq!(record["outcome"], "blocked", "{record}");
    assert!(requests.is_empty());
    assert!(record["detail"]
        .as_str()
        .unwrap()
        .contains("required-published-reference-policy"));
    assert_eq!(
        git(
            &env,
            &project(&env),
            &["for-each-ref", "--format=%(refname)", "refs/heads/agent/test"]
        ),
        ""
    );
}

#[test]
fn unrelated_published_tip_keeps_the_base() {
    let env = TestEnv::new();
    setup(&env, "[]");
    // An orphan commit shares no history with the captured base, so it is not
    // proof that the pushed branch continues this task.
    git(&env, &project(&env), &["checkout", "--orphan", "agent/test"]);
    commit_file(&env, "unrelated.txt", "from an unrelated history\n");
    git(&env, &project(&env), &["push", "origin", "agent/test"]);
    git(&env, &project(&env), &["checkout", "main"]);
    git(&env, &project(&env), &["branch", "-D", "agent/test"]);
    git(&env, &project(&env), &["fetch", "origin"]);
    let base = git(&env, &project(&env), &["rev-parse", "main"]);
    let record = run(&env, vec![ScriptTurn::text("nothing")]);
    assert_eq!(record["outcome"], "nothing_to_do", "{record}");
    assert_eq!(git(&env, tree(&record), &["rev-parse", "HEAD"]), base);
    assert!(!tree(&record).join("unrelated.txt").exists());
    assert_eq!(git(&env, &project(&env), &["rev-parse", "agent/test"]), base);
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

fn lagging_base(env: &TestEnv) -> (String, String) {
    setup(env, "[]");
    lag_existing_base(env)
}

fn lag_existing_base(env: &TestEnv) -> (String, String) {
    // The source checkout pulled `main` while its local branch stayed behind.
    let stale = git(env, &project(env), &["rev-parse", "main"]);
    commit_file(env, "fetched.txt", "fetched base\n");
    git(env, &project(env), &["push", "origin", "main"]);
    git(env, &project(env), &["reset", "--hard", "HEAD~1"]);
    // The source checkout works on its own branch, so `main` is not checked out.
    git(env, &project(env), &["checkout", "-b", "source"]);
    git(env, &project(env), &["fetch", "origin"]);
    let fetched = git(env, &project(env), &["rev-parse", "origin/main"]);
    assert_ne!(fetched, stale);
    // A base named after the fetched ref names a commit, not the stale branch.
    env.write(
        "work/tasks.json",
        r#"[{"id":"test","branch":"agent/test","model":"fake","base":"origin/main","prompt":"Finish task"}]"#,
    );
    (stale, fetched)
}

#[test]
fn fresh_worktree_fast_forwards_a_local_base_branch_that_lags_it() {
    let env = TestEnv::new();
    let (stale, fetched) = lagging_base(&env);
    let record = run(&env, vec![ScriptTurn::text("nothing")]);
    assert_eq!(record["outcome"], "nothing_to_do", "{record}");
    assert_eq!(record["base_sha"], fetched, "{record}");
    assert_ne!(record["base_sha"], stale, "{record}");
    assert_eq!(
        git(&env, &project(&env), &["rev-parse", "main"]),
        fetched,
        "the local base branch still lags the fetched base"
    );
    assert_eq!(
        git(&env, tree(&record), &["diff", "--name-only", "main..HEAD"]),
        "",
        "the task's own diff against its base is not empty"
    );
}

#[test]
fn a_tag_spelling_a_remote_base_never_advances_the_local_branch() {
    let env = TestEnv::new();
    let (stale, fetched) = lagging_base(&env);
    commit_file(&env, "tagged.txt", "an explicitly tagged base\n");
    git(&env, &project(&env), &["tag", "origin/main"]);
    let tagged = git(&env, &project(&env), &["rev-parse", "refs/tags/origin/main"]);
    assert_ne!(tagged, fetched);
    let record = run(&env, vec![ScriptTurn::text("nothing")]);
    assert_eq!(record["base_sha"], tagged, "{record}");
    assert_eq!(
        git(&env, &project(&env), &["rev-parse", "refs/heads/main"]),
        stale
    );
}

#[test]
fn a_symbolic_local_base_never_moves_its_checked_out_target() {
    let env = TestEnv::new();
    let (stale, _) = lagging_base(&env);
    git(
        &env,
        &project(&env),
        &["symbolic-ref", "refs/heads/main", "refs/heads/source"],
    );
    let record = run(&env, vec![ScriptTurn::text("nothing")]);
    assert_eq!(record["outcome"], "nothing_to_do", "{record}");
    assert_eq!(git(&env, &project(&env), &["rev-parse", "source"]), stale);
    assert_eq!(
        git(&env, &project(&env), &["symbolic-ref", "refs/heads/main"]),
        "refs/heads/source"
    );
    assert_eq!(git(&env, &project(&env), &["status", "--porcelain"]), "");
}

#[test]
fn a_batch_freezes_remote_and_local_base_aliases_after_normalization() {
    let env = TestEnv::new();
    let (_, fetched) = lagging_base(&env);
    env.write(
        "work/tasks.json",
        r#"[
        {"id":"first","branch":"agent/first","model":"fake","base":"origin/main","prompt":"First task"},
        {"id":"second","branch":"agent/second","model":"fake","base":"main","prompt":"Second task"}
    ]"#,
    );
    let (records, _) = run_batch(
        &env,
        vec![ScriptTurn::text("nothing"), ScriptTurn::text("nothing")],
        &["work", "../tasks.json"],
        None,
    );
    assert_eq!(records.len(), 2);
    for record in records {
        assert_eq!(record["base_sha"], fetched, "{record}");
        assert_eq!(
            git(&env, tree(&record), &["diff", "--name-only", "main..HEAD"]),
            ""
        );
    }
}

#[test]
fn post_checkout_policy_observes_the_normalized_base() {
    use std::os::unix::fs::PermissionsExt;
    let env = TestEnv::new();
    let (_, fetched) = lagging_base(&env);
    let hook = env.write("work/project/.git/hooks/post-checkout", "#!/bin/sh\ntest \"$(git rev-parse HEAD)\" = \"$(git rev-parse main)\" || { echo stale-base-before-checkout >&2; exit 1; }\n");
    std::fs::set_permissions(hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    let record = run(&env, vec![ScriptTurn::text("nothing")]);
    assert_eq!(record["outcome"], "nothing_to_do", "{record}");
    assert_eq!(record["base_sha"], fetched);
}

#[test]
fn a_concurrent_checkout_during_ancestry_validation_keeps_its_head_and_index() {
    use std::os::unix::fs::PermissionsExt;
    let env = TestEnv::new();
    let (stale, _) = lagging_base(&env);
    let original_path = std::env::var("PATH").unwrap();
    env.write("bin/original-path", &format!("{original_path}\n"));
    let marker = env.work().join("checkout-race");
    // The trusted Git wrapper performs the real ancestry read, then interleaves an ordinary
    // source checkout before returning that successful read to the native runner.
    let wrapper = env.write("bin/git", "#!/bin/sh\nfixture_tools=${0%/*}\nIFS= read -r PATH < \"$fixture_tools/original-path\" || exit 1\nexport PATH\ngit \"$@\"\nrace_status=$?\ncase \" $* \" in *\" merge-base --is-ancestor \"*) if test \"$race_status\" -eq 0 && mkdir \"$fixture_tools/../work/checkout-race\" 2>/dev/null; then git -C \"$fixture_tools/../work/project\" checkout main >/dev/null 2>&1 || exit 1; fi;; esac\nexit \"$race_status\"\n");
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{original_path}", wrapper.parent().unwrap().display());
    let (record, _) = run_observed(
        &env,
        vec![ScriptTurn::text("nothing")],
        &["work", "../tasks.json"],
        Some(path),
    );
    assert!(
        marker.exists(),
        "the controlled concurrent checkout never ran: {record}"
    );
    assert_eq!(record["outcome"], "nothing_to_do", "{record}");
    assert_eq!(
        git(&env, &project(&env), &["symbolic-ref", "HEAD"]),
        "refs/heads/main"
    );
    assert_eq!(git(&env, &project(&env), &["rev-parse", "HEAD"]), stale);
    assert_eq!(git(&env, &project(&env), &["status", "--porcelain"]), "");
}

#[test]
fn local_base_branch_checked_out_elsewhere_is_never_moved() {
    let env = TestEnv::new();
    setup(&env, "[]");
    // `main` lives in the review worktree; the source checkout is off it.
    git(&env, &project(&env), &["checkout", "-b", "source"]);
    let other = env.work().join("review");
    git(
        &env,
        &project(&env),
        &["worktree", "add", "-q", other.to_str().unwrap(), "main"],
    );
    let checked_out = git(&env, &project(&env), &["rev-parse", "main"]);
    commit_file(&env, "fetched.txt", "fetched base\n");
    git(&env, &project(&env), &["push", "origin", "source:main"]);
    git(&env, &project(&env), &["fetch", "origin"]);
    let fetched = git(&env, &project(&env), &["rev-parse", "origin/main"]);
    env.write(
        "work/tasks.json",
        r#"[{"id":"test","branch":"agent/test","model":"fake","base":"origin/main","prompt":"Finish task"}]"#,
    );
    let record = run(&env, vec![ScriptTurn::text("nothing")]);
    assert_eq!(record["outcome"], "nothing_to_do", "{record}");
    assert_eq!(record["base_sha"], fetched, "{record}");
    assert_eq!(
        git(&env, tree(&record), &["rev-parse", "HEAD"]),
        fetched,
        "{record}"
    );
    assert_eq!(
        git(&env, &project(&env), &["rev-parse", "main"]),
        checked_out,
        "a branch checked out in another worktree was moved"
    );
}

#[test]
fn base_branch_fast_forward_runs_no_repository_hook_on_the_host() {
    use std::os::unix::fs::PermissionsExt;
    let env = TestEnv::new();
    let (_, fetched) = lagging_base(&env);
    // Install the hook only now: the fixture's own commits and pushes are
    // expected to run it, the runner's host metadata is not.
    let hook = env.write(
        "work/project/.git/hooks/reference-transaction",
        "#!/bin/sh\ncat >/dev/null\nprintf escaped > \"$HOME/branch-advance-escape\"\nprintf ran > branch-advance-ran\n",
    );
    std::fs::set_permissions(hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    let record = run(&env, vec![ScriptTurn::text("nothing")]);
    // Sandboxed branch creation already runs this hook and saves its task-local marker.
    assert_eq!(record["outcome"], "done", "{record}");
    assert_eq!(
        std::fs::read_to_string(tree(&record).join("branch-advance-ran")).unwrap(),
        "ran"
    );
    assert_eq!(
        git(&env, &project(&env), &["rev-parse", "main"]),
        fetched,
        "the host refused to advance the base branch"
    );
    assert!(
        !env.home().join("branch-advance-escape").exists(),
        "advancing the base branch ran a host hook: {record}"
    );
    assert!(
        !project(&env).join("branch-advance-ran").exists(),
        "advancing the base branch ran a hook: {record}"
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
        "#!/bin/sh\nprintf escaped > \"$HOME/continuation-hook-escape\" 2>/dev/null || true\nprintf ran > post-merge-ran\nif test -n \"${GH_TOKEN-}${GITHUB_TOKEN-}\"; then printf exposed > \"$(git rev-parse --git-path merge-credentials)\"; fi\nif test -r \"$HOME/.config/gh/config.yml\"; then printf exposed > \"$(git rev-parse --git-path merge-config)\"; fi\nreadlink /proc/self/ns/net > \"$(git rev-parse --git-path merge-network)\"\n",
    );
    std::fs::set_permissions(hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    env.write("home/.config/gh/config.yml", "version: 1\n");
    let (record, _) = work::run_observed_with(
        &env,
        vec![ScriptTurn::text("nothing")],
        &["work", "../tasks.json"],
        Some(synthetic_github_path(&env)),
        |command| {
            command.envs([
                ("OXA_TEST_ISOLATION", "0"),
                ("GH_TOKEN", "SYNTH_AMBIENT_CREDENTIAL"),
                ("GITHUB_TOKEN", "SYNTH_AMBIENT_CREDENTIAL"),
            ]);
        },
    );
    assert!(!env.home().join("continuation-hook-escape").exists(), "{record}");
    assert_eq!(record["outcome"], "done", "{record}");
    assert_eq!(
        std::fs::read_to_string(tree(&record).join("post-merge-ran")).unwrap(),
        "ran"
    );
    assert!(!project(&env).join("post-merge-ran").exists());
    let marker = |name: &str| {
        PathBuf::from(git(
            &env,
            tree(&record),
            &["rev-parse", "--path-format=absolute", "--git-path", name],
        ))
    };
    assert!(!marker("merge-credentials").exists(), "{record}");
    assert!(!marker("merge-config").exists(), "{record}");
    assert_ne!(
        std::fs::read_to_string(marker("merge-network")).unwrap().trim(),
        std::fs::read_link("/proc/self/ns/net").unwrap().to_str().unwrap()
    );
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
