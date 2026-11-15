//! T2 / R6 (wave H): the malformed-call repair counter in the footer and in the
//! status after a `tool_repair` event.

use oxa_interactive::{render_footer, StatusModel, ViewModel};
use oxa_tui::Theme;

fn footer(repairs: u32) -> String {
    let vm = ViewModel {
        status: StatusModel {
            cwd: "/work/proj".to_string(),
            model: "qwen".to_string(),
            tool_repairs: repairs,
            ..StatusModel::default()
        },
        ..ViewModel::default()
    };
    render_footer(&vm, 120, &Theme::plain())[1].clone()
}

#[test]
fn the_counter_shows_only_when_a_repair_happened() {
    assert!(!footer(0).contains("repairs"), "{}", footer(0));
    assert!(footer(2).contains("repairs 2"), "{}", footer(2));
}

/// A compaction without a model summary shows "no summary" in the footer until a compaction
/// with one replaces it.
#[test]
fn degraded_context_shows_until_a_model_summary() {
    use oxa_interactive::apply_event;
    use oxa_proto::{AgentEvent, CompactReason, CompactionResult};
    let end = |details: serde_json::Value| AgentEvent::CompactionEnd {
        reason: CompactReason::Threshold,
        result: Some(CompactionResult {
            summary: "s".into(),
            first_kept_entry_id: "e1".into(),
            tokens_before: 10,
            estimated_tokens_after: 5,
            usage: None,
            details,
        }),
        aborted: false,
        will_retry: false,
        error_message: None,
    };
    let mut vm = ViewModel::default();
    vm.status.model = "qwen".to_string();
    assert!(!render_footer(&vm, 120, &Theme::plain())[1].contains("no summary"));
    vm.status.context_tokens = 10;
    apply_event(&mut vm, &end(serde_json::json!({"mechanical": true})));
    assert_eq!(vm.status.context_tokens, 5);
    assert!(vm.blocks.iter().any(|b| matches!(
        b,
        oxa_interactive::Block::Notice { text, .. }
            if text == "Compacted from 10 to about 5 tokens"
    )));
    assert!(render_footer(&vm, 120, &Theme::plain())[1].contains("no summary"));
    apply_event(&mut vm, &end(serde_json::json!({})));
    assert!(!render_footer(&vm, 120, &Theme::plain())[1].contains("no summary"));
}
