//! P6: the spill directory of a session goes with the session (`remove_spill_dir`), a wipe's
//! purge unlinks instead of using `trash`, and a hostile id cannot reach outside the spill root.
use oxa_session::{count_files, purge_session_file, remove_all_spill, remove_spill_dir};
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
    store.remove(&other).unwrap();
    assert_eq!(
        remove_spill_dir_unless_forked(&store, &tmp_dir, &other).unwrap(),
        1
    );
    assert!(!tmp_dir.join(&other.id).exists());
    // The source has a child: the child's wire still names the source's spill files.
    store.remove(&src).unwrap();
    assert_eq!(remove_spill_dir_unless_forked(&store, &tmp_dir, &src).unwrap(), 0);
    assert!(tmp_dir.join(&src.id).join("bash.log").is_file());
    // A child without children of its own has nothing to keep and nothing to remove.
    store.remove(&child).unwrap();
    assert_eq!(
        remove_spill_dir_unless_forked(&store, &tmp_dir, &child).unwrap(),
        0
    );
    let _ = fs::remove_dir_all(&tmp);
}
