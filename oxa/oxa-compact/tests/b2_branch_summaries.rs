//! B2 acceptance tests: Branch summaries. Owner: task B2.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.
//! (These tests touch no files: the session tree and the wire are built in memory.)

use oxa_compact::prompt::{self, SummaryKind};
use oxa_compact::{branch_entries, branch_summary_request};
use oxa_proto::{wire_is_prefix, Entry, EntryBody, Value, WireMsg};
use oxa_session::Tree;
use serde_json::json;

/// A message entry with the given user text, parent and id.
fn msg(id: &str, parent: Option<&str>, text: &str) -> Entry {
    Entry {
        id: id.to_string(),
        parent: parent.map(str::to_string),
        ts_ms: 0,
        body: EntryBody::Msg {
            wire: WireMsg::user(text),
            parts: Vec::new(),
        },
    }
}

fn ids(v: &[&Entry]) -> Vec<String> {
    v.iter().map(|e| e.id.clone()).collect()
}

/// Tree: r -> a -> b -> d (leaf of the old branch), and a -> c (the target branch).
fn fork_tree() -> Tree {
    Tree::new(vec![
        msg("r", None, "root"),
        msg("a", Some("r"), "a"),
        msg("b", Some("a"), "bb"),
        msg("c", Some("a"), "cc"),
        msg("d", Some("b"), "ddd"),
    ])
}

fn len_count(s: &str) -> u32 {
    s.len() as u32
}

/// order and bounds
#[test]
fn entries_old_leaf_back_to_ancestor() {
    let t = fork_tree();
    // From d back to the common ancestor a (excluded), oldest first.
    assert_eq!(ids(&branch_entries(&t, "d", "c")), vec!["b", "d"]);
    // A shorter abandoned branch.
    assert_eq!(ids(&branch_entries(&t, "b", "c")), vec!["b"]);
    // Moving to the root abandons everything below the root.
    assert_eq!(ids(&branch_entries(&t, "d", "r")), vec!["a", "b", "d"]);
    // Bounds: the result never contains the common ancestor or anything above it.
    for e in branch_entries(&t, "d", "c") {
        assert!(e.id != "a" && e.id != "r");
    }
    // An unknown old leaf has no branch.
    assert!(branch_entries(&t, "zz", "c").is_empty());
}

/// nothing abandoned
#[test]
fn target_descendant_gives_empty() {
    let t = fork_tree();
    // b is a child of a: moving from a down to b abandons nothing.
    assert!(branch_entries(&t, "a", "b").is_empty());
    // Same for a deeper descendant.
    assert!(branch_entries(&t, "r", "d").is_empty());
    // Same leaf: nothing abandoned.
    assert!(branch_entries(&t, "d", "d").is_empty());
}

/// stops at the first entry that does not fit
#[test]
fn budget_newest_first() {
    let a = msg("a", None, "aaaa"); // 4
    let b = msg("b", Some("a"), "bb"); // 2
    let c = msg("c", Some("b"), "cccccc"); // 6
    let all: Vec<&Entry> = vec![&a, &b, &c];

    // 6 + 2 = 8 fits; 4 more would be 12 and ends the walk.
    let kept = oxa_compact::budget_newest_first(&all, 8, &len_count);
    assert_eq!(ids(&kept), vec!["b", "c"]);

    // The newest entry alone does not fit: the older, smaller ones are not kept either.
    let kept = oxa_compact::budget_newest_first(&all, 5, &len_count);
    assert!(kept.is_empty());

    // Everything fits.
    let kept = oxa_compact::budget_newest_first(&all, 100, &len_count);
    assert_eq!(ids(&kept), vec!["a", "b", "c"]);

    // Nothing fits in a zero budget, and an empty input gives an empty result.
    assert!(oxa_compact::budget_newest_first(&all, 0, &len_count).is_empty());
    assert!(oxa_compact::budget_newest_first(&[], 100, &len_count).is_empty());
}

/// details of nested entries are kept whole
#[test]
fn nested_summaries_carry_file_lists() {
    let compaction_summary = format!(
        "Goal: fix the parser{}",
        prompt::file_lists_block(&["src/a.rs".to_string()], &["src/b.rs".to_string()])
    );
    let branch_summary_text = format!(
        "{}Goal: try the other design{}",
        prompt::BRANCH_SUMMARY_PREAMBLE,
        prompt::file_lists_block(&[], &["src/c.rs".to_string()])
    );
    let comp = Entry {
        id: "cmp".into(),
        parent: None,
        ts_ms: 0,
        body: EntryBody::Compaction {
            summary: compaction_summary.clone(),
            first_kept: "x".into(),
            tokens_before: 1000,
            details: json!({"readFiles": ["src/a.rs"], "modifiedFiles": ["src/b.rs"]}),
            from_hook: false,
        },
    };
    let bs_details: Value = json!({"readFiles": [], "modifiedFiles": ["src/c.rs"]});
    let bs = Entry {
        id: "bsm".into(),
        parent: Some("cmp".into()),
        ts_ms: 0,
        body: EntryBody::BranchSummary {
            from: "old".into(),
            summary: branch_summary_text.clone(),
            details: bs_details.clone(),
            from_hook: false,
        },
    };
    let tail = msg("tail", Some("bsm"), "continue");
    let all: Vec<&Entry> = vec![&comp, &bs, &tail];

    // Generous budget: the nested summaries come back unchanged, text and details.
    let kept = oxa_compact::budget_newest_first(&all, 10_000, &len_count);
    assert_eq!(ids(&kept), vec!["cmp", "bsm", "tail"]);
    match &kept[0].body {
        EntryBody::Compaction { summary, details, .. } => {
            assert_eq!(summary, &compaction_summary);
            assert_eq!(
                details,
                &json!({"readFiles": ["src/a.rs"], "modifiedFiles": ["src/b.rs"]})
            );
            assert!(summary.contains("<read-files>\nsrc/a.rs\n</read-files>"));
        }
        other => panic!("expected a compaction entry, got {other:?}"),
    }
    match &kept[1].body {
        EntryBody::BranchSummary { summary, details, .. } => {
            assert_eq!(summary, &branch_summary_text);
            assert_eq!(details, &bs_details);
        }
        other => panic!("expected a branch summary entry, got {other:?}"),
    }

    // A budget that fits the tail but not the whole summary drops the summary entry entirely
    // (never truncated): the walk ends at it.
    let tail_cost = "continue".len() as u32;
    let kept =
        oxa_compact::budget_newest_first(&all, tail_cost + branch_summary_text.len() as u32 - 1, &len_count);
    assert_eq!(ids(&kept), vec!["tail"]);
}

/// old wire is a prefix
#[test]
fn request_is_pure_append() {
    let old = vec![
        WireMsg::user("please read the file"),
        WireMsg::assistant("reading"),
        WireMsg::tool_result("call_1", "file contents"),
        WireMsg::assistant("done"),
    ];
    let req = branch_summary_request(&old, None);
    assert_eq!(req.len(), old.len() + 1);
    assert!(wire_is_prefix(&old, &req));

    // Canonical bytes of the old wire are a byte prefix of the request.
    let mut old_bytes = Vec::new();
    oxa_proto::write_wire_json(&old, &mut old_bytes);
    let mut req_bytes = Vec::new();
    oxa_proto::write_wire_json(&req, &mut req_bytes);
    assert!(req_bytes.starts_with(&old_bytes[..old_bytes.len() - 1]));

    // The one appended message is the branch instruction, with no focus text.
    let last = req.last().expect("request has one appended message");
    assert_eq!(
        last,
        &WireMsg::user(prompt::summary_instruction(SummaryKind::Branch, None))
    );

    // Custom instructions extend the same instruction.
    let req = branch_summary_request(&old, Some("the parser"));
    assert!(wire_is_prefix(&old, &req));
    let text = req.last().and_then(|m| m.content.clone()).unwrap_or_default();
    assert!(text.ends_with(&format!("{}the parser", prompt::ADDITIONAL_FOCUS)));
    assert!(text.contains(prompt::BRANCH_SUMMARY_PROMPT));
}

#[test]
fn empty_old_wire_gives_only_the_instruction() {
    let req = branch_summary_request(&[], None);
    assert_eq!(req.len(), 1);
    assert!(req[0]
        .content
        .as_deref()
        .unwrap_or("")
        .starts_with(prompt::SUMMARY_INSTRUCTION_HEADER));
}
