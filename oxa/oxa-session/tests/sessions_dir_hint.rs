//! A session directory that cannot be created says how to relocate state.
#![cfg(unix)]
use oxa_fake::TestEnv;
use oxa_session::{FsStore, SessionStore};
use std::os::unix::fs::PermissionsExt;

#[test]
fn unwritable_sessions_dir_hints_relocation() {
    let env = TestEnv::new();
    let ro = env.root().join("ro");
    std::fs::create_dir_all(&ro).unwrap();
    std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o500)).unwrap();
    let store = FsStore::new(ro.join("sessions"));
    let err = store.create(&env.work(), None, None).unwrap_err().to_string();
    assert!(err.contains("cannot create session directory"), "{err}");
    assert!(
        err.contains("OXA_AGENT_DIR") && err.contains("--session-dir"),
        "{err}"
    );
}
