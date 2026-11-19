//! B1 acceptance tests: Compaction policy and summary shapes.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.
//!
//! The tests are pure: they build transcripts in memory (`Transcript::push`) and never touch the
//! file system, so no `TestEnv` is needed.

use oxa_compact::prompt::{self, SummaryKind};
use oxa_compact::{
    decide, file_lists, first_kept_index, head_after, policy_from, summary_request, CompactPolicy, When,
};
use oxa_proto::{rebase_why, wire_is_prefix, Entry, EntryBody, Role, ToolCall, WireMsg};
use oxa_session::Transcript;
use serde_json::json;

/// chars / 4, rounded up: the token count used by the tests.
fn count(s: &str) -> u32 {
    s.chars().count().div_ceil(4) as u32
}

/// A policy with a given keep budget and no soft or hard limit in the way.
fn pol(keep: u32) -> CompactPolicy {
    CompactPolicy {
        soft: 1,
        hard: 1_000_000,
        keep_tokens: keep,
        summary_max: 1024,
        enabled: true,
    }
}

fn transcript(msgs: Vec<WireMsg>) -> Transcript {
    let mut t = Transcript::default();
    for m in msgs {
        t.push(m);
    }
    t
}

/// An assistant message with tool calls `(name, args_raw)`; ids are `call_0`, `call_1`, ...
fn assistant_calls(text: Option<&str>, calls: &[(&str, &str)]) -> WireMsg {
    WireMsg {
        images: Vec::new(),
        role: Role::Assistant,
        content: text.map(str::to_owned),
        reasoning: None,
        tool_calls: calls
            .iter()
            .enumerate()
            .map(|(i, (name, args))| ToolCall {
                id: format!("call_{i}"),
                name: (*name).to_owned(),
                args_raw: (*args).to_owned(),
            })
            .collect(),
        provider_history: None,
        tool_call_id: None,
    }
}

fn entry(id: &str, body: EntryBody) -> Entry {
    Entry {
        id: id.to_owned(),
        parent: None,
        ts_ms: 0,
        body,
    }
}

/// soft crossed at run end
#[test]
fn decide_after_reply() {
    // hard = 10000 - 1000 - 1000 = 8000, soft = 5000
    let p = policy_from(10_000, 1_000, 1_000, 4000, Some(5000), true);
    assert_eq!(p.hard, 8000);
    assert_eq!(p.soft, 5000);
    assert_eq!(decide(5001, true, &p), When::AfterReply);
    // Only idle time after a run compacts between soft and hard.
    assert_eq!(decide(5001, false, &p), When::No);
    // Not strictly above soft.
    assert_eq!(decide(5000, true, &p), When::No);
    assert_eq!(decide(6000, false, &p), When::No);
    // Disabled: no soft compaction at all.
    let off = CompactPolicy { enabled: false, ..p };
    assert_eq!(decide(5001, true, &off), When::No);
}

/// also when disabled
#[test]
fn decide_mid_run_only_above_hard() {
    let p = policy_from(10_000, 1_000, 1_000, 4000, Some(5000), true);
    let off = CompactPolicy { enabled: false, ..p };
    // Above hard: mid-run, with or without the run having ended, and when disabled.
    assert_eq!(decide(8001, false, &p), When::MidRun);
    assert_eq!(decide(8001, true, &p), When::MidRun);
    assert_eq!(decide(8001, false, &off), When::MidRun);
    // Exactly hard is not above it.
    assert_eq!(decide(8000, false, &off), When::No);
    assert_eq!(decide(8000, true, &p), When::AfterReply);
}

/// wire is a prefix of the request
#[test]
fn summary_request_is_pure_append() {
    let t = transcript(vec![
        WireMsg::user("u1"),
        assistant_calls(Some("looking"), &[("read", r#"{"path":"a.rs"}"#)]),
        WireMsg::tool_result("call_0", "fn main() {}"),
        WireMsg::assistant("done"),
    ]);
    let req = summary_request(&t, None);
    assert!(wire_is_prefix(t.wire(), &req));
    assert_eq!(req.len(), t.wire().len() + 1);
    let last = req.last().unwrap();
    assert_eq!(last.role, Role::User);
    assert!(last.tool_calls.is_empty());
    let text = last.content.as_deref().unwrap();
    assert!(text.starts_with(prompt::SUMMARY_INSTRUCTION_HEADER));
    assert!(text.ends_with(prompt::SUMMARIZATION_PROMPT));

    // Canonical bytes: the prefix bytes are untouched.
    let mut before = Vec::new();
    oxa_proto::write_wire_json(t.wire(), &mut before);
    let mut after = Vec::new();
    oxa_proto::write_wire_json(&req[..t.wire().len()], &mut after);
    assert_eq!(before, after);

    // `/compact <instructions>` adds the focus line and nothing else changes.
    let focused = summary_request(&t, Some("keep the API  "));
    assert!(wire_is_prefix(t.wire(), &focused));
    assert!(focused
        .last()
        .unwrap()
        .content
        .as_deref()
        .unwrap()
        .ends_with("\n\nAdditional focus: keep the API"));
}

/// the current prompt is kept verbatim, never folded
#[test]
fn mid_run_prompt_is_own_message() {
    let prompt_text = "PROMPT: fix the parser, the current request";
    let t = transcript(vec![
        WireMsg::user("an older question"),
        WireMsg::assistant("an older answer"),
        WireMsg::user(prompt_text),
        assistant_calls(None, &[("read", r#"{"path":"p.rs"}"#)]),
        WireMsg::tool_result("call_0", "parser source"),
    ]);
    // A tiny budget would cut at the tool-calling assistant message; MidRun keeps the prompt.
    let p = pol(1);
    let cut = first_kept_index(&t, When::MidRun, &p, &count);
    assert_eq!(cut, 2);
    let head = head_after(&t, When::MidRun, "SUMMARY OF EARLIER WORK", &p, &count);
    assert_eq!(head.len(), 4);
    assert_eq!(
        head[0],
        WireMsg::user(prompt::summary_user_message("SUMMARY OF EARLIER WORK"))
    );
    // The prompt is its own message, verbatim, and is not inside the summary.
    assert_eq!(head[1], WireMsg::user(prompt_text));
    assert!(!head[0].content.as_deref().unwrap().contains("PROMPT"));
    assert_eq!(head[2].role, Role::Assistant);
    assert_eq!(head[3].role, Role::Tool);
}

/// old summary is not kept; update instruction used
#[test]
fn repeated_compaction() {
    let t = transcript(vec![
        WireMsg::user(prompt::summary_user_message("OLD SUMMARY TEXT")),
        WireMsg::user("u2"),
        WireMsg::assistant("a2"),
        WireMsg::user("u3"),
        WireMsg::assistant("a3"),
    ]);
    // The request uses the update instruction and is still a pure append.
    let req = summary_request(&t, None);
    assert!(wire_is_prefix(t.wire(), &req));
    let text = req.last().unwrap().content.as_deref().unwrap();
    assert!(text.contains(prompt::UPDATE_SUMMARIZATION_LEAD));
    assert!(text.contains(prompt::UPDATE_SUMMARIZATION_INSTRUCTIONS));
    assert!(!text.contains("The messages above are a conversation to summarize."));

    // Whatever the budget, the old summary is summarised, never kept.
    for keep in [0, 1, 10, 1_000_000] {
        let head = head_after(&t, When::AfterReply, "NEW SUMMARY TEXT", &pol(keep), &count);
        assert_eq!(
            head[0],
            WireMsg::user(prompt::summary_user_message("NEW SUMMARY TEXT"))
        );
        for m in &head {
            assert!(
                !m.content.as_deref().unwrap_or("").contains("OLD SUMMARY"),
                "keep={keep}"
            );
        }
    }
    // A budget that holds the whole conversation keeps all of it after the new summary.
    let all = head_after(&t, When::AfterReply, "NEW", &pol(1_000_000), &count);
    assert_eq!(all.len(), 5);
    assert_eq!(all[1], WireMsg::user("u2"));
    // Budget zero keeps only the last message.
    let last = head_after(&t, When::AfterReply, "NEW", &pol(0), &count);
    assert_eq!(last.len(), 2);
    assert_eq!(last[1], WireMsg::assistant("a3"));
}

/// valid cut points only
#[test]
fn cut_never_splits_tool_results() {
    let big = "x".repeat(400);
    let t = transcript(vec![
        WireMsg::user("q1"),
        assistant_calls(Some("reading"), &[("read", r#"{"path":"a.rs"}"#)]),
        WireMsg::tool_result("call_0", &big),
        assistant_calls(
            None,
            &[("read", r#"{"path":"b.rs"}"#), ("grep", r#"{"pattern":"x"}"#)],
        ),
        WireMsg::tool_result("call_0", &big),
        WireMsg::tool_result("call_1", &big),
        WireMsg::assistant("answer"),
        WireMsg::user("q2"),
        assistant_calls(None, &[("edit", r#"{"path":"a.rs"}"#)]),
        WireMsg::tool_result("call_0", &big),
    ]);
    for keep in 0..=1500 {
        let p = pol(keep);
        let cut = first_kept_index(&t, When::AfterReply, &p, &count);
        assert!(cut <= t.wire().len());
        if cut < t.wire().len() {
            assert!(
                matches!(t.wire()[cut].role, Role::User | Role::Assistant),
                "keep={keep}: cut lands on {:?}",
                t.wire()[cut].role
            );
        }
        let head = head_after(&t, When::AfterReply, "S", &p, &count);
        assert!(
            head.get(1).is_none_or(|m| m.role != Role::Tool),
            "keep={keep}: tail starts with a tool result"
        );
    }
    // Trailing tool results that exceed the budget by themselves keep their call with them.
    let tail_heavy = transcript(vec![
        WireMsg::user("q"),
        assistant_calls(None, &[("read", r#"{"path":"z.rs"}"#)]),
        WireMsg::tool_result("call_0", "y".repeat(4000)),
    ]);
    assert_eq!(
        first_kept_index(&tail_heavy, When::AfterReply, &pol(10), &count),
        1
    );
}

/// tail size bound
#[test]
fn keep_tokens_budget() {
    // 20 messages of 100 tokens each.
    let msgs: Vec<WireMsg> = (0..20)
        .map(|i| {
            let text = "w".repeat(400);
            if i % 2 == 0 {
                WireMsg::user(text)
            } else {
                WireMsg::assistant(text)
            }
        })
        .collect();
    let t = transcript(msgs);
    let tail_tokens = |keep: u32| -> u32 {
        let cut = first_kept_index(&t, When::AfterReply, &pol(keep), &count);
        t.wire()[cut..]
            .iter()
            .map(|m| count(m.content.as_deref().unwrap()))
            .sum()
    };
    assert_eq!(tail_tokens(1000), 1000);
    for keep in 1..=1900 {
        let tail = tail_tokens(keep);
        assert!(tail >= keep, "keep={keep}: tail {tail} below the budget");
        assert!(
            tail <= keep + 100,
            "keep={keep}: tail {tail} above budget plus one message"
        );
    }
    // A budget larger than the whole wire keeps all of it.
    assert_eq!(first_kept_index(&t, When::AfterReply, &pol(5000), &count), 0);
}

/// across earlier compaction details
#[test]
fn file_lists_accumulate() {
    let entries = [
        entry(
            "c1",
            EntryBody::Compaction {
                summary: "s1".into(),
                first_kept: "m1".into(),
                tokens_before: 9000,
                details: json!({"readFiles": ["src/a.rs", "src/b.rs"], "modifiedFiles": ["src/c.rs"]}),
                from_hook: false,
            },
        ),
        // Hook-generated details are not read.
        entry(
            "c2",
            EntryBody::Compaction {
                summary: "s2".into(),
                first_kept: "m2".into(),
                tokens_before: 9000,
                details: json!({"readFiles": ["src/hook.rs"], "modifiedFiles": ["src/hook2.rs"]}),
                from_hook: true,
            },
        ),
        entry(
            "m3",
            EntryBody::Msg {
                wire: assistant_calls(
                    None,
                    &[
                        ("read", r#"{"path":"src/d.rs"}"#),
                        // a.rs was read earlier and is edited now: only in modified
                        ("edit", r#"{"path":"src/a.rs"}"#),
                        ("write", r#"{"path":"src/e.rs"}"#),
                        // not file operations
                        ("read", r#"{"path":""}"#),
                        ("read", "{oops"),
                        ("bash", r#"{"path":"src/nope.rs"}"#),
                    ],
                ),
                parts: vec![],
                tool: None,
            },
        ),
        entry(
            "b1",
            EntryBody::BranchSummary {
                from: "x".into(),
                summary: "branch".into(),
                details: json!({"readFiles": [], "modifiedFiles": ["src/f.rs"]}),
                from_hook: false,
            },
        ),
    ];
    let branch: Vec<&Entry> = entries.iter().collect();
    let (read, modified) = file_lists(&branch);
    assert_eq!(read, vec!["src/b.rs".to_owned(), "src/d.rs".to_owned()]);
    assert_eq!(
        modified,
        vec![
            "src/a.rs".to_owned(),
            "src/c.rs".to_owned(),
            "src/e.rs".to_owned(),
            "src/f.rs".to_owned()
        ]
    );
    assert_eq!(file_lists(&[]), (vec![], vec![]));
}

/// soft < hard, minimums
#[test]
fn policy_from_clamps() {
    // hard saturates to the 1024 minimum; soft default is 75% of that and below it.
    let small = policy_from(8192, 4096, 16384, 4000, None, true);
    assert_eq!(small.hard, 1024);
    assert_eq!(small.soft, 768);
    assert!(small.soft < small.hard);
    // summary_max = min(max_output, reserve * 4 / 5)
    assert_eq!(small.summary_max, 4096);

    // A soft setting at or above hard is clamped below it.
    let clamped = policy_from(8192, 4096, 16384, 4000, Some(5000), true);
    assert_eq!(clamped.soft, 1023);
    assert!(clamped.soft < clamped.hard);

    // Zero is clamped up to one token.
    assert_eq!(policy_from(8192, 4096, 16384, 4000, Some(0), true).soft, 1);

    // Normal window: hard = n_ctx - max_output - reserve, soft = 75% of hard.
    let big = policy_from(131_072, 8192, 16384, 4000, None, false);
    assert_eq!(big.hard, 106_496);
    assert_eq!(big.soft, 79_872);
    assert_eq!(big.keep_tokens, 4000);
    assert_eq!(big.summary_max, 8192);
    assert!(!big.enabled);

    // reserve * 4 / 5 limits the summary request.
    assert_eq!(
        policy_from(131_072, 8192, 1000, 4000, None, true).summary_max,
        800
    );
    // Subtraction does not wrap when max_output exceeds the window.
    assert_eq!(policy_from(100, 4096, 16384, 4000, None, true).hard, 1024);
}

/// A summary request without a compaction decision is a bug.
#[test]
#[should_panic]
fn head_after_without_decision_panics() {
    let t = transcript(vec![WireMsg::user("u"), WireMsg::assistant("a")]);
    let _ = head_after(&t, When::No, "S", &pol(10), &count);
}

/// A wire of nothing but a summary has no cut point: nothing is kept after it.
#[test]
fn nothing_fits_returns_wire_len() {
    let t = transcript(vec![WireMsg::user(prompt::summary_user_message("only summary"))]);
    assert_eq!(first_kept_index(&t, When::AfterReply, &pol(0), &count), 1);
    let head = head_after(&t, When::AfterReply, "next", &pol(0), &count);
    assert_eq!(head, vec![WireMsg::user(prompt::summary_user_message("next"))]);
}

/// The summary request of a first compaction uses the initial instruction.
#[test]
fn initial_request_uses_initial_instruction() {
    let t = transcript(vec![WireMsg::user("hello"), WireMsg::assistant("hi")]);
    let req = summary_request(&t, None);
    let text = req.last().unwrap().content.as_deref().unwrap();
    assert_eq!(text, prompt::summary_instruction(SummaryKind::Initial, None));
}

/// Compaction entries carry the rebase cause that the fold uses.
#[test]
fn rebase_cause_is_compaction() {
    assert_eq!(rebase_why::COMPACTION, "compaction");
}

/// Both first-summary and update-summary requests keep identifiers verbatim. The rule lives in
/// the oxa-owned header, because pi's summary prompts are pinned byte-for-byte by parity.
#[test]
fn summary_requests_require_verbatim_identifiers() {
    let line = "Keep verbatim: PR and issue numbers, commit SHAs, branch names, CI run ids with their conclusions, worker names and session ids, and open questions awaiting the owner.";
    assert!(prompt::SUMMARY_INSTRUCTION_HEADER.contains(line));
    assert!(prompt::summary_instruction(SummaryKind::Initial, None).contains(line));
    assert!(prompt::summary_instruction(SummaryKind::Update, None).contains(line));
}
