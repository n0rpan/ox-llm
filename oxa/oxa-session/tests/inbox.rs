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

#[test]
fn inbox_oversized_files_stay_on_disk_without_delivery() {
    let env = TestEnv::new();
    let agent = env.root().join("agent");
    send_inbox(&agent, "bounded", "normal").unwrap();
    let oversized = agent.join("inbox/bounded/oversized.msg");
    assert_eq!(
        send_inbox(&agent, "bounded", &"x".repeat(256 * 1024 + 1))
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::InvalidInput
    );
    fs::write(&oversized, vec![b'x'; 256 * 1024 + 1]).unwrap();
    let (tx, rx) = mpsc::channel();
    let watch = Inbox::watch(&agent, "bounded", move |s| tx.send(s).is_ok()).unwrap();
    assert_eq!(rx.recv_timeout(Duration::from_secs(3)).unwrap(), "normal");
    drop(watch);
    assert!(
        rx.try_recv().is_err(),
        "oversized text must not enter memory queues"
    );
    assert!(oversized.exists());
}

#[test]
fn inbox_retries_backpressure_without_another_writer_event() {
    let env = TestEnv::new();
    let agent = env.root().join("agent");
    send_inbox(&agent, "bounded", "retained").unwrap();
    let (tx, rx) = mpsc::channel();
    let mut attempts = 0;
    let watch = Inbox::watch(&agent, "bounded", move |_| {
        attempts += 1;
        tx.send(attempts).unwrap();
        attempts > 1
    })
    .unwrap();
    assert_eq!(rx.recv_timeout(Duration::from_secs(3)).unwrap(), 1);
    watch.retry(); // The owner has released capacity; no producer event is needed.
    assert_eq!(rx.recv_timeout(Duration::from_secs(3)).unwrap(), 2);
    drop(watch);
    assert_eq!(fs::read_dir(agent.join("inbox/bounded")).unwrap().count(), 0);
}

#[test]
fn inbox_bounded_scan_advances_past_invalid_batches() {
    let env = TestEnv::new();
    let agent = env.root().join("agent");
    send_inbox(&agent, "bounded", "valid after invalid batch").unwrap();
    let dir = agent.join("inbox/bounded");
    for n in 0..70 {
        fs::write(dir.join(format!("000-{n:03}.msg")), [0xff]).unwrap();
    }
    let (tx, rx) = mpsc::channel();
    let watch = Inbox::watch(&agent, "bounded", move |s| tx.send(s).is_ok()).unwrap();
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(3)).unwrap(),
        "valid after invalid batch"
    );
    drop(watch);
    assert_eq!(
        fs::read_dir(dir).unwrap().count(),
        70,
        "invalid files are retained, not repeatedly scanned"
    );
}

#[test]
fn inbox_durable_pickup_receipt_survives_restart_and_stale_message() {
    let env = TestEnv::new();
    let agent = env.root().join("agent");
    let id = send_inbox(&agent, "durable", "follow up").unwrap();
    assert!(!Inbox::receipt(&agent, "durable", &id).unwrap());
    assert_eq!(
        Inbox::take(&agent, "durable").unwrap(),
        Some((id.clone(), "follow up".into()))
    );
    assert!(Inbox::receipt(&agent, "durable", &id).unwrap());
    assert_eq!(Inbox::take(&agent, "durable").unwrap(), None);
    // Simulate a crash after the receipt is durable but before unlinking the message.
    fs::write(agent.join("inbox/durable").join(format!("{id}.msg")), "follow up").unwrap();
    assert_eq!(Inbox::take(&agent, "durable").unwrap(), None);
    let (tx, rx) = mpsc::channel();
    let watch = Inbox::watch(&agent, "durable", move |s| tx.send(s).is_ok()).unwrap();
    send_inbox(&agent, "durable", "next message").unwrap();
    assert_eq!(rx.recv_timeout(Duration::from_secs(3)).unwrap(), "next message");
    drop(watch);
    assert!(rx.try_recv().is_err());
    assert!(Inbox::receipt(&agent, "durable", &id).unwrap());
}

#[test]
fn inbox_durable_pickup_respects_owner_lock_and_rejects_unsafe_receipts() {
    let env = TestEnv::new();
    let agent = env.root().join("agent");
    let id = send_inbox(&agent, "durable", "follow up").unwrap();
    let watch = Inbox::watch(&agent, "durable", |_| false).unwrap();
    assert!(Inbox::take(&agent, "durable").is_err());
    drop(watch);
    assert!(Inbox::receipt(&agent, "durable", "../outside").is_err());
    let outside = env.root().join("outside");
    fs::write(&outside, "untouched").unwrap();
    let receipt = agent.join("inbox/durable").join(format!("{id}.taken"));
    symlink(&outside, &receipt).unwrap();
    assert!(Inbox::receipt(&agent, "durable", &id).is_err());
    assert!(Inbox::take(&agent, "durable").is_err());
    fs::remove_file(&receipt).unwrap();
    fs::hard_link(&outside, &receipt).unwrap();
    assert!(Inbox::receipt(&agent, "durable", &id).is_err());
    assert_eq!(fs::read_to_string(outside).unwrap(), "untouched");
}

#[test]
fn inbox_durable_pickup_is_ordered_bounded_private_and_wipeable() {
    let env = TestEnv::new();
    let agent = env.root().join("agent");
    let first = send_inbox(&agent, "durable", "first").unwrap();
    let second = send_inbox(&agent, "durable", "second").unwrap();
    let dir = agent.join("inbox/durable");
    let outside = env.root().join("outside");
    fs::write(&outside, "untouched").unwrap();
    symlink(&outside, dir.join("000-symlink.msg")).unwrap();
    fs::hard_link(&outside, dir.join("000-hardlink.msg")).unwrap();
    fs::create_dir(dir.join("000-directory.msg")).unwrap();
    fs::write(dir.join("000-oversized.msg"), vec![b'x'; 256 * 1024 + 1]).unwrap();
    fs::write(dir.join("000-invalid.msg"), [0xff]).unwrap();
    assert_eq!(
        Inbox::take(&agent, "durable").unwrap(),
        Some((first.clone(), "first".into()))
    );
    assert_eq!(
        Inbox::take(&agent, "durable").unwrap(),
        Some((second.clone(), "second".into()))
    );
    assert_eq!(Inbox::take(&agent, "durable").unwrap(), None);
    for id in [first, second] {
        let marker = fs::metadata(dir.join(format!("{id}.taken"))).unwrap();
        assert_eq!(marker.len(), 0, "receipts must not retain prompt text");
        assert_eq!(marker.permissions().mode() & 0o777, 0o600);
    }
    oxa_session::remove_inbox(&agent, Some("durable")).unwrap();
    assert!(!dir.exists());
    assert_eq!(fs::read_to_string(outside).unwrap(), "untouched");
}

#[test]
fn inbox_durable_pickup_releases_shared_ownership_without_a_watch_receipt() {
    let env = TestEnv::new();
    let agent = env.root().join("agent");
    let id = send_inbox(&agent, "released", "still queued").unwrap();
    // A refusing watcher retains the message and owns the inbox while it lives.
    let watch = Inbox::watch(&agent, "released", |_| false).unwrap();
    assert!(Inbox::take(&agent, "released").is_err());
    assert!(
        !Inbox::receipt(&agent, "released", &id).unwrap(),
        "watch is not a receipt"
    );
    drop(watch);
    assert_eq!(
        Inbox::take(&agent, "released").unwrap(),
        Some((id.clone(), "still queued".into()))
    );
    assert!(Inbox::receipt(&agent, "released", &id).unwrap());
}

#[test]
fn inbox_unverifiable_receipt_suppresses_only_its_message() {
    let env = TestEnv::new();
    let agent = env.root().join("agent");
    let stale = send_inbox(&agent, "durable", "already taken").unwrap();
    let dir = agent.join("inbox/durable");
    // A crash left the message behind, and its marker is now a symlink (and later a hardlink)
    // that no pickup may trust. Every message here sorts inside the same batch, so stopping
    // the batch would strand the one behind it.
    let outside = env.root().join("outside");
    fs::write(&outside, "untouched").unwrap();
    symlink(&outside, dir.join(format!("{stale}.taken"))).unwrap();
    send_inbox(&agent, "durable", "still queued").unwrap();
    let (tx, rx) = mpsc::channel();
    let watch = Inbox::watch(&agent, "durable", move |s| tx.send(s).is_ok()).unwrap();
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(3)).unwrap(),
        "still queued",
        "a malformed marker must not stop the watcher behind it"
    );
    drop(watch);
    let receipt = dir.join(format!("{stale}.taken"));
    fs::remove_file(&receipt).unwrap();
    fs::hard_link(&outside, &receipt).unwrap();
    let (tx, rx) = mpsc::channel();
    let watch = Inbox::watch(&agent, "durable", move |s| tx.send(s).is_ok()).unwrap();
    fs::remove_file(dir.join(format!("{stale}.msg"))).unwrap();
    send_inbox(&agent, "durable", "after the hardlink").unwrap();
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(3)).unwrap(),
        "after the hardlink"
    );
    drop(watch);
    assert!(Inbox::receipt(&agent, "durable", &stale).is_err());
    assert_eq!(fs::read_to_string(&outside).unwrap(), "untouched");
}

#[test]
fn inbox_take_returns_after_the_receipt_is_durable() {
    // `take` reports success only once the receipt is durable and the consumed message is
    // gone from the directory: a caller that trusts the result, and a later watcher, both
    // see the same queue.
    let env = TestEnv::new();
    let agent = env.root().join("agent");
    send_inbox(&agent, "session", "first").unwrap();
    let (id, text) = Inbox::take(&agent, "session").unwrap().unwrap();
    let dir = agent.join("inbox/session");
    assert!(Inbox::receipt(&agent, "session", &id).unwrap());
    assert_eq!(text, "first");
    assert_eq!(
        fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>(),
        vec![format!("{id}.taken")],
        "pickup leaves only the payload-free receipt"
    );
    assert_eq!(Inbox::take(&agent, "session").unwrap(), None);
}
