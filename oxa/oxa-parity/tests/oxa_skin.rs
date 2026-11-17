//! OXA's own look (`ui.skin` / `OXA_SKIN=oxa`) in a real pty against the real `oxa` binary and
//! the fake engine. Not PARITY rows: pi has no such look. The PARITY rows drive pi's look
//! (`oxa_parity::pty` sets `OXA_SKIN=pi`); these tests set `OXA_SKIN=oxa`.
//! Build the binaries first: `cargo build -p oxa-cli -p oxa-fake`.
#![cfg(target_os = "linux")]
use oxa_fake::{parse_script, FakeOpts, FakeServer, Fault, ScriptTurn, TestEnv};
use oxa_parity::pty::{PtyOpts, PtySession, Screen};
use serde_json::json;
use std::time::Duration;

const REPLY: &str = "PONG from the fake engine";
const WAIT: Duration = Duration::from_secs(90);
/// Glyphs only the working sprites draw.
/// Glyphs of the moving scene: the small sprites and the stacking columns (the default).
const SPRITE_GLYPHS: [char; 12] = ['▪', '▒', '▜', '▙', '▟', '▛', '▇', '▁', '▂', '▃', '▄', '▆'];

struct Rig {
    // Field order = drop order: the pty (and the child) go first, then the engine, then the dirs.
    pty: PtySession,
    _server: FakeServer,
    env: TestEnv,
}

fn start_in(env: TestEnv, extra_env: &[(&str, &str)], fake: FakeOpts) -> Rig {
    let line = format!("{{\"text\":\"{REPLY}\"}}\n");
    let server =
        FakeServer::start(parse_script(&line.repeat(6)).expect("script"), fake).expect("start fake engine");
    let mut vars = vec![
        ("OXA_SKIN".to_string(), "oxa".to_string()),
        ("LANG".to_string(), "C.UTF-8".to_string()),
    ];
    vars.extend(extra_env.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    let pty = PtySession::spawn_with(
        &env,
        &server.url(),
        &[],
        PtyOpts {
            size: (30, 110),
            env: vars,
            ..PtyOpts::default()
        },
    );
    pty.wait_ready().unwrap();
    Rig {
        pty,
        _server: server,
        env,
    }
}

fn start(extra_env: &[(&str, &str)], fake: FakeOpts) -> Rig {
    let env = TestEnv::new();
    // No boot preload: the first frame is idle.
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"oxa":{"bootPreload":false}}"#,
    );
    start_in(env, extra_env, fake)
}

/// [`start_in`] with a script of its own (the #871 receipts need a fault and a tool call).
fn start_script(env: TestEnv, extra_env: &[(&str, &str)], script: Vec<ScriptTurn>, fake: FakeOpts) -> Rig {
    let server = FakeServer::start(script, fake).expect("start fake engine");
    let mut vars = vec![
        ("OXA_SKIN".to_string(), "oxa".to_string()),
        ("LANG".to_string(), "C.UTF-8".to_string()),
    ];
    vars.extend(extra_env.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    let pty = PtySession::spawn_with(
        &env,
        &server.url(),
        &[],
        PtyOpts {
            size: (30, 110),
            env: vars,
            ..PtyOpts::default()
        },
    );
    pty.wait_ready().unwrap();
    Rig {
        pty,
        _server: server,
        env,
    }
}

fn splash_on(s: &Screen) -> bool {
    s.contains("OXA v") && s.contains("▄▀▀▄ ▀▄ ▄▀ ▄▀▀▄")
}

/// The row of the activity line (the one that names the cancel key), if a run is shown.
fn activity_row(s: &Screen) -> Option<String> {
    s.find_row("to cancel").map(|r| s.row(r))
}

/// O-184: the splash heads every empty chat: at the start and again after `/new`; it carries
/// the suite and the author credit; it is part of the transcript, so the conversation
/// follows below it.
#[test]
fn splash_heads_every_empty_chat() {
    // Rendering the reply is not completion: hold [DONE] to exercise that gap reliably.
    let rig = start(
        &[],
        FakeOpts {
            done_gap_ms: 1000,
            ..FakeOpts::default()
        },
    );
    let first = rig.pty.wait_for_named("the splash", splash_on, WAIT).unwrap();
    assert!(first.contains("part of ox-llm"), "{first:?}");
    assert!(first.contains("written by Oscar"), "{first:?}");
    assert_eq!(
        first.find_row("OXA v"),
        Some(1),
        "the splash is at the top: {first:?}"
    );
    assert!(first.contains("● READY"), "the HUD is idle: {first:?}");
    assert!(first.contains("enter send"), "the hint bar is shown: {first:?}");

    rig.pty.send_text("ping");
    rig.pty.send_key("enter");
    let replied = rig.pty.wait_for_text(REPLY).unwrap();
    let splash_row = replied
        .find_row("OXA v")
        .expect("a short chat still shows the splash");
    let reply_row = replied.find_row(REPLY).expect("the reply");
    assert!(
        splash_row < reply_row,
        "the conversation follows the splash: {replied:?}"
    );

    // The reply can be visible while its stream is still open. /new requires an idle
    // session, so synchronize on the completed run rather than a streamed text delta.
    rig.pty
        .wait_for_named(
            "the completed reply and idle prompt",
            |s| s.contains(REPLY) && s.contains("● READY") && s.contains("enter send"),
            WAIT,
        )
        .unwrap();
    rig.pty.send_text("/new");
    rig.pty.send_key("enter");
    let fresh = rig
        .pty
        .wait_for_named(
            "an empty chat with the splash",
            |s| splash_on(s) && !s.contains(REPLY),
            WAIT,
        )
        .unwrap();
    assert!(fresh.contains("written by Oscar"), "{fresh:?}");
}

/// `quietStartup` still hides the splash; the HUD and the hint bar stay.
#[test]
fn quiet_startup_hides_the_splash() {
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"quietStartup":true,"oxa":{"bootPreload":false}}"#,
    );
    let rig = start_in(env, &[], FakeOpts::default());
    let screen = rig.pty.wait_for_text("● READY").unwrap();
    assert!(
        !screen.contains("OXA v") && !screen.contains("written by"),
        "{screen:?}"
    );
    assert!(screen.contains(" OXA │ "), "{screen:?}");
    let _ = &rig.env;
}

/// O-184: while a run is active the activity line shows a moving sprite; when the run ends
/// the line is gone (after the arcade's short closing scene) and the idle UI writes nothing
/// at all.
#[test]
fn animation_runs_while_working_and_stops_when_idle() {
    let rig = start(
        &[],
        FakeOpts {
            hold_headers_ms: 2500,
            ..FakeOpts::default()
        },
    );
    rig.pty.wait_for_text("● READY").unwrap();
    rig.pty.send_text("ping");
    rig.pty.send_key("enter");
    let working = rig
        .pty
        .wait_for_named("the activity line", |s| activity_row(s).is_some(), WAIT)
        .unwrap();
    let row = activity_row(&working).expect("activity row");
    assert!(
        row.chars().any(|c| SPRITE_GLYPHS.contains(&c)),
        "the activity line carries a sprite: {row:?}"
    );
    // It moves: within a second the row is drawn differently.
    rig.pty
        .wait_for_named(
            "the next sprite frame",
            |s| activity_row(s).is_some_and(|r| r != row),
            Duration::from_secs(2),
        )
        .unwrap();

    let done = rig.pty.wait_for_text(REPLY).unwrap();
    let idle = rig
        .pty
        .wait_for_named(
            "the idle HUD",
            |s| s.contains("● READY") && activity_row(s).is_none(),
            WAIT,
        )
        .unwrap();
    let _ = idle;
    // The closing scene ("stage clear") shows for a moment, then that line is gone too.
    rig.pty
        .wait_for_named(
            "the closing scene to end",
            |s| s.find_row(" CLEAR ").is_none(),
            WAIT,
        )
        .unwrap();
    let _ = done;
    // Idle: nothing is written for a whole second (no timer keeps drawing). The closing
    // scene draws every 250 ms, so "quiet" must be clearly longer than that.
    rig.pty.wait_stable(700, WAIT).unwrap();
    let mark = rig.pty.mark();
    std::thread::sleep(Duration::from_millis(1000));
    assert!(
        rig.pty.raw_since(mark).is_empty(),
        "the idle UI wrote {} bytes: {:?}",
        rig.pty.raw_since(mark).len(),
        String::from_utf8_lossy(rig.pty.raw_since(mark).as_ref())
    );
}

/// A reduced-motion case: its name, the extra environment and the settings file.
type Case = (
    &'static str,
    &'static [(&'static str, &'static str)],
    &'static str,
);

/// O-184: reduced motion (the environment variable, the setting, `NO_COLOR`): a static text
/// badge, no sprite.
#[test]
fn reduced_motion_shows_a_static_badge() {
    let held = || FakeOpts {
        hold_headers_ms: 2000,
        ..FakeOpts::default()
    };
    let cases: [Case; 3] = [
        (
            "OXA_REDUCED_MOTION",
            &[("OXA_REDUCED_MOTION", "1")],
            r#"{"oxa":{"bootPreload":false}}"#,
        ),
        (
            "ui.reducedMotion",
            &[],
            r#"{"ui":{"reducedMotion":true},"oxa":{"bootPreload":false}}"#,
        ),
        (
            "NO_COLOR",
            &[("NO_COLOR", "1")],
            r#"{"oxa":{"bootPreload":false}}"#,
        ),
    ];
    for (name, vars, settings) in cases {
        let env = TestEnv::new();
        env.write("home/.oxa/agent/settings.json", settings);
        let rig = start_in(env, vars, held());
        rig.pty.wait_for_text("READY").unwrap();
        rig.pty.send_text("ping");
        rig.pty.send_key("enter");
        let working = rig
            .pty
            .wait_for_named("the activity line", |s| activity_row(s).is_some(), WAIT)
            .unwrap();
        let row = activity_row(&working).expect("activity row");
        assert!(
            !row.chars().any(|c| SPRITE_GLYPHS.contains(&c)),
            "{name}: no sprite with reduced motion: {row:?}"
        );
        assert!(
            row.chars().any(|c| c.is_ascii_uppercase()),
            "{name}: the state is a text badge: {row:?}"
        );
        // Still: between two looks half a second apart only the elapsed time may differ.
        std::thread::sleep(Duration::from_millis(400));
        let later = activity_row(&rig.pty.screen()).unwrap_or_default();
        let strip_time = |r: &str| r.chars().filter(|c| !c.is_ascii_digit()).collect::<String>();
        if !later.is_empty() {
            assert_eq!(
                strip_time(&row).replace("s ·", "·"),
                strip_time(&later).replace("s ·", "·"),
                "{name}"
            );
        }
        rig.pty.wait_for_text(REPLY).unwrap();
    }
}

/// O-185: `oxa-omarchy` follows the desktop while oxa runs: the palette file changes
/// mid-session and the UI is repainted in the new colours; a half-written file in between
/// changes nothing.
#[test]
fn desktop_theme_change_repaints_a_running_session() {
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"oxa":{"bootPreload":false}}"#,
    );
    let palette =
        |accent: &str| format!("background = \"#101010\"\nforeground = \"#e0e0e0\"\naccent = \"{accent}\"\n");
    env.write(
        "home/.local/state/omarchy/current/theme/colors.toml",
        &palette("#12ab34"),
    );
    env.write("home/.local/state/omarchy/current/theme.name", "first\n");
    env.write("home/.local/state/omarchy/current/theme.generation", "1\n");
    let rig = start_in(env, &[("COLORTERM", "truecolor")], FakeOpts::default());
    let first = rig.pty.wait_for_text("oxa-omarchy (first)").unwrap();
    let title = first.cell_of("OXA v").expect("the splash title");
    let before = format!("{:?}", first.cell_of("▄▀▀▄").map(|c| c.fg));
    let _ = title;

    // A half-written file: the old colours stay.
    rig.env.write(
        "home/.local/state/omarchy/current/theme/colors.toml",
        "background = \"#101010\"\nforegr",
    );
    std::thread::sleep(Duration::from_millis(3200));
    let held = rig.pty.screen();
    assert_eq!(
        format!("{:?}", held.cell_of("▄▀▀▄").map(|c| c.fg)),
        before,
        "the old palette is kept"
    );
    assert!(held.contains("oxa-omarchy (first)"));

    // The desktop finishes the switch: new colours and the new name, without a key press.
    rig.env.write(
        "home/.local/state/omarchy/current/theme/colors.toml",
        &palette("#ff2020"),
    );
    rig.env
        .write("home/.local/state/omarchy/current/theme.name", "second\n");
    rig.env
        .write("home/.local/state/omarchy/current/theme.generation", "2\n");
    let after = rig
        .pty
        .wait_for_named(
            "the repaint",
            |s| s.contains("oxa-omarchy (second)"),
            Duration::from_secs(10),
        )
        .unwrap();
    assert_ne!(
        format!("{:?}", after.cell_of("▄▀▀▄").map(|c| c.fg)),
        before,
        "the logo is drawn in the new accent"
    );
}

/// #871: a retry names a bounded reason next to its attempt and delay, in the real UI.
#[test]
fn a_retry_names_its_reason_on_screen() {
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"oxa":{"bootPreload":false}}"#,
    );
    let rig = start_script(
        env,
        &[],
        vec![
            ScriptTurn::fault(Fault::Busy { retry_after: Some(3) }),
            ScriptTurn::text(REPLY),
        ],
        FakeOpts::default(),
    );
    rig.pty.wait_for_text("● READY").unwrap();
    rig.pty.send_text("ping");
    rig.pty.send_key("enter");
    let retrying = rig
        .pty
        .wait_for_named("the retry line", |s| s.contains("RETRY"), WAIT)
        .unwrap();
    let row = retrying
        .find_row("RETRY")
        .map(|r| retrying.row(r))
        .expect("the retry row");
    assert!(row.contains("1/3"), "the attempt and its cap: {row:?}");
    assert!(row.contains("in 3s"), "the wait: {row:?}");
    assert!(row.contains("engine busy"), "the bounded reason: {row:?}");
    assert!(
        !row.contains("queue_full") && !row.contains("Too many requests"),
        "no provider text: {row:?}"
    );
    rig.pty.wait_for_text(REPLY).unwrap();
}

/// #871: a `subagent` call is its own activity, with the child's name on screen.
#[test]
fn a_subagent_call_shows_the_child_on_screen() {
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"oxa":{"bootPreload":false}}"#,
    );
    env.write(
        "home/.oxa/agent/agents/scout.md",
        "---\nname: scout\ndescription: Looks around\ntools: read\n---\nYou are a scout.\n",
    );
    let rig = start_script(
        env,
        &[],
        vec![
            ScriptTurn::tool(
                "sub_1",
                "subagent",
                &json!({"tasks": [{"agent": "scout", "task": "look around"}]}).to_string(),
            ),
            ScriptTurn::text("child done"),
            ScriptTurn::text(REPLY),
        ],
        FakeOpts {
            // The child's request is held, so its progress line stays on screen.
            hold_headers_ms: 2500,
            ..FakeOpts::default()
        },
    );
    rig.pty.wait_for_text("● READY").unwrap();
    rig.pty.send_text("ping");
    rig.pty.send_key("enter");
    let running = rig
        .pty
        .wait_for_named(
            "the subagent line",
            |s| {
                s.find_row("AGENTS")
                    .map(|r| s.row(r).contains("scout"))
                    .unwrap_or(false)
            },
            WAIT,
        )
        .unwrap();
    let row = running
        .find_row("AGENTS")
        .map(|r| running.row(r))
        .expect("the subagent row");
    assert!(row.contains("scout"), "the child's name: {row:?}");
    assert!(
        row.contains("scout: running"),
        "the child and what it does: {row:?}"
    );
    rig.pty.wait_for_text(REPLY).unwrap();
}

/// The process ends cleanly with the palette watch active (there is no watcher thread to
/// stop: the UI loop polls) and the terminal is restored.
#[test]
fn exit_with_the_palette_watch_active() {
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"oxa":{"bootPreload":false}}"#,
    );
    env.write(
        "home/.local/state/omarchy/current/theme/colors.toml",
        "background = \"#101010\"\nforeground = \"#e0e0e0\"\n",
    );
    let mut rig = start_in(env, &[], FakeOpts::default());
    rig.pty.wait_for_text("● READY").unwrap();
    rig.pty.send_key("ctrl+d");
    let status = rig.pty.wait_exit(WAIT).unwrap();
    assert!(status.success());
    assert!(!rig.pty.raw_mode(), "the terminal is back in cooked mode");
}
