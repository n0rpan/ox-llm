//! A12 acceptance tests: Terminal raw mode and key parser.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_tui::{
    encode_legacy, flush_pending, install_panic_hook, parse_input, restore_sequence, InputEvent, Key, KeyId,
    KeyKind, MouseButton, MouseKind, TermMode, Terminal, TestTerminal,
};

/// Parse `bytes` and insist that they are exactly one key event.
fn only_key(bytes: &[u8]) -> Key {
    let (events, used) = parse_input(bytes);
    assert_eq!(used, bytes.len(), "not every byte was consumed: {bytes:?}");
    assert_eq!(
        events.len(),
        1,
        "expected one event for {bytes:?}, got {events:?}"
    );
    match events.into_iter().next() {
        Some(InputEvent::Key(k)) => k,
        other => panic!("expected a key for {bytes:?}, got {other:?}"),
    }
}

fn id_of(bytes: &[u8]) -> String {
    only_key(bytes).id.to_string()
}

/// arrows, home/end, F-keys, ctrl and alt combos
#[test]
fn legacy_sequences_table() {
    let table: &[(&[u8], &str)] = &[
        (b"\x1b[A", "up"),
        (b"\x1bOA", "up"),
        (b"\x1b[B", "down"),
        (b"\x1b[C", "right"),
        (b"\x1b[D", "left"),
        (b"\x1b[1;5C", "ctrl+right"),
        (b"\x1b[1;2A", "shift+up"),
        (b"\x1b[1;3D", "alt+left"),
        (b"\x1b[H", "home"),
        (b"\x1bOH", "home"),
        (b"\x1b[1~", "home"),
        (b"\x1b[7~", "home"),
        (b"\x1b[F", "end"),
        (b"\x1b[4~", "end"),
        (b"\x1b[2~", "insert"),
        (b"\x1b[3~", "delete"),
        (b"\x1b[5~", "pageup"),
        (b"\x1b[6~", "pagedown"),
        (b"\x1bOP", "f1"),
        (b"\x1bOS", "f4"),
        (b"\x1b[11~", "f1"),
        (b"\x1b[15~", "f5"),
        (b"\x1b[17~", "f6"),
        (b"\x1b[21~", "f10"),
        (b"\x1b[23~", "f11"),
        (b"\x1b[24~", "f12"),
        (b"\x1b[1;2P", "shift+f1"),
        (b"\x1b[15;5~", "ctrl+f5"),
        (b"\x1b[Z", "shift+tab"),
        (b"\t", "tab"),
        (b"\r", "enter"),
        // LF: ctrl+j (pi's editor inserts a new line for it, editor.ts `data === "\n"`).
        (b"\n", "ctrl+j"),
        (b"\x1f", "ctrl+-"),
        (b"\x7f", "backspace"),
        (b"\x08", "backspace"),
        (b"\x03", "ctrl+c"),
        (b"\x01", "ctrl+a"),
        (b"\x1a", "ctrl+z"),
        (b"\x00", "ctrl+space"),
        (b"\x1c", "ctrl+\\"),
        (b"\x1b\x7f", "alt+backspace"),
        (b"\x1bx", "alt+x"),
        (b"\x1b\r", "alt+enter"),
        (b"\x1b\x03", "ctrl+alt+c"),
        (b"\x1b[27;5;99~", "ctrl+c"),
        (b" ", "space"),
        (b"+", "+"),
    ];
    for (bytes, id) in table {
        let k = only_key(bytes);
        assert_eq!(k.id.to_string(), *id, "bytes {bytes:?}");
        assert_eq!(k.kind, KeyKind::Press);
    }
    // Printable text: the id is the lowercase base key and the text is what is inserted. A
    // capital letter is `shift+<letter>` (D-INT), so `shift+l` bindings match what was typed.
    let a = only_key(b"a");
    assert_eq!((a.id.to_string().as_str(), a.text.as_deref()), ("a", Some("a")));
    let big = only_key(b"A");
    assert_eq!(
        (big.id.to_string().as_str(), big.text.as_deref()),
        ("shift+a", Some("A"))
    );
    let e = only_key("é".as_bytes());
    assert_eq!(e.text.as_deref(), Some("é"));
    let emoji = only_key("😀".as_bytes());
    assert_eq!(emoji.text.as_deref(), Some("😀"));
    // Chords carry no text.
    assert_eq!(only_key(b"\x03").text, None);
    assert_eq!(only_key(b"\x1bx").text, None);
}

/// CSI u with modifiers, press/repeat/release
#[test]
fn kitty_protocol_table() {
    // Plain press: no text field, so the parser supplies the text.
    let k = only_key(b"\x1b[97u");
    assert_eq!(
        (k.id.to_string().as_str(), k.text.as_deref(), k.kind),
        ("a", Some("a"), KeyKind::Press)
    );
    // ctrl+a: a chord, no text.
    let k = only_key(b"\x1b[97;5u");
    assert_eq!(
        (k.id.to_string().as_str(), k.text.clone(), k.kind),
        ("ctrl+a", None, KeyKind::Press)
    );
    // shift without a text field or a shifted alternate: the shift stays in the id.
    assert_eq!(id_of(b"\x1b[97;2u"), "shift+a");
    // shift with text: the text is the capital and a capital letter keeps `shift+` (D-INT).
    let k = only_key(b"\x1b[97;2;65u");
    assert_eq!(
        (k.id.to_string().as_str(), k.text.as_deref()),
        ("shift+a", Some("A"))
    );
    // other shifted characters keep the plain base id: the text carries the shift.
    let k = only_key(b"\x1b[49;2;33u");
    assert_eq!((k.id.to_string().as_str(), k.text.as_deref()), ("1", Some("!")));
    // shifted alternate (code:shifted) with shift and no text field.
    let k = only_key(b"\x1b[97:65;2u");
    assert_eq!(
        (k.id.to_string().as_str(), k.text.as_deref()),
        ("shift+a", Some("A"))
    );
    // Base layout key: a Cyrillic es (1089 = 0x441) reports base 'c' (99) for shortcuts.
    assert_eq!(id_of("\x1b[1089::99;5u".as_bytes()), "ctrl+c");
    // Event types: release and repeat.
    let k = only_key(b"\x1b[97;1:3u");
    assert_eq!(
        (k.id.to_string().as_str(), k.text.clone(), k.kind),
        ("a", None, KeyKind::Release)
    );
    let k = only_key(b"\x1b[97;1:2u");
    assert_eq!(k.kind, KeyKind::Repeat);
    let k = only_key(b"\x1b[1;5:3A");
    assert_eq!((k.id.to_string().as_str(), k.kind), ("ctrl+up", KeyKind::Release));
    let k = only_key(b"\x1b[1;1:2B");
    assert_eq!((k.id.to_string().as_str(), k.kind), ("down", KeyKind::Repeat));
    // Functional keys in CSI u.
    assert_eq!(id_of(b"\x1b[13u"), "enter");
    assert_eq!(id_of(b"\x1b[13;2u"), "shift+enter");
    assert_eq!(id_of(b"\x1b[9;2u"), "shift+tab");
    assert_eq!(id_of(b"\x1b[27u"), "escape");
    assert_eq!(id_of(b"\x1b[127u"), "backspace");
    assert_eq!(id_of(b"\x1b[57419;5u"), "ctrl+up");
    assert_eq!(id_of(b"\x1b[57417u"), "left");
    assert_eq!(id_of(b"\x1b[57364u"), "f1");
    // Keypad digits insert their text.
    let k = only_key(b"\x1b[57399u");
    assert_eq!((k.id.to_string().as_str(), k.text.as_deref()), ("0", Some("0")));
    // Kitty flags reply and modifyOtherKeys.
    let (events, _) = parse_input(b"\x1b[?31u");
    assert_eq!(events, vec![InputEvent::KittyFlags(31)]);
    assert_eq!(id_of(b"\x1b[27;5;97~"), "ctrl+a");
}

/// lone ESC vs alt+key
#[test]
fn esc_timeout_disambiguates() {
    // A lone ESC is not decided by the parser: it waits for the caller's timeout.
    let (events, used) = parse_input(b"\x1b");
    assert!(events.is_empty());
    assert_eq!(used, 0);
    // The timeout fired: the lone ESC is the Escape key.
    assert_eq!(
        flush_pending(b"\x1b"),
        vec![InputEvent::Key(Key::press("escape"))]
    );
    // ESC followed by a key in the same read is alt+key, decided at once.
    let (events, used) = parse_input(b"\x1bx");
    assert_eq!(used, 2);
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], InputEvent::Key(k) if k.is("alt+x")));
    // ESC followed by a bracket that never completes: alt+[ and the rest as literal keys.
    let flushed = flush_pending(b"\x1b[");
    assert_eq!(flushed.len(), 1);
    assert!(matches!(&flushed[0], InputEvent::Key(k) if k.is("alt+[")));
}

/// paste markers
#[test]
fn bracketed_paste_is_one_event() {
    let bytes = b"\x1b[200~hello\r\nworld\x1b[201~";
    let (events, used) = parse_input(bytes);
    assert_eq!(used, bytes.len());
    assert_eq!(events, vec![InputEvent::Paste("hello\nworld".to_string())]);

    // Key-like bytes inside a paste stay text.
    let bytes = b"\x1b[200~a\x1b[Ab\x03\x1b[201~";
    let (events, used) = parse_input(bytes);
    assert_eq!(used, bytes.len());
    assert_eq!(events, vec![InputEvent::Paste("a\x1b[Ab\x03".to_string())]);

    // A bare CR inside the paste becomes a newline.
    assert_eq!(
        parse_input(b"\x1b[200~x\ry\x1b[201~").0,
        vec![InputEvent::Paste("x\ny".to_string())]
    );

    // Unterminated: not consumed; flushing delivers what arrived.
    let (events, used) = parse_input(b"\x1b[200~abc");
    assert!(events.is_empty());
    assert_eq!(used, 0);
    assert_eq!(
        flush_pending(b"\x1b[200~abc"),
        vec![InputEvent::Paste("abc".to_string())]
    );
}

/// SGR mouse
#[test]
fn mouse_and_resize_and_focus_events() {
    let (events, _) = parse_input(b"\x1b[<0;10;5M");
    let InputEvent::Mouse(m) = &events[0] else {
        panic!("mouse expected: {events:?}")
    };
    assert_eq!(
        (m.kind, m.button, m.col, m.row),
        (MouseKind::Press, MouseButton::Left, 9, 4)
    );
    assert!(!m.ctrl && !m.shift && !m.alt);

    let (events, _) = parse_input(b"\x1b[<0;10;5m");
    assert!(matches!(&events[0], InputEvent::Mouse(m) if m.kind == MouseKind::Release));

    let (events, _) = parse_input(b"\x1b[<64;1;1M");
    assert!(matches!(&events[0], InputEvent::Mouse(m) if m.kind == MouseKind::WheelUp && m.col == 0));
    let (events, _) = parse_input(b"\x1b[<65;1;1M");
    assert!(matches!(&events[0], InputEvent::Mouse(m) if m.kind == MouseKind::WheelDown));

    let (events, _) = parse_input(b"\x1b[<32;3;3M");
    assert!(
        matches!(&events[0], InputEvent::Mouse(m) if m.kind == MouseKind::Drag && m.button == MouseButton::Left)
    );
    let (events, _) = parse_input(b"\x1b[<35;3;3M");
    assert!(
        matches!(&events[0], InputEvent::Mouse(m) if m.kind == MouseKind::Move && m.button == MouseButton::None)
    );

    // ctrl is bit 16 of the button code.
    let (events, _) = parse_input(b"\x1b[<16;1;1M");
    assert!(matches!(&events[0], InputEvent::Mouse(m) if m.ctrl && m.kind == MouseKind::Press));

    assert_eq!(parse_input(b"\x1b[I").0, vec![InputEvent::FocusGained]);
    assert_eq!(parse_input(b"\x1b[O").0, vec![InputEvent::FocusLost]);

    // Cell size: CSI 6 ; h ; w t.
    assert_eq!(
        parse_input(b"\x1b[6;20;10t").0,
        vec![InputEvent::CellSize { w: 10, h: 20 }]
    );

    // Resize comes from the terminal, not the parser: the test terminal shows the contract.
    let mut term = TestTerminal::new(80, 24);
    term.resize(100, 40);
    assert_eq!(term.poll(0), vec![InputEvent::Resize { cols: 100, rows: 40 }]);

    // Device attributes reply.
    assert_eq!(
        parse_input(b"\x1b[?62;4c").0,
        vec![InputEvent::DeviceAttributes("62;4".to_string())]
    );
}

/// consumed count leaves the tail
#[test]
fn partial_sequence_is_not_consumed() {
    // Partial CSI: nothing consumed.
    assert_eq!(parse_input(b"\x1b[1;5"), (Vec::new(), 0));
    // A complete key before a partial one: the key is consumed, the tail is left.
    let (events, used) = parse_input(b"a\x1b[");
    assert_eq!(used, 1);
    assert_eq!(events.len(), 1);
    // Lone ESC, partial UTF-8, unterminated OSC, partial SS3, partial paste start.
    assert_eq!(parse_input(b"\x1b"), (Vec::new(), 0));
    assert_eq!(parse_input(b"\xc3"), (Vec::new(), 0));
    assert_eq!(parse_input(b"\x1b]11;rgb:1/2/3"), (Vec::new(), 0));
    assert_eq!(parse_input(b"\x1bO"), (Vec::new(), 0));
    assert_eq!(parse_input(b"\x1b[200~ab"), (Vec::new(), 0));
    assert_eq!(parse_input(b"\x1b[20"), (Vec::new(), 0));

    // Splitting at every offset, re-offering the unconsumed tail, gives the same events.
    let parts: [&[u8]; 10] = [
        b"ab",
        b"\x1b[1;5C",
        b"\x1bx",
        "é😀".as_bytes(),
        b"\x1b[200~p\r\nq\x1b[201~",
        b"\x1b]11;rgb:1e1e/1e1e/1e1e\x1b\\",
        b"\x1b[<0;3;4M",
        b"\x1b[97;5u",
        b"\x1b[Z",
        b"\x1bOP",
    ];
    let stream: Vec<u8> = parts.concat();
    let (whole, whole_used) = parse_input(&stream);
    assert_eq!(whole_used, stream.len());
    for cut in 0..=stream.len() {
        let (head, tail) = stream.split_at(cut);
        let (mut events, used) = parse_input(head);
        let mut rest = head[used..].to_vec();
        rest.extend_from_slice(tail);
        let (more, used2) = parse_input(&rest);
        events.extend(more);
        assert_eq!(used2, rest.len(), "cut {cut}");
        assert_eq!(events, whole, "cut {cut}");
    }
}

/// the hook writes the restore sequence
#[test]
fn panic_restores_terminal() {
    if std::env::var_os("OXA_A12_PANIC_CHILD").is_some() {
        // Child process: install the hook and panic. The hook must write the restore sequence
        // before the default message appears.
        install_panic_hook();
        panic!("a12 deliberate panic");
    }
    let exe = std::env::current_exe().expect("test binary path");
    let out = std::process::Command::new(exe)
        .args(["--exact", "panic_restores_terminal", "--test-threads=1"])
        .env("OXA_A12_PANIC_CHILD", "1")
        .output()
        .expect("run the child test");
    assert!(!out.status.success(), "the child was meant to panic");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(restore_sequence()),
        "restore sequence missing from the child's stdout: {stdout:?}"
    );
    // The restore sequence switches the modes off and leaves the cursor visible.
    let seq = restore_sequence();
    assert!(seq.contains("\x1b[?1049l") && seq.contains("\x1b[?25h") && seq.contains("\x1b[<u"));
}

/// OSC 10/11 colour replies
#[test]
fn osc_reply_parsed() {
    let bytes = b"\x1b]11;rgb:1e1e/1e1e/1e1e\x07";
    let (events, used) = parse_input(bytes);
    assert_eq!(used, bytes.len());
    assert_eq!(
        events,
        vec![InputEvent::OscReply {
            code: 11,
            data: "rgb:1e1e/1e1e/1e1e".to_string()
        }]
    );
    let bytes = b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\";
    let (events, used) = parse_input(bytes);
    assert_eq!(used, bytes.len());
    assert_eq!(
        events,
        vec![InputEvent::OscReply {
            code: 10,
            data: "rgb:ffff/ffff/ffff".to_string()
        }]
    );
}

// ---- additional coverage -----------------------------------------------------------------

#[test]
fn encode_legacy_round_trips_through_the_parser() {
    assert_eq!(encode_legacy(&KeyId::parse("ctrl+c").unwrap()), Some(vec![0x03]));
    assert_eq!(
        encode_legacy(&KeyId::parse("up").unwrap()),
        Some(b"\x1b[A".to_vec())
    );
    assert_eq!(
        encode_legacy(&KeyId::parse("ctrl+up").unwrap()),
        Some(b"\x1b[1;5A".to_vec())
    );
    assert_eq!(
        encode_legacy(&KeyId::parse("f5").unwrap()),
        Some(b"\x1b[15~".to_vec())
    );
    assert_eq!(
        encode_legacy(&KeyId::parse("alt+x").unwrap()),
        Some(b"\x1bx".to_vec())
    );
    assert_eq!(encode_legacy(&KeyId::parse("super+a").unwrap()), None);
    for id in [
        "up",
        "shift+up",
        "ctrl+right",
        "alt+left",
        "home",
        "end",
        "insert",
        "delete",
        "pageup",
        "pagedown",
        "f1",
        "f2",
        "f4",
        "f5",
        "f12",
        "ctrl+f5",
        "shift+tab",
        "enter",
        "tab",
        "backspace",
        "escape",
        "space",
        "ctrl+space",
        "ctrl+c",
        "ctrl+z",
        "ctrl+\\",
        "alt+x",
        "alt+enter",
        "alt+backspace",
        "ctrl+alt+x",
        "+",
    ] {
        let parsed = KeyId::parse(id).unwrap();
        let bytes = encode_legacy(&parsed).unwrap_or_else(|| panic!("no legacy bytes for {id}"));
        // The flush path, because a lone ESC (escape key) is only decided by the timeout.
        let events = flush_pending(&bytes);
        assert_eq!(events.len(), 1, "round trip of {id} via {bytes:?}: {events:?}");
        match &events[0] {
            InputEvent::Key(k) => assert_eq!(k.id, parsed, "round trip of {id} via {bytes:?}"),
            other => panic!("round trip of {id}: {other:?}"),
        }
    }
}

#[test]
fn parser_never_panics_and_never_overconsumes() {
    // Deterministic pseudo-random bytes (LCG), including the escape introducers.
    let mut seed: u64 = 0x5eed_1234_abcd_0001;
    let alphabet = [
        0x1b, b'[', b'O', b']', b'<', b'?', b';', b'0', b'9', b'u', b'~', b'M', b'\\', 0x07, 0xc3, 0xa9,
        0xf0, b'a', b'\r', 0x7f,
    ];
    for _ in 0..3000 {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let len = (seed >> 33) as usize % 48;
        let mut bytes = Vec::with_capacity(len);
        for _ in 0..len {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let r = (seed >> 33) as usize;
            bytes.push(if r.is_multiple_of(3) {
                (r % 256) as u8
            } else {
                alphabet[r % alphabet.len()]
            });
        }
        let (_, used) = parse_input(&bytes);
        assert!(used <= bytes.len());
        let _ = flush_pending(&bytes);
    }
}

#[test]
fn terminal_modes_are_recorded_by_the_test_terminal() {
    // The mode sequences are plain text, and the restore sequence switches the same modes off.
    // pi terminal.ts pushes flags 7 (disambiguate, event types, alternate keys).
    assert_eq!(oxa_tui::mode_sequence(TermMode::KittyKeyboard, true), "\x1b[>7u");
    assert_eq!(oxa_tui::mode_sequence(TermMode::KittyKeyboard, false), "\x1b[<u");
    let mut term = TestTerminal::new(10, 5);
    term.set_mode(TermMode::AltScreen, true);
    term.set_mode(TermMode::AltScreen, false);
    assert!(!term.mode_on(TermMode::AltScreen));
}

/// Wave H (pty harness finding): leaving the program (exit, suspend) restores the entry state:
/// every active mode off in reverse order, and a hidden cursor shown again (never hidden).
#[test]
fn leave_sequence_shows_a_hidden_cursor_and_never_hides_it() {
    use oxa_tui::{leave_sequence, TermMode};
    let active = [
        TermMode::BracketedPaste,
        TermMode::AltScreen,
        TermMode::CursorVisible,
    ];
    let hidden = String::from_utf8(leave_sequence(&active, true)).unwrap();
    assert_eq!(hidden, "\x1b[?1049l\x1b[?2004l\x1b[?25h");
    let shown = String::from_utf8(leave_sequence(&active, false)).unwrap();
    assert!(!shown.contains("\x1b[?25l"), "{shown:?}");
    assert!(!shown.contains("\x1b[?25h"), "{shown:?}");
    assert!(leave_sequence(&[], false).is_empty());
}

/// Several SGR mouse reports in one read (a fast drag) are each an event.
#[test]
fn back_to_back_mouse_reports() {
    let buf: &[u8] = b"\x1b[<0;2;9M\x1b[<32;10;9M\x1b[<0;19;9m";
    let (events, used) = parse_input(buf);
    assert_eq!(used, buf.len());
    let kinds: Vec<_> = events
        .iter()
        .map(|e| match e {
            InputEvent::Mouse(m) => (m.kind, m.col, m.row),
            other => panic!("not a mouse event: {other:?}"),
        })
        .collect();
    assert_eq!(
        kinds,
        vec![
            (MouseKind::Press, 1, 8),
            (MouseKind::Drag, 9, 8),
            (MouseKind::Release, 18, 8)
        ]
    );
}

/// modifyOtherKeys keys (`CSI 27 ; mods ; code ~`) parse as the modified keys (tmux 3.2-3.4,
/// xterm), so Shift+Enter, Ctrl+Enter and Alt+Enter are told apart from Enter.
#[test]
fn modify_other_keys_input() {
    assert_eq!(
        only_key(b"\x1b[27;2;13~").id,
        KeyId::parse("shift+enter").unwrap()
    );
    assert_eq!(only_key(b"\x1b[27;5;13~").id, KeyId::parse("ctrl+enter").unwrap());
    assert_eq!(only_key(b"\x1b[27;3;13~").id, KeyId::parse("alt+enter").unwrap());
    assert_eq!(
        only_key(b"\x1b[27;6;97~").id,
        KeyId::parse("ctrl+shift+a").unwrap()
    );
}

/// O-199 first run (foot): the kitty keyboard protocol keeps one flags stack per screen, so
/// entering the alternate screen with the protocol on pushes the flags there too, and leaving
/// it pops them before the switch back (the main screen's push stays until the program exits).
#[test]
fn the_alternate_screen_gets_its_own_kitty_push() {
    use oxa_tui::{leave_sequence, switch_sequence, TermMode};
    let kitty_first = [TermMode::KittyKeyboard];
    assert_eq!(
        switch_sequence(&kitty_first, TermMode::AltScreen, true),
        "\x1b[?1049h\x1b[>7u"
    );
    assert_eq!(
        switch_sequence(&kitty_first, TermMode::AltScreen, false),
        "\x1b[<u\x1b[?1049l"
    );
    // Without the protocol, or for other modes: the plain sequence.
    assert_eq!(switch_sequence(&[], TermMode::AltScreen, true), "\x1b[?1049h");
    assert_eq!(
        switch_sequence(&kitty_first, TermMode::Mouse, false),
        "\x1b[?1006l\x1b[?1002l\x1b[?1000l"
    );
    // Leaving: both pushes are popped, each on its own screen.
    let active = [
        TermMode::BracketedPaste,
        TermMode::KittyKeyboard,
        TermMode::AltScreen,
    ];
    let leave = String::from_utf8(leave_sequence(&active, false)).unwrap();
    assert_eq!(leave, "\x1b[<u\x1b[?1049l\x1b[<u\x1b[?2004l");
}
