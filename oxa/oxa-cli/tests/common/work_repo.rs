use oxa_fake::TestEnv;
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
