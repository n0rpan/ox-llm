use oxa_fake::TestEnv;
use std::{fs, os::unix::fs::PermissionsExt};

#[test]
fn an_incomplete_tail_is_quarantined_without_corrupting_later_samples() {
    let env = TestEnv::new();
    let path = env.tmp().join("shadow.jsonl");
    fs::write(&path, b"{}\n{\"partial\":").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let mut log = oxa_work::shadow::open_log(&path).unwrap();
    oxa_work::shadow::run(
        &mut log,
        true,
        || Ok(oxa_work::board::Board::default()),
        |_| panic!("unexpected wait"),
    )
    .unwrap();
    let text = fs::read_to_string(path).unwrap();
    assert!(text
        .lines()
        .all(|line| serde_json::from_str::<serde_json::Value>(line).is_ok()));
    let fragment = fs::read_dir(env.tmp())
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.file_name().unwrap().to_string_lossy().contains("incomplete"))
        .unwrap();
    assert_eq!(fs::read(&fragment).unwrap(), b"{\"partial\":");
    assert_eq!(
        fs::metadata(fragment).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn sampling_appends_and_waits_five_minutes_and_stops_on_read_error() {
    let env = TestEnv::new();
    let path = env.tmp().join("shadow.jsonl");
    let mut log = oxa_work::shadow::open_log(&path).unwrap();
    assert!(oxa_work::shadow::open_log(&path).is_err());
    let mut reads = 0;
    let mut waits = vec![];
    assert!(oxa_work::shadow::run(
        &mut log,
        false,
        || {
            reads += 1;
            if reads == 3 {
                return Err("synthetic read failure".into());
            }
            Ok(oxa_work::board::Board {
                observed_at: reads * 300,
                entries: vec![],
            })
        },
        |duration| waits.push(duration)
    )
    .is_err());
    assert_eq!(fs::read_to_string(&path).unwrap().lines().count(), 2);
    assert_eq!(waits.len(), 2);
    assert!(waits
        .iter()
        .all(|d| *d <= oxa_work::shadow::INTERVAL && d.as_secs() >= 299));
    drop(log);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(oxa_work::shadow::open_log(&path).is_err());
}
