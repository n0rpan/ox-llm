//! Real child oxa with fake engine and local bare git remote; no network.
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
use std::{
    path::{Path, PathBuf},
    process::Command,
};

fn storage(env: &TestEnv) -> PathBuf {
    use sha2::{Digest, Sha256};
    let common = env.work().join("project/.git").canonicalize().unwrap();
    let key: String = Sha256::digest(common.as_os_str().as_encoded_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    env.agent_dir().join("work").join(key)
}
fn git(env: &TestEnv, dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env_clear()
        .envs(env.envs("http://127.0.0.1:1"))
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}
fn setup(env: &TestEnv, done: &str) {
    git(env, &env.work(), &["init", "--bare", "remote.git"]);
    git(env, &env.work(), &["init", "-b", "main", "project"]);
    let project = env.work().join("project");
    env.write(
        "work/project/.oxa/settings.json",
        &format!(r#"{{"done":{done}}}"#),
    );
    git(env, &project, &["add", "."]);
    git(env, &project, &["commit", "-m", "fixture"]);
    git(env, &project, &["remote", "add", "origin", "../remote.git"]);
    git(env, &project, &["push", "origin", "main"]);
    // Approve the source repository only. The separate worker worktree still
    // uses the normal project-resource decision of its own child session.
    oxa_perm::TrustStore::default()
        .save(&env.agent_dir().join("trust.json"), &project, true)
        .unwrap();
    env.write(
        "work/tasks.json",
        r#"[{"id":"test","branch":"agent/test","model":"fake","prompt":"Finish task"}]"#,
    );
}
fn run(env: &TestEnv, turns: Vec<ScriptTurn>) -> serde_json::Value {
    run_args(env, turns, &["work", "../tasks.json"], None)
}
fn run_args(env: &TestEnv, turns: Vec<ScriptTurn>, args: &[&str], path: Option<String>) -> serde_json::Value {
    let server = FakeServer::start(turns, FakeOpts::default()).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_oxa"));
    command
        .args(args)
        .current_dir(env.work().join("project"))
        .env_clear()
        .envs(env.envs(&server.url()));
    if let Some(path) = path {
        command.env("PATH", path);
    }
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        tx.send(command.output().unwrap()).unwrap();
    });
    let out = rx
        .recv_timeout(std::time::Duration::from_secs(90))
        .expect("runner finished");
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "{e}: {} / {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}
#[test]
fn external_checks_run_without_edits_and_retain_worktree() {
    let env = TestEnv::new();
    setup(&env, r#"["printf checked > receipt"]"#);
    let record = run(&env, vec![ScriptTurn::text("finished")]);
    assert_eq!(record["outcome"], "done", "{record}");
    let tree = Path::new(record["worktree"].as_str().unwrap());
    assert_eq!(std::fs::read_to_string(tree.join("receipt")).unwrap(), "checked");
    assert!(storage(&env).join("test.json").exists());
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
fn failed_agent_with_failed_checks_is_blocked_and_keeps_both_causes() {
    let env = TestEnv::new();
    setup(&env, r#"["echo external-check-failed; exit 1"]"#);
    env.write(
        "work/tasks.json",
        r#"[{"id":"test","branch":"agent/test","model":"missing-model","prompt":"Finish task"}]"#,
    );
    let record = run(&env, vec![]);
    assert_eq!(record["outcome"], "blocked", "{record}");
    let detail = record["detail"].as_str().unwrap();
    assert!(detail.contains("missing-model"), "{detail}");
    assert!(detail.contains("external-check-failed"), "{detail}");
}
#[test]
fn retry_can_make_acceptance_green() {
    let env = TestEnv::new();
    setup(
        &env,
        r#"["if test -f once; then true; else touch once; exit 1; fi"]"#,
    );
    let server = FakeServer::start(
        vec![ScriptTurn::text("finished"), ScriptTurn::text("retry")],
        FakeOpts::default(),
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_oxa"))
        .args(["work", "../tasks.json"])
        .current_dir(env.work().join("project"))
        .env_clear()
        .envs(env.envs(&server.url()))
        .output()
        .unwrap();
    let record: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(record["outcome"], "done", "{record}");
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0].session, requests[1].session,
        "repair lost the worker session"
    );
    assert_eq!(record["session_id"], requests[0].session);
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
    assert!(!env.agent_dir().join("work").exists());
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
fn setup_failure_records_blocked_and_keeps_existing_branch() {
    let env = TestEnv::new();
    setup(&env, "[]");
    git(&env, &env.work().join("project"), &["branch", "agent/test"]);
    let record = run(&env, vec![]);
    assert_eq!(record["outcome"], "blocked", "{record}");
    assert!(record["detail"].as_str().unwrap().contains("already exists"));
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
    std::fs::create_dir_all(storage(&env)).unwrap();
    std::fs::write(storage(&env).join("test.json"), "{}").unwrap();
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
        std::fs::read_to_string(storage(&env).join("test.json")).unwrap(),
        "{}"
    );
}
#[test]
fn abandoned_lock_file_does_not_block_a_task() {
    let env = TestEnv::new();
    setup(&env, "[]");
    std::fs::create_dir_all(storage(&env)).unwrap();
    std::fs::write(storage(&env).join("test.lock"), "").unwrap();
    let record = run(&env, vec![ScriptTurn::text("done")]);
    assert_eq!(record["outcome"], "nothing_to_do", "{record}");
}

#[test]
fn untrusted_project_never_executes_external_checks() {
    let env = TestEnv::new();
    setup(&env, r#"["printf escaped > \"$HOME/untrusted-check\""]"#);
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"defaultProjectTrust":"never"}"#,
    );
    oxa_perm::TrustStore::default()
        .save(
            &env.agent_dir().join("trust.json"),
            &env.work().join("project"),
            false,
        )
        .unwrap();
    let record = run(&env, vec![ScriptTurn::text("finished")]);
    assert!(!env.home().join("untrusted-check").exists(), "untrusted code ran");
    assert_eq!(record["outcome"], "blocked", "{record}");
    assert!(record["detail"].as_str().unwrap().contains("trust"), "{record}");
}

#[test]
fn agent_written_acceptance_code_cannot_write_outside_the_sandbox() {
    let env = TestEnv::new();
    setup(&env, r#"["sh check.sh"]"#);
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"defaultProjectTrust":"always"}"#,
    );
    env.write("work/project/check.sh", "exit 0\n");
    let project = env.work().join("project");
    git(&env, &project, &["add", "check.sh"]);
    git(&env, &project, &["commit", "-m", "base check"]);
    let script = "if printf escaped > \"$HOME/acceptance-escape\" 2>/dev/null; then true; fi\nprintf checked > acceptance-ran\n";
    let args = serde_json::json!({"path":"check.sh", "content":script}).to_string();
    let record = run(
        &env,
        vec![
            ScriptTurn::tool("change-check", "write", &args),
            ScriptTurn::text("finished"),
        ],
    );
    assert!(
        !env.home().join("acceptance-escape").exists(),
        "agent-written code escaped acceptance"
    );
    assert_eq!(record["outcome"], "done", "{record}");
    assert_eq!(
        std::fs::read_to_string(Path::new(record["worktree"].as_str().unwrap()).join("acceptance-ran"))
            .unwrap(),
        "checked"
    );
}

#[test]
fn worker_result_and_session_come_from_the_real_child_contract() {
    let env = TestEnv::new();
    setup(&env, "[]");
    let record = run(&env, vec![ScriptTurn::text("finished")]);
    assert_eq!(record["worker"]["outcome"], "done", "{record}");
    assert_eq!(record["worker"]["exit_code"], 0, "{record}");
    assert!(!record["session_id"].as_str().unwrap().is_empty());
}

#[test]
fn blocked_worker_keeps_its_result_and_does_not_start_a_repair() {
    use oxa_fake::Fault;
    let env = TestEnv::new();
    setup(&env, r#"["echo external-check-failed; exit 1"]"#);
    env.write("home/.oxa/agent/settings.json", r#"{"retry":{"enabled":false}}"#);
    let server = FakeServer::start(
        vec![ScriptTurn::fault(Fault::Http {
            status: 429,
            body: serde_json::json!({"error":{"code":"budget_exceeded", "message":"Synthetic daily cap"}})
                .to_string(),
        })],
        FakeOpts::default(),
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_oxa"))
        .args(["work", "../tasks.json"])
        .current_dir(env.work().join("project"))
        .env_clear()
        .envs(env.envs(&server.url()))
        .output()
        .unwrap();
    let record: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(record["outcome"], "blocked", "{record}");
    assert_eq!(record["worker"]["outcome"], "blocked", "{record}");
    assert_eq!(record["worker"]["error_class"], "budget_exhausted", "{record}");
    assert_eq!(record["worker"]["exit_code"], oxa_proto::EXIT_BUDGET, "{record}");
    assert_eq!(
        server.requests().len(),
        1,
        "a budget refusal must not start a repair worker"
    );
    assert!(record["detail"]
        .as_str()
        .unwrap()
        .contains("external-check-failed"));
}

#[test]
fn large_streamed_reply_does_not_enter_the_runner_receipt() {
    let env = TestEnv::new();
    setup(&env, "[]");
    let mut turn = ScriptTurn::text(&"Synthetic response ".repeat(8192));
    turn.completion_tokens = Some(5);
    turn.chunk = 8192;
    let record = run(&env, vec![turn]);
    assert_eq!(record["worker"]["outcome"], "done", "{record}");
    assert!(
        record.to_string().len() < 2048,
        "runner retained streamed model output"
    );
}

#[test]
fn changed_project_commands_need_the_existing_content_approval() {
    let env = TestEnv::new();
    setup(&env, "[]");
    let project = env.work().join("project");
    oxa_perm::TrustStore::load(&env.agent_dir().join("trust.json"))
        .approve(&env.agent_dir().join("trust.json"), &project)
        .unwrap();
    env.write(
        "work/project/.oxa/settings.json",
        r#"{"done":["printf escaped > \"$HOME/changed-check\""]}"#,
    );
    git(&env, &project, &["add", ".oxa/settings.json"]);
    git(&env, &project, &["commit", "-m", "changed commands"]);
    let record = run(&env, vec![ScriptTurn::text("finished")]);
    assert_eq!(record["outcome"], "blocked", "{record}");
    assert!(!env.home().join("changed-check").exists());
}

#[test]
fn acceptance_has_no_unsandboxed_fallback() {
    use std::os::unix::fs::symlink;
    let env = TestEnv::new();
    setup(&env, r#"["printf ran > acceptance-ran"]"#);
    let tools = env.tmp().join("tools");
    std::fs::create_dir(&tools).unwrap();
    for name in ["git", "sh"] {
        symlink(Path::new("/usr/bin").join(name), tools.join(name)).unwrap();
    }
    let record = run_args(
        &env,
        vec![ScriptTurn::text("finished")],
        &["work", "../tasks.json"],
        Some(tools.to_string_lossy().into_owned()),
    );
    assert_eq!(record["outcome"], "blocked", "{record}");
    assert!(record["detail"].as_str().unwrap().contains("bwrap"), "{record}");
    assert!(!Path::new(record["worktree"].as_str().unwrap())
        .join("acceptance-ran")
        .exists());
}

#[test]
fn runner_state_is_private_and_outside_the_workers_git_bind() {
    use std::os::unix::fs::PermissionsExt;
    let env = TestEnv::new();
    setup(&env, "[]");
    let record = run(&env, vec![ScriptTurn::text("finished")]);
    assert_eq!(record["outcome"], "nothing_to_do", "{record}");
    assert_eq!(
        std::fs::metadata(storage(&env)).unwrap().permissions().mode() & 0o777,
        0o700
    );
    for file in ["test.json", "test.lock"] {
        assert_eq!(
            std::fs::metadata(storage(&env).join(file))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    assert!(!env.work().join("project/.git/oxa-work").exists());
}

#[test]
fn acceptance_preserves_the_normal_read_only_git_configuration() {
    let env = TestEnv::new();
    setup(&env, r#"["sh check.sh"]"#);
    env.write("work/project/check.sh", "git config core.fsmonitor 'printf escaped > \"$HOME/git-probe-escape\"; printf \"\\0\"' 2>/dev/null || true\nprintf checked > acceptance-ran\n");
    let project = env.work().join("project");
    git(&env, &project, &["add", "check.sh"]);
    git(&env, &project, &["commit", "-m", "check fixture"]);
    let record = run(&env, vec![ScriptTurn::text("finished")]);
    assert_eq!(record["outcome"], "done", "{record}");
    assert!(!env.home().join("git-probe-escape").exists());
    let config = Command::new("git")
        .args(["config", "--get", "core.fsmonitor"])
        .current_dir(project)
        .env_clear()
        .envs(env.envs("http://127.0.0.1:1"))
        .output()
        .unwrap();
    assert_eq!(
        config.status.code(),
        Some(1),
        "acceptance rewrote executable Git configuration"
    );
}

#[test]
fn github_lookup_ignores_project_local_executable() {
    use std::os::unix::fs::PermissionsExt;
    let env = TestEnv::new();
    setup(&env, "[]");
    let bad = env.write("work/project/bin/gh", "#!/bin/sh\nprintf invoked > \"$HOME/project-gh-ran\"\nprintf '%s' '{\"title\":\"Synthetic task\",\"body\":\"Fixture\"}'\n");
    let good = env.write("tmp/trusted/gh", "#!/bin/sh\nprintf invoked > \"$HOME/trusted-gh-ran\"\nprintf '%s' '{\"title\":\"Synthetic task\",\"body\":\"Fixture\"}'\n");
    for path in [&bad, &good] {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = format!(
        "{}:{}:{}",
        bad.parent().unwrap().display(),
        good.parent().unwrap().display(),
        std::env::var("PATH").unwrap()
    );
    let record = run_args(
        &env,
        vec![ScriptTurn::text("finished")],
        &["work", "--issue", "42", "--model", "fake"],
        Some(path),
    );
    assert_eq!(record["outcome"], "nothing_to_do", "{record}");
    assert!(
        !env.home().join("project-gh-ran").exists(),
        "project executable ran on the host"
    );
    assert!(
        env.home().join("trusted-gh-ran").exists(),
        "the trusted mock must handle issue lookup"
    );
}

#[test]
fn runner_git_queries_ignore_project_local_executable() {
    use std::os::unix::fs::PermissionsExt;
    let env = TestEnv::new();
    setup(&env, "[]");
    let bad = env.write("work/project/bin/git", "#!/bin/sh\nprintf invoked > \"$HOME/project-git-ran\" 2>/dev/null || true\nexec /usr/bin/git \"$@\"\n");
    std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        bad.parent().unwrap().display(),
        std::env::var("PATH").unwrap()
    );
    let record = run_args(
        &env,
        vec![ScriptTurn::text("finished")],
        &["work", "../tasks.json"],
        Some(path),
    );
    assert_eq!(record["outcome"], "nothing_to_do", "{record}");
    assert!(
        !env.home().join("project-git-ran").exists(),
        "project executable ran on the host"
    );
}

#[test]
fn untrusted_issue_input_never_spawns_even_a_trusted_gh() {
    use std::os::unix::fs::PermissionsExt;
    let env = TestEnv::new();
    setup(&env, "[]");
    oxa_perm::TrustStore::default()
        .save(
            &env.agent_dir().join("trust.json"),
            &env.work().join("project"),
            false,
        )
        .unwrap();
    let gh = env.write("tmp/gh", "#!/bin/sh\nprintf invoked > \"$HOME/issue-lookup-ran\"\nprintf '%s' '{\"title\":\"Synthetic task\",\"body\":\"Fixture\"}'\n");
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_oxa"))
        .args(["work", "--issue", "42", "--model", "fake"])
        .current_dir(env.work().join("project"))
        .env_clear()
        .envs(env.envs("http://127.0.0.1:1"))
        .env(
            "PATH",
            format!("{}:{}", env.tmp().display(), std::env::var("PATH").unwrap()),
        )
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("trust"));
    assert!(!env.home().join("issue-lookup-ran").exists());
}
