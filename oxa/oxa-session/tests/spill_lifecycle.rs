//! Lifecycle locks are private, stable across reopen, and cover linked stores.
use oxa_session::{FsStore, SessionStore};
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

#[test]
fn cleanup_waits_for_publication_in_transitively_linked_stores() {
    let env = oxa_fake::TestEnv::new();
    let roots = ["sessions-a", "sessions-b", "sessions-c"].map(|s| env.root().join(s));
    let stores = roots.clone().map(FsStore::new);
    let cwd = Path::new("/work/project");
    let a = stores[0].create(cwd, None, None).unwrap();
    let b = stores[1].create(cwd, None, Some(&a.path)).unwrap();
    let c = stores[2].create(cwd, None, Some(&b.path)).unwrap();
    // A cleanup in A must coordinate with publication in C, even though only B links them.
    let publication = stores[2].spill_lifecycle_lock(&c).unwrap();
    let (started_tx, started_rx) = mpsc::channel();
    let (locked_tx, locked_rx) = mpsc::channel();
    let root = roots[0].clone();
    let cleanup = std::thread::spawn(move || {
        let reopened = FsStore::new(root);
        started_tx.send(()).unwrap();
        let _held = reopened.spill_cleanup_lock().unwrap();
        locked_tx.send(()).unwrap();
    });
    started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let blocked = locked_rx.recv_timeout(Duration::from_millis(200)).is_err();
    drop(publication);
    locked_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    cleanup.join().unwrap();
    assert!(
        blocked,
        "reclamation must not enumerate stores during publication"
    );
    for root in roots {
        let lock = root.join(".spill-lifecycle.lock");
        assert_eq!(fs::metadata(lock).unwrap().permissions().mode() & 0o777, 0o600);
    }
}

#[test]
fn lifecycle_locks_refuse_symlinks_and_special_files() {
    let env = oxa_fake::TestEnv::new();
    let root = env.root().join("sessions");
    let store = FsStore::new(root.clone());
    let source = store.create(Path::new("/work/project"), None, None).unwrap();
    let outside = env.root().join("untouched");
    fs::write(&outside, "unchanged").unwrap();
    let lock = root.join(".spill-lifecycle.lock");
    symlink(&outside, &lock).unwrap();
    assert!(store.spill_lifecycle_lock(&source).is_err());
    assert!(store.spill_cleanup_lock().is_err());
    assert_eq!(fs::read_to_string(&outside).unwrap(), "unchanged");
    fs::remove_file(&lock).unwrap();
    fs::create_dir(&lock).unwrap();
    assert!(store.spill_lifecycle_lock(&source).is_err());
    fs::remove_dir(&lock).unwrap();
    // Rejected acquisitions must release any earlier locks and allow normal work afterwards.
    drop(store.spill_lifecycle_lock(&source).unwrap());
    drop(store.spill_cleanup_lock().unwrap());
}
