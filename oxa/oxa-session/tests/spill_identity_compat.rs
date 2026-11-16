//! ID-only records from earlier versions must coexist safely with path-keyed records.
use oxa_session::{
    inherited_spill_dirs, remove_session_preserving_spills, remove_spill_dir_unless_forked, FsStore,
    SessionStore,
};
use std::{fs, path::Path};

#[test]
fn legacy_records_survive_id_reuse_and_untracked_duplicates_block_reclamation() {
    for untracked_duplicate in [false, true] {
        let env = oxa_fake::TestEnv::new();
        let tmp = env.root().join("tmp");
        let root = tmp.join(".spill-ancestry");
        let store = FsStore::new(env.root().join("sessions"));
        let source = store.create(Path::new("/work/a"), Some("source"), None).unwrap();
        let child = store
            .create(Path::new("/work/a"), Some("child"), Some(&source.path))
            .unwrap();
        fs::create_dir_all(tmp.join("source")).unwrap();
        fs::write(tmp.join("source/output.txt"), "keep").unwrap();
        remove_session_preserving_spills(&store, &tmp, &source).unwrap();
        let published = fs::read_dir(&root).unwrap().next().unwrap().unwrap().path();
        let legacy = root.join("source.json");
        if untracked_duplicate {
            let mut record: serde_json::Value =
                serde_json::from_slice(&fs::read(&published).unwrap()).unwrap();
            record.as_object_mut().unwrap().remove("spillStoresTracked");
            fs::write(&legacy, serde_json::to_vec(&record).unwrap()).unwrap();
        } else {
            fs::rename(&published, &legacy).unwrap();
        }
        let reused = store.create(Path::new("/work/b"), Some("source"), None).unwrap();
        remove_session_preserving_spills(&store, &tmp, &reused).unwrap();
        assert_eq!(remove_spill_dir_unless_forked(&store, &tmp, &reused).unwrap(), 0);
        assert_eq!(
            inherited_spill_dirs(&store, &tmp, &child),
            vec![tmp.join("source")]
        );
        remove_session_preserving_spills(&store, &tmp, &child).unwrap();
        assert_eq!(
            remove_spill_dir_unless_forked(&store, &tmp, &child).unwrap(),
            usize::from(!untracked_duplicate)
        );
        if untracked_duplicate {
            assert_eq!(fs::read_to_string(tmp.join("source/output.txt")).unwrap(), "keep");
            assert!(legacy.is_file());
        } else {
            assert!(!tmp.join("source").exists());
            assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
        }
    }
}

#[test]
fn path_keyed_records_reject_tampered_original_identity() {
    let env = oxa_fake::TestEnv::new();
    let tmp = env.root().join("tmp");
    let store = FsStore::new(env.root().join("sessions"));
    let source = store.create(Path::new("/work/a"), Some("source"), None).unwrap();
    let child = store
        .create(Path::new("/work/a"), Some("child"), Some(&source.path))
        .unwrap();
    remove_session_preserving_spills(&store, &tmp, &source).unwrap();
    let record = fs::read_dir(tmp.join(".spill-ancestry"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&record).unwrap()).unwrap();
    value["sessionPath"] = "/work/other/session.jsonl".into();
    fs::write(&record, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(inherited_spill_dirs(&store, &tmp, &child).is_empty());
    assert!(remove_spill_dir_unless_forked(&store, &tmp, &source).is_err());
    assert!(record.is_file());
}
