//! Desk Z09: the light thread index reads a bounded part of each session file, and the picker's
//! threaded sort and `thread_order` are one order.
//! Isolation: `oxa_fake::TestEnv` for every file. The byte counter is `/proc/self/io`, which
//! counts every thread, so the one test that reads it is the only test here that reads files.

use oxa_fake::TestEnv;
use oxa_proto::{Entry, EntryBody, Part, WireMsg};
use oxa_session::{
    fork_file, sort_sessions, thread_order, FsStore, Relation, SessionMeta, SessionRef, SessionSort,
    SessionStore, ThreadMeta, THREAD_HEAD_BYTES, THREAD_TAIL_BYTES,
};
use std::io::Write;
use std::path::PathBuf;

fn entry(id: &str, parent: Option<&str>, ts_ms: u64, body: EntryBody) -> Entry {
    Entry {
        id: id.to_string(),
        parent: parent.map(String::from),
        ts_ms,
        body,
    }
}

fn user(text: &str) -> EntryBody {
    EntryBody::Msg {
        wire: WireMsg::user(text),
        parts: vec![Part::Text(text.to_string())],
        tool: None,
    }
}

fn assistant(text: &str) -> EntryBody {
    EntryBody::Msg {
        wire: WireMsg::assistant(text),
        parts: vec![],
        tool: None,
    }
}

fn info(name: &str) -> EntryBody {
    EntryBody::SessionInfo {
        name: name.to_string(),
    }
}

/// Bytes this process has read so far.
#[cfg(target_os = "linux")]
fn bytes_read() -> u64 {
    let text = std::fs::read_to_string("/proc/self/io").expect("/proc/self/io");
    let value = text.lines().find_map(|l| l.strip_prefix("rchar:"));
    value.and_then(|v| v.trim().parse().ok()).expect("rchar")
}

/// A session far larger than the read windows: a prompt, `fill` assistant entries of 8 KiB
/// with a name in the middle, and a name at the end when `tail_name` is given. Returns the
/// session and the time of its last entry.
fn long_session(
    st: &FsStore,
    env: &TestEnv,
    k: usize,
    fill: usize,
    tail_name: Option<&str>,
) -> (SessionRef, u64) {
    let s = st.create(&env.work(), None, None).unwrap();
    let t0 = st.header(&s).unwrap().ts_ms;
    let pad = "x".repeat(8 * 1024);
    let mut entries = vec![entry("e0", None, t0 + 1, user(&format!("prompt {k}")))];
    for n in 1..=fill {
        let parent = format!("e{}", n - 1);
        let body = if n == fill / 2 {
            info("lost in the middle")
        } else {
            assistant(&pad)
        };
        entries.push(entry(&format!("e{n}"), Some(&parent), t0 + 1 + n as u64, body));
    }
    if let Some(name) = tail_name {
        let parent = format!("e{fill}");
        let ts = t0 + 2 + fill as u64;
        entries.push(entry("last", Some(&parent), ts, info(name)));
    }
    st.append_many(&s, &entries).unwrap();
    (s, entries.last().unwrap().ts_ms)
}

#[test]
fn index_reads_a_bounded_part_of_each_file_and_agrees_with_the_picker() {
    let env = TestEnv::new();
    let st = FsStore::new(env.root().join("sessions"));
    let cwd = env.work();

    // A short session and a fork of it: here the index must say what the picker says.
    let small = st.create(&cwd, None, None).unwrap();
    let t0 = st.header(&small).unwrap().ts_ms;
    st.append(&small, &entry("u1", None, t0 + 10, user("  first\n question ")))
        .unwrap();
    st.append(&small, &entry("a1", Some("u1"), t0 + 20, assistant("answer")))
        .unwrap();
    st.append(&small, &entry("i1", Some("a1"), t0 + 30, info(" Small one ")))
        .unwrap();
    let fork = fork_file(&st, &small, &cwd, None).unwrap();
    // A torn last line (a crash in the middle of a write) is not an entry.
    let mut raw = std::fs::OpenOptions::new()
        .append(true)
        .open(&small.path)
        .unwrap();
    raw.write_all(br#"{"type":"session_info","id":"zz","parentId":"i1","timestamp":"2030-01-01T00:00:00.000Z","name":"torn"#)
        .unwrap();
    drop(raw);

    // Thirty long sessions, about 480 KiB each; every second one is named at its end.
    let mut long = Vec::new();
    for k in 0..30 {
        let name = (k % 2 == 0).then(|| format!("tail name {k}"));
        long.push((k, long_session(&st, &env, k, 60, name.as_deref())));
    }
    // A session whose last line alone is larger than the tail window.
    let (giant, _) = long_session(&st, &env, 99, 20, None);
    let giant_t0 = st.header(&giant).unwrap().ts_ms;
    let huge = assistant(&"y".repeat(200 * 1024));
    st.append(
        &giant,
        &entry("huge", Some("e20"), giant_t0 + 1_000_000_000, huge),
    )
    .unwrap();

    let files = st.thread_files(Some(&cwd));
    assert_eq!(files.len(), 33);
    let on_disk: u64 = files.iter().map(|p| std::fs::metadata(p).unwrap().len()).sum();
    let window = THREAD_HEAD_BYTES + THREAD_TAIL_BYTES + 1;
    assert!(
        on_disk > 3 * 33 * window,
        "the fixture is too small to show a bound"
    );

    #[cfg(target_os = "linux")]
    let before = bytes_read();
    let index = st.thread_index(Some(&cwd));
    #[cfg(target_os = "linux")]
    {
        // 64 KiB of slack covers reading `/proc/self/io` itself.
        let read = bytes_read() - before;
        assert!(
            read <= 33 * window + 64 * 1024,
            "the index read {read} bytes of {on_disk} on disk; the bound is {window} per file"
        );
    }

    assert_eq!(index.len(), 33, "one row per session file");
    let paths: Vec<&PathBuf> = index.iter().map(|m| &m.sref.path).collect();
    assert_eq!(paths, files.iter().collect::<Vec<_>>(), "rows are in file order");
    let row = |s: &SessionRef| index.iter().find(|m| m.sref == *s).expect("row");
    for m in &index {
        assert_eq!(m.bytes, std::fs::metadata(&m.sref.path).unwrap().len());
        assert_eq!(m.cwd, cwd);
        assert_eq!(
            st.thread_meta(&m.sref.path).as_ref(),
            Some(m),
            "one file gives the same row"
        );
    }

    // Short files: the same facts as the picker's full read.
    let listed = st.list(Some(&cwd));
    for s in [&small, &fork] {
        let m = row(s);
        let full = listed.iter().find(|l| l.sref == *s).expect("listed");
        assert_eq!(m.name, full.name);
        assert_eq!(m.name.as_deref(), Some("Small one"));
        assert_eq!(m.first_message, full.first_message);
        assert_eq!(m.first_message, "first question");
        assert_eq!(m.modified_ms, full.modified_ms);
        assert_eq!(m.parent, full.parent_session);
    }
    assert_eq!(row(&small).relation, Relation::Root);
    assert_eq!(row(&fork).relation, Relation::Fork);
    assert_eq!(
        row(&fork).parent.as_deref(),
        Some(small.path.to_string_lossy().as_ref())
    );

    // Long files: the prompt from the head, the name and the last activity from the tail. The
    // name in the middle is outside both windows; only the full read of the picker finds it.
    for (k, (s, last_ms)) in &long {
        let m = row(s);
        assert_eq!(m.first_message, format!("prompt {k}"));
        assert_eq!(m.modified_ms, *last_ms);
        assert_eq!(m.relation, Relation::Root);
        let full = listed.iter().find(|l| l.sref == *s).expect("listed");
        if k % 2 == 0 {
            assert_eq!(m.name, Some(format!("tail name {k}")));
            assert_eq!(m.name, full.name);
        } else {
            assert_eq!(m.name, None);
            assert_eq!(full.name.as_deref(), Some("lost in the middle"));
        }
    }

    // A last line longer than the tail window: the time falls back to the file's own, which is
    // now, never the unread entry's.
    let m = row(&giant);
    assert_eq!(m.first_message, "prompt 99");
    assert!(m.modified_ms >= giant_t0 && m.modified_ms < giant_t0 + 1_000_000_000);

    // The fork is placed directly below its source.
    let order = thread_order(&index);
    assert_eq!(order.len(), index.len());
    let at = order.iter().position(|&(i, _)| index[i].sref == small).unwrap();
    assert_eq!(order[at].1, 0);
    assert_eq!(index[order[at + 1].0].sref, fork);
    assert_eq!(order[at + 1].1, 1);
}

// ---------------------------------------------------------------------------------------------
// The order (no files)
// ---------------------------------------------------------------------------------------------

/// `(path, parent, modified)`: two roots, forks two levels deep, an orphan and a parent cycle.
const TABLE: [(&str, Option<&str>, u64); 8] = [
    ("/s/a", None, 10),
    ("/s/b", Some("/s/a"), 50),
    ("/s/c", None, 30),
    ("/s/d", Some("/s/b"), 20),
    ("/s/e", Some("/s/a"), 60),
    ("/s/orphan", Some("/s/gone"), 40),
    ("/s/x", Some("/s/y"), 5),
    ("/s/y", Some("/s/x"), 6),
];

fn sref(path: &str) -> SessionRef {
    SessionRef {
        path: PathBuf::from(path),
        id: format!("id:{path}"),
    }
}

fn picker_metas() -> Vec<SessionMeta> {
    let meta = |&(path, parent, modified): &(&str, Option<&str>, u64)| SessionMeta {
        sref: sref(path),
        cwd: PathBuf::from("/w"),
        name: None,
        created_ms: 0,
        modified_ms: modified,
        message_count: 0,
        first_message: String::new(),
        all_text: String::new(),
        parent_session: parent.map(String::from),
    };
    TABLE.iter().map(meta).collect()
}

fn thread(path: &str, parent: Option<&str>, modified_ms: u64) -> ThreadMeta {
    ThreadMeta {
        sref: sref(path),
        cwd: PathBuf::from("/w"),
        parent: parent.map(String::from),
        relation: if parent.is_some() {
            Relation::Fork
        } else {
            Relation::Root
        },
        name: None,
        first_message: String::new(),
        modified_ms,
        bytes: 0,
    }
}

#[test]
fn the_picker_order_is_unchanged_and_thread_order_is_the_same_order() {
    // a's thread ranks by its newest member (e, 60); a child follows its parent, newest first;
    // an orphan is a root; the members of a cycle come last.
    let expect: [(usize, u16); 8] = [(0, 0), (4, 1), (1, 1), (3, 2), (5, 0), (2, 0), (6, 0), (7, 1)];

    let metas = picker_metas();
    let mut picker: Vec<usize> = (0..metas.len()).collect();
    sort_sessions(&metas, &mut picker, SessionSort::Threaded);
    assert_eq!(picker, expect.map(|(i, _)| i));

    let threads: Vec<ThreadMeta> = TABLE.iter().map(|&(p, parent, m)| thread(p, parent, m)).collect();
    assert_eq!(thread_order(&threads), expect);

    // A search result is a subset: a parent that was filtered out leaves its child a root.
    let mut subset = vec![1, 3, 2];
    sort_sessions(&metas, &mut subset, SessionSort::Threaded);
    assert_eq!(subset, [1, 3, 2]);
    let mut subset = vec![2, 3, 4];
    sort_sessions(&metas, &mut subset, SessionSort::Threaded);
    assert_eq!(subset, [4, 2, 3]);
}

#[test]
fn a_parent_given_by_session_id_nests_like_one_given_by_path() {
    // A worker's session file has no parent; the worker registry names the parent by its id.
    let mut worker = thread("/s/worker", Some("id:/s/main"), 70);
    worker.relation = Relation::Worker;
    let threads = vec![thread("/s/other", None, 90), worker, thread("/s/main", None, 10)];
    assert_eq!(thread_order(&threads), [(0, 0), (2, 0), (1, 1)]);
}

#[test]
fn a_long_chain_of_forks_is_ordered_without_deep_recursion() {
    let n = 100_000;
    let path = |i: usize| format!("/s/{i}");
    let threads: Vec<ThreadMeta> = (0..n)
        .map(|i| thread(&path(i), i.checked_sub(1).map(path).as_deref(), i as u64))
        .collect();
    let order = thread_order(&threads);
    assert_eq!(order.len(), n);
    assert_eq!(order[0], (0, 0));
    assert_eq!(order[1000], (1000, 1000));
    assert_eq!(order[n - 1], (n - 1, u16::MAX), "depth saturates");
}
