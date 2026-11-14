//! Lane l5-compact: the soft summary length target (V020-OPTIMIZE #7), the mechanical summary
//! (OXA-LOCAL-FIRST R4) and the purity of the cut (R4 first half). Pure: no engine, no files.

use oxa_compact::prompt::{self, SummaryKind};
use oxa_compact::{
    first_kept_index, head_after, mechanical_summary, summary_request, summary_request_with_target,
    CompactPolicy, When,
};
use oxa_proto::{Role, ToolCall, WireMsg};
use oxa_session::Transcript;

fn count(s: &str) -> u32 {
    s.chars().count().div_ceil(4) as u32
}

fn transcript(msgs: &[WireMsg]) -> Transcript {
    let mut t = Transcript::default();
    for m in msgs {
        t.push(m.clone());
    }
    t
}

fn call(id: &str, name: &str, path: &str) -> WireMsg {
    WireMsg {
        role: Role::Assistant,
        content: Some(String::new()),
        reasoning: None,
        tool_calls: vec![ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            args_raw: format!(r#"{{"path":"{path}"}}"#),
        }],
        tool_call_id: None,
    }
}

fn wire() -> Vec<WireMsg> {
    vec![
        WireMsg::user("first task"),
        call("c1", "read", "a.rs"),
        WireMsg::tool_result("c1", "A".repeat(400)),
        WireMsg::assistant("read it"),
        WireMsg::user("second task"),
        call("c2", "edit", "b.rs"),
        WireMsg::tool_result("c2", "ok"),
        WireMsg::assistant("edited"),
        WireMsg::user("third task"),
        WireMsg::assistant("done"),
    ]
}

#[test]
fn target_words_rounding() {
    assert_eq!(prompt::summary_target_words(0), 0);
    assert_eq!(prompt::summary_target_words(700), 550, "525 rounds half up");
    assert_eq!(prompt::summary_target_words(1000), 750);
    assert_eq!(prompt::summary_target_words(1), 50, "at least 50");
}

/// Golden of the instruction with and without the target.
#[test]
fn instruction_golden_with_and_without_target() {
    let plain = prompt::summary_instruction(SummaryKind::Initial, None);
    assert_eq!(
        plain,
        format!(
            "{}\n\n{}",
            prompt::SUMMARY_INSTRUCTION_HEADER,
            prompt::SUMMARIZATION_PROMPT
        )
    );
    assert_eq!(
        prompt::summary_instruction_with_target(SummaryKind::Initial, None, 0),
        plain
    );
    let target = prompt::summary_instruction_with_target(SummaryKind::Initial, None, 550);
    assert_eq!(
        target,
        format!("{plain}\n\nKeep the whole summary to at most about 550 words.")
    );
    let focus = prompt::summary_instruction_with_target(SummaryKind::Update, Some("the API"), 550);
    assert!(focus.ends_with(
        "Preserve exact file paths, function names, and error messages.\n\nKeep the whole summary to at most about 550 words.\n\nAdditional focus: the API"
    ));
    println!(
        "instruction bytes: {} without, {} with the target",
        plain.len(),
        target.len()
    );
}

/// The request with the target is still the whole wire plus ONE user message.
#[test]
fn target_keeps_one_instruction_message() {
    let t = transcript(&wire());
    let plain = summary_request(&t, None);
    let with = summary_request_with_target(&t, None, 550);
    assert_eq!(plain.len(), t.wire().len() + 1);
    assert_eq!(with.len(), plain.len());
    assert_eq!(with[..t.wire().len()], plain[..t.wire().len()]);
    assert_eq!(summary_request_with_target(&t, None, 0), plain);
}

/// Golden of the mechanical summary; same input, same bytes.
#[test]
fn mechanical_golden() {
    let w = wire();
    let long = format!("{}{}", "x".repeat(299), "ü".repeat(5));
    let mut msgs = w[..8].to_vec();
    msgs.push(WireMsg::user(&long));
    let read = vec!["a.rs".to_string()];
    let modified = vec!["b.rs".to_string()];
    let text = mechanical_summary(&msgs, &read, &modified);
    let expected = format!(
        "{}\n\n## User messages\n1. first task\n2. second task\n3. {}ü [...]\n\n<read-files>\na.rs\n</read-files>\n\n<modified-files>\nb.rs\n</modified-files>\n\nTool results left out: 2.",
        prompt::MECHANICAL_SUMMARY_HEADER,
        "x".repeat(299)
    );
    assert_eq!(text, expected);
    assert_eq!(mechanical_summary(&msgs, &read, &modified), text, "pure");
}

/// A previous summary at the head is carried (without its own file lists); no prompts -> (none).
#[test]
fn mechanical_keeps_earlier_summary() {
    let prev = format!(
        "## Goal\nold goal{}",
        prompt::file_lists_block(&["a.rs".into()], &[])
    );
    let msgs = vec![
        WireMsg::user(prompt::summary_user_message(&prev)),
        call("c1", "read", "a.rs"),
        WireMsg::tool_result("c1", "A"),
    ];
    let text = mechanical_summary(&msgs, &["a.rs".to_string()], &[]);
    assert_eq!(
        text,
        format!(
            "{}\n\n## Earlier summary\n## Goal\nold goal\n\n## User messages\n(none)\n\n<read-files>\na.rs\n</read-files>\n\nTool results left out: 1.",
            prompt::MECHANICAL_SUMMARY_HEADER
        )
    );
}

/// R4 first half: the cut and the kept tail are pure functions of the wire.
#[test]
fn cut_and_head_are_pure_functions_of_the_wire() {
    let p = CompactPolicy {
        soft: 1,
        hard: 1_000_000,
        keep_tokens: 20,
        summary_max: 1024,
        enabled: true,
    };
    for w in [When::AfterReply, When::MidRun] {
        let (a, b) = (transcript(&wire()), transcript(&wire()));
        let (ca, cb) = (
            first_kept_index(&a, w, &p, &count),
            first_kept_index(&b, w, &p, &count),
        );
        assert_eq!(ca, cb);
        assert!(ca > 0 && ca < a.wire().len());
        assert_eq!(
            head_after(&a, w, "S", &p, &count),
            head_after(&b, w, "S", &p, &count)
        );
        assert_eq!(summary_request(&a, None), summary_request(&b, None));
    }
}
