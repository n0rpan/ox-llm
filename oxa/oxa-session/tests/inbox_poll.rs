//! Off Linux the inbox polls its directory instead of using inotify. These tests pin that
//! poll path to the same single-consumer, delivery, symlink and size rules as the Linux path.
#![cfg(not(target_os = "linux"))]

use oxa_fake::TestEnv;
use oxa_session::{send_inbox, Inbox};
use std::fs;
use std::os::unix::fs::symlink;
use std::sync::mpsc;
use std::time::Duration;

#[test]
fn inbox_poll_path_delivers_and_retains_unaccepted() {
    let env = TestEnv::new();
    let agent = env.root().join("agent");
    send_inbox(&agent, "session-poll", "first").unwrap();
    let (tx, rx) = mpsc::channel();
    let watch = Inbox::watch(&agent, "session-poll", move |text| tx.send(text).is_ok()).unwrap();
    assert_eq!(rx.recv_timeout(Duration::from_secs(3)).unwrap(), "first");
    send_inbox(&agent, "session-poll", "second").unwrap();
    assert_eq!(rx.recv_timeout(Duration::from_secs(3)).unwrap(), "second");
    assert!(
        Inbox::watch(&agent, "session-poll", |_| true).is_err(),
        "one consumer per inbox"
    );
    drop(watch);
    assert_eq!(
        fs::read_dir(agent.join("inbox/session-poll")).unwrap().count(),
        0,
        "accepted messages are removed"
    );
}

#[test]
fn inbox_poll_path_skips_symlinks_and_oversize_files() {
    let env = TestEnv::new();
    let agent = env.root().join("agent");
    let dir = agent.join("inbox/session-poll");
    fs::create_dir_all(&dir).unwrap();
    let outside = env.root().join("outside.msg");
    fs::write(&outside, "outside").unwrap();
    symlink(&outside, dir.join("1-link.msg")).unwrap();
    fs::write(dir.join("2-big.msg"), vec![b'x'; 256 * 1024 + 1]).unwrap();
    let (tx, rx) = mpsc::channel();
    let watch = Inbox::watch(&agent, "session-poll", move |text| tx.send(text).is_ok()).unwrap();
    assert!(
        rx.recv_timeout(Duration::from_millis(1200)).is_err(),
        "neither a symlink nor an oversize file is delivered"
    );
    drop(watch);
    assert!(fs::symlink_metadata(dir.join("1-link.msg")).is_ok());
    assert!(fs::metadata(dir.join("2-big.msg")).is_ok());
    assert_eq!(fs::read_to_string(&outside).unwrap(), "outside");
}
