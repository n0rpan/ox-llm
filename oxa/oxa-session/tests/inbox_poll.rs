//! Off Linux the inbox polls its directory instead of using inotify. These tests pin that
//! poll path to the same single-consumer, delivery, symlink and size rules as the Linux path.
#![cfg(not(target_os = "linux"))]

use oxa_fake::TestEnv;
use oxa_session::{send_inbox, Inbox};
use std::fs;
use std::os::unix::fs::symlink;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
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
fn inbox_poll_path_revisits_rejected_message() {
    let env = TestEnv::new();
    let agent = env.root().join("agent");
    send_inbox(&agent, "session-poll", "retry me").unwrap();
    let accept = Arc::new(AtomicBool::new(false));
    let callback_accept = accept.clone();
    let (tx, rx) = mpsc::channel();
    let watch = Inbox::watch(&agent, "session-poll", move |text| {
        // Decide before notifying: the receiver may flip `accept` as soon as it wakes.
        let accepted = callback_accept.load(Ordering::SeqCst);
        tx.send((text, accepted)).unwrap();
        accepted
    })
    .unwrap();

    assert_eq!(
        rx.recv_timeout(Duration::from_secs(3)).unwrap(),
        ("retry me".to_string(), false)
    );
    accept.store(true, Ordering::SeqCst);
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(3)).unwrap(),
        ("retry me".to_string(), true),
        "a later poll retries a message rejected for lack of capacity"
    );
    drop(watch);
    assert_eq!(fs::read_dir(agent.join("inbox/session-poll")).unwrap().count(), 0);
}

#[test]
fn inbox_poll_path_drains_more_than_one_bounded_batch() {
    let env = TestEnv::new();
    let agent = env.root().join("agent");
    for index in 0..130 {
        send_inbox(&agent, "session-poll", &format!("message {index}")).unwrap();
    }
    let (tx, rx) = mpsc::channel();
    let watch = Inbox::watch(&agent, "session-poll", move |text| tx.send(text).is_ok()).unwrap();

    for _ in 0..130 {
        rx.recv_timeout(Duration::from_secs(3))
            .expect("every bounded batch is enumerated");
    }
    drop(watch);
    assert_eq!(fs::read_dir(agent.join("inbox/session-poll")).unwrap().count(), 0);
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

/// The poll is pinned to the directory this watcher opened and locked: replacing the pathname
/// afterwards must not hand it another directory's messages.
#[test]
fn inbox_poll_path_stays_pinned_when_the_directory_is_replaced() {
    let env = TestEnv::new();
    let agent = env.root().join("agent");
    let dir = agent.join("inbox/session-poll");
    fs::create_dir_all(&dir).unwrap();
    let (tx, rx) = mpsc::channel();
    let watch = Inbox::watch(&agent, "session-poll", move |text| tx.send(text).is_ok()).unwrap();

    // Swap the pinned path for a symlink to a directory holding a message.
    let attacker = env.root().join("attacker");
    fs::create_dir_all(&attacker).unwrap();
    fs::write(attacker.join("1-stolen.msg"), "stolen").unwrap();
    let moved = env.root().join("moved");
    fs::rename(&dir, &moved).unwrap();
    symlink(&attacker, &dir).unwrap();

    assert!(
        rx.recv_timeout(Duration::from_millis(1500)).is_err(),
        "a replaced inbox path must not redirect the watcher"
    );
    assert!(
        attacker.join("1-stolen.msg").exists(),
        "the replacement directory's file is left alone"
    );

    // The pinned directory still works: what lands in it is delivered.
    fs::write(moved.join("2-pinned.msg"), "pinned").unwrap();
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(3)).unwrap(),
        "pinned",
        "the watcher keeps reading the directory it opened"
    );
    drop(watch);
}
