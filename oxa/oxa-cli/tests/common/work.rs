use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
use std::{path::Path, process::Command};
pub fn git(env: &TestEnv, dir: &Path, args: &[&str]) -> String {
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
    String::from_utf8_lossy(&out.stdout).trim().into()
}
pub fn setup(env: &TestEnv, done: &str) {
    git(env, &env.work(), &["init", "--bare", "remote.git"]);
    git(env, &env.work(), &["init", "-b", "main", "project"]);
    let project = env.work().join("project");
    git(env, &project, &["config", "user.name", "Test"]);
    git(env, &project, &["config", "user.email", "test@example.invalid"]);
    env.write(
        "work/project/.oxa/settings.json",
        &format!(r#"{{"done":{done}}}"#),
    );
    git(env, &project, &["add", "."]);
    git(env, &project, &["commit", "-m", "fixture"]);
    git(env, &project, &["remote", "add", "origin", "../remote.git"]);
    git(env, &project, &["push", "origin", "main"]);
    oxa_perm::TrustStore::default()
        .save(&env.agent_dir().join("trust.json"), &project, true)
        .unwrap();
    env.write(
        "work/tasks.json",
        r#"[{"id":"test","branch":"agent/test","model":"fake","prompt":"Finish task"}]"#,
    );
}
pub fn run(env: &TestEnv, turns: Vec<ScriptTurn>) -> serde_json::Value {
    run_args(env, turns, &["work", "../tasks.json"], None)
}
pub fn run_args(
    env: &TestEnv,
    turns: Vec<ScriptTurn>,
    args: &[&str],
    path: Option<String>,
) -> serde_json::Value {
    run_observed(env, turns, args, path).0
}
pub fn run_observed(
    env: &TestEnv,
    turns: Vec<ScriptTurn>,
    args: &[&str],
    path: Option<String>,
) -> (serde_json::Value, Vec<oxa_fake::Recorded>) {
    run_observed_with_env(env, turns, args, path, &[])
}
pub fn run_observed_with_env(
    env: &TestEnv,
    turns: Vec<ScriptTurn>,
    args: &[&str],
    path: Option<String>,
    extra: &[(&str, &str)],
) -> (serde_json::Value, Vec<oxa_fake::Recorded>) {
    let (mut records, requests) = run_batch_with_env(env, turns, args, path, extra);
    assert_eq!(records.len(), 1, "expected one task record");
    (records.remove(0), requests)
}
pub fn run_batch(
    env: &TestEnv,
    turns: Vec<ScriptTurn>,
    args: &[&str],
    path: Option<String>,
) -> (Vec<serde_json::Value>, Vec<oxa_fake::Recorded>) {
    run_batch_with_env(env, turns, args, path, &[])
}
fn run_batch_with_env(
    env: &TestEnv,
    turns: Vec<ScriptTurn>,
    args: &[&str],
    path: Option<String>,
    extra: &[(&str, &str)],
) -> (Vec<serde_json::Value>, Vec<oxa_fake::Recorded>) {
    let server = FakeServer::start(turns, FakeOpts::default()).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_oxa"));
    command
        .args(args)
        .current_dir(env.work().join("project"))
        .env_clear()
        .envs(env.envs(&server.url()))
        .envs(extra.iter().copied());
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
    let records = serde_json::Deserializer::from_slice(&out.stdout)
        .into_iter()
        .map(|record| {
            record.unwrap_or_else(|e| {
                panic!(
                    "{e}: {} / {}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                )
            })
        })
        .collect();
    (records, server.requests())
}
