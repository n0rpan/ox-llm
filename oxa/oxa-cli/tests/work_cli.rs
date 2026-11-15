//! Real child oxa with fake engine and local bare git remote; no network.
#[path = "common/work.rs"]
mod work;
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
use std::{path::Path, process::Command};
use work::{git, run, run_args, setup};
#[test]
fn external_checks_run_without_edits_and_retain_worktree() {
    let env = TestEnv::new();
    setup(&env, r#"["printf checked > receipt"]"#);
    let record = run(&env, vec![ScriptTurn::text("finished")]);
    assert_eq!(record["outcome"], "done", "{record}");
    let tree = Path::new(record["worktree"].as_str().unwrap());
    assert_eq!(std::fs::read_to_string(tree.join("receipt")).unwrap(), "checked");
    assert!(env.work().join("project/.git/oxa-work/test.json").exists());
}
#[test]
fn clean_task_is_nothing_to_do() {
    let env = TestEnv::new();
    setup(&env, "[]");
    assert_eq!(
        run(&env, vec![ScriptTurn::text("nothing")])["outcome"],
        "nothing_to_do"
    );
}
#[test]
fn failed_checks_get_exactly_one_retry() {
    let env = TestEnv::new();
    setup(
        &env,
        r#"["printf attempt >> attempts; echo external-check; exit 1"]"#,
    );
    let record = run(
        &env,
        vec![ScriptTurn::text("finished"), ScriptTurn::text("cannot repair")],
    );
    assert_eq!(record["outcome"], "red", "{record}");
    assert!(record["detail"].as_str().unwrap().contains("external-check"));
    let tree = Path::new(record["worktree"].as_str().unwrap());
    assert_eq!(
        std::fs::read_to_string(tree.join("attempts")).unwrap(),
        "attemptattempt"
    );
}
#[test]
fn retry_can_make_acceptance_green() {
    let env = TestEnv::new();
    setup(
        &env,
        r#"["if test -f once; then true; else touch once; exit 1; fi"]"#,
    );
    let record = run(
        &env,
        vec![ScriptTurn::text("finished"), ScriptTurn::text("retry")],
    );
    assert_eq!(record["outcome"], "done", "{record}");
}

#[test]
fn agent_edits_stay_in_worktree() {
    let env = TestEnv::new();
    setup(&env, "[]");
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"defaultProjectTrust":"always"}"#,
    );
    let record = run(
        &env,
        vec![
            ScriptTurn::tool("edit", "write", r#"{"path":"answer.txt","content":"finished"}"#),
            ScriptTurn::text("done"),
        ],
    );
    assert_eq!(record["outcome"], "done", "{record}");
    let tree = Path::new(record["worktree"].as_str().unwrap());
    assert_eq!(
        std::fs::read_to_string(tree.join("answer.txt")).unwrap(),
        "finished"
    );
    assert!(!env.work().join("project/answer.txt").exists());
}
#[test]
fn invalid_task_file_is_rejected_before_creating_worktree() {
    let env = TestEnv::new();
    setup(&env, "[]");
    env.write(
        "work/tasks.json",
        r#"[{"id":"../escape","branch":"agent/test","model":"fake","prompt":"go"}]"#,
    );
    let out = Command::new(env!("CARGO_BIN_EXE_oxa"))
        .args(["work", "../tasks.json"])
        .current_dir(env.work().join("project"))
        .env_clear()
        .envs(env.envs("http://127.0.0.1:1"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(!env.work().join("project/.git/oxa-work").exists());
}

#[test]
fn github_issue_input_uses_mock_gh() {
    use std::os::unix::fs::PermissionsExt;
    let env = TestEnv::new();
    setup(&env, "[]");
    let gh = env.write(
        "tmp/gh",
        r##"#!/bin/sh
[ "$*" = 'issue view 42 --json title,body' ] || exit 9
printf '%s' '{"title":"Synthetic task","body":"Fix the test fixture"}'
"##,
    );
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", env.tmp().display(), std::env::var("PATH").unwrap());
    let record = run_args(
        &env,
        vec![ScriptTurn::text("done")],
        &["work", "--issue", "42", "--model", "fake"],
        Some(path),
    );
    assert_eq!(record["id"], "issue-42");
    assert_eq!(record["branch"], "agent/42-work");
    assert_eq!(record["outcome"], "nothing_to_do", "{record}");
}
#[test]
fn branch_checked_out_elsewhere_is_blocked() {
    let env = TestEnv::new();
    setup(&env, "[]");
    git(
        &env,
        &env.work().join("project"),
        &["worktree", "add", "-b", "agent/test", "../other"],
    );
    let record = run(&env, vec![]);
    assert_eq!(record["outcome"], "blocked", "{record}");
    assert!(record["detail"].as_str().unwrap().contains("other"));
}

#[test]
fn child_can_create_a_commit_with_the_real_sandbox() {
    let env = TestEnv::new();
    setup(&env, "[]");
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"defaultProjectTrust":"always"}"#,
    );
    let record = run(
        &env,
        vec![
            ScriptTurn::tool("write", "write", r#"{"path":"answer.txt","content":"committed"}"#),
            ScriptTurn::tool(
                "commit",
                "bash",
                r#"{"command":"git add answer.txt && git -c user.name=Test -c user.email=test@example.invalid commit -m task"}"#,
            ),
            ScriptTurn::text("done"),
        ],
    );
    assert_eq!(record["outcome"], "done", "{record}");
    let tree = Path::new(record["worktree"].as_str().unwrap());
    let status = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(tree)
        .env_clear()
        .envs(env.envs("http://127.0.0.1:1"))
        .output()
        .unwrap();
    assert!(status.status.success());
    assert!(
        status.stdout.is_empty(),
        "commit must persist: {}",
        String::from_utf8_lossy(&status.stdout)
    );
    let subject = Command::new("git")
        .args(["log", "-1", "--format=%s"])
        .current_dir(tree)
        .env_clear()
        .envs(env.envs("http://127.0.0.1:1"))
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&subject.stdout).trim(), "task");
}
#[test]
fn offline_issue_input_never_spawns_gh() {
    use std::os::unix::fs::PermissionsExt;
    for setting in [false, true] {
        let env = TestEnv::new();
        setup(&env, "[]");
        let gh = env.write(
            "tmp/gh",
            "#!/bin/sh\nprintf invoked > \"$HOME/gh-invoked\"\nexit 7\n",
        );
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        if setting {
            env.write("home/.oxa/agent/settings.json", r#"{"oxa":{"offline":true}}"#);
        }
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_oxa"));
        cmd.args(["work", "--issue", "42", "--model", "fake"])
            .current_dir(env.work().join("project"))
            .env_clear()
            .envs(env.envs("http://127.0.0.1:1"))
            .env(
                "PATH",
                format!("{}:{}", env.tmp().display(), std::env::var("PATH").unwrap()),
            );
        if !setting {
            cmd.env("OXA_OFFLINE", "1");
        }
        let out = cmd.output().unwrap();
        assert_eq!(out.status.code(), Some(2));
        assert!(
            !env.home().join("gh-invoked").exists(),
            "offline lookup ran gh, setting={setting}"
        );
        assert!(String::from_utf8_lossy(&out.stderr).contains("offline"));
    }
}

#[test]
fn command_line_offline_policy_reaches_child_agent() {
    let env = TestEnv::new();
    setup(&env, "[]");
    env.write(
        "work/tasks.json",
        r#"[{"id":"test","branch":"agent/test","model":"cloud-model","prompt":"go"}]"#,
    );
    let server = FakeServer::start(
        vec![ScriptTurn::text("done")],
        FakeOpts {
            model: "cloud-model".into(),
            remote: Some("openrouter"),
            models: vec![oxa_fake::FakeModel {
                alias: "cloud-model".into(),
                cap: 64000,
                max_output: Some(4096),
                caps: None,
            }],
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_oxa"))
        .args(["--offline", "work", "../tasks.json"])
        .current_dir(env.work().join("project"))
        .env_clear()
        .envs(env.envs(&server.url()))
        .output()
        .unwrap();
    let record: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(record["outcome"], "blocked", "{record}");
    assert!(
        record["detail"].as_str().unwrap().contains("offline mode is on"),
        "{record}"
    );
    assert!(
        server.requests().is_empty(),
        "no chat reached even the mock remote"
    );
}
#[test]
fn batch_reports_one_task_error_and_runs_later_tasks() {
    let env = TestEnv::new();
    setup(&env, "[]");
    env.write("work/project/.git/oxa-work/test.json", "{}");
    env.write("work/tasks.json", r#"[{"id":"test","branch":"agent/test","model":"fake","prompt":"go"},{"id":"next","branch":"agent/next","model":"fake","prompt":"go"}]"#);
    let server = FakeServer::start(vec![ScriptTurn::text("done")], FakeOpts::default()).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_oxa"))
        .args(["work", "../tasks.json"])
        .current_dir(env.work().join("project"))
        .env_clear()
        .envs(env.envs(&server.url()))
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let rows: Vec<serde_json::Value> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["outcome"], "blocked");
    assert_eq!(rows[1]["outcome"], "nothing_to_do");
    assert_eq!(
        std::fs::read_to_string(env.work().join("project/.git/oxa-work/test.json")).unwrap(),
        "{}"
    );
}
#[test]
fn abandoned_lock_file_does_not_block_a_task() {
    let env = TestEnv::new();
    setup(&env, "[]");
    env.write("work/project/.git/oxa-work/test.lock", "");
    let record = run(&env, vec![ScriptTurn::text("done")]);
    assert_eq!(record["outcome"], "nothing_to_do", "{record}");
}
