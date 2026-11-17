//! Real CLI, isolated home and generated guest bearers only. No model/network.
#[cfg(all(feature = "share", target_os = "linux"))]
#[path = "common/strace.rs"]
mod strace;
use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
    sync::atomic::{AtomicUsize, Ordering},
};

struct Home(PathBuf);
impl Home {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "oxa-share-cli-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_oxa"))
            .args(args)
            .env_clear()
            .env("HOME", &self.0)
            .env("OXA_TEST_ISOLATION", "1")
            .current_dir(&self.0)
            .output()
            .unwrap()
    }
    #[cfg(feature = "share")]
    fn marked(&self, args: &[&str], marker: &str, value: &str) -> Output {
        Command::new(env!("CARGO_BIN_EXE_oxa"))
            .args(args)
            .env_clear()
            .env("HOME", &self.0)
            .env("OXA_TEST_ISOLATION", "1")
            .env(marker, value)
            .current_dir(&self.0)
            .output()
            .unwrap()
    }
}
impl Drop for Home {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[cfg(not(feature = "share"))]
#[test]
fn default_build_refuses_share_without_creating_storage() {
    let home = Home::new();
    let result = home.run(&["share", "key", "list"]);
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
    assert!(!home.0.join(".oxa").exists());
}

#[cfg(feature = "share")]
const OWNER_REFUSAL: &str = "Guest key commands require the owner outside an agent tool";

#[cfg(feature = "share")]
fn assert_agent_refusal(output: &Output) {
    assert_eq!(output.status.code(), Some(1), "owner command must refuse");
    assert!(output.stdout.is_empty(), "owner output must stay private");
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains(OWNER_REFUSAL), "{error}");
    assert!(
        !error.contains("synthetic-device"),
        "arguments must not be echoed"
    );
    assert!(!error.contains("synthetic-id"), "arguments must not be echoed");
}

#[cfg(feature = "share")]
#[test]
fn agent_markers_refuse_all_key_commands_without_creating_storage() {
    for marker in ["OXA_CODING_AGENT", "AI_AGENT", "OXA_SUBAGENT_DEPTH"] {
        for value in ["", "1"] {
            for args in [
                vec![
                    "share",
                    "key",
                    "add",
                    "synthetic-device",
                    "--daily-requests",
                    "1",
                    "--daily-tokens",
                    "10",
                ],
                vec!["share", "key", "list"],
                vec!["share", "key", "revoke", "synthetic-id"],
            ] {
                let home = Home::new();
                assert_agent_refusal(&home.marked(&args, marker, value));
                assert!(!home.0.join(".oxa").exists(), "refusal must precede storage");
                assert!(!home.0.join(".config").exists());
            }
        }
    }
}

#[cfg(all(feature = "share", target_os = "linux"))]
#[test]
fn agent_refusal_does_not_open_a_preexisting_synthetic_store() {
    use std::os::unix::fs::PermissionsExt;
    for marker in ["OXA_CODING_AGENT", "AI_AGENT", "OXA_SUBAGENT_DEPTH"] {
        for args in [
            vec!["share", "key", "list"],
            vec!["share", "key", "revoke", "synthetic-id"],
        ] {
            let home = Home::new();
            let store = home.0.join(".oxa/share");
            fs::create_dir_all(&store).unwrap();
            fs::set_permissions(&store, fs::Permissions::from_mode(0o700)).unwrap();
            // Deliberately invalid synthetic bytes: this is never a key registry.
            let sentinel = store.join("share-keys.json");
            fs::write(&sentinel, b"synthetic unopened store").unwrap();
            fs::set_permissions(&sentinel, fs::Permissions::from_mode(0o600)).unwrap();
            let trace = home.0.join("store-opens.trace");
            let output = Command::new("timeout")
                .args(["--kill-after=1s", "5s"])
                .arg(strace::executable())
                .args(["-f", "-e", "trace=open,openat,openat2", "-o"])
                .arg(&trace)
                .arg(env!("CARGO_BIN_EXE_oxa"))
                .args(&args)
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap_or_default())
                .env("HOME", &home.0)
                .env("OXA_TEST_ISOLATION", "1")
                .env(marker, "")
                .current_dir(&home.0)
                .output()
                .expect("timeout and strace are mandatory for the no-store-open proof");
            assert_agent_refusal(&output);
            let opens = fs::read_to_string(trace).unwrap();
            assert!(opens.contains("open"), "tracer must record process startup");
            assert!(!opens.contains(store.to_str().unwrap()), "{opens}");
            assert!(!store.join(".lock").exists());
            assert_eq!(fs::read(&sentinel).unwrap(), b"synthetic unopened store");
            assert_eq!(fs::read_dir(&store).unwrap().count(), 1);
            assert!(!home.0.join(".oxa/agent").exists());
        }
    }
}

#[cfg(feature = "share")]
#[test]
fn real_key_commands_create_list_revoke_without_reprinting_bearer() {
    use oxc_gateway::share::keys::Store;
    use serde_json::Value;
    use std::os::unix::fs::PermissionsExt;
    let home = Home::new();
    let created = home.run(&[
        "share",
        "key",
        "add",
        "Tablet",
        "--daily-requests",
        "10",
        "--daily-tokens",
        "20000",
    ]);
    assert!(created.status.success());
    assert!(created.stderr.is_empty());
    let value: Value = serde_json::from_slice(&created.stdout).unwrap();
    let bearer = value["api_key"].as_str().unwrap();
    let id = value["id"].as_str().unwrap();
    let directory = home.0.join(".oxa/share");
    let store = Store::open(&directory).unwrap();
    let info = store.authenticate(bearer).unwrap().unwrap();
    assert_eq!(info.name, "Tablet");
    assert_eq!(info.limits.daily_requests, 10);
    assert_eq!(info.limits.daily_tokens, 20000);
    assert_eq!(
        fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(directory.join("share-keys.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(!fs::read_to_string(directory.join("share-keys.json"))
        .unwrap()
        .contains(bearer));
    for args in [
        vec!["share", "key", "list"],
        vec!["share", "key", "revoke", id],
        vec!["share", "key", "list"],
    ] {
        let result = home.run(&args);
        assert!(result.status.success());
        assert!(result.stderr.is_empty());
        assert!(!String::from_utf8(result.stdout).unwrap().contains(bearer));
    }
    assert!(store.authenticate(bearer).unwrap().is_none());
    let other = Home::new();
    Store::initialize(&other.0.join(".oxa/share")).unwrap();
    let missing = other.run(&["share", "key", "revoke", id]);
    assert!(missing.status.success());
    let value: Value = serde_json::from_slice(&missing.stdout).unwrap();
    assert_eq!(value["revoked"], false);
}

#[cfg(feature = "share")]
#[test]
fn invalid_commands_and_unsafe_directory_never_issue_keys() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let home = Home::new();
    for args in [
        vec!["share", "key", "add", "Tablet"],
        vec![
            "share",
            "key",
            "add",
            "Tablet",
            "--daily-requests",
            "0",
            "--daily-tokens",
            "10",
        ],
        vec![
            "share",
            "key",
            "add",
            "Tablet",
            "--daily-requests",
            "1",
            "--daily-requests",
            "10",
        ],
        vec!["share", "key", "list", "unexpected"],
    ] {
        let result = home.run(&args);
        assert!(!result.status.success());
        assert!(result.stdout.is_empty());
        assert!(!home.0.join(".oxa").exists());
    }
    let target = home.0.join("outside");
    fs::create_dir(&target).unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
    symlink(&target, home.0.join(".oxa")).unwrap();
    let result = home.run(&[
        "share",
        "key",
        "add",
        "Tablet",
        "--daily-requests",
        "1",
        "--daily-tokens",
        "10",
    ]);
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
    assert!(fs::read_dir(&target).unwrap().next().is_none());
}

#[cfg(feature = "share")]
#[test]
fn unsafe_modes_and_repository_homes_are_refused_without_repair() {
    use std::os::unix::fs::PermissionsExt;
    for repository in [false, true] {
        let home = Home::new();
        let directory = home.0.join(".oxa/share");
        if repository {
            fs::create_dir(home.0.join(".git")).unwrap();
        } else {
            fs::create_dir_all(&directory).unwrap();
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let result = home.run(&[
            "share",
            "key",
            "add",
            "Tablet",
            "--daily-requests",
            "1",
            "--daily-tokens",
            "10",
        ]);
        assert!(!result.status.success());
        assert!(result.stdout.is_empty());
        assert!(!directory.join("share-keys.json").exists());
        if repository {
            assert!(!home.0.join(".oxa").exists());
        } else {
            assert_eq!(
                fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
                0o755
            );
        }
    }
}

#[cfg(feature = "share")]
#[test]
fn output_failure_keeps_one_durable_key_without_retrying_or_leaking_diagnostics() {
    struct Closed;
    impl std::io::Write for Closed {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let home = Home::new();
    let directory = home.0.join(".oxa/share");
    let words = [
        "key",
        "add",
        "Tablet",
        "--daily-requests",
        "1",
        "--daily-tokens",
        "10",
    ]
    .map(String::from);
    let error = oxc_gateway::share::command::run(&words, &directory, &mut Closed).unwrap_err();
    assert!(matches!(error, oxc_gateway::share::command::Error::Output));
    assert_eq!(format!("{error:?}"), "Output");
    let store = oxc_gateway::share::keys::Store::open(&directory).unwrap();
    let rows = store.list().unwrap();
    assert_eq!(rows.len(), 1);
    assert!(!rows[0].revoked);
    assert!(store.revoke(&rows[0].id).unwrap());
}
