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
