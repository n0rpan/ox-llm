//! The inbox is local IPC, not a socket, and must stay in private, no-follow directories.
use oxa_fake::TestEnv;
use oxa_session::{send_inbox, Inbox};
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::sync::mpsc;
use std::time::Duration;

#[test]
fn inbox_backlog_live_atomic_delivery_and_restart() {
    let env = TestEnv::new();
    let agent = env.root().join("agent");
    send_inbox(&agent, "session-638", "before startup").unwrap();
    let (tx, rx) = mpsc::channel();
    let watch = Inbox::watch(&agent, "session-638", move |s| tx.send(s).is_ok()).unwrap();
    assert_eq!(rx.recv_timeout(Duration::from_secs(3)).unwrap(), "before startup");
    let dir = agent.join("inbox/session-638");
    fs::write(dir.join(".partial.tmp"), "not yet complete").unwrap();
    send_inbox(&agent, "session-638", "after startup").unwrap();
    assert_eq!(rx.recv_timeout(Duration::from_secs(3)).unwrap(), "after startup");
    assert!(rx.try_recv().is_err(), "temporary files are never messages");
    assert!(
        Inbox::watch(&agent, "session-638", |_| true).is_err(),
        "one consumer per inbox"
    );
    drop(watch);
    send_inbox(&agent, "session-638", "after shutdown").unwrap();
    let (tx, rx) = mpsc::channel();
    let watch = Inbox::watch(&agent, "session-638", move |s| tx.send(s).is_ok()).unwrap();
    assert_eq!(rx.recv_timeout(Duration::from_secs(3)).unwrap(), "after shutdown");
    drop(watch);
    assert_eq!(
        fs::read_dir(dir).unwrap().count(),
        1,
        "only unpublished temp file remains"
    );
}

#[test]
fn inbox_private_permissions_and_unaccepted_messages_are_retained() {
    let env = TestEnv::new();
    let agent = env.root().join("agent");
    send_inbox(&agent, "session-638", "keep until accepted").unwrap();
    let dir = agent.join("inbox/session-638");
    for path in [agent.join("inbox"), dir.clone()] {
        assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o700);
    }
    let path = fs::read_dir(&dir).unwrap().next().unwrap().unwrap().path();
    assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o600);
    let (tx, rx) = mpsc::channel();
    let watch = Inbox::watch(&agent, "session-638", move |_| {
        tx.send(()).unwrap();
        false
    })
    .unwrap();
    rx.recv_timeout(Duration::from_secs(3)).unwrap();
    drop(watch);
    assert_eq!(fs::read_dir(dir).unwrap().count(), 1);
}

#[test]
fn inbox_rejects_path_ids_symlinks_hardlinks_and_non_regular_files() {
    let env = TestEnv::new();
    let agent = env.root().join("agent");
    for id in ["", ".", "..", "../other", "a/b", "a\\b", "/tmp", "a\nb"] {
        assert!(send_inbox(&agent, id, "bad").is_err(), "{id:?}");
    }
    fs::create_dir_all(&agent).unwrap();
    let outside = env.root().join("outside");
    fs::create_dir(&outside).unwrap();
    symlink(&outside, agent.join("inbox")).unwrap();
    assert!(send_inbox(&agent, "session-638", "bad").is_err());
    fs::remove_file(agent.join("inbox")).unwrap();
    fs::create_dir(agent.join("inbox")).unwrap();
    symlink(&outside, agent.join("inbox/session-638")).unwrap();
    assert!(send_inbox(&agent, "session-638", "bad").is_err());
    assert!(Inbox::watch(&agent, "session-638", |_| true).is_err());
    fs::remove_file(agent.join("inbox/session-638")).unwrap();
    send_inbox(&agent, "session-638", "legitimate").unwrap();
    let secret = outside.join("not-a-message");
    fs::write(&secret, "do not deliver").unwrap();
    let dir = agent.join("inbox/session-638");
    symlink(&secret, dir.join("symlink.msg")).unwrap();
    fs::hard_link(&secret, dir.join("hardlink.msg")).unwrap();
    fs::create_dir(dir.join("directory.msg")).unwrap();
    let (tx, rx) = mpsc::channel();
    let watch = Inbox::watch(&agent, "session-638", move |s| tx.send(s).is_ok()).unwrap();
    assert_eq!(rx.recv_timeout(Duration::from_secs(3)).unwrap(), "legitimate");
    drop(watch);
    assert!(rx.try_recv().is_err());
    assert_eq!(fs::read_to_string(secret).unwrap(), "do not deliver");
}

#[test]
fn inbox_concurrent_writers_do_not_overwrite_messages() {
    let env = TestEnv::new();
    let agent = env.root().join("agent");
    let writers: Vec<_> = (0..12)
        .map(|n| {
            let agent = agent.clone();
            std::thread::spawn(move || send_inbox(&agent, "session-638", &format!("message {n}")).unwrap())
        })
        .collect();
    for writer in writers {
        writer.join().unwrap();
    }
    let (tx, rx) = mpsc::channel();
    let watch = Inbox::watch(&agent, "session-638", move |s| tx.send(s).is_ok()).unwrap();
    let mut received: Vec<_> = (0..12)
        .map(|_| rx.recv_timeout(Duration::from_secs(3)).unwrap())
        .collect();
    received.sort();
    received.dedup();
    assert_eq!(received.len(), 12);
    drop(watch);
}

#[test]
fn inbox_wipe_covers_orphans_temporary_files_and_rejects_symlink_parents() {
    use oxa_session::remove_inbox;
    let env = TestEnv::new();
    let agent = env.root().join("agent");
    assert_eq!(remove_inbox(&agent, None).unwrap(), 0);
    send_inbox(&agent, "session-638", "queued").unwrap();
    send_inbox(&agent, "orphan", "no session file").unwrap();
    fs::write(agent.join("inbox/session-638/.partial.tmp"), "unfinished").unwrap();
    assert_eq!(remove_inbox(&agent, Some("session-638")).unwrap(), 2);
    assert!(!agent.join("inbox/session-638").exists());
    assert!(agent.join("inbox/orphan").exists());
    assert_eq!(remove_inbox(&agent, None).unwrap(), 1);
    assert!(!agent.join("inbox").exists());
    let outside = env.root().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::create_dir(outside.join("session-638")).unwrap();
    fs::write(outside.join("session-638/keep.msg"), "untouched").unwrap();
    symlink(&outside, agent.join("inbox")).unwrap();
    assert!(remove_inbox(&agent, Some("session-638")).is_err());
    assert!(remove_inbox(&agent, None).is_err());
    fs::remove_file(agent.join("inbox")).unwrap();
    fs::create_dir(agent.join("inbox")).unwrap();
    symlink(outside.join("session-638"), agent.join("inbox/session-638")).unwrap();
    assert!(remove_inbox(&agent, Some("session-638")).is_err());
    assert!(remove_inbox(&agent, Some("../outside")).is_err());
    assert_eq!(
        fs::read_to_string(outside.join("session-638/keep.msg")).unwrap(),
        "untouched"
    );
}
