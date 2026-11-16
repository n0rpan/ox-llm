//! Retained ancestry is private, safe against path escapes, and reclaimed with the spills.
use oxa_session::{
    inherited_spill_dirs, remove_session_preserving_spills, remove_spill_dir_unless_forked, MemStore,
    SessionStore,
};
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};

fn record_path(tmp: &Path, session: &oxa_session::SessionRef) -> PathBuf {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(session.path.to_string_lossy().as_bytes());
    tmp.join(".spill-ancestry")
        .join(format!("{}-{hash:x}.json", session.id))
}

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
    let record = record_path(&tmp, &src);
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
fn reused_ids_preserve_every_original_identity_until_all_descendants_are_gone() {
    use oxa_session::FsStore;

    for other_store in [false, true] {
        let env = oxa_fake::TestEnv::new();
        let tmp = env.root().join("tmp");
        let store = FsStore::new(env.root().join("sessions"));
        let other = FsStore::new(env.root().join("other-sessions"));
        let source = store
            .create(Path::new("/work/original"), Some("reused"), None)
            .unwrap();
        let child = store
            .create(Path::new("/work/original"), Some("child"), Some(&source.path))
            .unwrap();
        fs::create_dir_all(tmp.join("reused")).unwrap();
        fs::write(tmp.join("reused/output.txt"), "keep").unwrap();
        remove_session_preserving_spills(&store, &tmp, &source).unwrap();
        remove_spill_dir_unless_forked(&store, &tmp, &source).unwrap();
        let deleting = if other_store { &other } else { &store };
        let replacement = deleting
            .create(Path::new("/work/replacement"), Some("reused"), None)
            .unwrap();
        remove_session_preserving_spills(deleting, &tmp, &replacement).unwrap();
        assert_eq!(
            remove_spill_dir_unless_forked(deleting, &tmp, &replacement).unwrap(),
            0
        );
        assert_eq!(fs::read_to_string(tmp.join("reused/output.txt")).unwrap(), "keep");
        assert_eq!(
            inherited_spill_dirs(&store, &tmp, &child),
            vec![tmp.join("reused")]
        );
        remove_session_preserving_spills(&store, &tmp, &child).unwrap();
        remove_spill_dir_unless_forked(&store, &tmp, &child).unwrap();
        // An unlinked store must keep the shared ID conservatively until it is checked too.
        if other_store {
            assert!(tmp.join("reused/output.txt").is_file());
            let bridge = other
                .create(Path::new("/work/replacement"), Some("bridge"), None)
                .unwrap();
            let leaf = store
                .create(Path::new("/work/original"), Some("leaf"), Some(&bridge.path))
                .unwrap();
            remove_session_preserving_spills(&other, &tmp, &bridge).unwrap();
            remove_session_preserving_spills(&store, &tmp, &leaf).unwrap();
            remove_spill_dir_unless_forked(&store, &tmp, &leaf).unwrap();
        }
        assert!(!tmp.join("reused").exists());
        assert_eq!(fs::read_dir(tmp.join(".spill-ancestry")).unwrap().count(), 0);
    }
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
        assert!(record_path(&tmp, &source).is_file());
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
fn cleanup_fails_closed_when_live_sessions_cannot_be_enumerated() {
    use oxa_session::FsStore;

    for failure in ["file", "directory", "header", "linked-file"] {
        let env = oxa_fake::TestEnv::new();
        let tmp = env.root().join("tmp");
        let store = FsStore::new(env.root().join("sessions-a"));
        let other = FsStore::new(env.root().join("sessions-b"));
        let cwd = Path::new("/work/project");
        let source = store.create(cwd, Some("source"), None).unwrap();
        let child_store = if failure == "linked-file" { &other } else { &store };
        let child = child_store
            .create(cwd, Some("child"), Some(&source.path))
            .unwrap();
        fs::create_dir_all(tmp.join("source")).unwrap();
        let output = tmp.join("source/output.txt");
        fs::write(&output, "keep").unwrap();
        remove_session_preserving_spills(&store, &tmp, &source).unwrap();
        let original = fs::read(&child.path).unwrap();
        let inaccessible = if failure == "directory" {
            child.path.parent().unwrap()
        } else {
            &child.path
        };
        if failure == "header" {
            fs::write(&child.path, "{\n").unwrap();
        } else {
            fs::set_permissions(inaccessible, fs::Permissions::from_mode(0o000)).unwrap();
        }
        let result = remove_spill_dir_unless_forked(&store, &tmp, &source);
        // Restore before assertions, including on a regression, so fixtures remain removable.
        if failure == "header" {
            fs::write(&child.path, original).unwrap();
        } else {
            fs::set_permissions(inaccessible, fs::Permissions::from_mode(0o700)).unwrap();
        }
        assert!(
            result.is_err(),
            "incomplete {failure} enumeration must abort cleanup"
        );
        assert_eq!(fs::read_to_string(&output).unwrap(), "keep");
        assert!(record_path(&tmp, &source).is_file());
        remove_session_preserving_spills(child_store, &tmp, &child).unwrap();
        assert_eq!(remove_spill_dir_unless_forked(&store, &tmp, &child).unwrap(), 1);
        assert!(!output.exists());
    }
}

#[test]
fn legacy_cross_store_forks_keep_untracked_source_spills() {
    use oxa_session::{fork_file, FsStore};

    for previously_retained in [false, true] {
        let env = oxa_fake::TestEnv::new();
        let tmp = env.root().join("tmp");
        let root_a = env.root().join("sessions-a");
        let root_b = env.root().join("sessions-b");
        let store_a = FsStore::new(root_a.clone());
        let store_b = FsStore::new(root_b.clone());
        let cwd = Path::new("/work/project");
        let source = store_a.create(cwd, Some("source"), None).unwrap();
        // Recreate a pre-upgrade header, without any tracking guarantee.
        let mut header = Vec::new();
        oxa_session::encode_header(&store_a.header(&source).unwrap(), &mut header);
        header.push(b'\n');
        fs::write(&source.path, header).unwrap();
        store_a
            .append(
                &source,
                &oxa_proto::Entry {
                    id: "00000001".into(),
                    parent: None,
                    ts_ms: 1,
                    body: oxa_proto::EntryBody::Msg {
                        wire: oxa_proto::WireMsg::user("synthetic history"),
                        parts: vec![],
                        tool: None,
                    },
                },
            )
            .unwrap();
        let child = fork_file(&store_b, &source, cwd, None).unwrap();
        // Pre-upgrade cross-store forks never published links in either store.
        for root in [&root_a, &root_b] {
            fs::remove_dir_all(root.join(".spill-stores")).unwrap();
        }
        fs::create_dir_all(tmp.join("source")).unwrap();
        let output = tmp.join("source/output.txt");
        fs::write(&output, "legacy output").unwrap();
        remove_session_preserving_spills(&store_a, &tmp, &source).unwrap();
        if previously_retained {
            let record = record_path(&tmp, &source);
            let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&record).unwrap()).unwrap();
            value.as_object_mut().unwrap().remove("spillStoresTracked");
            fs::write(record, serde_json::to_vec(&value).unwrap()).unwrap();
        }
        let reopened = FsStore::new(root_a);
        assert_eq!(
            remove_spill_dir_unless_forked(&reopened, &tmp, &source).unwrap(),
            0
        );
        assert_eq!(fs::read_to_string(&output).unwrap(), "legacy output");
        assert_eq!(
            inherited_spill_dirs(&store_b, &tmp, &child),
            vec![tmp.join("source")]
        );
        let unrelated = reopened.create(cwd, Some("unrelated"), None).unwrap();
        remove_session_preserving_spills(&reopened, &tmp, &unrelated).unwrap();
        remove_spill_dir_unless_forked(&reopened, &tmp, &unrelated).unwrap();
        assert!(
            output.is_file(),
            "later cleanup must not reclaim untracked ancestors"
        );
    }
}

#[test]
fn cross_store_links_are_private_deduplicated_and_fail_closed() {
    use oxa_session::FsStore;

    for corrupt in [false, true] {
        let env = oxa_fake::TestEnv::new();
        let tmp = env.root().join("tmp");
        let root_a = env.root().join("sessions-a");
        let root_b = env.root().join("sessions-b");
        let store_a = FsStore::new(root_a.clone());
        let store_b = FsStore::new(root_b.clone());
        let cwd = Path::new("/work/project");
        let source = store_a.create(cwd, Some("source"), None).unwrap();
        store_b.create(cwd, Some("child"), Some(&source.path)).unwrap();
        store_b.create(cwd, Some("sibling"), Some(&source.path)).unwrap();
        for root in [&root_a, &root_b] {
            let dir = root.join(".spill-stores");
            assert_eq!(fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
            let links: Vec<_> = fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).collect();
            assert_eq!(links.len(), 1, "repeated forks must not accumulate links");
            assert_eq!(
                fs::metadata(&links[0]).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        fs::create_dir_all(tmp.join("source")).unwrap();
        let output = tmp.join("source/output.txt");
        fs::write(&output, "keep").unwrap();
        remove_session_preserving_spills(&store_a, &tmp, &source).unwrap();
        // Interrupted publication is invisible; a malformed published link must block cleanup.
        fs::write(root_a.join(".spill-stores/.interrupted.tmp"), "{").unwrap();
        if corrupt {
            let link = fs::read_dir(root_a.join(".spill-stores"))
                .unwrap()
                .map(|e| e.unwrap().path())
                .find(|p| p.extension().is_some_and(|e| e == "json"))
                .unwrap();
            fs::write(link, "{").unwrap();
            assert!(remove_spill_dir_unless_forked(&store_a, &tmp, &source).is_err());
        } else {
            assert_eq!(
                remove_spill_dir_unless_forked(&store_a, &tmp, &source).unwrap(),
                0
            );
        }
        assert_eq!(fs::read_to_string(output).unwrap(), "keep");
    }
}

#[test]
fn cross_store_links_refuse_symlinks_without_publishing_a_child() {
    use oxa_session::FsStore;

    for link_file in [false, true] {
        let env = oxa_fake::TestEnv::new();
        let root_a = env.root().join("sessions-a");
        let root_b = env.root().join("sessions-b");
        let outside = env.root().join("outside");
        fs::create_dir_all(&outside).unwrap();
        let target = outside.join("keep.txt");
        fs::write(&target, "keep").unwrap();
        let store_a = FsStore::new(root_a.clone());
        let store_b = FsStore::new(root_b);
        let cwd = Path::new("/work/project");
        let source = store_a.create(cwd, Some("source"), None).unwrap();
        let dir = root_a.join(".spill-stores");
        if link_file {
            store_b.create(cwd, Some("child"), Some(&source.path)).unwrap();
            let link = fs::read_dir(&dir).unwrap().next().unwrap().unwrap().path();
            fs::remove_file(&link).unwrap();
            symlink(&target, link).unwrap();
        } else {
            symlink(&outside, &dir).unwrap();
        }
        assert!(store_b
            .create(cwd, Some("refused-child"), Some(&source.path))
            .is_err());
        assert!(!store_b.list(None).iter().any(|s| s.sref.id == "refused-child"));
        assert!(store_a.spill_cleanup_roots().is_err());
        assert_eq!(fs::read_to_string(target).unwrap(), "keep");
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
        let store = MemStore::new();
        let src = store
            .create(Path::new("/work/project"), Some("source"), None)
            .unwrap();
        if link_file {
            fs::create_dir_all(&root).unwrap();
            symlink(&target, record_path(&tmp, &src)).unwrap();
        } else {
            symlink(&outside, &root).unwrap();
        }
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
        let record = record_path(&tmp, &src);
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
    let record = record_path(&tmp, &src);
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
    assert!(!record_path(&tmp, &src).exists());
}

#[test]
fn failed_publication_keeps_the_session_and_removes_staging_files() {
    let env = oxa_fake::TestEnv::new();
    let tmp = env.root().join("tmp");
    let root = tmp.join(".spill-ancestry");
    let store = MemStore::new();
    let src = store
        .create(Path::new("/work/project"), Some("source"), None)
        .unwrap();
    // A directory at the destination makes rename fail after the staged file was synced.
    fs::create_dir_all(record_path(&tmp, &src)).unwrap();
    assert!(remove_session_preserving_spills(&store, &tmp, &src).is_err());
    assert!(store.header(&src).is_ok());
    assert!(record_path(&tmp, &src).is_dir());
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
