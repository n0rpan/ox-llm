//! Real CLI, isolated home and generated guest bearers only. No model/network.
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
