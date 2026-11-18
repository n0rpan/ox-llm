//! A4 acceptance tests: Transcript fold (W1) and session tree.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_proto::{
    rebase_why, Entry, EntryBody, FrozenPrefix, Mode, Role, ThinkingLevel, ToolCall, TurnKind, Usage, Value,
    WireMsg,
};
use oxa_session::{RebaseLog, Transcript, Tree};
use sha2::{Digest, Sha256};

fn entry(id: &str, parent: Option<&str>, body: EntryBody) -> Entry {
    Entry {
        id: id.to_string(),
        parent: parent.map(str::to_string),
        ts_ms: 0,
        body,
    }
}

fn prefix(text: &str) -> FrozenPrefix {
    FrozenPrefix {
        sections: vec![("preamble".to_string(), text.to_string())],
        tools_json: "[]".to_string(),
        sha256: [0u8; 32],
    }
}

fn msg(wire: WireMsg) -> EntryBody {
    EntryBody::Msg {
        wire,
        parts: Vec::new(),
        tool: None,
    }
}

fn usage(prompt: u32, completion: u32) -> Usage {
    Usage {
        prompt,
        completion,
        cached: 0,
        cached_missing: false,
    }
}

fn bash(command: &str, output: &str, exit: Option<i32>, excluded: bool) -> EntryBody {
    EntryBody::BashExecution {
        command: command.to_string(),
        output: output.to_string(),
        exit,
        excluded,
    }
}

/// Chain recomputed from scratch over `wire`, independent of the implementation under test.
fn chain_from_wire(wire: &[WireMsg]) -> [u8; 32] {
    let mut prev = [0u8; 32];
    for m in wire {
        let mut h = Sha256::new();
        h.update(prev);
        h.update(m.to_json());
        prev = h.finalize().into();
    }
    prev
}

/// Small deterministic xorshift generator (no external property-testing crate in the workspace).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len() as u64) as usize]
    }
}

/// A random body. `ids` holds the ids of the earlier entries (targets of edits and cuts).
/// `prefix_seen` says whether a prefix already exists (a second one is an epoch rebase).
fn random_body(rng: &mut Rng, ids: &[String], prefix_seen: bool) -> EntryBody {
    let target = |rng: &mut Rng| -> String {
        if ids.is_empty() {
            "missing".to_string()
        } else {
            rng.pick(ids).clone()
        }
    };
    let text = |rng: &mut Rng| -> String { rng.pick(&["a", "b c", "\"q\"", "línea\n2", ""]).to_string() };
    match rng.below(16) {
        0 => EntryBody::Prefix(prefix("p")),
        1 | 2 => EntryBody::Msg {
            wire: WireMsg::user(text(rng)),
            parts: Vec::new(),
            tool: None,
        },
        3 => EntryBody::Msg {
            wire: WireMsg {
                role: Role::Assistant,
                content: Some(text(rng)),
                reasoning: Some(text(rng)),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "read".into(),
                    args_raw: "{\"path\":\"x\"}".into(),
                }],
                provider_history: None,
                tool_call_id: None,
            },
            parts: Vec::new(),
            tool: None,
        },
        4 => EntryBody::Msg {
            wire: WireMsg::tool_result("c1", text(rng)),
            parts: Vec::new(),
            tool: None,
        },
        5 => bash("ls", &text(rng), Some(rng.below(3) as i32), rng.below(2) == 0),
        6 => EntryBody::Usage {
            kind: "chat".into(),
            usage: usage(rng.below(500) as u32, rng.below(50) as u32),
        },
        7 => EntryBody::ModelChange("m".into()),
        8 => EntryBody::ThinkingLevelChange(ThinkingLevel::High),
        9 => EntryBody::ModeChange(Mode::Ask),
        10 => EntryBody::Shown {
            text: text(rng),
            why: "notice",
        },
        11 => EntryBody::Custom {
            custom_type: "x".into(),
            data: Value::Null,
        },
        12 => EntryBody::CustomMessage {
            custom_type: "x".into(),
            content: text(rng),
            display: true,
            details: Value::Null,
        },
        13 => EntryBody::BranchSummary {
            from: target(rng),
            summary: text(rng),
            details: Value::Null,
            from_hook: false,
        },
        14 => EntryBody::Label {
            target: target(rng),
            label: Some(text(rng)),
        },
        _ => {
            // Rebasing bodies are rare: a cut, an edit or (once) a prefix.
            match rng.below(3) {
                0 if prefix_seen => EntryBody::Prefix(prefix("p2")),
                1 => EntryBody::Compaction {
                    summary: text(rng),
                    first_kept: target(rng),
                    tokens_before: 100,
                    details: Value::Null,
                    from_hook: false,
                },
                _ => EntryBody::ContextEdit {
                    target: target(rng),
                    replacement: if rng.below(2) == 0 { None } else { Some(text(rng)) },
                },
            }
        }
    }
}

/// A random sequence of entries with unique 8-hex-digit ids and parents chosen from earlier ids.
fn random_sequence(rng: &mut Rng, len: usize) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::new();
    let mut ids: Vec<String> = Vec::new();
    let mut prefix_seen = false;
    for i in 0..len {
        let id = format!("{:08x}", 0x1000_0000u64 + i as u64);
        let body = random_body(rng, &ids, prefix_seen);
        if matches!(body, EntryBody::Prefix(_)) {
            prefix_seen = true;
        }
        let parent = ids.last().cloned();
        out.push(Entry {
            id: id.clone(),
            parent,
            ts_ms: i as u64,
            body,
        });
        ids.push(id);
    }
    out
}

/// Bodies that leave the wire append-only (no rebase), as documented on `EntryBody`.
fn is_append_only(body: &EntryBody, prefix_seen: bool) -> bool {
    match body {
        EntryBody::Prefix(_) => !prefix_seen,
        EntryBody::Compaction { .. } | EntryBody::ContextEdit { .. } => false,
        _ => true,
    }
}

/// property test over random entry sequences
#[test]
fn fold_is_append_only_for_non_rebase_entries() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for _case in 0..300 {
        let len = 1 + rng.below(40) as usize;
        let seq = random_sequence(&mut rng, len);
        let mut t = Transcript::default();
        let mut prefix_seen = false;
        for e in &seq {
            let before_wire = t.wire().to_vec();
            let before_rebases = t.rebases().len();
            let append_only = is_append_only(&e.body, prefix_seen);
            if matches!(e.body, EntryBody::Prefix(_)) {
                prefix_seen = true;
            }
            t.apply(e);
            if append_only {
                assert!(
                    oxa_proto::wire_is_prefix(&before_wire, t.wire()),
                    "non-rebasing entry {} changed the existing wire",
                    e.id
                );
                assert_eq!(
                    t.rebases().len(),
                    before_rebases,
                    "non-rebasing entry {} logged a rebase",
                    e.id
                );
            }
            for log in &t.rebases()[before_rebases..] {
                assert!(rebase_why::ALL.contains(&log.why));
            }
        }
    }
}

/// project(branch) == fold of apply
#[test]
fn project_equals_incremental_apply() {
    let mut rng = Rng(0x0123_4567_89AB_CDEF);
    for _case in 0..200 {
        let len = rng.below(40) as usize;
        let seq = random_sequence(&mut rng, len);
        let projected = Transcript::project(&seq);
        let mut incremental = Transcript::default();
        for e in &seq {
            incremental.apply(e);
        }
        assert_eq!(projected, incremental);
    }
}

/// and is reproducible from the wire
#[test]
fn chain_hash_changes_with_every_push() {
    let mut t = Transcript::default();
    let mut seen = vec![t.chain()];
    for text in ["one", "two", "two", "three"] {
        t.push(WireMsg::user(text));
        let c = t.chain();
        assert!(!seen.contains(&c), "chain repeated after push of {text:?}");
        assert_eq!(c, chain_from_wire(t.wire()), "chain is not the fold of the wire");
        seen.push(c);
    }
    // Rebuilding the same wire from the bytes reproduces the chain.
    let mut again = Transcript::default();
    for m in t.wire().to_vec() {
        again.push(m);
    }
    assert_eq!(again.chain(), t.chain());
    // A rebase recomputes it from the new head.
    t.rebase(vec![WireMsg::system("s")], rebase_why::COMPACTION);
    assert_eq!(t.chain(), chain_from_wire(t.wire()));
    assert_ne!(t.chain(), again.chain());
}

/// unknown why panics; known why logs RebaseLog
#[test]
fn rebase_requires_known_cause() {
    let mut t = Transcript::default();
    t.push(WireMsg::user("a"));
    t.push(WireMsg::assistant("b"));

    let unknown = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut bad = t.clone();
        bad.rebase(vec![], "because");
    }));
    assert!(unknown.is_err(), "an unknown rebase cause must panic");

    let head = vec![WireMsg::user("only")];
    let chain = chain_from_wire(&head);
    t.rebase(head, rebase_why::EXTENSION);
    assert_eq!(
        t.rebases(),
        &[RebaseLog {
            why: rebase_why::EXTENSION,
            before: 2,
            after: 1,
            chain
        }][..]
    );
    assert_eq!(t.wire(), &[WireMsg::user("only")][..]);
    assert_eq!(t.origin(0), None, "a synthetic head has no origin entry");
}

/// prompt + completion - 1 after set_usage (F1: the last generated token is never fed); 0 after rebase
#[test]
fn expected_cache_follows_usage() {
    let mut t = Transcript::default();
    t.push(WireMsg::user("hi"));
    t.push(WireMsg::assistant("hello"));
    assert_eq!(t.expected_cache(), 0);
    t.set_usage(&usage(120, 30));
    assert_eq!(t.expected_cache(), 149);
    assert_eq!(t.context(), 150);
    // Pushing more does not change the expectation until the next usage.
    t.push(WireMsg::user("more"));
    assert_eq!(t.expected_cache(), 149);
    t.rebase(vec![WireMsg::user("x")], rebase_why::LEAF_MOVE);
    assert_eq!(t.expected_cache(), 0);
    assert_eq!(t.context(), 0);
    t.set_usage(&usage(7, 1));
    assert_eq!(t.expected_cache(), 7);

    // Through the fold: chat and prefill usages set it, side requests do not.
    let seq = vec![
        entry("p0", None, EntryBody::Prefix(prefix("p"))),
        entry("m1", Some("p0"), msg(WireMsg::user("q"))),
        entry("m2", Some("m1"), msg(WireMsg::assistant("a"))),
        entry(
            "u1",
            Some("m2"),
            EntryBody::Usage {
                kind: TurnKind::Chat.as_str().into(),
                usage: usage(30, 7),
            },
        ),
        entry(
            "u2",
            Some("u1"),
            EntryBody::Usage {
                kind: TurnKind::Compaction.as_str().into(),
                usage: usage(999, 1),
            },
        ),
    ];
    let folded = Transcript::project(&seq);
    assert_eq!(folded.expected_cache(), 36);
    assert_eq!(folded.context(), 37);
}

/// [user:summary] + messages from first_kept
#[test]
fn compaction_entry_folds_to_summary_plus_tail() {
    let seq = vec![
        entry("p0", None, EntryBody::Prefix(prefix("p"))),
        entry("m1", Some("p0"), msg(WireMsg::user("u1"))),
        entry("m2", Some("m1"), msg(WireMsg::assistant("a1"))),
        entry("m3", Some("m2"), msg(WireMsg::user("u2"))),
        entry("m4", Some("m3"), msg(WireMsg::assistant("a2"))),
        entry(
            "c1",
            Some("m4"),
            EntryBody::Compaction {
                summary: "SUMMARY".into(),
                first_kept: "m3".into(),
                tokens_before: 5000,
                details: Value::Null,
                from_hook: false,
            },
        ),
    ];
    let mut t = Transcript::project(&seq);
    assert_eq!(
        t.wire(),
        &[
            WireMsg::user("SUMMARY"),
            WireMsg::user("u2"),
            WireMsg::assistant("a2")
        ][..]
    );
    assert_eq!(t.origin(0), Some("c1"));
    assert_eq!(t.origin(1), Some("m3"));
    assert_eq!(t.origin(2), Some("m4"));
    let last = t.rebases().last().expect("compaction logs a rebase");
    assert_eq!(last.why, rebase_why::COMPACTION);
    assert_eq!((last.before, last.after), (4, 3));
    assert_eq!(t.expected_cache(), 0);

    // New messages after the compaction are plain appends.
    let before = t.wire().to_vec();
    t.apply(&entry("m5", Some("c1"), msg(WireMsg::user("u3"))));
    assert!(oxa_proto::wire_is_prefix(&before, t.wire()));
    assert_eq!(t.wire().len(), 4);
}

/// `!!` entries leave the wire untouched; `!` entries append one user message
#[test]
fn excluded_bash_not_in_wire() {
    let mut t = Transcript::default();
    t.push(WireMsg::user("hi"));
    let before = t.wire().to_vec();
    t.apply(&entry("b1", None, bash("secret", "hidden", Some(0), true)));
    assert_eq!(t.wire(), &before[..]);
    assert!(t.rebases().is_empty());

    let seq = vec![
        entry("b2", None, bash("ls", "a\nb", Some(0), false)),
        entry("b3", Some("b2"), bash("false", "", Some(2), false)),
        entry("b4", Some("b3"), bash("pwd", "", None, false)),
    ];
    for e in &seq {
        t.apply(e);
    }
    assert_eq!(t.wire()[1], WireMsg::user("Ran `ls`\n```\na\nb\n```"));
    assert_eq!(
        t.wire()[2],
        WireMsg::user("Ran `false`\n(no output)\n\nCommand exited with code 2")
    );
    assert_eq!(t.wire()[3], WireMsg::user("Ran `pwd`\n(no output)"));
    assert_eq!(t.wire().len(), 4);
    assert!(t.rebases().is_empty(), "a bash append is not a rebase");
    assert_eq!(t.origin(1), Some("b2"));
}

/// messages kept, prefix replaced
#[test]
fn later_prefix_entry_is_epoch_rebase() {
    let seq = vec![
        entry("p0", None, EntryBody::Prefix(prefix("first"))),
        entry("m1", Some("p0"), msg(WireMsg::user("u1"))),
        entry("m2", Some("m1"), msg(WireMsg::assistant("a1"))),
    ];
    let mut t = Transcript::project(&seq);
    assert!(
        t.rebases().is_empty(),
        "the first prefix sets the prefix without a rebase"
    );
    t.set_usage(&usage(10, 2));
    let wire_before = t.wire().to_vec();

    t.apply(&entry("p1", Some("m2"), EntryBody::Prefix(prefix("second"))));
    assert_eq!(t.prefix().map(|p| p.system_text()), Some("second".to_string()));
    assert_eq!(t.wire(), &wire_before[..]);
    assert_eq!(t.rebases().len(), 1);
    assert_eq!(t.rebases()[0].why, rebase_why::EPOCH);
    assert_eq!((t.rebases()[0].before, t.rebases()[0].after), (2, 2));
    assert_eq!(t.expected_cache(), 0);
}

/// structure queries
#[test]
fn tree_branch_children_roots() {
    let r = entry("r", None, msg(WireMsg::user("root")));
    let a = entry("a", Some("r"), msg(WireMsg::assistant("a")));
    let b = entry("b", Some("a"), msg(WireMsg::user("b")));
    let c = entry("c", Some("a"), msg(WireMsg::user("c")));
    let d = entry("d", None, msg(WireMsg::user("second root")));
    let ghost = entry("g", Some("nowhere"), msg(WireMsg::user("orphan")));
    let mut tree = Tree::new(vec![r, a, b, c, d, ghost]);

    assert_eq!(tree.leaf(), "g", "the last entry in file order is the leaf");
    let ids = |v: Vec<&Entry>| v.iter().map(|e| e.id.clone()).collect::<Vec<_>>();
    assert_eq!(ids(tree.branch("b")), ["r", "a", "b"]);
    assert_eq!(ids(tree.branch("r")), ["r"]);
    assert!(tree.branch("zzz").is_empty());
    assert_eq!(ids(tree.children("a")), ["b", "c"]);
    assert_eq!(ids(tree.children("r")), ["a"]);
    assert!(tree.children("b").is_empty());
    assert_eq!(
        ids(tree.roots()),
        ["r", "d", "g"],
        "an unknown parent makes an entry a root"
    );

    assert!(tree.set_leaf("b"));
    assert_eq!(tree.leaf(), "b");
    assert!(!tree.set_leaf("nope"));
    assert_eq!(tree.leaf(), "b", "a refused move changes nothing");

    tree.push(entry("e", Some("c"), msg(WireMsg::user("e"))));
    assert_eq!(tree.leaf(), "b", "a push off the leaf does not move it");
    tree.push(entry("f", Some("b"), msg(WireMsg::user("f"))));
    assert_eq!(tree.leaf(), "f");
    assert_eq!(tree.get("f").map(|e| e.id.as_str()), Some("f"));
    assert!(tree.get("zzz").is_none());
    assert_eq!(ids(tree.branch("f")), ["r", "a", "b", "f"]);
}

/// sibling leaves, ancestor/descendant, unrelated roots
#[test]
fn common_ancestor_cases() {
    let tree = Tree::new(vec![
        entry("r", None, msg(WireMsg::user("r"))),
        entry("a", Some("r"), msg(WireMsg::user("a"))),
        entry("b", Some("a"), msg(WireMsg::user("b"))),
        entry("c", Some("a"), msg(WireMsg::user("c"))),
        entry("d", Some("c"), msg(WireMsg::user("d"))),
        entry("s", None, msg(WireMsg::user("s"))),
    ]);
    assert_eq!(
        tree.common_ancestor("b", "d"),
        Some("a"),
        "sibling subtrees meet at their parent"
    );
    assert_eq!(tree.common_ancestor("a", "d"), Some("a"), "ancestor of the other");
    assert_eq!(tree.common_ancestor("d", "a"), Some("a"), "symmetric");
    assert_eq!(tree.common_ancestor("d", "d"), Some("d"), "an entry with itself");
    assert_eq!(tree.common_ancestor("r", "b"), Some("r"));
    assert_eq!(tree.common_ancestor("b", "s"), None, "unrelated roots");
    assert_eq!(tree.common_ancestor("b", "zzz"), None, "unknown id");
}

/// Tree::label
#[test]
fn labels_last_wins_and_clear() {
    let tree = Tree::new(vec![
        entry("a", None, msg(WireMsg::user("a"))),
        entry("b", Some("a"), msg(WireMsg::user("b"))),
        entry(
            "l1",
            Some("b"),
            EntryBody::Label {
                target: "a".into(),
                label: Some("x".into()),
            },
        ),
        entry(
            "l2",
            Some("l1"),
            EntryBody::Label {
                target: "a".into(),
                label: Some("y".into()),
            },
        ),
        entry(
            "l3",
            Some("l2"),
            EntryBody::Label {
                target: "b".into(),
                label: Some("keep".into()),
            },
        ),
    ]);
    assert_eq!(tree.label("a"), Some("y"));
    assert_eq!(tree.label("b"), Some("keep"));
    assert_eq!(tree.label("zzz"), None);

    let mut tree = tree;
    tree.push(entry(
        "l4",
        Some("l3"),
        EntryBody::Label {
            target: "a".into(),
            label: None,
        },
    ));
    assert_eq!(tree.label("a"), None, "a null label clears it");
    tree.push(entry(
        "l5",
        Some("l4"),
        EntryBody::Label {
            target: "a".into(),
            label: Some("z".into()),
        },
    ));
    assert_eq!(tree.label("a"), Some("z"));
}

/// F1: an H6 prefill (`completion == 0`) expects exactly `prompt`; a reply expects
/// `prompt + completion - 1` (the engine's `tokens_cached`); `context()` keeps the sum.
#[test]
fn f1_expected_cache_drops_the_last_generated_token() {
    let mut t = Transcript::default();
    t.push(WireMsg::user("hi"));
    t.set_usage(&usage(500, 0));
    assert_eq!(
        t.expected_cache(),
        500,
        "H6 prefill: nothing generated, nothing dropped"
    );
    assert_eq!(t.context(), 500);
    t.set_usage(&usage(500, 40));
    assert_eq!(t.expected_cache(), 539);
    assert_eq!(t.context(), 540);
    t.set_usage(&usage(0, 0));
    assert_eq!(t.expected_cache(), 0);
}

/// A reply the embedded gateway accepted without usage (0 prompt, 0 completion) is not a
/// measurement: it must not reset the context baseline to zero, or compaction would be skipped
/// while the conversation keeps growing.
#[test]
fn unreported_usage_keeps_the_previous_context_baseline() {
    let mut t = Transcript::default();
    let user = |text: &str| WireMsg {
        role: Role::User,
        content: Some(text.to_string()),
        reasoning: None,
        tool_calls: Vec::new(),
        provider_history: None,
        tool_call_id: None,
    };
    let reply = |text: &str| WireMsg {
        role: Role::Assistant,
        content: Some(text.to_string()),
        reasoning: None,
        tool_calls: Vec::new(),
        provider_history: None,
        tool_call_id: None,
    };
    t.apply(&entry("u1", None, msg(user("first"))));
    t.apply(&entry("a1", Some("u1"), msg(reply("first reply"))));
    t.apply(&entry(
        "use1",
        Some("a1"),
        EntryBody::Usage {
            kind: TurnKind::Chat.as_str().to_string(),
            usage: usage(100, 20),
        },
    ));
    assert_eq!(t.context(), 120);
    assert_eq!(t.usage_wire_len(), Some(t.wire().len()));

    t.apply(&entry("u2", Some("use1"), msg(user("second"))));
    t.apply(&entry("a2", Some("u2"), msg(reply("second reply"))));
    let before = t.usage_wire_len();
    t.apply(&entry(
        "use2",
        Some("a2"),
        EntryBody::Usage {
            kind: TurnKind::Chat.as_str().to_string(),
            usage: Usage {
                prompt: 0,
                completion: 0,
                cached: 0,
                cached_missing: true,
            },
        },
    ));
    assert_eq!(t.context(), 120, "an unreported usage is not a zero context");
    assert_eq!(
        t.usage_wire_len(),
        before,
        "the baseline stays where it was counted"
    );
}
