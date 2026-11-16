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
