//! The shared fixture events (`oxa_fake::fixtures`) through the terminal UI.
//!
//! The desktop app feeds the same sequences through the same reducer and block renderer and
//! compares its text with what is rendered here, so this file is the terminal half of that
//! contract: every fixture reduces and renders, at every point of the run, in every look.
//! Public API only; nothing touches a file or the environment.

use oxa_fake::fixtures::{self, Fixture};
use oxa_interactive::{
    apply_event, frames, transcript_lines, Activity, Block, Skin, ToolState, ViewModel, ViewSettings,
};
use oxa_proto::AgentEvent;
use oxa_tui::{builtin_theme, strip_ansi, visible_width, ColorMode, Theme};

const WIDTHS: [u16; 2] = [80, 120];

fn themes() -> Vec<Theme> {
    vec![
        Theme::plain(),
        builtin_theme("dark", ColorMode::TrueColor, None).expect("builtin dark theme"),
    ]
}

fn looks() -> Vec<ViewSettings> {
    [Skin::Pi, Skin::Oxa, Skin::OxaAscii]
        .into_iter()
        .map(|skin| ViewSettings {
            skin,
            ..ViewSettings::default()
        })
        .collect()
}

/// The model after `events`.
fn reduce(events: &[AgentEvent]) -> ViewModel {
    let mut vm = ViewModel::default();
    for e in events {
        apply_event(&mut vm, e);
    }
    vm
}

/// The transcript text of a model at 80 columns, plain theme, without styling.
fn text_of(vm: &ViewModel, settings: &ViewSettings) -> String {
    let (lines, _) = transcript_lines(vm, 80, &Theme::plain(), settings);
    lines.iter().map(|l| strip_ansi(l)).collect::<Vec<_>>().join("\n")
}

fn fixture(name: &str) -> Fixture {
    fixtures::by_name(name).unwrap_or_else(|| panic!("no fixture called {name}"))
}

/// Every fixture renders through `frames` in every look, theme and width: the texts it
/// promises are on the screen and no line is wider than the terminal.
#[test]
fn every_fixture_renders_through_frames() {
    for f in fixtures::all() {
        for theme in themes() {
            for settings in looks() {
                for width in WIDTHS {
                    let lines = frames(&f.events, width, &theme, &settings);
                    let at = format!("{} at {width} columns, {:?}", f.name, settings.skin);
                    assert!(!lines.is_empty(), "{at}: nothing rendered");
                    for l in &lines {
                        assert!(visible_width(l) <= width as usize, "{at}: line too wide: {l:?}");
                    }
                    let text: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
                    for want in f.shows {
                        assert!(
                            text.iter().any(|l| l.contains(want)),
                            "{at}: {want:?} is not shown in\n{}",
                            text.join("\n")
                        );
                    }
                }
            }
        }
    }
}

/// A front end draws while the run goes on: every prefix of every fixture reduces and renders
/// within the width, and a finished run leaves nothing open.
#[test]
fn every_point_of_a_run_can_be_drawn_and_the_end_is_settled() {
    for f in fixtures::all() {
        for settings in looks() {
            let mut vm = ViewModel::default();
            for (i, e) in f.events.iter().enumerate() {
                apply_event(&mut vm, e);
                let (lines, _) = transcript_lines(&vm, 80, &Theme::plain(), &settings);
                for l in &lines {
                    assert!(
                        visible_width(l) <= 80,
                        "{} after event {i} ({}): line too wide: {l:?}",
                        f.name,
                        e.name()
                    );
                }
            }
            assert!(
                matches!(vm.status.activity, Activity::Idle),
                "{}: the run ended but the activity is {:?}",
                f.name,
                vm.status.activity
            );
            for b in &vm.blocks {
                match b {
                    Block::Assistant { streaming, .. } => {
                        assert!(!streaming, "{}: a reply is still streaming", f.name)
                    }
                    Block::Tool(t) => assert!(
                        matches!(t.state, ToolState::Done | ToolState::Error),
                        "{}: tool {} ended in state {:?}",
                        f.name,
                        t.name,
                        t.state
                    ),
                    _ => {}
                }
            }
        }
    }
}

/// The prompt is reported twice (its stored entry and pi's `message_start`) and shown once.
#[test]
fn the_prompt_is_shown_once() {
    for f in fixtures::all() {
        let vm = reduce(&f.events);
        let users = vm
            .blocks
            .iter()
            .filter(|b| matches!(b, Block::User { .. }))
            .count();
        assert_eq!(users, 1, "{}: user blocks", f.name);
    }
}

/// While bash runs, its output so far is on the screen and the tool is the activity.
#[test]
fn streaming_bash_shows_live_output_while_it_runs() {
    let f = fixture("streaming_bash");
    let last_update = f
        .events
        .iter()
        .rposition(|e| matches!(e, AgentEvent::ToolExecutionUpdate { .. }))
        .expect("the fixture streams partial output");
    let vm = reduce(&f.events[..=last_update]);
    assert!(
        matches!(&vm.status.activity, Activity::Tool(name) if name == "bash"),
        "activity: {:?}",
        vm.status.activity
    );
    for settings in looks() {
        let text = text_of(&vm, &settings);
        assert!(text.contains("make test-retry"), "{text}");
        assert!(text.contains("test keeps_the_idempotency_key ... ok"), "{text}");
        assert!(
            !text.contains("test result"),
            "the result is not there yet:\n{text}"
        );
    }
}

/// The secret request opens while the command runs: the screen behind the sheet shows the
/// exact command as running, and nothing in the run holds an answer to show.
#[test]
fn the_secret_ask_opens_while_its_command_runs() {
    let f = fixture("secret_ask");
    let ask = f.ask.clone().expect("the fixture asks for a secret");
    let vm = reduce(&f.events[..ask.after]);
    let running = vm.blocks.iter().find_map(|b| match b {
        Block::Tool(t) if t.state == ToolState::Running => Some(t),
        _ => None,
    });
    let tool = running.expect("the asking command is running when the request opens");
    assert_eq!(tool.name, "bash");
    assert_eq!(tool.args["command"], ask.command);
    for settings in looks() {
        let text = text_of(&vm, &settings);
        assert!(text.contains(ask.command), "{text}");
    }
    // Only the secret-ask fixture asks.
    for other in fixtures::all() {
        assert_eq!(other.ask.is_some(), other.name == "secret_ask", "{}", other.name);
    }
}
