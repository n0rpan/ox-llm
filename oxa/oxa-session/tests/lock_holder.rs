//! `lock_holder` is a read-only liveness probe of the session lock sidecar.
use oxa_fake::TestEnv;
use oxa_session::{lock_holder, lock_path_of, SessionLock};

#[test]
fn holder_is_reported_only_while_the_lock_is_held() {
    let test = TestEnv::new();
    let session = test.root().join("one.jsonl");
    std::fs::write(&session, b"").unwrap();
    assert_eq!(lock_holder(&session), None);

    let lock = SessionLock::acquire(&session).unwrap();
    assert_eq!(lock_holder(&session), Some(std::process::id()));
    // A probe takes no lock and removes nothing: the holder still owns it and a second
    // acquire is still refused as before, not confused by the probe.
    assert_eq!(lock_holder(&session), Some(std::process::id()));
    assert!(SessionLock::acquire(&session).is_err());
    drop(lock);
    assert_eq!(lock_holder(&session), None);
    // After the holder left, a new opener succeeds: the probe left no lock behind.
    let again = SessionLock::acquire(&session).unwrap();
    assert_eq!(lock_holder(&session), Some(std::process::id()));
    drop(again);
}

#[test]
fn stale_sidecar_with_a_dead_pid_is_not_a_holder() {
    let test = TestEnv::new();
    let session = test.root().join("two.jsonl");
    std::fs::write(&session, b"").unwrap();
    for text in ["4000000000", "0", "not a pid", ""] {
        std::fs::write(lock_path_of(&session), text).unwrap();
        assert_eq!(lock_holder(&session), None, "{text:?}");
    }
    // The probe left the stale sidecar alone, and a real opener takes over.
    assert!(lock_path_of(&session).exists());
    assert!(SessionLock::acquire(&session).is_ok());
}
