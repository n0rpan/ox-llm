//! Durable outcomes through the real runner, fake engine and local bare remote.
#[path = "common/work.rs"]
mod work;
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
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
fn receipt(env: &TestEnv) -> PathBuf {
    project(env).join(".git/oxa-work/test.json")
}
fn tree(record: &serde_json::Value) -> &Path {
    Path::new(record["worktree"].as_str().unwrap())
}
fn trust(env: &TestEnv) {
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"defaultProjectTrust":"always"}"#,
    );
}
fn write_turn() -> ScriptTurn {
    ScriptTurn::tool("write", "write", r#"{"path":"answer.txt","content":"retained"}"#)
}
fn load(env: &TestEnv) -> serde_json::Value {
    serde_json::from_slice(&fs::read(receipt(env)).unwrap()).unwrap()
}
fn replace_state(env: &TestEnv, value: &serde_json::Value) {
    fs::write(receipt(env), serde_json::to_vec(value).unwrap()).unwrap();
}
#[test]
fn uncommitted_success_is_saved_as_a_commit() {
    let env = TestEnv::new();
    setup(&env, "[]");
    trust(&env);
    let record = run(&env, vec![write_turn(), ScriptTurn::text("done")]);
    assert_eq!(record["outcome"], "done", "{record}");
    assert_eq!(git(&env, tree(&record), &["show", "HEAD:answer.txt"]), "retained");
    assert!(git(&env, tree(&record), &["status", "--porcelain"]).is_empty());
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        fs::metadata(receipt(&env)).unwrap().permissions().mode() & 0o077,
        0
    );
    assert_eq!(record["step"], "finished");
    assert!(record["progress_at"].as_u64().unwrap() > 0);
}
#[test]
fn red_work_is_saved_too() {
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
fn completed_task_is_skipped_unless_rerun_is_explicit() {
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
}
#[test]
fn restart_at_saving_preserves_outcome_without_replaying_agent() {
    let env = TestEnv::new();
    setup(&env, "[]");
    let first = run(&env, vec![ScriptTurn::text("done")]);
    fs::write(tree(&first).join("recovered.txt"), "recovered").unwrap();
    let mut state = load(&env);
    state["step"] = "saving".into();
    state["outcome"] = "red".into();
    state["detail"] = "previous acceptance failed".into();
    replace_state(&env, &state);
    let (record, requests) = run_observed(&env, vec![], &["work", "../tasks.json"], None);
    assert_eq!(record["outcome"], "red");
    assert!(requests.is_empty());
    assert_eq!(
        git(&env, tree(&record), &["show", "HEAD:recovered.txt"]),
        "recovered"
    );
}
#[test]
fn restart_at_acceptance_keeps_captured_checks_and_base() {
    let env = TestEnv::new();
    setup(&env, r#"["printf ran >> attempts"]"#);
    let first = run(&env, vec![ScriptTurn::text("done")]);
    env.write("work/project/.oxa/settings.json", r#"{"done":["exit 7"]}"#);
    git(&env, &project(&env), &["add", ".oxa/settings.json"]);
    git(&env, &project(&env), &["commit", "-m", "advance base checks"]);
    let mut state = load(&env);
    state["step"] = "acceptance".into();
    state["outcome"] = "running".into();
    replace_state(&env, &state);
    let (record, requests) = run_observed(&env, vec![], &["work", "../tasks.json"], None);
    assert_eq!(record["outcome"], "done", "{record}");
    assert!(requests.is_empty());
    assert_eq!(
        fs::read_to_string(tree(&first).join("attempts")).unwrap(),
        "ranran"
    );
}
#[test]
fn status_reports_state_progress_time_and_current_step() {
    let env = TestEnv::new();
    setup(&env, "[]");
    let record = run(&env, vec![ScriptTurn::text("done")]);
    let out = Command::new(env!("CARGO_BIN_EXE_oxa"))
        .args(["work", "--status"])
        .current_dir(project(&env))
        .env_clear()
        .envs(env.envs("http://127.0.0.1:1"))
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        format!("test nothing_to_do {} finished", record["progress_at"])
    );
}
#[test]
fn failed_auto_commit_retains_every_file_and_blocks() {
    let env = TestEnv::new();
    setup(&env, "[]");
    trust(&env);
    // A user hook refuses the automatic commit. The runner must not bypass it.
    use std::os::unix::fs::PermissionsExt;
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
fn until(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !predicate() {
        assert!(Instant::now() < deadline, "timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
}
#[test]
fn child_keeps_lock_after_runner_is_killed_then_task_restarts() {
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
    until(|| !server.requests().is_empty());
    assert_eq!(load(&env)["step"], "agent");
    child.kill().unwrap();
    child.wait().unwrap();
    let blocked = run(&env, vec![]);
    assert_eq!(blocked["outcome"], "blocked", "{blocked}");
    assert!(
        blocked["detail"].as_str().unwrap().contains("already running"),
        "{blocked}"
    );
    assert_eq!(
        load(&env)["step"],
        "agent",
        "busy retry must not clobber the checkpoint"
    );
    let lock_path = project(&env).join(".git/oxa-work/test.lock");
    until(|| {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        file.try_lock().is_ok()
    });
    let restarted = run(&env, vec![ScriptTurn::text("resumed")]);
    assert_eq!(restarted["outcome"], "nothing_to_do", "{restarted}");
    assert_eq!(restarted["step"], "finished");
}

#[test]
fn saving_git_keeps_lock_after_runner_dies_during_commit_hook() {
    use std::os::unix::fs::PermissionsExt;
    struct Release(PathBuf);
    impl Drop for Release {
        fn drop(&mut self) {
            let _ = fs::write(&self.0, "release");
        }
    }
    let env = TestEnv::new();
    setup(&env, "[]");
    trust(&env);
    let release = Release(env.home().join("release-hook"));
    let hook = env.write(
        "work/project/.git/hooks/pre-commit",
        r#"#!/bin/sh
touch "$HOME/hook-started"
attempt=0
while ! test -f "$HOME/release-hook"; do
  attempt=$((attempt + 1))
  if test "$attempt" -ge 500; then exit 9; fi
  sleep 0.01
done
"#,
    );
    fs::set_permissions(hook, fs::Permissions::from_mode(0o755)).unwrap();
    let server =
        FakeServer::start(vec![write_turn(), ScriptTurn::text("done")], FakeOpts::default()).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_oxa"))
        .args(["work", "../tasks.json"])
        .current_dir(project(&env))
        .env_clear()
        .envs(env.envs(&server.url()))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    until(|| env.home().join("hook-started").exists());
    child.kill().unwrap();
    child.wait().unwrap();
    let blocked = run(&env, vec![]);
    assert!(
        blocked["detail"].as_str().unwrap().contains("already running"),
        "{blocked}"
    );
    assert_eq!(load(&env)["step"], "saving");
    fs::write(&release.0, "release").unwrap();
    let path = project(&env).join(".git/oxa-work/test.lock");
    until(|| {
        let file = fs::OpenOptions::new().read(true).write(true).open(&path).unwrap();
        file.try_lock().is_ok()
    });
    let record = run(&env, vec![]);
    assert_eq!(record["outcome"], "done", "{record}");
    assert_eq!(git(&env, tree(&record), &["show", "HEAD:answer.txt"]), "retained");
}
#[test]
fn status_reports_an_unlocked_interrupted_phase_as_blocked_without_mutating_state() {
    let env = TestEnv::new();
    setup(&env, "[]");
    run(&env, vec![ScriptTurn::text("done")]);
    let mut state = load(&env);
    state["step"] = "agent".into();
    state["outcome"] = "running".into();
    replace_state(&env, &state);
    let out = Command::new(env!("CARGO_BIN_EXE_oxa"))
        .args(["work", "--status"])
        .current_dir(project(&env))
        .env_clear()
        .envs(env.envs("http://127.0.0.1:1"))
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.starts_with("test blocked ") && text.trim().ends_with("agent"),
        "{text}"
    );
    assert_eq!(load(&env), state);
}
