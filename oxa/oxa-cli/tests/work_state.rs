//! Durable runner recovery through the real CLI, sandbox, local Git and fake engine.
#[path = "common/work.rs"]
mod work;
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use work::{git, run, run_args, run_observed, setup};

fn project(env: &TestEnv) -> PathBuf {
    env.work().join("project")
}
fn storage(env: &TestEnv) -> PathBuf {
    let common = project(env).join(".git").canonicalize().unwrap();
    let key: String = Sha256::digest(common.as_os_str().as_encoded_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    env.agent_dir().join("work").join(key)
}
fn path(env: &TestEnv) -> PathBuf {
    storage(env).join("test.json")
}
fn load(env: &TestEnv) -> serde_json::Value {
    serde_json::from_slice(&fs::read(path(env)).unwrap()).unwrap()
}
fn replace(env: &TestEnv, state: &serde_json::Value) {
    fs::write(path(env), serde_json::to_vec(state).unwrap()).unwrap();
}
fn tree(record: &serde_json::Value) -> &Path {
    Path::new(record["worktree"].as_str().unwrap())
}
fn write_turn() -> ScriptTurn {
    ScriptTurn::tool("write", "write", r#"{"path":"answer.txt","content":"retained"}"#)
}
fn until(mut ready: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(15);
    while !ready() {
        assert!(Instant::now() < end, "fixture did not reach its barrier");
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn status(env: &TestEnv) -> serde_json::Value {
    let output = Command::new(env!("CARGO_BIN_EXE_oxa"))
        .args(["work", "status", "--json"])
        .current_dir(project(env))
        .env_clear()
        .envs(env.envs("http://127.0.0.1:1"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn dirty_success_is_saved_and_records_the_actual_head_and_tree() {
    use std::os::unix::fs::PermissionsExt;
    let env = TestEnv::new();
    setup(&env, "[]");
    let record = run(&env, vec![write_turn(), ScriptTurn::text("done")]);
    assert_eq!(record["outcome"], "done", "{record}");
    assert_eq!(record["step"], "finished");
    assert_eq!(git(&env, tree(&record), &["show", "HEAD:answer.txt"]), "retained");
    assert!(git(&env, tree(&record), &["status", "--porcelain"]).is_empty());
    assert_eq!(
        record["head_sha"],
        git(&env, tree(&record), &["rev-parse", "HEAD"])
    );
    assert_eq!(
        record["tree_sha"],
        git(&env, tree(&record), &["rev-parse", "HEAD^{tree}"])
    );
    assert!(record["checkpoint_revision"].as_u64().unwrap() > 0);
    assert_eq!(
        fs::metadata(path(&env)).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn red_work_is_saved_without_reporting_local_acceptance() {
    let env = TestEnv::new();
    setup(&env, r#"["printf failed > failure.txt; exit 1"]"#);
    let record = run(
        &env,
        vec![ScriptTurn::text("done"), ScriptTurn::text("cannot repair")],
    );
    assert_eq!(record["outcome"], "red", "{record}");
    assert_eq!(git(&env, tree(&record), &["show", "HEAD:failure.txt"]), "failed");
    assert!(git(&env, tree(&record), &["status", "--porcelain"]).is_empty());
}

#[test]
fn finished_success_is_cached_and_explicit_rerun_keeps_revisions_monotonic() {
    let env = TestEnv::new();
    setup(&env, "[]");
    let first = run(&env, vec![ScriptTurn::text("done")]);
    let (again, requests) = run_observed(&env, vec![], &["work", "../tasks.json"], None);
    assert_eq!(first, again);
    assert!(requests.is_empty());
    let repeated = run_args(
        &env,
        vec![ScriptTurn::text("repeated")],
        &["work", "../tasks.json", "--rerun"],
        None,
    );
    assert_eq!(repeated["outcome"], "nothing_to_do", "{repeated}");
    assert!(
        repeated["checkpoint_revision"].as_u64().unwrap() > first["checkpoint_revision"].as_u64().unwrap()
    );
}

#[test]
fn restart_at_saving_preserves_the_verdict_and_does_not_replay_the_worker() {
    let env = TestEnv::new();
    setup(&env, "[]");
    let first = run(&env, vec![ScriptTurn::text("done")]);
    fs::write(tree(&first).join("recovered.txt"), "recovered").unwrap();
    let mut state = load(&env);
    state["record"]["step"] = "saving".into();
    state["record"]["outcome"] = "red".into();
    state["record"]["detail"] = "previous acceptance failed".into();
    replace(&env, &state);
    let (record, requests) = run_observed(&env, vec![], &["work", "../tasks.json"], None);
    assert_eq!(record["outcome"], "red", "{record}");
    assert!(requests.is_empty());
    assert_eq!(
        git(&env, tree(&record), &["show", "HEAD:recovered.txt"]),
        "recovered"
    );
}

#[test]
fn restart_at_acceptance_uses_the_durable_base_and_commands() {
    let env = TestEnv::new();
    setup(&env, r#"["printf ran >> attempts"]"#);
    let first = run(&env, vec![ScriptTurn::text("done")]);
    let old_base = git(&env, &project(&env), &["rev-parse", "main"]);
    env.write("work/project/.oxa/settings.json", r#"{"done":["exit 7"]}"#);
    git(&env, &project(&env), &["add", "."]);
    git(&env, &project(&env), &["commit", "-m", "advance base checks"]);
    let new_base = git(&env, &project(&env), &["rev-parse", "main"]);
    assert_ne!(old_base, new_base);
    // Source resources remain exactly what the owner approved. The mutable base
    // now differs, so planning sees its new commands but recovery must ignore them.
    git(&env, &project(&env), &["checkout", "--detach", &old_base]);
    let mut state = load(&env);
    state["record"]["step"] = "acceptance".into();
    state["record"]["outcome"] = "running".into();
    replace(&env, &state);
    let (record, requests) = run_observed(&env, vec![], &["work", "../tasks.json"], None);
    assert_eq!(record["outcome"], "done", "{record}");
    assert_eq!(record["base_sha"], old_base);
    assert!(requests.is_empty());
    assert_eq!(
        fs::read_to_string(tree(&first).join("attempts")).unwrap(),
        "ranran"
    );
}

#[test]
fn failed_commit_hook_preserves_files_and_blocks() {
    use std::os::unix::fs::PermissionsExt;
    let env = TestEnv::new();
    setup(&env, "[]");
    let hook = env.write("work/project/.git/hooks/pre-commit", "#!/bin/sh\nexit 1\n");
    fs::set_permissions(hook, fs::Permissions::from_mode(0o755)).unwrap();
    let record = run(&env, vec![write_turn(), ScriptTurn::text("done")]);
    assert_eq!(record["outcome"], "blocked", "{record}");
    assert_eq!(
        fs::read_to_string(tree(&record).join("answer.txt")).unwrap(),
        "retained"
    );
    assert!(!git(&env, tree(&record), &["status", "--porcelain"]).is_empty());
}

#[test]
fn saving_hooks_run_inside_the_real_sandbox() {
    use std::os::unix::fs::PermissionsExt;
    let env = TestEnv::new();
    setup(&env, "[]");
    let hook = env.write(
        "work/project/.git/hooks/pre-commit",
        r#"#!/bin/sh
printf escaped > "$HOME/save-hook-escape" 2>/dev/null || true
printf ran > "$(git rev-parse --git-path save-hook-ran)"
"#,
    );
    fs::set_permissions(hook, fs::Permissions::from_mode(0o755)).unwrap();
    let record = run(&env, vec![write_turn(), ScriptTurn::text("done")]);
    assert_eq!(record["outcome"], "done", "{record}");
    assert!(!env.home().join("save-hook-escape").exists());
    let marker = PathBuf::from(git(
        &env,
        tree(&record),
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "save-hook-ran",
        ],
    ));
    assert_eq!(fs::read_to_string(marker).unwrap(), "ran");
}

#[test]
fn a_parent_crash_retains_session_and_child_lock_then_resumes_the_same_session() {
    let env = TestEnv::new();
    setup(&env, "[]");
    let server = FakeServer::start(
        vec![ScriptTurn::text("first")],
        FakeOpts {
            hold_headers_ms: 1500,
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_oxa"))
        .args(["work", "../tasks.json"])
        .current_dir(project(&env))
        .env_clear()
        .envs(env.envs(&server.url()))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    until(|| !server.requests().is_empty() && load(&env)["record"]["session_id"].is_string());
    let before = fs::read(path(&env)).unwrap();
    let id = load(&env)["record"]["session_id"].as_str().unwrap().to_owned();
    child.kill().unwrap();
    child.wait().unwrap();
    let blocked = run(&env, vec![]);
    assert_eq!(blocked["outcome"], "blocked", "{blocked}");
    assert!(blocked["detail"].as_str().unwrap().contains("already running"));
    assert_eq!(fs::read(path(&env)).unwrap(), before);
    assert_eq!(status(&env)[0]["liveness"], "running");
    let lock_path = storage(&env).join("test.lock");
    until(|| {
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap()
            .try_lock()
            .is_ok()
    });
    let observed = status(&env);
    assert!(
        matches!(observed[0]["liveness"].as_str(), Some("interrupted" | "unknown")),
        "{observed}"
    );
    assert_eq!(fs::read(path(&env)).unwrap(), before);
    let resumed = run(&env, vec![ScriptTurn::text("resumed")]);
    assert_eq!(resumed["outcome"], "nothing_to_do", "{resumed}");
    assert_eq!(resumed["session_id"], id);
    assert_eq!(resumed["step"], "finished");
}

#[test]
fn persistence_failure_aborts_the_batch_before_checks_or_saving() {
    use std::os::unix::fs::PermissionsExt;
    struct Restore(PathBuf);
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o700));
        }
    }
    let env = TestEnv::new();
    setup(&env, r#"["printf forbidden > acceptance-ran"]"#);
    env.write("work/tasks.json", r#"[{"id":"test","branch":"agent/test","model":"fake","prompt":"Finish task"},{"id":"later","branch":"agent/later","model":"fake","prompt":"Later task"}]"#);
    let server = FakeServer::start(
        vec![ScriptTurn::text("first")],
        FakeOpts {
            hold_headers_ms: 700,
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_oxa"))
        .args(["work", "../tasks.json"])
        .current_dir(project(&env))
        .env_clear()
        .envs(env.envs(&server.url()))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    until(|| !server.requests().is_empty());
    let restore = Restore(storage(&env));
    fs::set_permissions(&restore.0, fs::Permissions::from_mode(0o500)).unwrap();
    let output = child.wait_with_output().unwrap();
    drop(restore);
    assert_eq!(
        output.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stdout.is_empty(),
        "fatal persistence is not a normal per-task block"
    );
    assert!(!env
        .work()
        .join(".oxa-worktrees/project/test/acceptance-ran")
        .exists());
    assert!(!env.work().join(".oxa-worktrees/project/later").exists());
    assert_eq!(load(&env)["record"]["step"], "worker");
}

#[test]
fn offline_policy_survives_a_later_online_invocation() {
    let env = TestEnv::new();
    setup(&env, "[]");
    env.write(
        "work/tasks.json",
        r#"[{"id":"test","branch":"agent/test","model":"cloud-model","prompt":"Finish task"}]"#,
    );
    let server = FakeServer::start(
        vec![ScriptTurn::text("never")],
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
    for args in [
        vec!["--offline", "work", "../tasks.json"],
        vec!["work", "../tasks.json"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_oxa"))
            .args(args)
            .current_dir(project(&env))
            .env_clear()
            .envs(env.envs(&server.url()))
            .output()
            .unwrap();
        let record: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(record["outcome"], "blocked", "{record}");
        assert!(
            record["detail"].as_str().unwrap().contains("offline mode is on"),
            "{record}"
        );
        assert!(load(&env)["offline"].as_bool().unwrap());
    }
    assert!(server.requests().is_empty());
}

#[test]
fn corrupt_checkpoints_are_preserved_and_status_reports_an_error() {
    let env = TestEnv::new();
    setup(&env, "[]");
    run(&env, vec![ScriptTurn::text("done")]);
    let mut state = load(&env);
    state["version"] = 99.into();
    replace(&env, &state);
    let before = fs::read(path(&env)).unwrap();
    let (record, requests) = run_observed(&env, vec![], &["work", "../tasks.json"], None);
    assert_eq!(record["outcome"], "blocked", "{record}");
    assert!(requests.is_empty());
    assert_eq!(fs::read(path(&env)).unwrap(), before);
    let output = Command::new(env!("CARGO_BIN_EXE_oxa"))
        .args(["work", "status", "--json"])
        .current_dir(project(&env))
        .env_clear()
        .envs(env.envs("http://127.0.0.1:1"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
}

#[test]
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn status_works_when_the_kernel_refuses_every_flock_syscall() {
    let env = TestEnv::new();
    setup(&env, "[]");
    run(&env, vec![ScriptTurn::text("done")]);
    let mut state = load(&env);
    state["record"]["step"] = "worker".into();
    state["record"]["outcome"] = "running".into();
    replace(&env, &state);
    let before = fs::read(path(&env)).unwrap();
    let syscall = if cfg!(target_arch = "aarch64") {
        32_u32
    } else {
        73_u32
    };
    let mut program = Vec::new();
    for (code, yes, no, value) in [
        (0x20_u16, 0_u8, 0_u8, 0_u32),
        (0x15, 0, 1, syscall),
        (0x06, 0, 0, 0x80000000),
        (0x06, 0, 0, 0x7fff0000),
    ] {
        program.extend_from_slice(&code.to_ne_bytes());
        program.push(yes);
        program.push(no);
        program.extend_from_slice(&value.to_ne_bytes());
    }
    let filter = env.tmp().join("deny-flock.bpf");
    fs::write(&filter, program).unwrap();
    let output = Command::new("bwrap")
        .args([
            "--bind",
            "/",
            "/",
            "--tmpfs",
            "/run",
            "--proc",
            "/proc",
            "--dev-bind",
            "/dev",
            "/dev",
            "--seccomp",
            "0",
            "--",
            env!("CARGO_BIN_EXE_oxa"),
            "work",
            "status",
            "--json",
        ])
        .stdin(fs::File::open(filter).unwrap())
        .current_dir(project(&env))
        .env_clear()
        .envs(env.envs("http://127.0.0.1:1"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rows: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        matches!(rows[0]["liveness"].as_str(), Some("interrupted" | "unknown")),
        "{rows}"
    );
    assert_eq!(fs::read(path(&env)).unwrap(), before);
}

#[test]
fn the_owner_time_limit_reaches_the_real_child_and_keeps_its_wrap_up_work() {
    let env = TestEnv::new();
    setup(&env, "[]");
    env.write(
        "work/tasks.json",
        r#"[{"id":"test","branch":"agent/test","model":"fake","prompt":"Finish task","max_time":1}]"#,
    );
    let (record, requests) = run_observed(
        &env,
        vec![
            ScriptTurn::tool(
                "edit",
                "bash",
                r#"{"command":"printf draft > change.txt; sleep 1.2; printf finished > tool-finished"}"#,
            ),
            ScriptTurn::tool(
                "save",
                "bash",
                r#"{"command":"git add change.txt tool-finished && git commit -m 'Save interrupted work'"}"#,
            ),
        ],
        &["work", "../tasks.json"],
        None,
    );
    assert_eq!(record["outcome"], "interrupted", "{record}");
    assert_eq!(record["worker"]["outcome"], "interrupted", "{record}");
    assert_eq!(requests.len(), 2, "one graceful wrap-up turn");
    assert_eq!(
        fs::read_to_string(tree(&record).join("tool-finished")).unwrap(),
        "finished"
    );
    assert_eq!(git(&env, tree(&record), &["show", "HEAD:change.txt"]), "draft");
    assert_eq!(load(&env)["task"]["max_time"], 1);
    assert_eq!(record["step"], "finished");
    assert_eq!(record["interruption"], "deadline");
    assert_eq!(record["interruption"], "deadline");
}

#[test]
fn a_crash_between_worker_finished_and_acceptance_entry_does_not_restart_the_worker() {
    let env = TestEnv::new();
    setup(&env, r#"["printf ran >> attempts"]"#);
    run(&env, vec![ScriptTurn::text("done")]);
    let mut state = load(&env);
    state["record"]["step"] = "acceptance".into();
    state["record"]["outcome"] = "running".into();
    state["record"]["checkpoint_kind"] = "worker_finished".into();
    replace(&env, &state);
    let (record, requests) = run_observed(&env, vec![], &["work", "../tasks.json"], None);
    assert!(requests.is_empty());
    assert_eq!(record["outcome"], "done", "{record}");
    assert_eq!(record["checkpoint_kind"], "task_transition");
    assert_eq!(
        fs::read_to_string(tree(&record).join("attempts")).unwrap(),
        "ranran"
    );
}

#[test]
fn acceptance_time_cannot_give_the_repair_worker_a_fresh_time_budget() {
    let env = TestEnv::new();
    setup(&env, r#"["sleep 2.2; printf failed > failure.txt; exit 1"]"#);
    env.write(
        "work/tasks.json",
        r#"[{"id":"test","branch":"agent/test","model":"fake","prompt":"Finish task","max_time":2}]"#,
    );
    let (record, requests) = run_observed(
        &env,
        vec![ScriptTurn::text("first"), ScriptTurn::text("must not run")],
        &["work", "../tasks.json"],
        None,
    );
    assert_eq!(requests.len(), 1, "expired task budget refuses repair admission");
    assert_eq!(record["outcome"], "interrupted", "{record}");
    assert_eq!(record["interruption"], "deadline");
    assert_eq!(
        record["worker"]["outcome"], "done",
        "worker success remains distinct from task interruption"
    );
    assert_eq!(git(&env, tree(&record), &["show", "HEAD:failure.txt"]), "failed");
}

#[test]
fn a_restart_keeps_the_original_deadline_instead_of_starting_another_budget() {
    let env = TestEnv::new();
    setup(&env, "[]");
    run(&env, vec![ScriptTurn::text("done")]);
    let mut state = load(&env);
    state["record"]["step"] = "worker".into();
    state["record"]["outcome"] = "running".into();
    state["deadline_at_ms"] = 1.into();
    replace(&env, &state);
    let (record, requests) = run_observed(&env, vec![], &["work", "../tasks.json"], None);
    assert!(requests.is_empty());
    assert_eq!(record["outcome"], "interrupted", "{record}");
    assert_eq!(record["interruption"], "deadline");
    assert_eq!(load(&env)["deadline_at_ms"], 1);
}
