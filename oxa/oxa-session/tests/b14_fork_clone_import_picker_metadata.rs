//! B14 acceptance tests: Fork, clone, picker metadata.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_fake::TestEnv;
use oxa_proto::{codes, Entry, EntryBody, Part, WireMsg};
use oxa_session::{
    clone_session, fork_file, fork_session, meta_of, search_sessions, set_label, set_name, sort_sessions,
    FsStore, SessionMeta, SessionRef, SessionSort, SessionStore, Tree,
};
use std::path::{Path, PathBuf};

const BASE_MS: u64 = 1_750_000_000_000;

fn store(env: &TestEnv) -> FsStore {
    FsStore::new(env.root().join("sessions"))
}

fn user(id: &str, parent: Option<&str>, text: &str) -> Entry {
    Entry {
        id: id.to_string(),
        parent: parent.map(String::from),
        ts_ms: BASE_MS + id_offset(id),
        body: EntryBody::Msg {
            wire: WireMsg::user(text),
            parts: vec![Part::Text(text.to_string())],
            tool: None,
        },
    }
}

fn assistant(id: &str, parent: Option<&str>, text: &str) -> Entry {
    Entry {
        id: id.to_string(),
        parent: parent.map(String::from),
        ts_ms: BASE_MS + id_offset(id),
        body: EntryBody::Msg {
            wire: WireMsg::assistant(text),
            parts: vec![],
            tool: None,
        },
    }
}

/// A deterministic timestamp offset from the last hex digit of an id.
fn id_offset(id: &str) -> u64 {
    u64::from(u8::from_str_radix(&id[id.len() - 1..], 16).unwrap_or(0)) * 1000
}

/// The tree used by the fork and clone tests:
/// `a` (user) -> `b` (assistant) -> `c` (user, left branch) and `d` (assistant) -> `e` (user).
fn make_tree(st: &FsStore, cwd: &Path) -> SessionRef {
    let src = st.create(cwd, None, None).unwrap();
    st.append(&src, &user("0000000a", None, "start")).unwrap();
    st.append(&src, &assistant("0000000b", Some("0000000a"), "reply"))
        .unwrap();
    st.append(&src, &user("0000000c", Some("0000000b"), "left branch"))
        .unwrap();
    st.append(&src, &assistant("0000000d", Some("0000000b"), "right branch"))
        .unwrap();
    st.append(&src, &user("0000000e", Some("0000000d"), "right follow-up"))
        .unwrap();
    src
}

fn ids(entries: &[Entry]) -> Vec<String> {
    entries.iter().map(|e| e.id.clone()).collect()
}

fn meta(
    path: &str,
    name: Option<&str>,
    parent: Option<&str>,
    modified: u64,
    first: &str,
    all: &str,
) -> SessionMeta {
    SessionMeta {
        sref: SessionRef {
            path: PathBuf::from(path),
            id: path.to_string(),
        },
        cwd: PathBuf::from("/work"),
        name: name.map(String::from),
        created_ms: 0,
        modified_ms: modified,
        message_count: 0,
        first_message: first.to_string(),
        all_text: all.to_string(),
        parent_session: parent.map(String::from),
    }
}

#[test]
fn picker_metadata_excludes_recorded_env_notes() {
    let env = TestEnv::new();
    let st = store(&env);
    let src = st.create(&env.work(), None, None).unwrap();
    let note = "<env>\nDate: synthetic-date\nModel: hidden-model\n</env>";
    for (id, typed, note) in [
        ("0000000a", "", note),
        ("0000000b", "fix the picker", "[local time 12:34 UTC]"),
    ] {
        let content = if typed.is_empty() {
            note.to_string()
        } else {
            format!("{typed}\n\n{note}")
        };
        let mut entry = user(id, None, &content);
        if let EntryBody::Msg { parts, .. } = &mut entry.body {
            parts.push(Part::EnvNote(note.to_string()));
        }
        st.append(&src, &entry).unwrap();
    }
    // Literal user-authored blocks have no provenance and must remain searchable.
    st.append(&src, &user("0000000c", None, "<env>literal</env>"))
        .unwrap();
    let entries = st.load(&src).unwrap();
    let m = meta_of(&src, &st.header(&src).unwrap(), &entries);
    assert_eq!(m.first_message, "fix the picker");
    assert_eq!(m.all_text, "fix the picker\n<env>literal</env>");
    let metas = [m];
    assert!(search_sessions(&metas, "hidden-model", false).is_empty());
    assert!(search_sessions(&metas, "12:34", false).is_empty());
    assert_eq!(search_sessions(&metas, "literal", false), [0]);
    assert_eq!(search_sessions(&metas, "fix picker", false), [0]);
}

/// and the branch up to the entry
#[test]
fn fork_header_has_parent_session() {
    let env = TestEnv::new();
    let st = store(&env);
    let src = make_tree(&st, &env.work());

    let dst = fork_session(&st, &src, "0000000b", &env.work()).unwrap();
    let header = st.header(&dst).unwrap();
    assert_eq!(header.parent_session.as_deref(), Some(src.path.to_str().unwrap()));
    assert_eq!(header.cwd, env.work());
    assert_ne!(dst.id, src.id, "a fork is a new session");

    let entries = st.load(&dst).unwrap();
    assert_eq!(ids(&entries), ["0000000a", "0000000b"]);
    assert_eq!(entries[1].parent.as_deref(), Some("0000000a"));

    let deeper = fork_session(&st, &src, "0000000e", &env.work()).unwrap();
    assert_eq!(
        ids(&st.load(&deeper).unwrap()),
        ["0000000a", "0000000b", "0000000d", "0000000e"]
    );

    let missing = fork_session(&st, &src, "ffffffff", &env.work()).unwrap_err();
    assert_eq!(missing.code, codes::NOT_FOUND);
}

/// ids kept
#[test]
fn clone_copies_active_branch() {
    let env = TestEnv::new();
    let st = store(&env);
    let src = make_tree(&st, &env.work());
    // A label on an entry of the branch is carried into the clone.
    set_label(&st, &src, "0000000e", "0000000b", Some("keep")).unwrap();

    let dst = clone_session(&st, &src, "0000000e", &env.work()).unwrap();
    assert_eq!(
        st.header(&dst).unwrap().parent_session.as_deref(),
        Some(src.path.to_str().unwrap())
    );

    let entries = st.load(&dst).unwrap();
    // The active branch of `e` is a, b, d, e (the left branch `c` is not copied).
    let copied: Vec<&Entry> = entries
        .iter()
        .filter(|e| !matches!(e.body, EntryBody::Label { .. }))
        .collect();
    assert_eq!(
        copied.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
        ["0000000a", "0000000b", "0000000d", "0000000e"]
    );
    for pair in copied.windows(2) {
        assert_eq!(
            pair[1].parent.as_deref(),
            Some(pair[0].id.as_str()),
            "parents kept"
        );
    }
    assert_eq!(Tree::new(entries.clone()).label("0000000b"), Some("keep"));

    let missing = clone_session(&st, &src, "0000000c-not-here", &env.work()).unwrap_err();
    assert_eq!(missing.code, codes::NOT_FOUND);
}

/// entries appended
#[test]
fn labels_set_and_clear() {
    let env = TestEnv::new();
    let st = store(&env);
    let src = make_tree(&st, &env.work());
    let before = st.load(&src).unwrap().len();

    let set = set_label(&st, &src, "0000000e", "0000000b", Some("  ship it ")).unwrap();
    let after_set = st.load(&src).unwrap();
    assert_eq!(after_set.len(), before + 1, "a label is an appended entry");
    let label_entry = after_set.last().unwrap();
    assert_eq!(label_entry.id, set);
    assert_eq!(
        label_entry.parent.as_deref(),
        Some("0000000e"),
        "appended under the given leaf"
    );
    assert_eq!(
        Tree::new(after_set.clone()).label("0000000b"),
        Some("ship it"),
        "trimmed"
    );

    set_label(&st, &src, "0000000e", "0000000b", None).unwrap();
    let after_clear = st.load(&src).unwrap();
    assert_eq!(after_clear.len(), before + 2);
    assert_eq!(
        Tree::new(after_clear).label("0000000b"),
        None,
        "the last label wins; None clears"
    );

    let bad_target = set_label(&st, &src, "0000000e", "ffffffff", Some("x")).unwrap_err();
    assert_eq!(bad_target.code, codes::NOT_FOUND);
    let bad_leaf = set_label(&st, &src, "ffffffff", "0000000b", Some("x")).unwrap_err();
    assert_eq!(bad_leaf.code, codes::NOT_FOUND);
    assert_eq!(
        st.load(&src).unwrap().len(),
        before + 2,
        "a refused label appends nothing"
    );
}

/// SessionInfo entry and meta name
#[test]
fn names() {
    let env = TestEnv::new();
    let st = store(&env);
    let src = make_tree(&st, &env.work());

    let id = set_name(&st, &src, "0000000e", "  Refactor  parser ").unwrap();
    let entries = st.load(&src).unwrap();
    let info = entries.last().unwrap();
    assert_eq!(info.id, id);
    assert!(matches!(&info.body, EntryBody::SessionInfo { name } if name == "Refactor  parser"));

    let listed = st.list(Some(&env.work()));
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name.as_deref(), Some("Refactor  parser"));

    set_name(&st, &src, "0000000e", "Second").unwrap();
    let header = st.header(&src).unwrap();
    assert_eq!(
        meta_of(&src, &header, &st.load(&src).unwrap()).name.as_deref(),
        Some("Second"),
        "last name wins"
    );

    set_name(&st, &src, "0000000e", "   ").unwrap();
    let cleared = st.list(Some(&env.work()));
    assert_eq!(cleared[0].name, None, "a blank name clears it");

    let missing = set_name(&st, &src, "ffffffff", "x").unwrap_err();
    assert_eq!(missing.code, codes::NOT_FOUND);
}

#[test]
fn meta_counts_messages_and_first_message() {
    let env = TestEnv::new();
    let st = store(&env);
    let src = make_tree(&st, &env.work());
    let header = st.header(&src).unwrap();
    let entries = st.load(&src).unwrap();
    let m = meta_of(&src, &header, &entries);
    assert_eq!(m.message_count, 5);
    assert_eq!(m.first_message, "start");
    assert_eq!(m.parent_session, None);
    assert_eq!(m.modified_ms, entries.last().unwrap().ts_ms);
    assert!(m.all_text.contains("right follow-up"));
}

/// recent / threaded / relevance
#[test]
fn list_sort_modes() {
    // a (10) has child b (50); c (30) is alone.
    let metas = vec![
        meta("/s/a", None, None, 10, "a", ""),
        meta("/s/b", None, Some("/s/a"), 50, "b", ""),
        meta("/s/c", None, None, 30, "c", ""),
    ];

    let mut recent: Vec<usize> = vec![0, 1, 2];
    sort_sessions(&metas, &mut recent, SessionSort::Recent);
    assert_eq!(recent, [1, 2, 0], "newest first");

    let mut threaded: Vec<usize> = vec![0, 1, 2];
    sort_sessions(&metas, &mut threaded, SessionSort::Threaded);
    assert_eq!(
        threaded,
        [0, 1, 2],
        "a thread ranks by its newest member and the child follows its parent"
    );

    let mut relevance: Vec<usize> = vec![2, 0, 1];
    sort_sessions(&metas, &mut relevance, SessionSort::Relevance);
    assert_eq!(relevance, [2, 0, 1], "relevance keeps the order of the search");

    // A search with no query lists everything, newest first.
    assert_eq!(search_sessions(&metas, "", false), [1, 2, 0]);
}

/// picker search
#[test]
fn search_terms_phrases_regex() {
    let metas = vec![
        meta(
            "/work/p1",
            Some("alpha notes"),
            None,
            300,
            "fix the parser bug",
            "fix the parser bug\nthanks",
        ),
        meta(
            "/work/p2",
            None,
            None,
            200,
            "parser internals",
            "the parser is slow",
        ),
        meta(
            "/work/p3",
            Some("beta"),
            None,
            100,
            "hello world",
            "hello   world ok, hello",
        ),
    ];

    assert_eq!(search_sessions(&metas, "", false), [0, 1, 2]);
    // Terms are ANDed; both sessions 0 and 1 match `parser` with the same rank, newer first.
    assert_eq!(search_sessions(&metas, "parser", false), [0, 1]);
    assert_eq!(search_sessions(&metas, "alpha parser", false), [0]);
    assert_eq!(
        search_sessions(&metas, "PARSER", false),
        [0, 1],
        "case-insensitive"
    );
    // Quoted phrases match literally, spaces included.
    assert_eq!(search_sessions(&metas, "\"hello world\"", false), [2]);
    assert_eq!(search_sessions(&metas, "\"hello   world\"", false), [2]);
    assert_eq!(search_sessions(&metas, "\"parser is\"", false), [1]);
    // The path is searched too.
    assert_eq!(search_sessions(&metas, "p3", false), [2]);
    // Regexes.
    assert_eq!(search_sessions(&metas, "re:^fix", false), [0]);
    assert_eq!(search_sessions(&metas, "re:hel+o\\s+world", false), [2]);
    assert_eq!(search_sessions(&metas, "re:l{2}", false), [2]);
    assert_eq!(search_sessions(&metas, "re:p(ar|er)ser", false), [0, 1]);
    assert_eq!(search_sessions(&metas, "re:f.x", false), [0]);
    // The paths hold digits, so a digit class matches through the path field; `q{3}` appears nowhere.
    assert_eq!(search_sessions(&metas, "re:[0-9]", false), [0, 1, 2]);
    assert_eq!(search_sessions(&metas, "re:q{3}", false), Vec::<usize>::new());
    // The path field is searched as well: its `/` is outside [a-z ], so every session matches.
    // Ranked: sessions 0 and 2 match in all text and in the path (2 each), session 1 only in the path.
    assert_eq!(search_sessions(&metas, "re:\"[^a-z ]\"", false), [0, 2, 1]);
    // Unquoted, the space splits the class, so the pattern is invalid and matches nothing.
    assert_eq!(search_sessions(&metas, "re:[^a-z ]", false), Vec::<usize>::new());
    // Every first message is only letters and spaces, so the whole-field anchor matches all three.
    // Sessions 0 and 2 also have a name of that shape (score 6); session 1 scores 2.
    assert_eq!(search_sessions(&metas, "re:\"^[a-z ]+$\"", false), [0, 2, 1]);
    assert_eq!(
        search_sessions(&metas, "re:^parser", false),
        [1],
        "anchored to the start of a field"
    );
    // An invalid pattern matches nothing rather than everything.
    assert_eq!(search_sessions(&metas, "re:(", false), Vec::<usize>::new());
    // The picker's named-only filter.
    assert_eq!(search_sessions(&metas, "", true), [0, 2]);
    assert_eq!(search_sessions(&metas, "parser", true), [0]);
}

/// `--fork` (pi `SessionManager.forkFrom`): the whole tree is copied (every branch, file order,
/// so the leaf on load is the source's), the header points at the source, `--session-id` names
/// the copy, and an empty source is refused.
#[test]
fn fork_file_copies_the_whole_tree() {
    let env = TestEnv::new();
    let st = store(&env);
    let src = make_tree(&st, &env.work());
    let other = env.root().join("other-cwd");
    let dst = fork_file(&st, &src, &other, Some("forked-id")).unwrap();
    assert_eq!(dst.id, "forked-id");
    let header = st.header(&dst).unwrap();
    assert_eq!(header.parent_session.as_deref(), Some(src.path.to_str().unwrap()));
    assert_eq!(header.cwd, other);
    let entries = st.load(&dst).unwrap();
    assert_eq!(ids(&entries), ids(&st.load(&src).unwrap()));
    assert_eq!(Tree::new(entries).leaf(), "0000000e");

    let empty = st.create(&env.work(), None, None).unwrap();
    let err = fork_file(&st, &empty, &env.work(), None).unwrap_err();
    assert_eq!(err.code, codes::INVALID);
}
