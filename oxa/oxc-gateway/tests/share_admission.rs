#![cfg(feature = "share")]
#[path = "common/share.rs"]
mod support;
use oxc_gateway::share::keys::{Error, Limits, Store};
use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::{symlink, PermissionsExt},
    process::{Command, Stdio},
};
use support::{limits, Directory};

#[test]
fn daily_limits_survive_restart_and_unknown_work_keeps_its_reservation() {
    let d = Directory::new();
    let store = d.store();
    let key = store
        .create(
            "Device",
            Limits {
                daily_requests: 3,
                daily_tokens: 100,
            },
        )
        .unwrap();
    let held = store.admit(key.bearer.expose(), 1, 60).unwrap().unwrap();
    assert_eq!(held.key(), &key.info);
    drop(held);
    assert_eq!(d.store().guest_usage(1, &key.info.id).unwrap(), (1, 60));
    assert_eq!(
        d.store().admit(key.bearer.expose(), 1, 41).unwrap_err(),
        Error::TokenLimit
    );
    store
        .admit(key.bearer.expose(), 1, 20)
        .unwrap()
        .unwrap()
        .settle(10)
        .unwrap();
    store
        .admit(key.bearer.expose(), 1, 30)
        .unwrap()
        .unwrap()
        .settle(0)
        .unwrap();
    assert_eq!(store.guest_usage(1, &key.info.id).unwrap(), (3, 70));
    assert_eq!(
        store.admit(key.bearer.expose(), 1, 1).unwrap_err(),
        Error::RequestLimit
    );
    assert!(store.admit(key.bearer.expose(), 2, 100).unwrap().is_some());
    assert_eq!(store.guest_usage(2, &key.info.id).unwrap(), (1, 100));
    assert_eq!(
        fs::metadata(d.0.join("share-usage-1.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let raw = fs::read_to_string(d.0.join("share-usage-1.json")).unwrap();
    assert!(!raw.contains(key.bearer.expose()));
}

#[test]
fn midnight_settlement_stays_on_admission_day_and_revoked_inflight_work_can_settle() {
    let d = Directory::new();
    let store = d.store();
    let key = store.create("Device", limits()).unwrap();
    let mut first = store.admit(key.bearer.expose(), 1, 800).unwrap().unwrap();
    let mut second = store.admit(key.bearer.expose(), 2, 800).unwrap().unwrap();
    store.revoke(&key.info.id).unwrap();
    assert!(store.admit(key.bearer.expose(), 2, 1).unwrap().is_none());
    first.settle(250).unwrap();
    second.settle(0).unwrap();
    assert_eq!(d.store().guest_usage(1, &key.info.id).unwrap(), (1, 250));
    assert_eq!(d.store().guest_usage(2, &key.info.id).unwrap(), (1, 0));
}

#[test]
fn untrusted_usage_and_busy_storage_never_release_reserved_capacity() {
    let d = Directory::new();
    let store = d.store();
    let key = store.create("Device", limits()).unwrap();
    let mut held = store.admit(key.bearer.expose(), 1, 1000).unwrap().unwrap();
    assert_eq!(held.settle(1001), Err(Error::Invalid));
    assert_eq!(store.guest_usage(1, &key.info.id).unwrap(), (1, 1000));
    let file = fs::File::open(d.0.join("share-keys.lock")).unwrap();
    let lock = nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusive).unwrap();
    assert_eq!(store.try_authenticate(key.bearer.expose()), Err(Error::Busy));
    assert_eq!(store.admit(key.bearer.expose(), 2, 10).unwrap_err(), Error::Busy);
    drop(lock);
    assert!(store.try_authenticate(key.bearer.expose()).unwrap().is_some());
}

#[test]
fn corrupt_or_linked_usage_and_impossible_holds_fail_closed() {
    let d = Directory::new();
    let store = d.store();
    let key = store.create("Device", limits()).unwrap();
    drop(store.admit(key.bearer.expose(), 1, 100).unwrap().unwrap());
    let path = d.0.join("share-usage-1.json");
    let valid: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let mut impossible = valid.clone();
    impossible["usage"][&key.info.id] = serde_json::json!([1, 99]);
    for bytes in [
        b"{".to_vec(),
        vec![b' '; 65537],
        serde_json::to_vec(&impossible).unwrap(),
    ] {
        fs::write(&path, bytes).unwrap();
        assert_eq!(
            store.admit(key.bearer.expose(), 1, 1).unwrap_err(),
            Error::Corrupt
        );
    }
    fs::remove_file(&path).unwrap();
    let outside = Directory::new();
    let target = outside.0.join("state");
    fs::write(&target, serde_json::to_vec(&valid).unwrap()).unwrap();
    symlink(&target, &path).unwrap();
    assert_eq!(store.admit(key.bearer.expose(), 1, 1).unwrap_err(), Error::Unsafe);
    assert_eq!(fs::read(&target).unwrap(), serde_json::to_vec(&valid).unwrap());
}

#[test]
fn concurrent_processes_cannot_overadmit_one_key() {
    if let Some(path) = std::env::var_os("OXA_SHARE_ADMISSION_TEST_DIRECTORY") {
        let store = Store::open(&std::path::PathBuf::from(path)).unwrap();
        let mut bearer = String::new();
        std::io::stdin().take(65).read_to_string(&mut bearer).unwrap();
        let start = std::time::Instant::now();
        loop {
            match store.admit(&bearer, 1, 30) {
                Err(Error::Busy) if start.elapsed() < std::time::Duration::from_secs(5) => {
                    std::thread::sleep(std::time::Duration::from_millis(1))
                }
                Ok(Some(hold)) => {
                    drop(hold);
                    break;
                }
                Err(Error::RequestLimit) => break,
                other => panic!("unexpected admission result: {other:?}"),
            }
        }
        return;
    }
    let d = Directory::new();
    let key = d
        .store()
        .create(
            "Device",
            Limits {
                daily_requests: 3,
                daily_tokens: 100,
            },
        )
        .unwrap();
    let binary = std::env::current_exe().unwrap();
    let children: Vec<_> = (0..8)
        .map(|_| {
            let mut c = Command::new(&binary)
                .args(["--exact", "concurrent_processes_cannot_overadmit_one_key"])
                .env_clear()
                .env("HOME", &d.0)
                .env("XDG_CONFIG_HOME", &d.0)
                .env("OXA_SHARE_ADMISSION_TEST_DIRECTORY", &d.0)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap();
            c.stdin
                .take()
                .unwrap()
                .write_all(key.bearer.expose().as_bytes())
                .unwrap();
            c
        })
        .collect();
    for mut c in children {
        assert!(c.wait().unwrap().success());
    }
    assert_eq!(d.store().guest_usage(1, &key.info.id).unwrap(), (3, 90));
}

#[test]
fn busy_settlement_can_retry_without_double_refund() {
    let d = Directory::new();
    let store = d.store();
    let key = store.create("Device", limits()).unwrap();
    let mut admission = store.admit(key.bearer.expose(), 1, 100).unwrap().unwrap();
    let file = fs::File::open(d.0.join("share-keys.lock")).unwrap();
    let lock = nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusive).unwrap();
    assert_eq!(admission.settle(8), Err(Error::Busy));
    drop(lock);
    admission.settle(8).unwrap();
    assert_eq!(admission.settle(8), Err(Error::Invalid));
    assert_eq!(store.guest_usage(1, &key.info.id).unwrap(), (1, 8));
}
