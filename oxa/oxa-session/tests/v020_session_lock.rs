//! One writer per session file: a second opener is refused with the pid, a crash leaves no
//! stale block, a dropped lock releases the session, and `--fork` copies a locked session.
//! Isolation: `oxa_fake::TestEnv` for every file.

use oxa_fake::TestEnv;
use oxa_session::{fork_file, FsStore, SessionStore, LOCKED};
use std::process::Command;

fn entry() -> oxa_proto::Entry {
    oxa_proto::Entry {
        id: "e1".to_string(),
        parent: None,
        ts_ms: 1,
        body: oxa_proto::EntryBody::Msg {
            wire: oxa_proto::WireMsg::user("hi"),
            parts: Vec::new(),
        },
    }
}

fn store(env: &TestEnv) -> FsStore {
    FsStore::new(env.root().join("sessions"))
}

#[test]
fn a_second_opener_is_refused_with_the_pid() {
    let env = TestEnv::new();
    let a = store(&env);
    let s = a.create(&env.work(), None, None).unwrap();
    a.acquire(&s).unwrap();
    // Another process is another store (and another open file description).
    let b = store(&env);
    let err = b.acquire(&s).expect_err("locked");
    assert_eq!(err.code, LOCKED);
    assert!(
        err.message.contains(&format!("pid {}", std::process::id())),
        "{}",
        err.message
    );
    assert!(err.message.contains("--fork"), "{}", err.message);
    // The holder itself may ask again.
    a.acquire(&s).unwrap();
}

#[test]
fn dropping_the_holder_releases_the_session_and_removes_the_sidecar() {
    let env = TestEnv::new();
    let a = store(&env);
    let s = a.create(&env.work(), None, None).unwrap();
    a.acquire(&s).unwrap();
    drop(a);
    assert!(!oxa_session::lock_path_of(&s.path).exists());
    store(&env).acquire(&s).expect("free again");
}

#[test]
fn fork_of_a_locked_session_works_and_the_copy_is_free() {
    let env = TestEnv::new();
    let a = store(&env);
    let s = a.create(&env.work(), None, None).unwrap();
    a.acquire(&s).unwrap();
    a.append(&s, &entry()).unwrap();
    let b = store(&env);
    let copy = fork_file(&b, &s, &env.work(), None).expect("fork reads the locked file");
    b.acquire(&copy).expect("the fork has its own lock");
    a.acquire(&s).unwrap();
}

/// A real second process holds the lock and is killed: no stale block remains (the kernel drops
/// the lock; the leftover sidecar file does not matter).
#[test]
fn a_crashed_holder_leaves_no_stale_block() {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;
    let env = TestEnv::new();
    let a = store(&env);
    let s = a.create(&env.work(), None, None).unwrap();
    let lock = oxa_session::lock_path_of(&s.path);
    // `flock(1)` holds the sidecar's lock from a child process, as a live oxa would.
    let mut child = match Command::new("flock")
        .arg(&lock)
        .args(["-c", "echo held; exec sleep 60"])
        .stdout(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return, // no util-linux flock on this machine
    };
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line.trim(), "held");
    assert_eq!(a.acquire(&s).expect_err("held by the child").code, LOCKED);
    child.kill().unwrap(); // a crash: no cleanup runs
    child.wait().unwrap();
    assert!(lock.exists(), "the sidecar stays after a crash");
    a.acquire(&s).expect("no stale block after the holder died");
}
