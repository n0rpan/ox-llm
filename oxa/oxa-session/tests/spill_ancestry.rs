//! Retained ancestry is private, safe against path escapes, and reclaimed with the spills.
use oxa_session::{
    inherited_spill_dirs, remove_session_preserving_spills, remove_spill_dir_unless_forked, MemStore,
    SessionStore,
};
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::Path;

#[test]
fn retained_ancestry_is_private_and_only_lives_as_long_as_its_descendant() {
    let env = oxa_fake::TestEnv::new();
    let tmp = env.root().join("tmp");
    let store = MemStore::new();
    let cwd = Path::new("/work/project");
    let src = store.create(cwd, Some("source"), None).unwrap();
    let child = store.create(cwd, Some("child"), Some(&src.path)).unwrap();
    fs::create_dir_all(tmp.join("unrelated")).unwrap();
    fs::write(tmp.join("unrelated/keep.txt"), "keep").unwrap();
    remove_session_preserving_spills(&store, &tmp, &src).unwrap();
    let record = tmp.join(".spill-ancestry/source.json");
    assert_eq!(fs::metadata(&record).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(
        fs::metadata(record.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        inherited_spill_dirs(&store, &tmp, &child),
        vec![tmp.join("source")]
    );
    assert_eq!(remove_spill_dir_unless_forked(&store, &tmp, &src).unwrap(), 0);
    assert!(
        record.is_file(),
        "an ancestor with no spills still connects the chain"
    );
    remove_session_preserving_spills(&store, &tmp, &child).unwrap();
    assert_eq!(remove_spill_dir_unless_forked(&store, &tmp, &child).unwrap(), 0);
    assert_eq!(fs::read_dir(tmp.join(".spill-ancestry")).unwrap().count(), 0);
    assert_eq!(
        fs::read_to_string(tmp.join("unrelated/keep.txt")).unwrap(),
        "keep"
    );
    // Repeated cleanup has nothing to remove.
    assert_eq!(remove_spill_dir_unless_forked(&store, &tmp, &child).unwrap(), 0);
}

#[test]
fn cleanup_in_another_session_store_preserves_unchecked_ancestry() {
    use oxa_session::FsStore;

    for remove_source_from_other_store in [false, true] {
        let env = oxa_fake::TestEnv::new();
        let tmp = env.root().join("tmp");
        let root_a = env.root().join("sessions-a");
        let store_a = FsStore::new(root_a.clone());
        let store_b = FsStore::new(env.root().join("sessions-b"));
        let cwd = Path::new("/work/project");
        let source = store_a.create(cwd, Some("source"), None).unwrap();
        let child = store_a.create(cwd, Some("child"), Some(&source.path)).unwrap();
        let unrelated = store_b.create(cwd, Some("unrelated"), None).unwrap();
        for session in [&source, &unrelated] {
            fs::create_dir_all(tmp.join(&session.id)).unwrap();
            fs::write(tmp.join(&session.id).join("output.txt"), "output").unwrap();
        }
        // FsStore can also resolve and delete an explicit path outside its own root.
        let deleting_store = if remove_source_from_other_store {
            &store_b
        } else {
            &store_a
        };
        remove_session_preserving_spills(deleting_store, &tmp, &source).unwrap();
        assert_eq!(
            remove_spill_dir_unless_forked(deleting_store, &tmp, &source).unwrap(),
            0
        );
        remove_session_preserving_spills(&store_b, &tmp, &unrelated).unwrap();
        assert_eq!(
            remove_spill_dir_unless_forked(&store_b, &tmp, &unrelated).unwrap(),
            1
        );
        assert!(!tmp.join("unrelated").exists());
        assert_eq!(
            fs::read_to_string(tmp.join("source/output.txt")).unwrap(),
            "output"
        );
        assert!(tmp.join(".spill-ancestry/source.json").is_file());
        // Reopening the original store must still grant the child's inherited read scope.
        let reopened = FsStore::new(root_a);
        assert_eq!(
            inherited_spill_dirs(&reopened, &tmp, &child),
            vec![tmp.join("source")]
        );
        remove_session_preserving_spills(&reopened, &tmp, &child).unwrap();
        assert_eq!(
            remove_spill_dir_unless_forked(&reopened, &tmp, &child).unwrap(),
            1
        );
        assert!(!tmp.join("source").exists());
        assert_eq!(fs::read_dir(tmp.join(".spill-ancestry")).unwrap().count(), 0);
    }
}

#[test]
fn retained_ancestry_refuses_symlinked_metadata_before_deleting_the_session() {
    for link_file in [false, true] {
        let env = oxa_fake::TestEnv::new();
        let tmp = env.root().join("tmp");
        let outside = env.root().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::create_dir_all(&tmp).unwrap();
        let root = tmp.join(".spill-ancestry");
        let target = outside.join("keep.txt");
        fs::write(&target, "keep").unwrap();
        if link_file {
            fs::create_dir_all(&root).unwrap();
            symlink(&target, root.join("source.json")).unwrap();
        } else {
            symlink(&outside, &root).unwrap();
        }
        let store = MemStore::new();
        let src = store
            .create(Path::new("/work/project"), Some("source"), None)
            .unwrap();
        assert!(remove_session_preserving_spills(&store, &tmp, &src).is_err());
        assert!(
            store.header(&src).is_ok(),
            "failed preservation must not delete the session"
        );
        assert_eq!(fs::read_to_string(&target).unwrap(), "keep");
        assert!(remove_spill_dir_unless_forked(&store, &tmp, &src).is_err());
    }
}

#[test]
fn retained_ancestry_refuses_invalid_ids_and_mismatched_records() {
    for bad_id in ["../outside", "other-id"] {
        let env = oxa_fake::TestEnv::new();
        let tmp = env.root().join("tmp");
        let store = MemStore::new();
        let cwd = Path::new("/work/project");
        let src = store.create(cwd, Some("source"), None).unwrap();
        let child = store.create(cwd, Some("child"), Some(&src.path)).unwrap();
        remove_session_preserving_spills(&store, &tmp, &src).unwrap();
        let record = tmp.join(".spill-ancestry/source.json");
        let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&record).unwrap()).unwrap();
        value["id"] = bad_id.into();
        fs::write(record, serde_json::to_vec(&value).unwrap()).unwrap();
        let outside = env.root().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("keep.txt"), "keep").unwrap();
        assert!(inherited_spill_dirs(&store, &tmp, &child).is_empty());
        assert!(remove_spill_dir_unless_forked(&store, &tmp, &src).is_err());
        assert_eq!(fs::read_to_string(outside.join("keep.txt")).unwrap(), "keep");
    }
}

#[test]
fn malformed_records_do_not_hide_unrelated_valid_ancestry() {
    for bad_record in [
        "{\"type\":\"session\",",
        "{}",
        r#"{"type":"session","version":3,"id":"../outside"}"#,
    ] {
        let env = oxa_fake::TestEnv::new();
        let tmp = env.root().join("tmp");
        let store = MemStore::new();
        let cwd = Path::new("/work/project");
        let src = store.create(cwd, Some("source"), None).unwrap();
        let child = store.create(cwd, Some("child"), Some(&src.path)).unwrap();
        fs::create_dir_all(tmp.join("source")).unwrap();
        fs::write(tmp.join("source/output.txt"), "inherited output").unwrap();
        remove_session_preserving_spills(&store, &tmp, &src).unwrap();
        fs::write(tmp.join(".spill-ancestry/broken.json"), bad_record).unwrap();
        assert_eq!(
            inherited_spill_dirs(&store, &tmp, &child),
            vec![tmp.join("source")],
            "unrelated invalid record: {bad_record}"
        );
        // Incomplete ancestry cannot safely justify deleting any retained output.
        assert!(remove_spill_dir_unless_forked(&store, &tmp, &src).is_err());
        assert_eq!(
            fs::read_to_string(tmp.join("source/output.txt")).unwrap(),
            "inherited output"
        );
        fs::remove_file(tmp.join(".spill-ancestry/broken.json")).unwrap();
        remove_session_preserving_spills(&store, &tmp, &child).unwrap();
        assert_eq!(remove_spill_dir_unless_forked(&store, &tmp, &child).unwrap(), 1);
        assert!(!tmp.join("source").exists());
    }
}

#[test]
fn retained_ancestry_is_published_by_replacement_not_in_place() {
    let env = oxa_fake::TestEnv::new();
    let tmp = env.root().join("tmp");
    let store = MemStore::new();
    let src = store
        .create(Path::new("/work/project"), Some("source"), None)
        .unwrap();
    remove_session_preserving_spills(&store, &tmp, &src).unwrap();
    let record = tmp.join(".spill-ancestry/source.json");
    let snapshot = env.root().join("previous.json");
    fs::hard_link(&record, &snapshot).unwrap();
    let previous = fs::read(&snapshot).unwrap();
    // Simulate replacing a record on a later preservation attempt.
    let src = store
        .create(Path::new("/work/other"), Some("source"), None)
        .unwrap();
    remove_session_preserving_spills(&store, &tmp, &src).unwrap();
    assert_ne!(fs::read(&record).unwrap(), previous);
    assert_eq!(
        fs::read(&snapshot).unwrap(),
        previous,
        "published records must never be truncated"
    );
    assert_eq!(fs::metadata(&record).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(fs::read_dir(record.parent().unwrap()).unwrap().count(), 1);
}

#[test]
fn unpublished_temporary_records_do_not_affect_ancestry_or_cleanup() {
    let env = oxa_fake::TestEnv::new();
    let tmp = env.root().join("tmp");
    let store = MemStore::new();
    let src = store
        .create(Path::new("/work/project"), Some("source"), None)
        .unwrap();
    let child = store
        .create(Path::new("/work/project"), Some("child"), Some(&src.path))
        .unwrap();
    remove_session_preserving_spills(&store, &tmp, &src).unwrap();
    fs::write(tmp.join(".spill-ancestry/.interrupted.tmp"), "{").unwrap();
    assert_eq!(
        inherited_spill_dirs(&store, &tmp, &child),
        vec![tmp.join("source")]
    );
    assert_eq!(remove_spill_dir_unless_forked(&store, &tmp, &src).unwrap(), 0);
    remove_session_preserving_spills(&store, &tmp, &child).unwrap();
    assert_eq!(remove_spill_dir_unless_forked(&store, &tmp, &child).unwrap(), 0);
    assert!(!tmp.join(".spill-ancestry/source.json").exists());
}

#[test]
fn failed_publication_keeps_the_session_and_removes_staging_files() {
    let env = oxa_fake::TestEnv::new();
    let tmp = env.root().join("tmp");
    let root = tmp.join(".spill-ancestry");
    // A directory at the destination makes rename fail after the staged file was synced.
    fs::create_dir_all(root.join("source.json")).unwrap();
    let store = MemStore::new();
    let src = store
        .create(Path::new("/work/project"), Some("source"), None)
        .unwrap();
    assert!(remove_session_preserving_spills(&store, &tmp, &src).is_err());
    assert!(store.header(&src).is_ok());
    assert!(root.join("source.json").is_dir());
    assert_eq!(
        fs::read_dir(&root).unwrap().count(),
        1,
        "no staging files left after failure"
    );
}

#[test]
fn retained_ancestry_cycles_do_not_hang_or_grant_the_active_scope_as_an_ancestor() {
    let env = oxa_fake::TestEnv::new();
    let tmp = env.root().join("tmp");
    let store = MemStore::new();
    let cwd = Path::new("/work/project");
    let a = store.create(cwd, Some("a"), Some(Path::new("mem:b"))).unwrap();
    let b = store.create(cwd, Some("b"), Some(&a.path)).unwrap();
    assert_eq!(inherited_spill_dirs(&store, &tmp, &a), vec![tmp.join("b")]);
    remove_session_preserving_spills(&store, &tmp, &b).unwrap();
    remove_spill_dir_unless_forked(&store, &tmp, &b).unwrap();
    assert_eq!(inherited_spill_dirs(&store, &tmp, &a), vec![tmp.join("b")]);
    remove_session_preserving_spills(&store, &tmp, &a).unwrap();
    remove_spill_dir_unless_forked(&store, &tmp, &a).unwrap();
    assert_eq!(fs::read_dir(tmp.join(".spill-ancestry")).unwrap().count(), 0);
}
