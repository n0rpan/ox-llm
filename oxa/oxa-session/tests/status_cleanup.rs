//! The approved status sibling is removed with its session, without following links.
use oxa_fake::TestEnv;
use oxa_session::{purge_session_file, FsStore, SessionStore};
use std::os::unix::fs::symlink;

#[test]
fn delete_and_purge_remove_only_the_exact_status_sibling() {
    for purge in [false, true] {
        let env = TestEnv::new();
        let store = FsStore::new(env.agent_dir().join("sessions"));
        let sref = store.create(&env.work(), None, None).unwrap();
        let status = sref.path.with_extension("status.json");
        let unrelated = status.with_extension("unrelated");
        std::fs::write(&status, "synthetic status").unwrap();
        std::fs::write(&unrelated, "keep").unwrap();
        if purge { purge_session_file(&sref.path).unwrap(); } else { store.remove(&sref).unwrap(); }
        assert!(!sref.path.exists());
        assert!(!status.exists());
        assert_eq!(std::fs::read_to_string(unrelated).unwrap(), "keep");
        if purge { purge_session_file(&sref.path).unwrap(); }
    }
}

#[test]
fn status_symlinks_are_unlinked_without_touching_their_targets() {
    for purge in [false, true] {
        let env = TestEnv::new();
        let store = FsStore::new(env.agent_dir().join("sessions"));
        let sref = store.create(&env.work(), None, None).unwrap();
        let status = sref.path.with_extension("status.json");
        let target = env.work().join("keep");
        std::fs::write(&target, "synthetic unrelated data").unwrap();
        symlink(&target, &status).unwrap();
        if purge { purge_session_file(&sref.path).unwrap(); } else { store.remove(&sref).unwrap(); }
        assert!(std::fs::symlink_metadata(status).is_err());
        assert_eq!(std::fs::read_to_string(target).unwrap(), "synthetic unrelated data");
    }
}

#[test]
fn cleanup_errors_are_reported_without_deleting_a_directory_or_its_session() {
    for purge in [false, true] {
        let env = TestEnv::new();
        let store = FsStore::new(env.agent_dir().join("sessions"));
        let sref = store.create(&env.work(), None, None).unwrap();
        let status = sref.path.with_extension("status.json");
        std::fs::create_dir(&status).unwrap();
        std::fs::write(status.join("keep"), "unchanged").unwrap();
        let result = if purge { purge_session_file(&sref.path) } else { store.remove(&sref).map(|_| ()) };
        assert!(result.is_err());
        assert!(sref.path.is_file());
        assert_eq!(std::fs::read_to_string(status.join("keep")).unwrap(), "unchanged");
    }
}
