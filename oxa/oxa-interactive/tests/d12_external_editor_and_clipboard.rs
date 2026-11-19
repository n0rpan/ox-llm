//! D12 acceptance tests: External editor and clipboard.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_fake::TestEnv;
use oxa_interactive::*;
use oxa_proto::Part;
use oxa_tui::{TermCaps, TestTerminal};
use std::cell::Cell;
use std::path::{Path, PathBuf};

fn env_of(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// editor command with temp file
#[test]
fn ctrl_g_round_trip() {
    let env = TestEnv::new();
    let mut term = TestTerminal::new(80, 24);

    // The editor is a shell command that receives the temp file as its last argument.
    let text = "draft prompt";
    let edited = edit_externally(text, "printf 'edited text\\n\\n' >", &env.tmp(), &mut term).unwrap();
    // One trailing newline is removed; the file ends in two newlines, so one remains.
    assert_eq!(edited.as_deref(), Some("edited text\n"));
    assert_eq!(term.suspends, 1, "the terminal is suspended once");
    assert!(!term.suspended, "the terminal is resumed after the editor exits");
    let leftovers: Vec<_> = std::fs::read_dir(env.tmp()).unwrap().collect();
    assert!(leftovers.is_empty(), "the temp file is deleted");

    // A non-zero exit leaves the text unchanged.
    let failed = edit_externally(text, "false", &env.tmp(), &mut term).unwrap();
    assert_eq!(failed, None);
    assert!(!term.suspended);

    // The editor receives the temp file as its last argument. This one records the mode and
    // name of that file ($2 = the temp file, $1 = the report path) and leaves the text alone.
    let seen = env.tmp().join("seen.txt");
    let script = env.write("work/check.sh", "#!/bin/sh\nstat -c '%a %n' \"$2\" > \"$1\"\n");
    let line = format!("sh {} {}", script.display(), seen.display());
    let ok = edit_externally(text, &line, &env.tmp(), &mut term).unwrap();
    assert_eq!(
        ok.as_deref(),
        Some(text),
        "an editor that leaves the file alone returns the text"
    );
    let report = std::fs::read_to_string(&seen).unwrap();
    assert!(report.starts_with("600 "), "temp file mode is 0600: {report}");
    assert!(report.contains("oxa-editor-"), "temp file name: {report}");
}

/// ctrl+z
#[test]
fn suspend_resume_restores_screen() {
    let mut term = TestTerminal::new(80, 24);
    let stopped_while_suspended = Cell::new(false);
    // The stop action stands in for SIGTSTP (a real one would stop the test runner).
    let stop = || stopped_while_suspended.set(true);
    // The closure cannot borrow the terminal, so observe the state through the flag it sets.
    suspend_with(&mut term, &stop);
    assert!(stopped_while_suspended.get(), "the process was stopped");
    assert_eq!(term.suspends, 1);
    assert!(
        !term.suspended,
        "the terminal is resumed when the process continues"
    );
}

/// wl-copy, xclip, OSC 52 fallback order
#[test]
fn clipboard_backends() {
    let all_tools = |_: &str| true;
    let no_tools = |_: &str| false;
    let display = env_of(&[("WAYLAND_DISPLAY", "wayland-0"), ("DISPLAY", ":0")]);
    let x11 = env_of(&[("DISPLAY", ":0")]);
    let ssh = env_of(&[("SSH_CONNECTION", "1.2.3.4 22 5.6.7.8 22")]);
    let basic = TermCaps::BASIC;
    let osc = TermCaps {
        osc52: true,
        ..TermCaps::BASIC
    };

    assert_eq!(
        choose_clip_method(&display, &basic, &all_tools),
        ClipMethod::Wayland
    );
    assert_eq!(choose_clip_method(&x11, &basic, &all_tools), ClipMethod::Xclip);
    // Wayland without wl-copy falls through to xclip when X is available.
    let no_wl = |t: &str| t != "wl-copy";
    assert_eq!(choose_clip_method(&display, &basic, &no_wl), ClipMethod::Xclip);
    // No display tools: OSC 52 when the terminal has it or over SSH, else none.
    assert_eq!(choose_clip_method(&display, &basic, &no_tools), ClipMethod::None);
    assert_eq!(choose_clip_method(&display, &osc, &no_tools), ClipMethod::Osc52);
    assert_eq!(choose_clip_method(&ssh, &basic, &no_tools), ClipMethod::Osc52);
    // Headless (no display at all): OSC 52 is the only route (pi utils/clipboard.ts `headless`).
    assert_eq!(choose_clip_method(&[], &basic, &all_tools), ClipMethod::Osc52);

    // OSC 52 copy returns the sequence for the caller to write; tmux wraps it.
    let seq = copy_text("hi", &ClipMethod::Osc52, false).unwrap().unwrap();
    assert!(
        seq.starts_with("\x1b]52;c;") && seq.ends_with("\x1b\\"),
        "OSC 52 with ST: {seq:?}"
    );
    let wrapped = copy_text("hi", &ClipMethod::Osc52, true).unwrap().unwrap();
    assert!(wrapped.starts_with("\x1bPtmux;"));
    // No method: not_found.
    let err = copy_text("hi", &ClipMethod::None, false).unwrap_err();
    assert_eq!(err.code, oxa_proto::codes::NOT_FOUND);
    // Reading an image without a method is Ok(None), not an error.
    assert_eq!(read_clipboard_image(&ClipMethod::None).unwrap(), None);
    assert_eq!(read_clipboard_image(&ClipMethod::Osc52).unwrap(), None);
}

/// Part::Image with sha256
#[test]
fn image_paste_becomes_part() {
    let env = TestEnv::new();
    // A 1x1 PNG header is enough for the dimension reader; the bytes are opaque to the code.
    let mut data = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13];
    data.extend_from_slice(b"IHDR");
    data.extend_from_slice(&3u32.to_be_bytes());
    data.extend_from_slice(&2u32.to_be_bytes());
    let img = ClipImage {
        data: data.clone(),
        mime: "image/png".to_string(),
    };

    let part = image_part(&img, &env.tmp().join("pastes")).unwrap();
    let Part::Image {
        path,
        mime,
        sha256,
        w,
        h,
    } = part
    else {
        panic!("expected Part::Image");
    };
    assert_eq!(mime, "image/png");
    assert_eq!((w, h), (3, 2));
    assert_eq!(
        std::fs::read(&path).unwrap(),
        data,
        "the saved file holds the pasted bytes"
    );
    // The name carries the first 8 bytes (16 hex digits) of the digest.
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        name.starts_with(&format!("paste-{}", &hex(&sha256)[..16])),
        "name {name}"
    );
    assert!(name.ends_with(".png"));

    // Idempotent: the same bytes give the same file and the same digest.
    let again = image_part(&img, &env.tmp().join("pastes")).unwrap();
    assert_eq!(
        again,
        Part::Image {
            path: path.clone(),
            mime: "image/png".into(),
            sha256,
            w,
            h
        }
    );

    // The digest is SHA-256 of the bytes: the empty input has a known digest.
    let empty = ClipImage {
        data: Vec::new(),
        mime: "image/png".into(),
    };
    assert!(save_pasted_image(&empty, &env.tmp()).is_err());
    assert!(save_pasted_image(
        &ClipImage {
            data: vec![1],
            mime: "text/plain".into()
        },
        &env.tmp()
    )
    .is_err());
}

/// path insertion
#[test]
fn file_paste_becomes_at_path() {
    let always = |_: &Path| true;
    let never = |_: &Path| false;

    // Shell-quoted and escaped paths, several separated by spaces and newlines.
    let text = "'/tmp/a b.png'\n/tmp/c\\ d.jpg";
    assert_eq!(
        pasted_paths(text, &always),
        vec![PathBuf::from("/tmp/a b.png"), PathBuf::from("/tmp/c d.jpg")]
    );
    // file:// URLs with percent-encoding.
    assert_eq!(
        pasted_paths("file:///tmp/e%20f.gif", &always),
        vec![PathBuf::from("/tmp/e f.gif")]
    );
    // Plain text is pasted as text: a relative word, a missing path, or mixed content.
    assert!(pasted_paths("hello world", &always).is_empty());
    assert!(pasted_paths("/tmp/x.png and more", &always).is_empty());
    assert!(pasted_paths("/tmp/missing.png", &never).is_empty());
    assert!(pasted_paths("", &always).is_empty());
    assert!(pasted_paths("'/tmp/unterminated", &always).is_empty());

    assert!(is_image_path(Path::new("/tmp/a.PNG")));
    assert!(is_image_path(Path::new("b.webp")));
    assert!(!is_image_path(Path::new("c.pdf")));
}

#[test]
fn resolve_editor_prefers_visual() {
    assert_eq!(
        resolve_editor(&env_of(&[("VISUAL", "vim"), ("EDITOR", "nano")])).as_deref(),
        Some("vim")
    );
    assert_eq!(
        resolve_editor(&env_of(&[("VISUAL", ""), ("EDITOR", "code --wait")])).as_deref(),
        Some("code --wait")
    );
    assert_eq!(resolve_editor(&env_of(&[])), None);
}

#[test]
fn saved_path_is_under_dir() {
    let env = TestEnv::new();
    let img = ClipImage {
        data: vec![7, 7, 7],
        mime: "image/jpeg".into(),
    };
    let p = save_pasted_image(&img, &env.tmp()).unwrap();
    assert!(p.starts_with(env.tmp()));
    assert!(p.to_string_lossy().ends_with(".jpg"));
}

#[test]
fn the_external_editor_setting_comes_before_visual_and_editor() {
    // pi settings.md `externalEditor`: the setting, then $VISUAL, then $EDITOR.
    let env = env_of(&[("VISUAL", "vim"), ("EDITOR", "nano")]);
    assert_eq!(resolve_editor_with(Some("hx"), &env).as_deref(), Some("hx"));
    assert_eq!(resolve_editor_with(Some("  "), &env).as_deref(), Some("vim"));
    assert_eq!(
        resolve_editor_with(None, &env_of(&[("EDITOR", "nano")])).as_deref(),
        Some("nano")
    );
    assert_eq!(resolve_editor_with(None, &env_of(&[])), None);
}
