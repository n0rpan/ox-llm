use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
#[path = "work_confirm.rs"]
mod confirm;
#[path = "work_repo.rs"]
mod work_repo;
use std::process::Command;
pub use work_repo::{git, setup};
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
    run_observed_with(env, turns, args, path, |_| {})
}
pub fn run_observed_with(
    env: &TestEnv,
    turns: Vec<ScriptTurn>,
    args: &[&str],
    path: Option<String>,
    configure: impl FnOnce(&mut Command),
) -> (serde_json::Value, Vec<oxa_fake::Recorded>) {
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
    configure(&mut command);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        tx.send(confirm::output(command)).unwrap();
    });
    let out = rx
        .recv_timeout(std::time::Duration::from_secs(90))
        .expect("runner finished");
    let record = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "{e}: {} / {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    });
    (record, server.requests())
}
