//! The graphics tier of the arcade animation (ARCADE-ANIMATION.md): the capability query, one
//! transmission per frame, the placeholder row, cleanup, and the fallback chain. Public API only.
use oxa_config::Keymap;
use oxa_interactive::{
    graphics_wanted, oxa_pixel_line, pixel_frame, pixel_key, Activity, ArcadeRun, Finish, PixelKey,
    PixelStage, PixelState, Skin, StatusModel, PIXEL_CELL_W, PIXEL_H, PIXEL_ID_BASE, SPRITE_FRAME_MS,
};
use oxa_tui::{
    env_gfx_placeholders, gfx_delete, gfx_placeholder_row, gfx_query, gfx_reply_in, gfx_transmit,
    parse_input, strip_ansi, visible_width, ColorMode, InputEvent, Theme, GFX_PLACEHOLDER,
};

fn colour() -> Theme {
    oxa_tui::builtin_theme("dark", ColorMode::TrueColor, None).expect("builtin dark theme")
}

fn status(activity: Activity, frame: u64) -> StatusModel {
    StatusModel {
        activity,
        activity_ms: frame * SPRITE_FRAME_MS,
        ..StatusModel::default()
    }
}

fn tool(name: &str) -> Activity {
    Activity::Tool(name.to_string())
}

fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

const RUN: ArcadeRun = ArcadeRun {
    coins: 3,
    finish: None,
};

fn line(
    st: &StatusModel,
    run: &ArcadeRun,
    width: u16,
    stage: &mut PixelStage,
    out: &mut String,
) -> Option<String> {
    oxa_pixel_line(
        st,
        &Keymap::defaults(),
        run,
        width,
        &colour(),
        Skin::Oxa,
        stage,
        0,
        out,
    )
}

// ---------------------------------------------------------------------------------------------
// Capability detection

#[test]
fn the_query_is_the_standard_one_and_its_reply_is_read_from_the_input() {
    assert_eq!(gfx_query(), "\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\");
    // A terminal with the protocol: the OK arrives before the device attributes.
    let (events, used) = parse_input(b"\x1b_Gi=31;OK\x1b\\\x1b[?62;4c");
    assert_eq!(used, 20);
    assert_eq!(
        events,
        vec![
            InputEvent::Apc("Gi=31;OK".to_string()),
            InputEvent::DeviceAttributes("62;4".to_string())
        ]
    );
    assert_eq!(gfx_reply_in(&events), Some(true));
    // It answers with an error: no graphics.
    let (events, _) = parse_input(b"\x1b_Gi=31;ENOTSUPPORTED:no\x1b\\\x1b[?62c");
    assert_eq!(gfx_reply_in(&events), Some(false));
    // A terminal without it (foot): only the device attributes come back; the wait ends
    // there, or at the timeout when nothing comes at all.
    let (events, _) = parse_input(b"\x1b[?62;4;22c");
    assert_eq!(gfx_reply_in(&events), None);
    assert_eq!(gfx_reply_in(&[]), None);
    // Half a reply is not a key press: the parser waits for the rest.
    let (events, used) = parse_input(b"\x1b_Gi=31;O");
    assert_eq!((events, used), (Vec::new(), 0));
    // The reply never reaches the editor as text, and alt+_ is still a key.
    let (events, _) = parse_input(b"\x1b_");
    assert!(matches!(events.as_slice(), [InputEvent::Key(_)]));
}

#[test]
fn the_fallback_chain() {
    // (scene setting, graphics setting, answered, placeholders) -> pixels?
    assert!(
        graphics_wanted(None, None, true, true),
        "Ghostty and kitty: pixels"
    );
    assert!(
        !graphics_wanted(None, None, false, true),
        "no reply to the query: text (stack)"
    );
    assert!(
        !graphics_wanted(None, None, true, false),
        "an unknown terminal that answers: text"
    );
    assert!(
        graphics_wanted(Some("graphics"), None, true, false),
        "unless the user asks for it"
    );
    assert!(
        !graphics_wanted(Some("graphics"), None, false, true),
        "never without the protocol"
    );
    assert!(
        !graphics_wanted(None, Some("off"), true, true),
        "ui.animationGraphics = off"
    );
    assert!(!graphics_wanted(Some("graphics"), Some("off"), true, true));
    for text in ["stack", "stars", "charge"] {
        assert!(
            !graphics_wanted(Some(text), None, true, true),
            "{text} is a text scene"
        );
    }
    // Which terminals draw placeholders.
    assert!(env_gfx_placeholders(&env(&[("TERM", "xterm-ghostty")])));
    assert!(env_gfx_placeholders(&env(&[("TERM", "xterm-kitty")])));
    assert!(env_gfx_placeholders(&env(&[
        ("TERM_PROGRAM", "ghostty"),
        ("TERM", "xterm-256color")
    ])));
    assert!(!env_gfx_placeholders(&env(&[("TERM", "foot")])));
    assert!(!env_gfx_placeholders(&env(&[
        ("TERM", "xterm-256color"),
        ("WEZTERM_PANE", "1")
    ])));
    assert!(!env_gfx_placeholders(&env(&[
        ("TERM", "xterm-kitty"),
        ("TMUX", "/tmp/t")
    ])));
    // Without colour, in the ASCII skin, or with nothing to show: no pixel line (the caller
    // draws the text line, which is the static badge under NO_COLOR / reduced motion).
    let (mut stage, mut out) = (PixelStage::new(None), String::new());
    let st = status(Activity::Thinking, 0);
    let km = Keymap::defaults();
    assert!(oxa_pixel_line(
        &st,
        &km,
        &RUN,
        100,
        &Theme::plain(),
        Skin::Oxa,
        &mut stage,
        0,
        &mut out
    )
    .is_none());
    assert!(oxa_pixel_line(
        &st,
        &km,
        &RUN,
        100,
        &colour(),
        Skin::OxaAscii,
        &mut stage,
        0,
        &mut out
    )
    .is_none());
    assert!(line(&status(Activity::Idle, 0), &RUN, 100, &mut stage, &mut out).is_none());
    assert!(line(&status(Activity::Blocked, 0), &RUN, 100, &mut stage, &mut out).is_none());
    assert!(line(&st, &RUN, 30, &mut stage, &mut out).is_none(), "no room");
    assert!(out.is_empty() && stage.images() == 0, "nothing was sent for them");
}

// ---------------------------------------------------------------------------------------------
// Escape sequences (golden)

#[test]
fn golden_escape_sequences() {
    // One 2x1 RGBA image, 3 cells wide: transmitted with a virtual placement, quietly.
    let rgba = [255, 0, 0, 255, 0, 0, 0, 0];
    assert_eq!(
        gfx_transmit(0xF0A000, 2, 1, &rgba, 3, 1),
        "\x1b_Ga=T,U=1,f=32,q=2,i=15769600,s=2,v=1,c=3,r=1,m=0;/wAA/wAAAAA=\x1b\\"
    );
    // The row that shows it: the id as a colour, one placeholder per cell.
    assert_eq!(
        gfx_placeholder_row(0xF0A001, 3),
        "\x1b[38;2;240;160;1m\u{10EEEE}\u{10EEEE}\u{10EEEE}\x1b[39m"
    );
    assert_eq!(visible_width(&gfx_placeholder_row(0xF0A001, 12)), 12);
    assert_eq!(gfx_delete(0xF0A001), "\x1b_Ga=d,d=I,i=15769601,q=2\x1b\\");
    // A large image goes in chunks of 4096 base64 characters; only the first has the keys.
    let big = vec![7u8; 100 * 20 * 4];
    let sent = gfx_transmit(9, 100, 20, &big, 12, 1);
    let parts: Vec<&str> = sent.split("\x1b\\").filter(|p| !p.is_empty()).collect();
    assert_eq!(parts.len(), 3);
    assert!(parts[0].starts_with("\x1b_Ga=T,U=1,f=32,q=2,i=9,s=100,v=20,c=12,r=1,m=1;"));
    assert!(parts[1].starts_with("\x1b_Gm=1;") && parts[2].starts_with("\x1b_Gm=0;"));
    assert!(parts
        .iter()
        .all(|p| p.split(';').nth(1).is_some_and(|d| d.len() <= 4096)));
    // A size that does not match the data, or id 0: nothing is sent.
    assert!(gfx_transmit(9, 3, 1, &rgba, 3, 1).is_empty());
    assert!(gfx_transmit(0, 2, 1, &rgba, 3, 1).is_empty());
}

#[test]
fn golden_activity_line_with_the_pixel_scene() {
    let (mut stage, mut out) = (PixelStage::new(None), String::new());
    let st = StatusModel {
        activity: Activity::Thinking,
        activity_ms: 8000,
        ..StatusModel::default()
    };
    let l = line(&st, &RUN, 80, &mut stage, &mut out).expect("a line");
    let row = "\u{10EEEE}".repeat(12);
    assert_eq!(
        strip_ansi(&l),
        format!(" {row}  THINKING   8s · escape to cancel")
    );
    assert!(
        l.starts_with(" \x1b[38;2;240;160;0m\u{10EEEE}"),
        "the first image: {l:?}"
    );
    assert_eq!(visible_width(&l), 47);
    // A tool: the coins of the run are text behind the picture.
    let l = line(&status(tool("bash"), 0), &RUN, 80, &mut stage, &mut out).expect("a line");
    assert_eq!(
        strip_ansi(&l),
        format!(" {row} ◆3  TOOL   bash · escape to cancel")
    );
    // The closing scene.
    let done = ArcadeRun {
        coins: 3,
        finish: Some((Finish::Clear, 0)),
    };
    let l = line(&status(Activity::Idle, 0), &done, 80, &mut stage, &mut out).expect("a line");
    assert_eq!(strip_ansi(&l), format!(" {row}  CLEAR   ◆3 tools"));
    // Narrow: 8 cells; never wider than the terminal, always one row.
    let l = line(&st, &RUN, 60, &mut stage, &mut out).expect("a line");
    assert_eq!(
        strip_ansi(&l).chars().filter(|c| *c == GFX_PLACEHOLDER).count(),
        8
    );
    for width in 1..=140u16 {
        if let Some(l) = line(&status(tool("read"), 5), &RUN, width, &mut stage, &mut out) {
            assert!(
                visible_width(&l) <= usize::from(width) && !l.contains('\n'),
                "{width}"
            );
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Frames are transmitted once; cleanup

/// The image ids transmitted in `out`.
fn transmitted(out: &str) -> Vec<u32> {
    out.split("\x1b_G")
        .filter(|p| p.starts_with("a=T,"))
        .filter_map(|p| p.split(',').find_map(|k| k.strip_prefix("i=")?.parse().ok()))
        .collect()
}

#[test]
fn every_frame_is_transmitted_once_and_then_only_the_row_changes() {
    let (mut stage, mut out) = (PixelStage::new(None), String::new());
    // Two seconds of thinking: 16 distinct frames, each sent on its first use.
    let mut rows = Vec::new();
    for f in 0..16u64 {
        let before = out.len();
        rows.push(line(&status(Activity::Thinking, f), &RUN, 80, &mut stage, &mut out).expect("a line"));
        assert!(out.len() > before, "frame {f} is new: it is sent");
    }
    let first = transmitted(&out);
    assert_eq!(first, (PIXEL_ID_BASE..PIXEL_ID_BASE + 16).collect::<Vec<u32>>());
    assert_eq!(stage.images(), 16);
    let distinct: std::collections::BTreeSet<&String> = rows.iter().collect();
    assert_eq!(distinct.len(), 16, "every frame has its own row");
    let sent = out.len();
    assert_eq!(stage.sent_bytes, sent as u64);

    // Ten more seconds: the loop repeats, NOTHING more is sent; a tick is one short row.
    for f in 16..96u64 {
        let l = line(&status(Activity::Thinking, f), &RUN, 80, &mut stage, &mut out).expect("a line");
        // The same picture as the first time round (the elapsed time behind it moves on).
        let picture = |l: &str| l.split("\x1b[39m").next().unwrap_or_default().to_string();
        assert_eq!(picture(&l), picture(&rows[(f % 16) as usize]));
        assert!(l.len() < 400, "a tick is a few hundred bytes: {}", l.len());
    }
    assert_eq!(out.len(), sent, "frames are transmitted once");

    // Another state brings its own frames, once.
    for round in 0..3 {
        for f in 0..8u64 {
            line(&status(tool("bash"), f), &RUN, 80, &mut stage, &mut out);
        }
        assert_eq!(stage.images(), 16 + 8, "round {round}");
    }
    let all = transmitted(&out);
    let unique: std::collections::BTreeSet<&u32> = all.iter().collect();
    assert_eq!((all.len(), unique.len()), (24, 24), "no id is sent twice");
}

#[test]
fn cleanup_deletes_every_image_and_a_new_theme_starts_over() {
    let (mut stage, mut out) = (PixelStage::new(None), String::new());
    for f in 0..4u64 {
        line(&status(Activity::Streaming, f), &RUN, 80, &mut stage, &mut out);
    }
    let ids = transmitted(&out);
    assert_eq!(ids.len(), 4);
    // Exit, resize, suspend: one delete per image, with its data (`d=I`), and nothing held.
    let mut bye = String::new();
    stage.clear(&mut bye);
    let expected: String = ids.iter().map(|id| gfx_delete(*id)).collect();
    assert_eq!(bye, expected);
    assert_eq!(stage.images(), 0);
    let mut again = String::new();
    stage.clear(&mut again);
    assert!(again.is_empty(), "nothing to delete twice");

    // After a cleanup the frames are sent again when they are needed.
    let mut out = String::new();
    line(&status(Activity::Streaming, 0), &RUN, 80, &mut stage, &mut out);
    assert_eq!(transmitted(&out).len(), 1);
    // A theme change (a new epoch): the old frames are deleted before the new one is sent.
    let mut out = String::new();
    let st = status(Activity::Streaming, 0);
    let km = Keymap::defaults();
    let light = oxa_tui::builtin_theme("light", ColorMode::TrueColor, None).expect("light");
    oxa_pixel_line(&st, &km, &RUN, 80, &light, Skin::Oxa, &mut stage, 1, &mut out);
    let delete_at = out.find("a=d,d=I").expect("the old frame is deleted");
    let send_at = out.find("a=T,").expect("the new frame is sent");
    assert!(delete_at < send_at);
    assert_eq!(stage.images(), 1);
}

// ---------------------------------------------------------------------------------------------
// The pictures

#[test]
fn frames_are_theme_coloured_pixel_strips() {
    let theme = colour();
    let key = pixel_key(&status(Activity::Thinking, 3), &RUN, 12).expect("a key");
    assert_eq!(
        key,
        PixelKey {
            state: PixelState::Think,
            frame: 3,
            cells: 12,
            hero: "thinkingHigh"
        }
    );
    let image = pixel_frame(key, &theme, 2);
    assert_eq!(
        (image.w, image.h),
        ((12 * PIXEL_CELL_W * 2) as u32, (PIXEL_H * 2) as u32)
    );
    assert_eq!(image.rgba.len(), (image.w * image.h * 4) as usize);
    // Transparent where nothing is (the terminal's background shows), opaque where the ox is.
    let alphas: std::collections::BTreeSet<u8> = image.rgba.chunks(4).map(|p| p[3]).collect();
    assert!(alphas.contains(&0) && alphas.contains(&255), "{alphas:?}");
    // The ox has the state's colour from the theme; another theme, another picture.
    let hero = oxa_tui::color_rgb(theme.color("thinkingHigh")).expect("an RGB colour");
    assert!(image
        .rgba
        .chunks(4)
        .any(|p| (p[0], p[1], p[2]) == hero && p[3] == 255));
    let light = oxa_tui::builtin_theme("light", ColorMode::TrueColor, None).expect("light");
    assert_ne!(image, pixel_frame(key, &light, 2));
    // Deterministic, and the scale only multiplies pixels.
    assert_eq!(image, pixel_frame(key, &theme, 2));
    assert_eq!(pixel_frame(key, &theme, 1).rgba.len() * 4, image.rgba.len());
    // YOLO: the ox burns.
    let hot = StatusModel {
        yolo: true,
        ..status(Activity::Thinking, 3)
    };
    assert_eq!(pixel_key(&hot, &RUN, 12).map(|k| k.hero), Some("error"));
}

#[test]
fn every_state_has_its_own_moving_picture() {
    let theme = colour();
    let states: Vec<(Activity, u64)> = vec![
        (Activity::Thinking, 16),
        (Activity::Streaming, 16),
        (tool("bash"), 8),
        (Activity::Compacting, 8),
        (
            Activity::Retrying {
                attempt: 1,
                max_attempts: 3,
                delay_ms: 100,
            },
            4,
        ),
        (Activity::Queued { pos: None, wait_s: 0 }, 8),
    ];
    let mut firsts = Vec::new();
    for (activity, period) in states {
        let frames: Vec<_> = (0..period)
            .map(|f| pixel_key(&status(activity.clone(), f), &RUN, 12).expect("a key"))
            .map(|k| pixel_frame(k, &theme, 1))
            .collect();
        let distinct: std::collections::BTreeSet<&Vec<u8>> = frames.iter().map(|i| &i.rgba).collect();
        assert!(distinct.len() >= 2, "{activity:?} moves");
        // The loop closes: frame `period` is frame 0 again.
        assert_eq!(
            pixel_key(&status(activity.clone(), period), &RUN, 12),
            pixel_key(&status(activity.clone(), 0), &RUN, 12),
            "{activity:?}"
        );
        firsts.push(frames[0].rgba.clone());
    }
    let distinct: std::collections::BTreeSet<&Vec<u8>> = firsts.iter().collect();
    assert_eq!(distinct.len(), firsts.len(), "the states look different");
    // A tool's crate carries the icon of its kind; the prefill block is where the prefill is.
    let crate_of = |name: &str| pixel_key(&status(tool(name), 0), &RUN, 12).map(|k| k.state);
    assert_ne!(crate_of("bash"), crate_of("read"));
    assert_eq!(crate_of("read"), Some(PixelState::Tool(1)));
    let prefill = |done: u32| {
        let st = status(
            Activity::Prefill {
                done,
                total: 8,
                secs: 0,
            },
            0,
        );
        pixel_key(&st, &RUN, 12).map(|k| k.state)
    };
    assert_eq!(prefill(0), Some(PixelState::Prefill(0)));
    assert_eq!(prefill(8), Some(PixelState::Prefill(15)));
    // The closing scene plays once (12 frames) and holds its last frame.
    let clear = |ms: u64| {
        let run = ArcadeRun {
            coins: 1,
            finish: Some((Finish::Clear, ms)),
        };
        pixel_key(&status(Activity::Idle, 0), &run, 12).map(|k| (k.state, k.frame))
    };
    assert_eq!(clear(0), Some((PixelState::Clear, 0)));
    assert_eq!(clear(11 * SPRITE_FRAME_MS), Some((PixelState::Clear, 11)));
    assert_eq!(clear(60 * SPRITE_FRAME_MS), Some((PixelState::Clear, 11)));
}

// ---------------------------------------------------------------------------------------------
// Cost and previews (run with `--ignored --nocapture`)

#[test]
#[ignore = "a measurement and a preview dump, not a gate: OXA_PIXEL_DUMP=<dir> cargo test --release -- --ignored --nocapture pixel_cost"]
fn pixel_cost_and_previews() {
    let theme = colour();
    let (mut stage, mut out) = (PixelStage::new(Some((10, 20))), String::new());
    // First pass: every frame of a typical run is painted and encoded once.
    let run: Vec<(Activity, u64)> = vec![
        (Activity::Thinking, 16),
        (tool("read"), 8),
        (Activity::Streaming, 16),
    ];
    let start = std::time::Instant::now();
    let mut frames = 0u64;
    for (activity, n) in &run {
        for f in 0..*n {
            line(&status(activity.clone(), f), &RUN, 100, &mut stage, &mut out);
            frames += 1;
        }
    }
    let first = start.elapsed();
    println!(
        "pixel_cost first use: {frames} frames, {:.0} us per frame to paint + encode, {} bytes per frame sent once ({} KiB in all)",
        first.as_micros() as f64 / frames as f64,
        out.len() as u64 / frames,
        out.len() / 1024
    );
    // Steady state: the frames are held; a tick is a lookup and one row.
    let rounds = 200_000u64;
    let km = Keymap::defaults();
    let sent = out.len();
    let mut bytes = 0usize;
    let start = std::time::Instant::now();
    for f in 0..rounds {
        let st = status(Activity::Thinking, f);
        let l = oxa_pixel_line(&st, &km, &RUN, 100, &theme, Skin::Oxa, &mut stage, 0, &mut out);
        bytes += l.map_or(0, |l| l.len());
    }
    let ns = start.elapsed().as_nanos() as f64 / rounds as f64;
    assert_eq!(out.len(), sent);
    println!(
        "pixel_cost steady: {ns:.0} ns per frame, {} bytes per frame, {:.4} % of one core at 8 fps",
        bytes / rounds as usize,
        ns * 8.0 / 1e9 * 100.0
    );

    // Previews: raw RGBA strips for the PNG contact sheet next to ARCADE-ANIMATION.md.
    let Ok(dir) = std::env::var("OXA_PIXEL_DUMP") else {
        return;
    };
    let done = |how: Finish, f: u64| ArcadeRun {
        coins: 3,
        finish: Some((how, f * SPRITE_FRAME_MS)),
    };
    let mut shots: Vec<(String, StatusModel, ArcadeRun)> = Vec::new();
    for (name, activity, picks) in [
        ("1-thinking", Activity::Thinking, vec![0u64, 2, 4, 6, 9, 13]),
        ("2-writing", Activity::Streaming, vec![0, 2, 4, 6, 9, 13]),
        ("3-tool-bash", tool("bash"), vec![0, 2, 4, 6]),
        ("3-tool-read", tool("read"), vec![4]),
        ("3-tool-edit", tool("edit"), vec![4]),
        ("4-compacting", Activity::Compacting, vec![0, 2, 4, 6, 7]),
        (
            "5-prefill",
            Activity::Prefill {
                done: 3,
                total: 8,
                secs: 0,
            },
            vec![0],
        ),
        (
            "6-retry",
            Activity::Retrying {
                attempt: 1,
                max_attempts: 3,
                delay_ms: 0,
            },
            vec![0, 1],
        ),
        ("7-queued", Activity::Queued { pos: None, wait_s: 0 }, vec![0, 4]),
    ] {
        for f in picks {
            shots.push((format!("{name}-{f:02}"), status(activity.clone(), f), RUN));
        }
    }
    for f in [0u64, 2, 4, 6, 9, 11] {
        shots.push((
            format!("8-clear-{f:02}"),
            status(Activity::Idle, 0),
            done(Finish::Clear, f),
        ));
    }
    shots.push((
        "9-failed-00".to_string(),
        status(Activity::Idle, 0),
        done(Finish::Fail, 0),
    ));
    for theme_name in ["dark", "light"] {
        let theme = oxa_tui::builtin_theme(theme_name, ColorMode::TrueColor, None).expect("theme");
        for (name, st, run) in &shots {
            let key = pixel_key(st, run, 12).expect("a key");
            let image = pixel_frame(key, &theme, 4);
            let path = format!("{dir}/{theme_name}_{name}_{}x{}.rgba", image.w, image.h);
            std::fs::write(&path, &image.rgba).expect("write the preview");
        }
    }
    let _ = theme;
}
