//! T2 / R6 (wave H lane `local`): the malformed-call repair counter in the footer and in the
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
