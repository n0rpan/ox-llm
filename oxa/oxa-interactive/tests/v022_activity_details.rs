//! v0.2.2 (#871): subagent and retry activity carry bounded, readable text, and the closing
//! line of a run reports how long it took, behind `ui.activityDetails` (off unless the user
//! asks for it). Public API and rendered lines only: every assertion here reads the text a user
//! sees, with the setting on where the detail line is the subject.
use oxa_config::Keymap;
use oxa_interactive::{
    apply_event, arcade_scene, oxa_activity_line_with, oxa_arcade_line, retry_reason, Activity, Animation,
    ArcadeScene, ArcadeState, Block, Skin, StatusModel, ToolBlock, ToolState, ViewModel,
};
use oxa_proto::{AgentEvent, ToolOutput, Value};
use oxa_tui::{strip_ansi, visible_width, ColorMode, Theme};

fn colour() -> Theme {
    oxa_tui::builtin_theme("dark", ColorMode::TrueColor, None).expect("builtin dark theme")
}

fn user_block() -> Block {
    Block::User {
        text: "go".to_string(),
        images: Vec::new(),
    }
}

fn tool_block(id: &str) -> Block {
    Block::Tool(ToolBlock {
        call_id: id.to_string(),
        name: "read".to_string(),
        args: Value::Null,
        args_raw: String::new(),
        state: ToolState::Done,
        partial: String::new(),
        result: None,
        duration_ms: None,
        expanded: false,
    })
}

fn start_call(id: &str, name: &str) -> AgentEvent {
    AgentEvent::ToolExecutionStart {
        tool_call_id: id.to_string(),
        tool_name: name.to_string(),
        args: Value::Null,
    }
}

fn update_call(id: &str, name: &str, partial: &str) -> AgentEvent {
    AgentEvent::ToolExecutionUpdate {
        tool_call_id: id.to_string(),
        tool_name: name.to_string(),
        args: Value::Null,
        partial: partial.to_string(),
    }
}

/// The static text line of an activity (reduced motion: the same facts, no sprite).
fn text_line(st: &StatusModel, width: u16) -> String {
    let line = oxa_activity_line_with(st, &Keymap::defaults(), false, width, &colour(), Skin::Oxa)
        .expect("a running activity has a line");
    strip_ansi(&line)
}

/// A view model with `ui.activityDetails` on (off is the default; the tests below that read
/// the detail lines switch it on the way the setting does).
fn details() -> ViewModel {
    ViewModel {
        activity_details: true,
        ..ViewModel::default()
    }
}

/// The activity of a `subagent` call whose tool reported `partial`.
fn subagent(partial: &str) -> StatusModel {
    let mut vm = details();
    apply_event(&mut vm, &start_call("c1", "subagent"));
    apply_event(&mut vm, &update_call("c1", "subagent", partial));
    vm.status
}

/// A subagent call is its own activity (not the generic tool state), and the child's own
/// progress line reaches the screen as readable text under an `AGENTS` badge.
#[test]
fn subagent_progress_is_a_distinct_agents_activity() {
    let mut vm = details();
    apply_event(&mut vm, &start_call("c1", "subagent"));
    assert!(
        !matches!(vm.status.activity, Activity::Tool(_)),
        "a subagent call is its own activity: {:?}",
        vm.status.activity
    );

    apply_event(
        &mut vm,
        &update_call(
            "c1",
            "subagent",
            "subagent 1/2 explore: reading src/main.rs\nsubagent 2/2, queued",
        ),
    );
    let line = text_line(&vm.status, 120);
    assert!(line.contains("AGENTS"), "the badge names the state: {line:?}");
    assert!(
        line.contains("explore: reading src/main.rs"),
        "the running child and what it does: {line:?}"
    );
    assert!(
        !line.contains("queued"),
        "only the running child is shown: {line:?}"
    );

    // Finish: the activity is no longer the subagent one.
    apply_event(
        &mut vm,
        &AgentEvent::ToolExecutionEnd {
            tool_call_id: "c1".to_string(),
            tool_name: "subagent".to_string(),
            result: ToolOutput::ok("done"),
            duration_ms: Some(1200),
        },
    );
    assert!(
        !text_line(&vm.status, 120).contains("AGENTS"),
        "the subagent state is over: {:?}",
        vm.status.activity
    );
}

/// The progress block keeps finished children in task order, so the activity must follow the
/// child that is still running, not the first line (#871 review).
#[test]
fn the_activity_follows_the_running_child_not_the_first_line() {
    let mut vm = details();
    apply_event(&mut vm, &start_call("c1", "subagent"));
    apply_event(
        &mut vm,
        &update_call(
            "c1",
            "subagent",
            "subagent 1/2 scout: done\nsubagent 2/2 mapper: reading src/lib.rs",
        ),
    );
    let line = text_line(&vm.status, 120);
    assert!(
        line.contains("mapper: reading src/lib.rs"),
        "the running child, not the finished one: {line:?}"
    );
    assert!(
        !line.contains("scout"),
        "the finished child is not shown: {line:?}"
    );

    // A child that has not started yet is not the running one either.
    apply_event(
        &mut vm,
        &update_call("c1", "subagent", "subagent 1/2 scout: done\nsubagent 2/2, queued"),
    );
    let line = text_line(&vm.status, 120);
    assert!(
        !line.contains("queued"),
        "a queued child is not the running one: {line:?}"
    );

    // Every child is over: the block's own order is kept, and the count still comes from it.
    apply_event(
        &mut vm,
        &update_call(
            "c1",
            "subagent",
            "subagent 1/2 scout: done\nsubagent 2/2 mapper: failed",
        ),
    );
    let line = text_line(&vm.status, 120);
    assert!(line.contains("scout: done"), "{line:?}");
}

/// A retry names a bounded reason next to its attempt and delay; a provider's own text never
/// reaches the screen.
#[test]
fn retry_shows_a_bounded_reason_with_the_attempt_and_delay() {
    let mut vm = details();
    apply_event(
        &mut vm,
        &AgentEvent::AutoRetryStart {
            attempt: 1,
            max_attempts: 3,
            delay_ms: 2000,
            error_message: "the engine answered HTTP 503: queue full (retry after 2 s)".to_string(),
        },
    );
    let line = text_line(&vm.status, 120);
    assert!(line.contains("RETRY"), "the badge names the state: {line:?}");
    assert!(line.contains("1/3"), "the attempt and its cap: {line:?}");
    assert!(line.contains("in 2s"), "the wait: {line:?}");
    assert!(line.contains("engine busy"), "the typed reason: {line:?}");

    let mut vm = details();
    apply_event(
        &mut vm,
        &AgentEvent::AutoRetryStart {
            attempt: 2,
            max_attempts: 3,
            delay_ms: 1000,
            error_message: "api_key sk-live-abc123 rejected by upstream https://example.invalid".to_string(),
        },
    );
    let line = text_line(&vm.status, 200);
    assert!(!line.contains("sk-live-abc123"), "no provider text: {line:?}");
    assert!(!line.contains("example.invalid"), "no provider URL: {line:?}");
    assert!(
        line.contains("transient error"),
        "a fixed label instead: {line:?}"
    );
}

/// The classifier reads the messages the engine layer really produces, hint included, and
/// never invents a reason it cannot name.
#[test]
fn the_retry_reason_reads_the_real_engine_messages() {
    assert_eq!(
        retry_reason(
            "the engine answered HTTP 503: Too many requests are waiting for the local model; \
             retry in 3 s. (retry after 3 s)\n\
             the engine is busy (its queue is full or no model is loaded); the request is retried"
        ),
        "engine busy"
    );
    assert_eq!(
        retry_reason(
            "cannot reach the engine at http://127.0.0.1:8080: connection refused\n\
             start the engine or set OXA_ENGINE_URL"
        ),
        "connection lost"
    );
    assert_eq!(
        retry_reason(
            "connecting to the engine at http://127.0.0.1:8080 timed out\n\
             the engine did not accept the connection in time; check OXA_ENGINE_URL"
        ),
        "timeout"
    );
    assert_eq!(
        retry_reason("the engine answered HTTP 500: internal error"),
        "engine error"
    );
    assert_eq!(retry_reason("HTTP 429: rate limited"), "rate limited");
    assert_eq!(
        retry_reason("the engine closed the stream before [DONE]"),
        "connection lost"
    );
    assert_eq!(
        retry_reason("the engine reported an error mid-stream: overloaded"),
        "connection lost"
    );
    // `upstream` is not a broken stream: a provider's own wording must not pick a label.
    assert_eq!(
        retry_reason("api_key sk-live-abc123 rejected by upstream https://example.invalid"),
        "transient error"
    );
    assert_eq!(retry_reason("something else entirely"), "transient error");
}

/// The unreleased detail lines are off by default (`ui.activityDetails`): a subagent call
/// stays the plain tool state with the tool's name, and a retry keeps its attempt and delay
/// without a reason. Turning the setting on is what names them.
#[test]
fn the_detail_lines_are_off_by_default() {
    assert!(!ViewModel::default().activity_details, "off unless asked for");
    let mut vm = ViewModel::default();
    apply_event(&mut vm, &start_call("c1", "subagent"));
    apply_event(
        &mut vm,
        &update_call("c1", "subagent", "subagent 1/1 explore: reading src/main.rs"),
    );
    assert_eq!(
        vm.status.activity,
        Activity::Tool("subagent".to_string()),
        "the plain tool state, not the child's progress"
    );
    let line = text_line(&vm.status, 120);
    assert!(!line.contains("AGENTS"), "{line:?}");
    assert!(!line.contains("explore"), "no child's name: {line:?}");

    let mut vm = ViewModel::default();
    apply_event(
        &mut vm,
        &AgentEvent::AutoRetryStart {
            attempt: 1,
            max_attempts: 3,
            delay_ms: 2000,
            error_message: "the engine answered HTTP 503: queue full".to_string(),
        },
    );
    assert_eq!(
        vm.status.activity,
        Activity::Retrying {
            attempt: 1,
            max_attempts: 3,
            delay_ms: 2000,
            reason: None,
        },
        "the attempt and the delay only"
    );
    let line = text_line(&vm.status, 120);
    assert!(line.contains("1/3") && line.contains("in 2s"), "{line:?}");
    assert!(!line.contains("engine busy"), "no reason: {line:?}");
}

/// `ui.activityDetails` is a known setting, off by default, and the reducer reads it from
/// here (the interactive loop copies its value into `ViewModel::activity_details`).
#[test]
fn the_setting_is_known_and_off_by_default() {
    let key = oxa_config::schema()
        .iter()
        .find(|k| k.key == "ui.activityDetails")
        .expect("ui.activityDetails is in the settings schema");
    assert!(key.supported, "a working setting, not an ignored key");
    assert_eq!(key.default, "false", "off by default");
    let unset = oxa_config::Settings::from_values(Value::Null, Value::Null);
    assert!(
        !unset.bool("ui.activityDetails", false),
        "off unless asked for"
    );
    let on = oxa_config::Settings::from_values(
        serde_json::json!({"ui": {"activityDetails": true}}),
        Value::Null,
    );
    assert!(on.bool("ui.activityDetails", false), "the setting is read");
}

/// The closing `CLEAR` / `FAILED` line keeps the tool count and adds how long the run took;
/// the count is not repeated. Untimed (the default), the count is all it reports.
#[test]
fn the_closing_line_reports_elapsed_time_and_the_tool_count_once() {
    let mut state = ArcadeState::default();
    let mut blocks = vec![user_block()];
    state.observe(true, &blocks, 1_000, true);
    blocks.extend([tool_block("a"), tool_block("b"), tool_block("c")]);
    state.observe(true, &blocks, 5_000, true);
    state.observe(false, &blocks, 43_000, true);
    let run = state.run(43_200);
    let st = StatusModel {
        activity: Activity::Idle,
        ..StatusModel::default()
    };
    let line = oxa_arcade_line(
        &st,
        &Keymap::defaults(),
        Animation::Arcade,
        ArcadeScene::Charge,
        &run,
        100,
        &colour(),
        Skin::Oxa,
    )
    .expect("the closing line");
    let line = strip_ansi(&line);
    assert!(line.contains("CLEAR"), "{line:?}");
    assert!(line.contains("3 tools"), "the tool count stays: {line:?}");
    assert!(line.contains("42s"), "the run's elapsed time: {line:?}");
    assert_eq!(
        line.matches('3').count(),
        1,
        "the tool count is not duplicated: {line:?}"
    );

    // Untimed (the default), the closing line reports the count and no time.
    let mut untimed = ArcadeState::default();
    untimed.observe(true, &blocks, 1_000, false);
    untimed.observe(false, &blocks, 43_000, false);
    assert_eq!(untimed.run(43_200).elapsed_ms, None, "no time is kept");
    let line = oxa_arcade_line(
        &st,
        &Keymap::defaults(),
        Animation::Arcade,
        ArcadeScene::Charge,
        &untimed.run(43_200),
        100,
        &colour(),
        Skin::Oxa,
    )
    .expect("the closing line");
    let line = strip_ansi(&line);
    assert!(line.contains("3 tools"), "{line:?}");
    assert!(!line.contains("42s"), "no elapsed time: {line:?}");
}

/// Reduced motion and a plain theme keep the same facts as static text, and the line stays one
/// line that fits the width.
#[test]
fn the_same_facts_survive_reduced_motion_and_plain_colour() {
    let mut retry = details();
    apply_event(
        &mut retry,
        &AgentEvent::AutoRetryStart {
            attempt: 1,
            max_attempts: 3,
            delay_ms: 2000,
            error_message: "HTTP 429: rate limited".to_string(),
        },
    );
    for st in [
        subagent("subagent 1/1 explore: reading src/main.rs"),
        retry.status,
    ] {
        for theme in [colour(), Theme::plain()] {
            let line = oxa_activity_line_with(&st, &Keymap::defaults(), false, 120, &theme, Skin::Oxa)
                .expect("a running activity has a line");
            let visible = strip_ansi(&line);
            assert!(
                visible.contains("AGENTS") || visible.contains("RETRY"),
                "{visible:?}"
            );
            assert_eq!(line.lines().count(), 1, "{line:?}");
            assert!(visible_width(&line) <= 120, "{visible:?}");
        }
    }
}

/// The moving line of a subagent is its own sprite: the ox and the children it spawns, not
/// the plain tool sprite (#871).
#[test]
fn the_subagent_sprite_is_its_own() {
    let lead = |st: &StatusModel| {
        let line = oxa_activity_line_with(st, &Keymap::defaults(), true, 120, &colour(), Skin::Oxa)
            .expect("a running activity has a line");
        strip_ansi(&line)
    };
    let mut tool = details();
    apply_event(&mut tool, &start_call("c1", "read"));
    let agents = lead(&subagent("subagent 1/1 explore: running"));
    let plain = lead(&tool.status);
    assert!(agents.contains("AGENTS"), "{agents:?}");
    assert!(
        agents.contains('▪') && !plain.contains('▪'),
        "the subagent sprite shows its children: {agents:?} vs {plain:?}"
    );
}

/// A subagent draws one crate per child it reports; an ordinary tool draws exactly one.
#[test]
fn the_subagent_scene_shows_a_crate_per_child() {
    let run = oxa_interactive::ArcadeRun::default();
    let crates = |st: &StatusModel| {
        let scene = arcade_scene(ArcadeScene::Charge, st, &run, 12, &colour(), Skin::Oxa)
            .expect("a scene while the call runs");
        strip_ansi(&scene).matches('▐').count()
    };
    assert_eq!(
        crates(&subagent("subagent 1/2 explore: reading src/main.rs")),
        2,
        "one crate per child"
    );
    assert_eq!(
        crates(&subagent("subagent 1/1 explore: running")),
        1,
        "a single child is one crate"
    );
    let mut tool = details();
    apply_event(&mut tool, &start_call("c1", "read"));
    assert_eq!(crates(&tool.status), 1, "a plain tool is one crate");
}
