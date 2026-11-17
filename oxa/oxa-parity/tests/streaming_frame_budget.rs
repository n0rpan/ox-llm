//! #875: the real CLI streams a long fenced reply through the TUI and everything arrives.
//!
//! The loop draws streaming text at most once per frame budget (`CONTENT_FRAME_MS`, about 30 Hz)
//! instead of once per delta. This test is the end-to-end behaviour that must survive that cap:
//! the fake engine streams a 300-line fenced reply in many small deltas, and the reply reaches
//! the screen complete while the editor keeps taking input. It is not a PARITY row, so it is not
//! named after one; the loop-level batching itself is checked deterministically in
//! `oxa-interactive` (`streaming_text_is_batched_while_keys_and_the_final_reply_stay_immediate`
//! and the `content_frames_tests`).
//!
//! Build the binaries first: `cargo build -p oxa-cli -p oxa-fake`.
#![cfg(target_os = "linux")]
use oxa_fake::{parse_script, FakeOpts, FakeServer, TestEnv};
use oxa_parity::pty::{PtyOpts, PtySession, Screen, TermProfile, DEFAULT_TIMEOUT};
use std::time::Duration;

const EXIT: Duration = DEFAULT_TIMEOUT;
const LAST: &str = "END-OF-REPLY";

/// The editor's text row: the row between the two horizontal rules at the bottom.
fn editor_row(s: &Screen) -> String {
    let rules: Vec<usize> = s
        .rows()
        .iter()
        .enumerate()
        .filter(|(_, r)| !r.is_empty() && r.chars().all(|c| c == '─'))
        .map(|(i, _)| i)
        .collect();
    assert!(rules.len() >= 2, "editor rules not found:\n{s:?}");
    s.row(rules[rules.len() - 2] as u16 + 1)
}

/// The scripted reply: a fenced block of 300 lines, sent in 8-byte deltas (chunk 8), so the TUI
/// sees a fast stream rather than one delta per line.
fn script() -> String {
    let lines: Vec<String> = (1..=300).map(|i| format!("line-{i:03}")).collect();
    let text = format!("```\\n{}\\n{LAST}\\n```\\n", lines.join("\\n"));
    format!("{{\"text\":\"{text}\",\"chunk\":8}}\n")
}

/// A 300-line fenced reply streamed in small deltas arrives complete: its last line is on the
/// screen, and the editor still takes a key afterwards.
#[test]
fn a_long_streamed_reply_arrives_complete_and_the_editor_still_takes_keys() {
    let env = TestEnv::new();
    let turn = parse_script(&script()).expect("script");
    let server = FakeServer::start(turn, FakeOpts::default()).expect("start fake engine");
    let mut pty = PtySession::spawn_with(
        &env,
        &server.url(),
        &[],
        PtyOpts {
            size: (40, 100),
            profile: TermProfile::xterm(),
            ..PtyOpts::default()
        },
    );
    pty.wait_ready().unwrap();
    pty.wait_for_text("ctrl+d exit").unwrap();
    pty.send_text("long");
    let named = |s: &Screen| editor_row(s).contains("long");
    pty.wait_for_named("the prompt in the editor", named, EXIT)
        .unwrap();
    pty.send_key("enter");
    // The end of the reply: every delta before it was streamed, so the whole block is there.
    pty.wait_for_text(LAST).unwrap();
    // Input after the stream: the frame budget must not sit on the editor.
    pty.send_text("x");
    let typed = |s: &Screen| editor_row(s).contains('x');
    pty.wait_for_named("the typed key in the editor", typed, EXIT)
        .unwrap();
    pty.send_key("ctrl+d");
    let status = pty.wait_exit(EXIT).unwrap();
    assert!(status.success(), "exit status {status}");
}
