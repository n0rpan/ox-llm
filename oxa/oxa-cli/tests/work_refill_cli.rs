use oxa_fake::TestEnv;
use serde_json::json;
use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Stdio},
};

fn invoke(env: &TestEnv, config: &Path, token: &str, offline: bool) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_oxa"));
    command
        .current_dir(env.work())
        .env_clear()
        .envs(env.envs("http://127.0.0.1:1"));
    if offline {
        command.arg("--offline");
    }
    command
        .args([
            "work",
            "board",
            "--refill",
            "--repo",
            "example/project",
            "--once",
            "--token-stdin",
            "--config",
        ])
        .arg(config)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    child.stdin.take().unwrap().write_all(token.as_bytes()).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn refill_startup_retains_pause_and_refuses_worker_writable_authority() {
    let env = TestEnv::new();
    assert!(
        Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(env.work())
            .output()
            .unwrap()
            .status
            .success()
    );
    oxa_perm::TrustStore::default()
        .save(&env.agent_dir().join("trust.json"), &env.work(), true)
        .unwrap();
    let controls = env.agent_dir().join("board-control");
    fs::create_dir(&controls).unwrap();
    fs::set_permissions(&controls, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(env.tmp(), fs::Permissions::from_mode(0o700)).unwrap();
    let config = controls.join("refill.json");
    let pause = controls.join("paused");
    let queue = controls.join("queue");
    let claims = controls.join("claims");
    for path in [&queue, &claims] {
        fs::create_dir(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let sentinel = claims.join("preserved");
    fs::write(&sentinel, b"synthetic imported attempts and holds").unwrap();
    let write_config = |value: serde_json::Value| {
        fs::write(&config, value.to_string()).unwrap();
        fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
    };
    write_config(json!({"enabled":false}));
    assert!(invoke(&env, &config, "", false).status.success());
    write_config(json!({"enabled":true,"queue":queue,"pause":pause,"claims":claims}));
    fs::write(&pause, b"owner paused").unwrap();
    // Separate real processes simulate a service starting again while the pause persists.
    for _ in 0..2 {
        assert!(invoke(&env, &config, "", false).status.success());
    }
    assert_eq!(
        fs::read(&sentinel).unwrap(),
        b"synthetic imported attempts and holds"
    );
    assert_eq!(fs::read_dir(&claims).unwrap().count(), 1);
    assert_eq!(fs::read_dir(&queue).unwrap().count(), 0);
    assert_eq!(invoke(&env, &config, "", true).status.code(), Some(2));
    let outside = env.tmp().join("untrusted-control.json");
    fs::write(&outside, b"{\"enabled\":false}").unwrap();
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(invoke(&env, &outside, "", false).status.code(), Some(2));
}
