//! P6: the spill directory of a session goes with the session (`remove_spill_dir`), a wipe's
//! purge unlinks instead of using `trash`, and a hostile id cannot reach outside the spill root.
use oxa_session::{
    count_files, purge_session_file, remove_all_spill, remove_session_preserving_spills, remove_spill_dir,
};
use std::fs;

fn tmp_root(tag: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("oxa-p6-{}-{tag}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    root
}

#[test]
fn spill_dir_of_one_session_is_removed_and_the_others_stay() {
    let tmp = tmp_root("spill");
    fs::create_dir_all(tmp.join("aa-1/nested")).unwrap();
    fs::write(tmp.join("aa-1/bash.log"), "x").unwrap();
    fs::write(tmp.join("aa-1/nested/sub.txt"), "y").unwrap();
    fs::create_dir_all(tmp.join("bb-2")).unwrap();
    fs::write(tmp.join("bb-2/bash.log"), "z").unwrap();
    assert_eq!(remove_spill_dir(&tmp, "aa-1").unwrap(), 2);
    assert!(!tmp.join("aa-1").exists());
    assert!(tmp.join("bb-2/bash.log").is_file());
    // Missing is not an error.
    assert_eq!(remove_spill_dir(&tmp, "aa-1").unwrap(), 0);
    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn an_id_that_is_not_a_session_id_is_refused() {
    let tmp = tmp_root("hostile");
    fs::create_dir_all(tmp.join("keep")).unwrap();
    fs::write(tmp.join("keep/x"), "k").unwrap();
    for bad in ["../keep", "a/b", "..", ""] {
        assert!(remove_spill_dir(&tmp, bad).is_err(), "accepted {bad:?}");
    }
    assert!(tmp.join("keep/x").is_file());
    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn all_spill_is_counted_and_removed_and_purge_is_idempotent() {
    let tmp = tmp_root("all");
    fs::create_dir_all(tmp.join("codemode")).unwrap();
    fs::write(tmp.join("codemode/c.txt"), "c").unwrap();
    fs::write(tmp.join("s.txt"), "s").unwrap();
    assert_eq!(count_files(&tmp), 2);
    assert_eq!(remove_all_spill(&tmp).unwrap(), 2);
    assert!(!tmp.exists());
    let file = tmp
        .parent()
        .unwrap()
        .join(format!("oxa-p6-{}-purge.jsonl", std::process::id()));
    fs::write(&file, "{}\n").unwrap();
    purge_session_file(&file).unwrap();
    assert!(!file.exists());
    purge_session_file(&file).unwrap();
}

/// #576: deleting a session keeps its spill directory while a fork or clone still names it
/// (`parentSession`); a session nobody forked loses it as before.
#[test]
fn deleting_a_forked_session_keeps_its_spill_dir_until_the_last_child_is_gone() {
    use oxa_session::{remove_spill_dir_unless_forked, FsStore, SessionStore};
    let tmp = tmp_root("forked");
    let store = FsStore::new(tmp.join("sessions"));
    let tmp_dir = tmp.join("tmp");
    let cwd = std::path::Path::new("/work/project");
    let src = store.create(cwd, None, None).unwrap();
    let child = store.create(cwd, None, Some(src.path.as_path())).unwrap();
    let other = store.create(cwd, None, None).unwrap();
    for s in [&src, &other] {
        fs::create_dir_all(tmp_dir.join(&s.id)).unwrap();
        fs::write(tmp_dir.join(&s.id).join("bash.log"), "x").unwrap();
    }
    // A session without children: its spill files go with it.
    remove_session_preserving_spills(&store, &tmp_dir, &other).unwrap();
    assert_eq!(
        remove_spill_dir_unless_forked(&store, &tmp_dir, &other).unwrap(),
        1
    );
    assert!(!tmp_dir.join(&other.id).exists());
    // The source has a child: the child's wire still names the source's spill files.
    remove_session_preserving_spills(&store, &tmp_dir, &src).unwrap();
    assert_eq!(remove_spill_dir_unless_forked(&store, &tmp_dir, &src).unwrap(), 0);
    assert!(tmp_dir.join(&src.id).join("bash.log").is_file());
    // The last child's deletion also reclaims the retained source's output.
    remove_session_preserving_spills(&store, &tmp_dir, &child).unwrap();
    assert_eq!(
        remove_spill_dir_unless_forked(&store, &tmp_dir, &child).unwrap(),
        1
    );
    assert!(!tmp_dir.join(&src.id).exists());
    let _ = fs::remove_dir_all(&tmp);
}

/// Deleting the middle of a fork chain must not sever the surviving leaf's ancestry.
#[test]
fn spill_cleanup_follows_forks_of_forks_in_every_deletion_order() {
    use oxa_proto::{Entry, EntryBody, WireMsg};
    use oxa_session::{clone_session, fork_file, remove_spill_dir_unless_forked, FsStore, SessionStore};
    for order in [[1, 0, 2], [0, 1, 2], [0, 2, 1], [1, 2, 0], [2, 0, 1], [2, 1, 0]] {
        let tmp = tmp_root(&format!("chain-{order:?}"));
        let store = FsStore::new(tmp.join("sessions"));
        let tmp_dir = tmp.join("tmp");
        let cwd = std::path::Path::new("/work/project");
        let src = store.create(cwd, None, None).unwrap();
        fs::create_dir_all(tmp_dir.join(&src.id)).unwrap();
        let output = tmp_dir.join(&src.id).join("bash.log");
        fs::write(&output, "inherited output").unwrap();
        store
            .append(
                &src,
                &Entry {
                    id: "00000001".into(),
                    parent: None,
                    ts_ms: 1,
                    body: EntryBody::Msg {
                        wire: WireMsg::user(format!("full output in {}", output.display())),
                        parts: vec![],
                        tool: None,
                    },
                },
            )
            .unwrap();
        let child = fork_file(&store, &src, cwd, None).unwrap();
        let grandchild = clone_session(&store, &child, "00000001", cwd).unwrap();
        assert_eq!(store.load(&grandchild).unwrap(), store.load(&src).unwrap());
        let sessions = [src, child, grandchild];
        let mut live = [true; 3];
        for index in order {
            remove_session_preserving_spills(&store, &tmp_dir, &sessions[index]).unwrap();
            remove_spill_dir_unless_forked(&store, &tmp_dir, &sessions[index]).unwrap();
            live[index] = false;
            if live.iter().any(|&v| v) {
                assert_eq!(
                    fs::read_to_string(&output).unwrap(),
                    "inherited output",
                    "{order:?}"
                );
            } else {
                assert!(!output.exists(), "orphaned source output after {order:?}");
            }
        }
        let _ = fs::remove_dir_all(&tmp);
    }
}

#[test]
fn cross_store_forks_keep_spills_until_the_last_descendant_is_deleted() {
    use oxa_proto::{Entry, EntryBody, WireMsg};
    use oxa_session::{
        clone_session, fork_file, inherited_spill_dirs, remove_spill_dir_unless_forked, FsStore, SessionStore,
    };
    for order in [[0, 1, 2], [0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0]] {
        let env = oxa_fake::TestEnv::new();
        let tmp = env.root().join("tmp");
        let roots = ["sessions-a", "sessions-b", "sessions-c"].map(|s| env.root().join(s));
        let stores = roots.clone().map(FsStore::new);
        let cwd = std::path::Path::new("/work/project");
        let source = stores[0].create(cwd, Some("source"), None).unwrap();
        fs::create_dir_all(tmp.join("source")).unwrap();
        let output = tmp.join("source/output.txt");
        fs::write(&output, "inherited output").unwrap();
        stores[0]
            .append(
                &source,
                &Entry {
                    id: "00000001".into(),
                    parent: None,
                    ts_ms: 1,
                    body: EntryBody::Msg {
                        wire: WireMsg::user(format!("full output in {}", output.display())),
                        parts: vec![],
                        tool: None,
                    },
                },
            )
            .unwrap();
        let child = fork_file(&stores[1], &source, cwd, None).unwrap();
        let grandchild = clone_session(&stores[2], &child, "00000001", cwd).unwrap();
        assert_eq!(
            stores[2].load(&grandchild).unwrap(),
            stores[0].load(&source).unwrap()
        );
        let sessions = [source, child, grandchild];
        let mut live = [true; 3];
        for index in order {
            // Dependencies must survive reopening, not just the process that forked.
            let store = FsStore::new(roots[index].clone());
            remove_session_preserving_spills(&store, &tmp, &sessions[index]).unwrap();
            remove_spill_dir_unless_forked(&store, &tmp, &sessions[index]).unwrap();
            live[index] = false;
            for survivor in (0..3).filter(|&i| live[i]) {
                assert_eq!(
                    fs::read_to_string(&output).unwrap(),
                    "inherited output",
                    "{order:?}"
                );
                let reopened = FsStore::new(roots[survivor].clone());
                if survivor != 0 {
                    assert!(inherited_spill_dirs(&reopened, &tmp, &sessions[survivor])
                        .contains(&tmp.join("source")));
                }
            }
        }
        assert!(!output.exists(), "orphaned cross-store output after {order:?}");
        assert_eq!(fs::read_dir(tmp.join(".spill-ancestry")).unwrap().count(), 0);
    }
}

#[test]
fn spill_cleanup_waits_for_siblings_in_other_workspaces() {
    use oxa_session::{remove_spill_dir_unless_forked, FsStore, SessionStore};
    let tmp = tmp_root("siblings");
    let store = FsStore::new(tmp.join("sessions"));
    let tmp_dir = tmp.join("tmp");
    let cwd = std::path::Path::new("/work/project");
    let src = store.create(cwd, None, None).unwrap();
    let child = store.create(cwd, None, Some(&src.path)).unwrap();
    let sibling = store
        .create(std::path::Path::new("/work/other"), None, Some(&src.path))
        .unwrap();
    fs::create_dir_all(tmp_dir.join(&src.id)).unwrap();
    fs::write(tmp_dir.join(&src.id).join("bash.log"), "x").unwrap();
    for session in [&src, &child] {
        remove_session_preserving_spills(&store, &tmp_dir, session).unwrap();
        assert_eq!(
            remove_spill_dir_unless_forked(&store, &tmp_dir, session).unwrap(),
            0
        );
        assert!(tmp_dir.join(&src.id).join("bash.log").is_file());
    }
    remove_session_preserving_spills(&store, &tmp_dir, &sibling).unwrap();
    assert_eq!(
        remove_spill_dir_unless_forked(&store, &tmp_dir, &sibling).unwrap(),
        1
    );
    assert!(!tmp_dir.join(&src.id).exists());
    let _ = fs::remove_dir_all(&tmp);
}
