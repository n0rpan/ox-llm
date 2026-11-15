//! Smoke tests of the pty harness (`oxa_parity::pty`) against the real `oxa` binary and the fake
//! engine. The harness is under test here, not a PARITY row: these prove that the screen model,
//! the query answers, both key encodings, bracketed paste, resize, exit and parallel use work.
//! Build the binaries first: `cargo build -p oxa-cli -p oxa-fake`.
#![cfg(target_os = "linux")]
use oxa_fake::{parse_script, FakeOpts, FakeServer, TestEnv};
use oxa_parity::pty::{encode_kitty, encode_legacy, KeyEncoding, PtyOpts, PtySession, Screen, TermProfile};
use oxa_proto::Role;
use std::time::Duration;

const REPLY: &str = "PONG from the fake engine";
const EXIT: Duration = Duration::from_secs(30);

struct Rig {
    // Field order = drop order: the pty (and the child) go first, then the engine, then the dirs.
    pty: PtySession,
    server: FakeServer,
    _env: TestEnv,
}

/// A TUI session on `profile` against a fake engine that answers every prompt with `REPLY`.
/// `preload: false` switches the O-152 boot preload off (`oxa.bootPreload`); the default
/// (`true`) runs it, as users do.
fn start(profile: TermProfile, size: (u16, u16), preload: bool) -> Rig {
    let env = TestEnv::new();
    if !preload {
        env.write(
            "home/.oxa/agent/settings.json",
            r#"{"oxa":{"bootPreload":false}}"#,
        );
    }
    let line = format!("{{\"text\":\"{REPLY}\"}}\n");
    let server = FakeServer::start(
        parse_script(&line.repeat(4)).expect("script"),
        FakeOpts::default(),
    )
    .expect("start fake engine");
    let pty = PtySession::spawn_with(
        &env,
        &server.url(),
        &[],
        PtyOpts {
            size,
            profile,
            ..PtyOpts::default()
        },
    );
    pty.wait_ready().unwrap();
    pty.wait_for_text("ctrl+d exit").unwrap();
    Rig {
        pty,
        server,
        _env: env,
    }
}

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

fn user_prompts(server: &FakeServer) -> Vec<String> {
    server
        .requests()
        .iter()
        .filter(|r| r.max_tokens > 0)
        .filter_map(|r| r.messages.iter().rev().find(|m| m.role == Role::User))
        .filter_map(|m| m.content.clone())
        .collect()
}

/// Type a prompt, submit it, and wait for the scripted reply on screen.
fn prompt_and_reply(rig: &Rig, text: &str) {
    rig.pty.send_text(text);
    rig.pty
        .wait_for_named(
            &format!("{text:?} in the editor"),
            |s| editor_row(s).contains(text),
            EXIT,
        )
        .unwrap();
    rig.pty.send_key("enter");
    rig.pty.wait_for_text(REPLY).unwrap();
}

// (a) the TUI starts, shows the editor, a prompt gets the scripted reply, Ctrl+D exits 0.
#[test]
fn prompt_reply_and_ctrl_d_exit() {
    for profile in [TermProfile::xterm(), TermProfile::kitty(), TermProfile::tmux()] {
        let name = profile.name;
        let mut rig = start(profile, (24, 80), true);
        let s = rig.pty.screen();
        assert!(s.contains("oxa v"), "{name}: header\n{s:?}");
        assert_eq!(editor_row(&s), "", "{name}: empty editor");
        prompt_and_reply(&rig, "ping");
        assert_eq!(user_prompts(&rig.server), vec!["ping".to_string()], "{name}");
        // The reply is in the transcript, the editor is empty again.
        let s = rig.pty.wait_for(|s| editor_row(s).is_empty(), EXIT).unwrap();
        assert!(
            s.find_row(REPLY).unwrap() < s.find_row("─").unwrap(),
            "{name}: reply above the editor"
        );
        rig.pty.wait_stable(200, EXIT).unwrap();
        rig.pty.send_key("ctrl+d");
        let status = rig.pty.wait_exit(EXIT).unwrap();
        assert!(status.success(), "{name}: exit status {status}");
    }
}

// (a) `/quit` exits 0 as well.
#[test]
fn slash_quit_exits() {
    let mut rig = start(TermProfile::xterm(), (24, 80), true);
    rig.pty.send_text("/quit");
    rig.pty
        .wait_for(|s| editor_row(s).contains("/quit"), EXIT)
        .unwrap();
    // The slash-command menu may be open: settle it before Enter.
    rig.pty.wait_stable(200, EXIT).unwrap();
    rig.pty.send_key("enter");
    let status = rig.pty.wait_exit(EXIT).unwrap();
    assert!(status.success(), "exit status {status}");
    assert!(
        rig.server.requests().iter().all(|r| r.max_tokens == 0),
        "no prompt was sent"
    );
}

// (b) a resize re-renders at the new width and height.
#[test]
fn resize_rerenders() {
    let rig = start(TermProfile::xterm(), (24, 80), true);
    let rule80 = "─".repeat(80);
    assert!(rig.pty.screen().rows().contains(&rule80));
    rig.pty.resize(30, 60);
    let s = rig
        .pty
        .wait_for_named(
            "a 60-column frame on 30 rows",
            |s| {
                let rows = s.rows();
                rows.iter().any(|r| *r == "─".repeat(60))
                    && rows.iter().all(|r| r.chars().count() <= 60)
                    && rows[29].contains("fake-model")
            },
            EXIT,
        )
        .unwrap();
    assert_eq!(s.size(), (30, 60));
    // Still interactive at the new size.
    prompt_and_reply(&rig, "after resize");
}

// (c) keys in kitty CSI-u encoding and in legacy encoding both arrive.
#[test]
fn kitty_and_legacy_keys_arrive() {
    // The encoders themselves.
    assert_eq!(encode_legacy("ctrl+c").unwrap(), b"\x03");
    assert_eq!(encode_legacy("alt+up").unwrap(), b"\x1b[1;3A");
    assert_eq!(encode_legacy("shift+tab").unwrap(), b"\x1b[Z");
    assert_eq!(encode_legacy("f1").unwrap(), b"\x1bOP");
    assert_eq!(encode_legacy("f5").unwrap(), b"\x1b[15~");
    assert!(encode_legacy("shift+enter").is_none());
    assert_eq!(encode_kitty("shift+enter", 15, false).unwrap(), b"\x1b[13;2u");
    assert_eq!(encode_kitty("a", 15, true).unwrap(), b"\x1b[97u\x1b[97;1:3u");
    assert_eq!(encode_kitty("shift+a", 15, false).unwrap(), b"\x1b[97:65;2u");
    assert_eq!(encode_kitty("ctrl+c", 1, false).unwrap(), b"\x1b[99;5u");
    assert_eq!(encode_kitty("a", 1, false).unwrap(), b"a");

    assert_eq!(
        encode_kitty("x", 7, true).unwrap(),
        b"x",
        "flags 7: text keys stay text"
    );
    assert_eq!(encode_kitty("escape", 7, false).unwrap(), b"\x1b[27u");

    // kitty profile: the app pushed pi's flags 7 and the terminal confirmed them.
    let rig = start(TermProfile::kitty(), (24, 80), true);
    assert_eq!(rig.pty.modes().kitty_flags(), 7);
    assert_eq!(rig.pty.key_bytes("x", KeyEncoding::Auto).unwrap(), b"x");
    assert_eq!(
        rig.pty.key_bytes("shift+enter", KeyEncoding::Auto).unwrap(),
        b"\x1b[13;2u\x1b[13;2:3u"
    );
    for (id, enc) in [
        ("a", KeyEncoding::Auto),
        ("c", KeyEncoding::Legacy),
        ("left", KeyEncoding::Auto),
        ("shift+b", KeyEncoding::Auto),
        ("right", KeyEncoding::Legacy),
        ("shift+d", KeyEncoding::Legacy),
    ] {
        rig.pty.send_key_as(id, enc);
    }
    rig.pty
        .wait_for_named("\"aBcD\" in the editor", |s| editor_row(s) == "aBcD", EXIT)
        .unwrap();
    rig.pty.send_key_as("backspace", KeyEncoding::Auto);
    rig.pty.send_key_as("backspace", KeyEncoding::Legacy);
    rig.pty
        .wait_for_named("\"aB\" in the editor", |s| editor_row(s) == "aB", EXIT)
        .unwrap();
    drop(rig);

    // xterm profile: no kitty protocol confirmed, Auto is legacy.
    let rig = start(TermProfile::xterm(), (24, 80), true);
    assert_eq!(rig.pty.modes().kitty_flags(), 0);
    assert_eq!(rig.pty.key_bytes("x", KeyEncoding::Auto).unwrap(), b"x");
    rig.pty.send_key("q");
    rig.pty.send_key("shift+w");
    rig.pty.send_key("left");
    rig.pty.send_key("e");
    rig.pty
        .wait_for_named("\"qeW\" in the editor", |s| editor_row(s) == "qeW", EXIT)
        .unwrap();
}

// (d) a bracketed paste arrives as one paste: its newline does not submit.
#[test]
fn bracketed_paste_is_one_paste() {
    let rig = start(TermProfile::xterm(), (24, 80), true);
    assert!(
        rig.pty.modes().bracketed_paste,
        "the app switched bracketed paste on"
    );
    let mark = rig.pty.mark();
    rig.pty.paste("first pasted line\nsecond pasted line");
    rig.pty
        .wait_for_named(
            "both pasted lines in the editor",
            |s| s.contains("first pasted line") && s.contains("second pasted line"),
            EXIT,
        )
        .unwrap();
    rig.pty.wait_stable(300, EXIT).unwrap();
    assert!(
        user_prompts(&rig.server).is_empty(),
        "the pasted newline did not submit"
    );
    assert!(
        !rig.pty.raw_since(mark).is_empty(),
        "the paste re-rendered the editor"
    );
    rig.pty.send_key("enter");
    rig.pty.wait_for_text(REPLY).unwrap();
    assert_eq!(
        user_prompts(&rig.server),
        vec!["first pasted line\nsecond pasted line".to_string()],
        "one prompt with both lines"
    );
}

// (e) the terminal state is restored at exit.
#[test]
fn terminal_restored_at_exit() {
    for profile in [TermProfile::xterm(), TermProfile::kitty()] {
        let name = profile.name;
        let mut rig = start(profile, (24, 80), true);
        let on = rig.pty.modes();
        assert!(on.bracketed_paste, "{name}: {on:?}");
        rig.pty.send_key("ctrl+d");
        let status = rig.pty.wait_exit(EXIT).unwrap();
        assert!(status.success(), "{name}: {status}");
        let s = rig.pty.screen();
        assert!(s.exited());
        let m = s.modes();
        assert!(!m.alt_screen, "{name}: alt screen left");
        assert!(!m.bracketed_paste, "{name}: bracketed paste off");
        assert!(!m.mouse, "{name}: mouse reporting off");
        assert!(!m.focus_reporting, "{name}: focus reporting off");
        assert!(!m.synchronized_output, "{name}: no open synchronized update");
        assert!(
            m.kitty_stack.is_empty(),
            "{name}: kitty flags popped: {:?}",
            m.kitty_stack
        );
        // Every switch-on in the raw log is followed by its switch-off.
        let raw = rig.pty.raw();
        let last = |needle: &[u8]| raw.windows(needle.len()).rposition(|w| w == needle);
        for (enter, leave) in [
            (&b"\x1b[?2004h"[..], &b"\x1b[?2004l"[..]),
            (b"\x1b[?1049h", b"\x1b[?1049l"),
            (b"\x1b[?1000h", b"\x1b[?1000l"),
        ] {
            if let Some(e) = last(enter) {
                let l = last(leave).unwrap_or_else(|| panic!("{name}: no leave sequence"));
                assert!(l > e, "{name}: leave after enter");
            }
        }
    }
}

// (f) eight sessions in parallel, each with its own pty, engine and directories.
#[test]
fn eight_parallel_sessions() {
    let handles: Vec<_> = (0..8)
        .map(|i| {
            std::thread::spawn(move || {
                let profile = if i % 2 == 0 {
                    TermProfile::xterm()
                } else {
                    TermProfile::kitty()
                };
                let mut rig = start(profile, (24, 80), true);
                let text = format!("parallel prompt {i}");
                prompt_and_reply(&rig, &text);
                assert_eq!(user_prompts(&rig.server), vec![text]);
                rig.pty.wait_for(|s| editor_row(s).is_empty(), EXIT).unwrap();
                rig.pty.wait_stable(200, EXIT).unwrap();
                rig.pty.send_key("ctrl+d");
                assert!(rig.pty.wait_exit(EXIT).unwrap().success());
            })
        })
        .collect();
    for h in handles {
        h.join().expect("parallel session");
    }
}

// Dropping a session that is still running kills it (and nothing else) and returns promptly.
#[test]
fn drop_kills_a_running_session() {
    let rig = start(TermProfile::xterm(), (24, 80), true);
    let pid = rig.pty.pid() as libc::pid_t;
    let t = std::time::Instant::now();
    drop(rig);
    assert!(t.elapsed() < Duration::from_secs(10));
    // SAFETY: probing a pid we started (reaped by the drop, so it no longer exists).
    let alive = unsafe { libc::kill(pid, 0) } == 0;
    assert!(!alive, "the child is gone");
}

/// Wave H fix (oxa-tui `ProcessTerminal`): exit restores the entry state, the cursor is
/// visible in the user's shell again (it was left hidden by `ESC[?25l`).
#[test]
fn cursor_visible_after_exit() {
    let mut rig = start(TermProfile::xterm(), (24, 80), true);
    rig.pty.send_key("ctrl+d");
    assert!(rig.pty.wait_exit(EXIT).unwrap().success());
    let s = rig.pty.screen();
    assert!(s.cursor_visible(), "cursor visible after exit\n{s:?}");
}

/// Wave H fix (oxa-interactive, O-152 boot preload): the background prefix preload is
/// invisible to input: the first prompt is sent as a prompt (not queued as steering), at once.
#[test]
fn boot_preload_leaves_the_tui_busy() {
    let rig = start(TermProfile::xterm(), (24, 80), true);
    // The preload request reached the engine (a `max_tokens: 0` prefill of the empty wire).
    let t = std::time::Instant::now();
    while !rig.server.requests().iter().any(|r| r.max_tokens == 0) {
        assert!(t.elapsed() < EXIT, "the boot preload was sent");
        std::thread::sleep(Duration::from_millis(20));
    }
    rig.pty.wait_stable(200, EXIT).unwrap();
    prompt_and_reply(&rig, "first prompt");
    assert_eq!(user_prompts(&rig.server), vec!["first prompt".to_string()]);
    let s = rig.pty.screen();
    assert!(!s.contains("Steering"), "no steering\n{s:?}");
    assert!(!s.contains("Prefill"), "the preload is not shown\n{s:?}");
}

/// Wave H fix (O-152): a prompt typed WHILE the boot preload is in flight (the engine holds its
/// headers) is sent as a prompt, not queued as steering; the preload is cancelled by it.
#[test]
fn prompt_during_boot_preload_is_a_prompt() {
    let env = TestEnv::new();
    let line = format!("{{\"text\":\"{REPLY}\"}}\n");
    let server = FakeServer::start(
        parse_script(&line.repeat(2)).expect("script"),
        FakeOpts {
            hold_headers_ms: 1500,
            ..FakeOpts::default()
        },
    )
    .expect("start fake engine");
    let pty = PtySession::spawn(&env, &server.url(), &[], (24, 80));
    pty.wait_ready().unwrap();
    pty.wait_for_text("ctrl+d exit").unwrap();
    // The preload request is in flight (recorded when it arrives).
    let t = std::time::Instant::now();
    while !server.requests().iter().any(|r| r.max_tokens == 0) {
        assert!(t.elapsed() < EXIT, "the boot preload was sent");
        std::thread::sleep(Duration::from_millis(10));
    }
    let rig = Rig {
        pty,
        server,
        _env: env,
    };
    prompt_and_reply(&rig, "early prompt");
    assert_eq!(user_prompts(&rig.server), vec!["early prompt".to_string()]);
    let s = rig.pty.screen();
    assert!(!s.contains("Steering"), "no steering\n{s:?}");
}

/// Harness: a snapshot never shows a frame that is still being drawn inside a synchronized
/// update (`CSI ? 2026 h` .. `CSI ? 2026 l`); it shows the last complete frame until the update
/// ends, then the new one.
#[test]
fn synchronized_updates_are_atomic() {
    use oxa_parity::pty::replay_output;
    let chunks: [&[u8]; 5] = [
        b"\x1b[?2026h\x1b[1;1HOLD FRAME\x1b[?2026l",
        b"\x1b[?2026h\x1b[1;1H\x1b[2K",
        b"NEW",
        b" FRAME\x1b[?2026l\x1b[?2026h\x1b[2;1Hnext",
        b" line\x1b[?2026l",
    ];
    let mut seen: Vec<(String, bool)> = Vec::new();
    replay_output(5, 40, &chunks, |s| {
        seen.push((s.row(0), s.modes().synchronized_output))
    });
    assert_eq!(seen[0], ("OLD FRAME".to_string(), false));
    // Inside the update: the old frame, not the cleared line or the half-written one.
    assert_eq!(seen[1], ("OLD FRAME".to_string(), true));
    assert_eq!(seen[2], ("OLD FRAME".to_string(), true));
    // The update ended (and the next one opened in the same chunk): the completed frame.
    assert_eq!(seen[3], ("NEW FRAME".to_string(), true));
    assert_eq!(seen[4], ("NEW FRAME".to_string(), false));
}

/// Harness: `wait_steady` returns only after the application has read the input sent before
/// it and the key's effect is on screen (content-based, not quiet-time-based).
#[test]
fn wait_stable_waits_for_the_input_to_be_read() {
    let rig = start(TermProfile::xterm(), (24, 80), true);
    for c in ["a", "b", "c", "d", "e"] {
        rig.pty.send_text(c);
        // Content-based: the key's effect (the character in the editor row) is the done-signal;
        // quiet time only confirms it stays that way. Under load the redraw can land well after
        // the key was read, so a quiet period alone is not a frame-complete signal.
        let s = rig
            .pty
            .wait_steady(
                &format!("{c:?} visible in the editor row"),
                |s| editor_row(s).ends_with(c),
                150,
                EXIT,
            )
            .unwrap();
        assert!(
            editor_row(&s).ends_with(c),
            "{c:?} processed before wait_steady returned\n{s:?}"
        );
    }
    assert_eq!(rig.pty.pending_input(), 0);
}
