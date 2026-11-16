#![cfg(feature = "share")]
use oxc_gateway::share::keys::{Error, Limits, Store};
use std::{
    fs,
    os::unix::fs::{symlink, MetadataExt, PermissionsExt},
    path::PathBuf,
    process::Command,
};

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let mut bytes = [0; 16];
        getrandom::fill(&mut bytes).unwrap();
        let name: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let path = std::env::temp_dir().join(format!("guest-key-test-{name}"));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }
    fn store(&self) -> Store {
        Store::open(&self.0).unwrap()
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn limits() -> Limits {
    Limits {
        daily_requests: 10,
        daily_tokens: 1000,
    }
}

fn git(home: &Directory, arguments: &[&str]) {
    assert!(Command::new("git")
        .args(arguments)
        .current_dir(&home.0)
        .env_clear()
        .env("HOME", &home.0)
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success());
}

#[test]
fn bare_and_separate_common_worktree_metadata_refuse_private_key_children() {
    let d = Directory::new();
    git(&d, &["init", "--bare", "bare"]);
    git(&d, &["init", "--separate-git-dir=metadata", "checkout"]);
    git(
        &d,
        &[
            "-C",
            "checkout",
            "-c",
            "user.name=Synthetic",
            "-c",
            "user.email=synthetic@example.invalid",
            "commit",
            "--allow-empty",
            "-m",
            "synthetic",
        ],
    );
    git(
        &d,
        &["-C", "checkout", "worktree", "add", "--detach", "../linked"],
    );
    for parent in ["bare", "bare/objects", "metadata", "metadata/worktrees/linked"] {
        let private = d.0.join(parent).join("private");
        fs::create_dir(&private).unwrap();
        fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            Store::open(&private).err(),
            Some(Error::Unsafe),
            "Git metadata must be refused"
        );
        assert!(fs::read_dir(&private).unwrap().next().is_none());
    }
}

#[test]
#[cfg(target_os = "linux")]
fn revoke_retry_does_not_acknowledge_a_failed_directory_sync() {
    if let Some(directory) = std::env::var_os("OXA_SHARE_REVOKE_TEST_DIRECTORY") {
        let store = Store::open(&PathBuf::from(directory)).unwrap();
        let c = store.create("Device", limits()).unwrap(); // fsync 1 (file), 2 (directory).
        assert_eq!(store.revoke(&c.info.id), Err(Error::Unavailable)); // fsync 4 fails after rename.
        assert!(store.list().unwrap()[0].revoked, "rename is already visible");
        // Every fsync from ordinal 4 onward fails. A durable acknowledgement is
        // impossible until the syscall failure is cleared, including on retry.
        assert_eq!(store.revoke(&c.info.id), Err(Error::Unavailable));
        return;
    }
    let d = Directory::new();
    let trace = d.0.join("sync-trace");
    let result = Command::new("strace")
        .args([
            "-f",
            "-qq",
            "-yy",
            "--kill-on-exit",
            "-e",
            "trace=fsync",
            "-e",
            "inject=fsync:error=EIO:when=4+1",
            "-o",
        ])
        .arg(&trace)
        .arg(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "revoke_retry_does_not_acknowledge_a_failed_directory_sync",
        ])
        .env_clear()
        .env("HOME", &d.0)
        .env("XDG_CONFIG_HOME", &d.0)
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("OXA_SHARE_REVOKE_TEST_DIRECTORY", &d.0)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "revoke retry acknowledged an injected sync failure: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let syscalls = fs::read_to_string(trace).unwrap();
    assert!(syscalls.matches("INJECTED").count() >= 2);
    // A new process/handle can durably revoke once normal I/O is restored.
    let store = d.store();
    let id = store.list().unwrap()[0].id.clone();
    assert!(store.revoke(&id).unwrap());
    assert!(d.store().list().unwrap()[0].revoked);
}

#[test]
fn lifecycle_is_durable_and_only_creation_exposes_the_bearer() {
    let directory = Directory::new();
    let store = directory.store();
    assert!(store.list().unwrap().is_empty());
    let created = store.create("Tablet", limits()).unwrap();
    let key = created.bearer.expose();
    assert_eq!(key.len(), 64);
    assert_eq!(store.authenticate(key).unwrap(), Some(created.info.clone()));
    assert_eq!(directory.store().list().unwrap(), vec![created.info.clone()]);
    let raw = fs::read_to_string(directory.0.join("share-keys.json")).unwrap();
    for safe in [
        format!("{created:?}"),
        format!("{store:?}"),
        format!("{:?}", store.list()),
        raw,
    ] {
        assert!(!safe.contains(key));
    }
    assert!(directory.store().revoke(&created.info.id).unwrap());
    assert!(store.authenticate(key).unwrap().is_none());
    assert!(store.list().unwrap()[0].revoked);
    assert!(store.revoke(&created.info.id).unwrap());
    assert!(!store.revoke(&"0".repeat(32)).unwrap());
    assert_eq!(store.create("Tablet", limits()).unwrap_err(), Error::Invalid);
    for file in ["share-keys.json", "share-keys.lock"] {
        let m = fs::metadata(directory.0.join(file)).unwrap();
        assert_eq!(m.mode() & 0o777, 0o600);
        assert_eq!(m.nlink(), 1);
    }
}

#[test]
fn mismatches_at_every_bearer_position_and_malformed_inputs_refuse() {
    let d = Directory::new();
    let store = d.store();
    let created = store.create("Device", limits()).unwrap();
    for i in 0..64 {
        let mut wrong = created.bearer.expose().as_bytes().to_vec();
        wrong[i] = if wrong[i] == b'0' { b'1' } else { b'0' };
        assert!(store
            .authenticate(std::str::from_utf8(&wrong).unwrap())
            .unwrap()
            .is_none());
    }
    for key in [
        "",
        "not a bearer",
        &"a".repeat(63),
        &"a".repeat(65),
        &"A".repeat(64),
    ] {
        assert!(store.authenticate(key).unwrap().is_none());
    }
    for name in [
        "",
        " ",
        " leading",
        "trailing ",
        "a/b",
        "line\nbreak",
        &"x".repeat(65),
    ] {
        assert_eq!(store.create(name, limits()).unwrap_err(), Error::Invalid);
    }
    for limit in [
        Limits {
            daily_requests: 0,
            ..limits()
        },
        Limits {
            daily_tokens: 0,
            ..limits()
        },
    ] {
        assert_eq!(store.create("bad", limit).unwrap_err(), Error::Invalid);
    }
}

#[test]
fn nonprivate_linked_or_special_storage_is_refused_without_repairing_it() {
    let d = Directory::new();
    let store = d.store();
    let c = store.create("Device", limits()).unwrap();
    let path = d.0.join("share-keys.json");
    let backup = d.0.join("backup");
    fs::rename(&path, &backup).unwrap();
    symlink(&backup, &path).unwrap();
    assert_eq!(store.list().unwrap_err(), Error::Unsafe);
    fs::remove_file(&path).unwrap();
    fs::hard_link(&backup, &path).unwrap();
    assert_eq!(store.authenticate(c.bearer.expose()).unwrap_err(), Error::Unsafe);
    fs::remove_file(&path).unwrap();
    fs::rename(&backup, &path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(store.revoke(&c.info.id).unwrap_err(), Error::Unsafe);
    assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o644);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    assert_eq!(store.list().unwrap_err(), Error::Unsafe);
    fs::remove_dir(&path).unwrap();
    nix::unistd::mkfifo(
        &path,
        nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
    )
    .unwrap();
    assert_eq!(store.list().unwrap_err(), Error::Unsafe);
    fs::remove_file(&path).unwrap();
    fs::set_permissions(&d.0, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(store.list().unwrap_err(), Error::Unsafe);
    assert_eq!(d.store_result(), Some(Error::Unsafe));
}
impl Directory {
    fn store_result(&self) -> Option<Error> {
        Store::open(&self.0).err()
    }
}

#[test]
fn unsafe_lock_and_parent_links_are_refused() {
    let d = Directory::new();
    let outside = Directory::new();
    symlink(&outside.0, d.0.join("linked")).unwrap();
    assert_eq!(Store::open(&d.0.join("linked")).unwrap_err(), Error::Unsafe);
    assert_eq!(
        Store::open(std::path::Path::new("relative")).unwrap_err(),
        Error::Invalid
    );
    let store = d.store();
    let lock = d.0.join("share-keys.lock");
    symlink(outside.0.join("absent"), &lock).unwrap();
    assert_eq!(store.create("Device", limits()).unwrap_err(), Error::Unsafe);
    assert!(!outside.0.join("absent").exists());
    fs::remove_file(&lock).unwrap();
    fs::write(&lock, b"").unwrap();
    fs::set_permissions(&lock, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(store.list().unwrap_err(), Error::Unsafe);
}

#[test]
fn repository_ancestors_and_parent_symlinks_are_refused_but_open_store_is_anchored() {
    let root = Directory::new();
    let private = root.0.join("private");
    fs::create_dir(&private).unwrap();
    fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(root.0.join(".git"), b"synthetic repository marker").unwrap();
    assert_eq!(Store::open(&private).unwrap_err(), Error::Unsafe);
    fs::remove_file(root.0.join(".git")).unwrap();
    symlink(&root.0, root.0.join("alias")).unwrap();
    assert_eq!(
        Store::open(&root.0.join("alias/private")).unwrap_err(),
        Error::Unsafe
    );
    let store = Store::open(&private).unwrap();
    let c = store.create("first", limits()).unwrap();
    let saved = root.0.join("saved");
    fs::rename(&private, &saved).unwrap();
    let elsewhere = Directory::new();
    symlink(&elsewhere.0, &private).unwrap();
    store.revoke(&c.info.id).unwrap();
    assert!(Store::open(&saved)
        .unwrap()
        .authenticate(c.bearer.expose())
        .unwrap()
        .is_none());
    assert!(fs::read_dir(&elsewhere.0).unwrap().next().is_none());
}

#[test]
fn malformed_unknown_version_oversized_and_duplicate_registry_rows_fail_closed() {
    let d = Directory::new();
    let store = d.store();
    let c = store.create("Device", limits()).unwrap();
    let path = d.0.join("share-keys.json");
    let valid: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let mut duplicate = valid.clone();
    let row = duplicate["keys"][0].clone();
    duplicate["keys"].as_array_mut().unwrap().push(row);
    let mut zero_limits = valid.clone();
    zero_limits["keys"][0]["daily_tokens"] = 0.into();
    let mut bad_digest = valid.clone();
    bad_digest["keys"][0]["digest"] = "invalid".into();
    let mut unknown = valid.clone();
    unknown["version"] = 2.into();
    for data in [
        b"{".to_vec(),
        vec![b' '; 65537],
        serde_json::to_vec(&duplicate).unwrap(),
        serde_json::to_vec(&zero_limits).unwrap(),
        serde_json::to_vec(&bad_digest).unwrap(),
        serde_json::to_vec(&unknown).unwrap(),
    ] {
        fs::write(&path, data).unwrap();
        assert_eq!(store.authenticate(c.bearer.expose()).unwrap_err(), Error::Corrupt);
        assert_eq!(store.create("Other", limits()).unwrap_err(), Error::Corrupt);
    }
}

#[test]
fn updates_are_atomic_and_registry_size_is_bounded() {
    let d = Directory::new();
    let store = d.store();
    let first = store.create("first", limits()).unwrap();
    let previous = fs::File::open(d.0.join("share-keys.json")).unwrap();
    store.create("second", limits()).unwrap();
    let old: serde_json::Value = serde_json::from_reader(previous).unwrap();
    assert_eq!(old["keys"].as_array().unwrap().len(), 1);
    for i in 2..64 {
        store.create(&format!("device-{i}"), limits()).unwrap();
    }
    assert_eq!(store.create("overflow", limits()).unwrap_err(), Error::Full);
    assert!(store.revoke(&first.info.id).unwrap());
    assert_eq!(store.list().unwrap().len(), 64);
    assert_eq!(
        fs::read_dir(&d.0).unwrap().count(),
        2,
        "temporary files are cleaned up"
    );
}

#[test]
fn concurrent_process_updates_do_not_lose_keys_and_revocation_is_fresh() {
    if let Some(dir) = std::env::var_os("OXA_SHARE_TEST_DIRECTORY") {
        let name = std::env::var("OXA_SHARE_TEST_NAME").unwrap();
        let store = Store::open(&PathBuf::from(dir)).unwrap();
        let created = store.create(&name, limits()).unwrap();
        assert_eq!(
            store.authenticate(created.bearer.expose()).unwrap(),
            Some(created.info)
        );
        return;
    }
    let d = Directory::new();
    let binary = std::env::current_exe().unwrap();
    let children: Vec<_> = (0..8)
        .map(|i| {
            Command::new(&binary)
                .args([
                    "--exact",
                    "concurrent_process_updates_do_not_lose_keys_and_revocation_is_fresh",
                ])
                .env_clear()
                .env("HOME", &d.0)
                .env("XDG_CONFIG_HOME", &d.0)
                .env("OXA_SHARE_TEST_DIRECTORY", &d.0)
                .env("OXA_SHARE_TEST_NAME", format!("device-{i}"))
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    for mut child in children {
        assert!(child.wait().unwrap().success());
    }
    assert_eq!(d.store().list().unwrap().len(), 8);
    let created = d.store().create("revoke", limits()).unwrap();
    let before = d.store();
    assert!(before.authenticate(created.bearer.expose()).unwrap().is_some());
    d.store().revoke(&created.info.id).unwrap();
    assert!(before.authenticate(created.bearer.expose()).unwrap().is_none());
}
