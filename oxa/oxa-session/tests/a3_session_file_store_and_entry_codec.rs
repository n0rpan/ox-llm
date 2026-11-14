//! A3 acceptance tests: Session file store and entry codec. Owner: task A3.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_fake::TestEnv;
use oxa_proto::{
    codes, shown_why, Entry, EntryBody, FrozenPrefix, Mode, Part, Role, ThinkingLevel, ToolCall, Usage,
    WireMsg,
};
use oxa_session::{
    decode_entry, decode_header, encode_entry, encode_header, session_dir_name, FsStore, MemStore,
    SessionHeader, SessionRef, SessionStore,
};
use serde_json::json;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Strings that stress the escaper and the byte-exact contract.
const ODD: &str =
    "quote \" back \\ slash / nl\n cr\r tab\t nul\u{0} bell\u{7} \u{1f}  ünï 😀 \u{2028} </script> ' ";

fn fs_store(env: &TestEnv) -> FsStore {
    FsStore::new(env.root().join("sessions"))
}

fn user_entry(id: &str, parent: Option<&str>, ts_ms: u64, text: &str) -> Entry {
    Entry {
        id: id.to_string(),
        parent: parent.map(String::from),
        ts_ms,
        body: EntryBody::Msg {
            wire: WireMsg::user(text),
            parts: vec![Part::Text(text.to_string())],
        },
    }
}

fn assistant_entry(id: &str, parent: Option<&str>, ts_ms: u64, text: &str) -> Entry {
    Entry {
        id: id.to_string(),
        parent: parent.map(String::from),
        ts_ms,
        body: EntryBody::Msg {
            wire: WireMsg {
                role: Role::Assistant,
                content: Some(text.to_string()),
                reasoning: None,
                tool_calls: vec![],
                tool_call_id: None,
            },
            parts: vec![],
        },
    }
}

fn line_of(e: &Entry) -> Vec<u8> {
    let mut v = Vec::new();
    encode_entry(e, &mut v);
    v
}

/// One entry per `EntryBody` variant, each with odd strings where a string is stored.
fn every_body() -> Vec<EntryBody> {
    let mut sha = [0u8; 32];
    for (i, b) in sha.iter_mut().enumerate() {
        *b = i as u8 * 7;
    }
    let mut bodies = vec![
        EntryBody::Prefix(FrozenPrefix {
            sections: vec![
                ("preamble".to_string(), ODD.to_string()),
                ("context".to_string(), String::new()),
                ("cwd".to_string(), "/work".to_string()),
            ],
            tools_json: "[{\"name\":\"read\",\"parameters\":{}}]".to_string(),
            sha256: sha,
        }),
        EntryBody::Prefix(FrozenPrefix {
            sections: vec![],
            tools_json: "[]".to_string(),
            sha256: [0xff; 32],
        }),
        EntryBody::Msg {
            wire: WireMsg::user(ODD),
            parts: vec![
                Part::Text(ODD.to_string()),
                Part::Image {
                    path: PathBuf::from("/work/pic.png"),
                    mime: "image/png".into(),
                    sha256: [9; 32],
                    w: 640,
                    h: 480,
                },
            ],
        },
        EntryBody::Msg {
            wire: WireMsg {
                role: Role::Assistant,
                content: Some(ODD.to_string()),
                reasoning: Some(ODD.to_string()),
                tool_calls: vec![ToolCall {
                    id: "call_1".into(),
                    name: "read".into(),
                    args_raw: ODD.into(),
                }],
                tool_call_id: None,
            },
            parts: vec![],
        },
        EntryBody::Msg {
            wire: WireMsg {
                role: Role::Assistant,
                content: None,
                reasoning: None,
                tool_calls: vec![],
                tool_call_id: None,
            },
            parts: vec![],
        },
        EntryBody::Msg {
            wire: WireMsg {
                role: Role::Assistant,
                content: Some(String::new()),
                reasoning: Some(String::new()),
                tool_calls: vec![],
                tool_call_id: None,
            },
            parts: vec![],
        },
        EntryBody::Msg {
            wire: WireMsg::tool_result("call_1", ODD),
            parts: vec![],
        },
        EntryBody::Msg {
            wire: WireMsg::system("system text"),
            parts: vec![],
        },
        EntryBody::BashExecution {
            command: ODD.into(),
            output: ODD.into(),
            exit: Some(-1),
            excluded: false,
        },
        EntryBody::BashExecution {
            command: "ls".into(),
            output: String::new(),
            exit: None,
            excluded: true,
        },
        EntryBody::ModelChange(ODD.into()),
        EntryBody::ModeChange(Mode::Ask),
        EntryBody::Usage {
            kind: "chat".into(),
            usage: Usage {
                prompt: 4096,
                completion: 17,
                cached: 4000,
                cached_missing: false,
            },
        },
        EntryBody::Compaction {
            summary: ODD.into(),
            first_kept: "abcd1234".into(),
            tokens_before: 50_000,
            details: json!({"readFiles": [ODD], "score": 1.5, "nested": {"x": null, "b": true}}),
            from_hook: true,
        },
        EntryBody::ContextEdit {
            target: "abcd1234".into(),
            replacement: None,
        },
        EntryBody::ContextEdit {
            target: "abcd1234".into(),
            replacement: Some(ODD.into()),
        },
        EntryBody::BranchSummary {
            from: "f00dcafe".into(),
            summary: ODD.into(),
            details: serde_json::Value::Null,
            from_hook: false,
        },
        EntryBody::Custom {
            custom_type: ODD.into(),
            data: json!([1, "two", {"three": 3}]),
        },
        EntryBody::CustomMessage {
            custom_type: "ext".into(),
            content: ODD.into(),
            display: true,
            details: json!({}),
        },
        EntryBody::Label {
            target: "abcd1234".into(),
            label: None,
        },
        EntryBody::Label {
            target: "abcd1234".into(),
            label: Some(ODD.into()),
        },
        EntryBody::SessionInfo { name: ODD.into() },
        EntryBody::SessionInfo { name: String::new() },
    ];
    for why in shown_why::ALL {
        bodies.push(EntryBody::Shown {
            text: ODD.into(),
            why,
        });
    }
    for level in ThinkingLevel::ALL {
        bodies.push(EntryBody::ThinkingLevelChange(level));
    }
    bodies
}

/// decode(encode(x)) == x for every EntryBody variant, odd strings included
#[test]
fn roundtrip_every_entry_body_bytewise() {
    let ts_values = [0u64, 1_733_234_400_123, 253_402_300_799_999];
    let mut count = 0;
    for (i, body) in every_body().into_iter().enumerate() {
        let entry = Entry {
            id: format!("{:08x}", i),
            parent: if i % 2 == 0 {
                None
            } else {
                Some(format!("{:08x}", i - 1))
            },
            ts_ms: ts_values[i % ts_values.len()],
            body,
        };
        let line = line_of(&entry);
        assert!(!line.contains(&b'\n'), "one entry is one line: {:?}", entry.body);
        let decoded =
            decode_entry(&line).unwrap_or_else(|e| panic!("decode failed for {:?}: {e}", entry.body));
        assert_eq!(decoded, entry, "roundtrip changed {:?}", entry.body);
        assert_eq!(line_of(&decoded), line, "re-encoding is not byte identical");
        count += 1;
    }
    assert!(count >= 30, "every variant must be covered, got {count}");
}

/// content/reasoning/args_raw survive untouched (W2)
#[test]
fn assistant_wire_strings_exact() {
    let content = "  leading\n\n trailing  \n";
    let reasoning = "Let me think\r\n";
    let args = "{\"path\" :  \"a b\"}\n";
    let truncated_args = "{\"cmd\": \"ls";
    let entry = Entry {
        id: "00000001".into(),
        parent: None,
        ts_ms: 1_733_234_400_000,
        body: EntryBody::Msg {
            wire: WireMsg {
                role: Role::Assistant,
                content: Some(content.into()),
                reasoning: Some(reasoning.into()),
                tool_calls: vec![
                    ToolCall {
                        id: "toolu_01/x".into(),
                        name: "bash".into(),
                        args_raw: args.into(),
                    },
                    ToolCall {
                        id: "toolu_02".into(),
                        name: "bash".into(),
                        args_raw: truncated_args.into(),
                    },
                ],
                tool_call_id: None,
            },
            parts: vec![],
        },
    };
    let line = line_of(&entry);
    let decoded = decode_entry(&line).expect("assistant entry decodes");
    let EntryBody::Msg { wire, .. } = &decoded.body else {
        panic!("not a message: {:?}", decoded.body)
    };
    assert_eq!(
        wire.content.as_deref().map(str::as_bytes),
        Some(content.as_bytes())
    );
    assert_eq!(
        wire.reasoning.as_deref().map(str::as_bytes),
        Some(reasoning.as_bytes())
    );
    assert_eq!(wire.tool_calls[0].args_raw.as_bytes(), args.as_bytes());
    assert_eq!(wire.tool_calls[1].args_raw.as_bytes(), truncated_args.as_bytes());
    assert_eq!(wire.tool_calls[0].id, "toolu_01/x");
    // The wire bytes that go back to the engine are the same before and after a store round trip.
    let EntryBody::Msg { wire: original, .. } = &entry.body else {
        unreachable!()
    };
    assert_eq!(
        wire.to_json(),
        original.to_json(),
        "W2: the echoed wire bytes changed"
    );
}

/// truncated final line is ignored on load, earlier entries intact
#[test]
fn torn_last_line_dropped() {
    let env = TestEnv::new();
    let store = fs_store(&env);
    let s = store
        .create(&env.work(), Some("torn-test"), None)
        .expect("create");
    let e1 = user_entry("00000001", None, 1_733_234_400_000, "first");
    let e2 = assistant_entry("00000002", Some("00000001"), 1_733_234_401_000, "second");
    let e3 = user_entry("00000003", Some("00000002"), 1_733_234_402_000, "third");
    for e in [&e1, &e2, &e3] {
        store.append(&s, e).expect("append");
    }
    // A crash mid-write: a fragment with no trailing newline.
    let fragment = "{\"type\":\"message\",\"id\":\"zz";
    OpenOptions::new()
        .append(true)
        .open(&s.path)
        .unwrap()
        .write_all(fragment.as_bytes())
        .unwrap();
    assert_eq!(
        store.load(&s).expect("load"),
        vec![e1.clone(), e2.clone(), e3.clone()]
    );
    // The fragment is never repaired in place, but a later append is not lost to it.
    let text = fs::read_to_string(&s.path).unwrap();
    assert!(text.ends_with(fragment), "the torn fragment is left as written");
    let e4 = user_entry("00000004", Some("00000003"), 1_733_234_403_000, "fourth");
    store.append(&s, &e4).expect("append after a torn line");
    assert_eq!(store.load(&s).expect("load"), vec![e1, e2, e3, e4]);
}

/// created modes
#[test]
fn file_mode_0600_dir_0700() {
    let env = TestEnv::new();
    let store = fs_store(&env);
    let cwd = env.work();
    let s = store.create(&cwd, Some("modes-test"), None).expect("create");
    let mode = |p: &Path| fs::symlink_metadata(p).unwrap().permissions().mode() & 0o777;
    let session_dir = s.path.parent().unwrap();
    assert_eq!(mode(store.root()), 0o700, "sessions root is 0700");
    assert_eq!(mode(session_dir), 0o700, "per-cwd directory is 0700");
    assert_eq!(mode(&s.path), 0o600, "session file is 0600");
    store
        .append(&s, &user_entry("00000001", None, 1_733_234_400_000, "x"))
        .expect("append");
    assert_eq!(mode(&s.path), 0o600, "mode kept after append");
}

/// a symlinked session file or directory -> unsafe_path
#[test]
fn symlink_refused() {
    let env = TestEnv::new();
    let store = fs_store(&env);

    // A symlinked session file.
    let s = store.create(&env.work(), Some("sym-file"), None).expect("create");
    let moved = env.root().join("moved-file.jsonl");
    fs::rename(&s.path, &moved).unwrap();
    std::os::unix::fs::symlink(&moved, &s.path).unwrap();
    let entry = user_entry("00000001", None, 1_733_234_400_000, "x");
    assert_eq!(store.load(&s).unwrap_err().code, codes::UNSAFE_PATH);
    assert_eq!(store.header(&s).unwrap_err().code, codes::UNSAFE_PATH);
    assert_eq!(store.append(&s, &entry).unwrap_err().code, codes::UNSAFE_PATH);
    let as_path = s.path.to_string_lossy().into_owned();
    assert_eq!(store.resolve(&as_path).unwrap_err().code, codes::UNSAFE_PATH);

    // A symlinked directory: the session lives in a directory that is a link.
    let other_cwd = env.root().join("work2");
    fs::create_dir_all(&other_cwd).unwrap();
    let d = store.create(&other_cwd, Some("sym-dir"), None).expect("create");
    let session_dir = d.path.parent().unwrap().to_path_buf();
    let moved_dir = env.root().join("moved-dir");
    fs::rename(&session_dir, &moved_dir).unwrap();
    std::os::unix::fs::symlink(&moved_dir, &session_dir).unwrap();
    assert_eq!(store.load(&d).unwrap_err().code, codes::UNSAFE_PATH);
    assert_eq!(store.append(&d, &entry).unwrap_err().code, codes::UNSAFE_PATH);
    assert!(
        store.list(Some(&other_cwd)).is_empty(),
        "a symlinked directory is not listed"
    );
}

/// `--<path>--/<ts>_<id>.jsonl`
#[test]
fn pi_path_scheme() {
    let env = TestEnv::new();
    let store = fs_store(&env);
    let cwd = env.work();
    let s = store.create(&cwd, None, None).expect("create");

    let expected_dir = format!(
        "--{}--",
        cwd.to_str().unwrap().trim_start_matches('/').replace('/', "-")
    );
    assert_eq!(
        s.path.parent().unwrap().file_name().unwrap().to_str().unwrap(),
        expected_dir
    );
    assert_eq!(session_dir_name(Path::new("/home/u/proj")), "--home-u-proj--");

    let name = s.path.file_name().unwrap().to_str().unwrap().to_string();
    // `<ts>_<id>.jsonl` with ts = `2024-12-03T14-00-00-000Z`: 24 characters, `-` in place of `:` and `.`.
    let (stamp, rest) = name.split_at(24);
    assert_eq!(rest, format!("_{}.jsonl", s.id), "file name is <ts>_<id>.jsonl");
    let b = stamp.as_bytes();
    for (i, c) in b.iter().enumerate() {
        let want_sep = matches!(i, 4 | 7 | 13 | 16 | 19);
        match i {
            10 => assert_eq!(*c, b'T'),
            23 => assert_eq!(*c, b'Z'),
            _ if want_sep => assert_eq!(*c, b'-', "separator at {i} in {stamp}"),
            _ => assert!(c.is_ascii_digit(), "digit at {i} in {stamp}"),
        }
    }
    // Ids are uuid v4 text by default; a given id names the file.
    assert_eq!(s.id.len(), 36);
    assert_eq!(s.id.as_bytes()[14], b'4', "uuid version nibble");
    let named = store
        .create(&cwd, Some("my-session_1"), None)
        .expect("create with id");
    assert!(named.path.to_str().unwrap().ends_with("_my-session_1.jsonl"));
}

/// concurrent appends never interleave within a line
#[test]
fn append_is_single_write_o_append() {
    let env = TestEnv::new();
    let store = Arc::new(fs_store(&env));
    let s = store
        .create(&env.work(), Some("concurrent"), None)
        .expect("create");
    let threads = 8;
    let per_thread = 200;
    let payload: String = "x".repeat(700);
    let handles: Vec<_> = (0..threads)
        .map(|t| {
            let store = Arc::clone(&store);
            let s: SessionRef = s.clone();
            let payload = payload.clone();
            std::thread::spawn(move || {
                for i in 0..per_thread {
                    let id = format!("{:04x}{:04x}", t, i);
                    let text = format!("{id} {payload}");
                    store
                        .append(&s, &user_entry(&id, None, 1_733_234_400_000 + i as u64, &text))
                        .expect("append");
                }
            })
        })
        .collect();
    for h in handles {
        h.join().expect("thread");
    }
    // Every complete line decodes on its own: no two appends were interleaved.
    let bytes = fs::read(&s.path).unwrap();
    let lines: Vec<&[u8]> = bytes.split(|b| *b == b'\n').collect();
    assert_eq!(
        lines.last().map(|l| l.is_empty()),
        Some(true),
        "the file ends with a newline"
    );
    let body = &lines[1..lines.len() - 1];
    assert_eq!(body.len(), threads * per_thread);
    let mut ids = std::collections::HashSet::new();
    for line in body {
        let e = decode_entry(line).unwrap_or_else(|err| panic!("interleaved line: {err}"));
        ids.insert(e.id);
    }
    assert_eq!(ids.len(), threads * per_thread);
    assert_eq!(store.load(&s).unwrap().len(), threads * per_thread);
}

/// unique prefix ok, ambiguous prefix = invalid
#[test]
fn resolve_by_path_id_and_prefix() {
    let env = TestEnv::new();
    let store = fs_store(&env);
    let cwd = env.work();
    let s1 = store.create(&cwd, Some("aaaa1111-x"), None).expect("create");
    let _s2 = store.create(&cwd, Some("aaaa2222-y"), None).expect("create");
    let s3 = store.create(&cwd, Some("bbbb3333"), None).expect("create");

    assert_eq!(store.resolve("aaaa1111-x").unwrap().id, "aaaa1111-x", "full id");
    assert_eq!(store.resolve("bbb").unwrap().id, "bbbb3333", "unique prefix");
    assert_eq!(
        store.resolve("aaaa1").unwrap().id,
        "aaaa1111-x",
        "longer unique prefix"
    );
    assert_eq!(
        store.resolve("aaaa").unwrap_err().code,
        codes::INVALID,
        "ambiguous prefix is invalid"
    );
    assert_eq!(
        store.resolve("cccc").unwrap_err().code,
        codes::NOT_FOUND,
        "no match"
    );
    assert_eq!(
        store.resolve("").unwrap_err().code,
        codes::INVALID,
        "empty reference"
    );

    let by_path = store.resolve(s1.path.to_str().unwrap()).expect("resolve by path");
    assert_eq!(by_path.path, s1.path);
    assert_eq!(by_path.id, "aaaa1111-x");
    assert_eq!(store.resolve(s3.path.to_str().unwrap()).unwrap().id, "bbbb3333");
    let missing = env.root().join("missing.jsonl");
    assert_eq!(
        store.resolve(missing.to_str().unwrap()).unwrap_err().code,
        codes::NOT_FOUND
    );
    assert_eq!(
        store.create(&cwd, Some("bbbb3333"), None).unwrap_err().code,
        codes::INVALID,
        "ids are unique"
    );
}

/// Exercise create/append/load/list/resolve (and delete when the store deletes without `trash`)
/// with the same expectations for both stores.
fn exercise_store(store: &dyn SessionStore, cwd: &Path, can_delete: bool) {
    let s1 = store.create(cwd, Some("sem-1"), None).expect("create");
    let header = store.header(&s1).expect("header");
    assert_eq!(header.id, "sem-1");
    assert_eq!(header.cwd, cwd);
    assert_eq!(header.parent_session, None);

    let e1 = user_entry("00000001", None, 1_733_234_400_000, "hi there");
    let e2 = assistant_entry("00000002", Some("00000001"), 1_733_234_401_000, "hello");
    let e3 = Entry {
        id: "00000003".into(),
        parent: Some("00000002".into()),
        ts_ms: 1_733_234_402_000,
        body: EntryBody::SessionInfo { name: "named".into() },
    };
    for e in [&e1, &e2, &e3] {
        store.append(&s1, e).expect("append");
    }
    assert_eq!(store.load(&s1).expect("load"), vec![e1, e2, e3]);

    let s2 = store.create(cwd, Some("sem-2"), None).expect("second create");
    let listed = store.list(Some(cwd));
    assert_eq!(listed.len(), 2, "both sessions of the cwd are listed");
    let first = listed
        .iter()
        .find(|m| m.sref.id == "sem-1")
        .expect("sem-1 listed");
    assert_eq!(first.message_count, 2);
    assert_eq!(first.name.as_deref(), Some("named"));
    assert_eq!(first.first_message, "hi there");
    assert_eq!(first.cwd, cwd);
    assert_eq!(store.list(None).len(), 2, "list(None) covers every directory");
    assert!(
        store.list(Some(&cwd.join("elsewhere"))).is_empty(),
        "other cwd lists nothing"
    );

    assert_eq!(store.resolve("sem-2").expect("exact").id, "sem-2");
    assert_eq!(
        store.resolve("sem").unwrap_err().code,
        codes::INVALID,
        "ambiguous prefix"
    );
    assert_eq!(
        store.resolve("nothing-like-this").unwrap_err().code,
        codes::NOT_FOUND
    );
    assert_eq!(
        store.create(cwd, Some("sem-1"), None).unwrap_err().code,
        codes::INVALID,
        "duplicate id"
    );
    assert_eq!(
        store.create(cwd, Some("bad/id"), None).unwrap_err().code,
        codes::INVALID,
        "separator in id"
    );

    if can_delete {
        store.delete(&s2).expect("delete");
        assert_eq!(
            store.resolve("sem-2").unwrap_err().code,
            codes::NOT_FOUND,
            "deleted session is gone"
        );
        assert_eq!(store.list(Some(cwd)).len(), 1);
    }
}

/// create/append/load/list/resolve/delete
#[test]
fn memstore_same_semantics() {
    let env = TestEnv::new();
    // MemStore: every operation, delete included.
    exercise_store(&MemStore::new(), &env.work(), true);
    // FsStore: the same expectations. Delete is not exercised here because it goes through the
    // system `trash` command, which must not touch the real trash from a test.
    let env2 = TestEnv::new();
    exercise_store(&fs_store(&env2), &env2.work(), false);
}

/// header codec
#[test]
fn header_roundtrip_with_parent_session() {
    let with_parent = SessionHeader {
        version: 3,
        id: "hdr-1".into(),
        ts_ms: 1_733_234_400_123,
        cwd: PathBuf::from("/home/u/proj"),
        parent_session: Some("/home/u/.oxa/agent/sessions/--home-u-proj--/parent.jsonl".into()),
    };
    let mut line = Vec::new();
    encode_header(&with_parent, &mut line);
    assert_eq!(
        String::from_utf8(line.clone()).unwrap(),
        "{\"type\":\"session\",\"version\":3,\"id\":\"hdr-1\",\"timestamp\":\"2024-12-03T14:00:00.123Z\",\"cwd\":\"/home/u/proj\",\"parentSession\":\"/home/u/.oxa/agent/sessions/--home-u-proj--/parent.jsonl\"}"
    );
    assert_eq!(decode_header(&line).expect("decode"), with_parent);

    let root = SessionHeader {
        parent_session: None,
        ..with_parent.clone()
    };
    let mut plain = Vec::new();
    encode_header(&root, &mut plain);
    assert!(!String::from_utf8_lossy(&plain).contains("parentSession"));
    assert_eq!(decode_header(&plain).expect("decode"), root);

    // Through a store: a forked session records its parent file.
    let env = TestEnv::new();
    let store = fs_store(&env);
    let parent_path = env.root().join("parent.jsonl");
    let child = store
        .create(&env.work(), Some("child-1"), Some(&parent_path))
        .expect("create");
    assert_eq!(
        store.header(&child).unwrap().parent_session.as_deref(),
        parent_path.to_str()
    );
    let mem = MemStore::new();
    let mem_child = mem
        .create(&env.work(), Some("child-2"), Some(&parent_path))
        .expect("create");
    assert_eq!(
        mem.header(&mem_child).unwrap().parent_session.as_deref(),
        parent_path.to_str()
    );
}

/// Extra: pi-written messages without oxa's `wire` object still decode, and unknown types do not.
#[test]
fn pi_native_message_decodes() {
    let line = br#"{"type":"message","id":"a1b2c3d4","parentId":null,"timestamp":"2024-12-03T14:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"Hello "},{"type":"text","text":"there"}],"timestamp":1733234401000}}"#;
    let e = decode_entry(line).expect("pi message decodes");
    assert_eq!(e.id, "a1b2c3d4");
    assert_eq!(e.parent, None);
    assert_eq!(e.ts_ms, 1_733_234_401_000);
    assert_eq!(
        e.body,
        EntryBody::Msg {
            wire: WireMsg::user("Hello there"),
            parts: vec![]
        }
    );

    let unknown =
        br#"{"type":"no_such_type","id":"a1b2c3d4","parentId":null,"timestamp":"2024-12-03T14:00:01.000Z"}"#;
    assert_eq!(decode_entry(unknown).unwrap_err().code, codes::INVALID);
    assert_eq!(
        decode_entry(b"{\"type\":\"message\",\"id\":\"x")
            .unwrap_err()
            .code,
        codes::INVALID
    );
}
