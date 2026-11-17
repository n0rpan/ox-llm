use oxa_fake::TestEnv;
use std::{fs, os::unix::fs::PermissionsExt};

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
