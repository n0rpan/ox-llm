//! Native command routing and private-file effects; never a real sign-in.
use oxa_fake::TestEnv;
use serde_json::json;
use std::{
    fs,
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    process::{Command, Output},
};
fn run(env: &TestEnv, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_oxa"))
        .env_clear()
        .envs(env.envs("http://127.0.0.1:9"))
        .current_dir(env.work())
        .args(args)
        .output()
        .unwrap()
}
#[test]
fn only_explicit_native_commands_are_accepted_and_errors_do_not_echo_arguments() {
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"oxa":{"engine":{"inProcessCloud":false}}}"#,
    );
    let settings = env.home().join(".oxa/agent/settings.json");
    let previous = fs::read(&settings).unwrap();
    for args in [
        vec!["login", "chatgpt"],
        vec![
            "login",
            "chatgpt",
            "--experimental",
            "synthetic-authorization-code",
        ],
        vec!["login", "other", "--experimental"],
        vec!["logout", "chatgpt", "--replace"],
    ] {
        let output = run(&env, &args);
        assert_eq!(output.status.code(), Some(2));
        let printed = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(printed.contains("Usage: oxa login chatgpt --experimental"));
        assert!(!printed.contains("synthetic-authorization-code"));
    }
    let output = run(&env, &["login", "chatgpt", "--experimental", "--paste"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("requires an owner terminal"));
    assert!(output.stdout.is_empty());
    assert_eq!(fs::read(settings).unwrap(), previous);
    assert!(!env.home().join(".config/oxc/llm").exists());
    let help = run(&env, &["--help"]);
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("oxa login chatgpt --experimental"));
    assert!(String::from_utf8_lossy(&help.stdout).contains("oxa logout chatgpt"));
}
#[test]
fn native_logout_removes_only_the_synthetic_token_and_keeps_settings_key_and_lock() {
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"oxa":{"engine":{"inProcessCloud":false}}}"#,
    );
    let directory = env.home().join(".config/oxc/llm");
    fs::create_dir_all(&directory).unwrap();
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
    let path = directory.join("chatgpt-auth.json");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .unwrap();
    write!(file,"{}",json!({"access_token":"synthetic-access","refresh_token":"synthetic-refresh","account_id":"synthetic-account","expires_at":2_000_000_000})).unwrap();
    drop(file);
    let key = directory.join("cloud-secrets.json");
    fs::write(&key, "synthetic untouched key sentinel").unwrap();
    let settings = env.home().join(".oxa/agent/settings.json");
    let previous = fs::read(&settings).unwrap();
    for _ in 0..2 {
        let output = run(&env, &["logout", "chatgpt"]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"ChatGPT signed out.\n");
        assert!(!path.exists());
        assert!(directory.join("chatgpt-auth.lock").is_file());
        assert_eq!(fs::read(&key).unwrap(), b"synthetic untouched key sentinel");
        assert_eq!(fs::read(&settings).unwrap(), previous);
    }
    assert!(!env.home().join(".oxa/agent/cloud-ledger").exists());
}

#[test]
fn mistakenly_pasted_callback_is_not_in_argument_debug() {
    let args = vec![
        "login".into(),
        "chatgpt".into(),
        "--experimental".into(),
        "synthetic-authorization-code".into(),
    ];
    let parsed = oxa_cli::parse_args(&args, &[]).unwrap();
    assert!(!format!("{parsed:?}").contains("synthetic-authorization-code"));
}
