//! Creating a session must not read the other session files, and fork/clone must write the
//! copied entries in one write, not one open+lock+write per entry (issue #578).
//! One test function in its own process: the counters come from `/proc/self/io`, which counts
//! every thread, so nothing else may run beside it.

use oxa_fake::TestEnv;
use oxa_proto::{Entry, EntryBody, WireMsg};
use oxa_session::{clone_session, fork_file, FsStore, SessionStore};

/// (bytes read, write syscalls) of this process so far.
fn io_counters() -> (u64, u64) {
    let text = std::fs::read_to_string("/proc/self/io").expect("/proc/self/io");
    let get = |key: &str| -> u64 {
        text.lines()
            .find_map(|l| l.strip_prefix(key))
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or_else(|| panic!("no {key} in /proc/self/io"))
    };
    (get("rchar:"), get("syscw:"))
}

fn entry(n: usize, parent: Option<usize>, pad: usize) -> Entry {
    Entry {
        id: format!("e{n}"),
        parent: parent.map(|p| format!("e{p}")),
        ts_ms: 1,
        body: EntryBody::Msg {
            wire: WireMsg::user(format!("{n} {}", "x".repeat(pad))),
            parts: Vec::new(),
            tool: None,
        },
    }
}

#[test]
fn create_reads_no_other_session_and_fork_and_clone_write_once() {
    let env = TestEnv::new();
    let store = FsStore::new(env.root().join("sessions"));
    // Many stored sessions, each larger than a header read buffer.
    for _ in 0..40 {
        let s = store.create(&env.work(), None, None).unwrap();
        store.append(&s, &entry(0, None, 20_000)).unwrap();
    }
    let (read0, _) = io_counters();
    store.create(&env.work(), None, None).unwrap();
    let (read1, _) = io_counters();
    assert!(
        read1 - read0 < 16 * 1024,
        "creating a session read {} bytes of other session files",
        read1 - read0
    );

    let src = store.create(&env.work(), None, None).unwrap();
    for n in 0..100 {
        store.append(&src, &entry(n, n.checked_sub(1), 10)).unwrap();
    }
    let (_, w0) = io_counters();
    let forked = fork_file(&store, &src, &env.work(), None).unwrap();
    let (_, w1) = io_counters();
    assert!(w1 - w0 < 20, "fork made {} write calls for 100 entries", w1 - w0);
    assert_eq!(store.load(&forked).unwrap().len(), 100);

    let (_, w2) = io_counters();
    let cloned = clone_session(&store, &src, "e99", &env.work()).unwrap();
    let (_, w3) = io_counters();
    assert!(w3 - w2 < 20, "clone made {} write calls for 100 entries", w3 - w2);
    assert_eq!(store.load(&cloned).unwrap().len(), 100);
}
