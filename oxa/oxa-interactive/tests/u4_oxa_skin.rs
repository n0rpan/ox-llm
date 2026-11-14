//! U4: OXA's look, end to end. The real app (`run_session`: worker thread + UI loop) with a
//! real `Session` against `oxa-fake` over HTTP on a scripted `TestTerminal`: OXA is the
//! default skin, `ui.skin` overrides it, the HUD / hint bar / panels reach the screen, a resize
//! keeps every line inside the terminal, and `NO_COLOR` writes no colour.

use oxa_config::{Dirs, Keymap, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_engine::HttpEngine;
use oxa_fake::{FakeOpts, FakeServer, Fault, ScriptTurn, TestEnv};
use oxa_interactive::{run_session, InteractiveOpts, Reopen};
use oxa_proto::{
    Access, CancelToken, Decision, Engine, ExecResult, ExecSpec, Mode, OxaError, Policy, Sandbox,
};
use oxa_resources::Resources;
use oxa_session::{FsStore, SessionStore};
use oxa_tui::{strip_ansi, visible_width, InputEvent, Key, TermCaps, TermMode, Terminal, TestTerminal};
use serde_json::json;
use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

struct AllowAll;

impl Policy for AllowAll {
    fn decide(&self, _mode: Mode, _a: &Access, _cwd: &Path) -> Decision {
        Decision::Allow
    }
    fn remember(&mut self, _a: &Access) {}
}

struct NoBash;

impl Sandbox for NoBash {
    fn check(&self) -> Result<(), OxaError> {
        Ok(())
    }
    fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
        Vec::new()
    }
    fn run(&self, _s: &ExecSpec, _c: &CancelToken, _live: &mut dyn FnMut(&[u8])) -> ExecResult {
        ExecResult {
            output: Vec::new(),
            exit: Some(0),
            timed_out: false,
        }
    }
}

/// Wait until the text written since the previous step holds every string of `wait`, then
/// resize the terminal (when asked) and send `events`.
struct Step {
    name: &'static str,
    wait: Vec<&'static str>,
    resize: Option<(u16, u16)>,
    events: Vec<InputEvent>,
}

fn step(name: &'static str, wait: &[&'static str], events: Vec<InputEvent>) -> Step {
    Step {
        name,
        wait: wait.to_vec(),
        resize: None,
        events,
    }
}

struct ScriptTerm {
    inner: TestTerminal,
    steps: VecDeque<Step>,
    since: String,
    /// Raw output written between step `name` firing and the next step firing.
    raw_after: Vec<(&'static str, usize)>,
    consumed: usize,
    deadline: Instant,
}

impl ScriptTerm {
    fn new(cols: u16, rows: u16, steps: Vec<Step>) -> ScriptTerm {
        ScriptTerm {
            inner: TestTerminal::new(cols, rows),
            steps: steps.into(),
            since: String::new(),
            raw_after: Vec::new(),
            consumed: 0,
            deadline: Instant::now() + Duration::from_secs(60),
        }
    }
    fn raw(&self) -> String {
        String::from_utf8_lossy(&self.inner.out).into_owned()
    }
    fn all_text(&self) -> String {
        strip_ansi(&self.raw())
    }
    /// The raw bytes written after step `name` fired.
    fn raw_since(&self, name: &str) -> String {
        let at = self
            .raw_after
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, at)| *at)
            .expect("the step fired");
        String::from_utf8_lossy(&self.inner.out[at..]).into_owned()
    }
}

impl Terminal for ScriptTerm {
    fn write(&mut self, bytes: &[u8]) {
        self.inner.write(bytes);
    }
    fn flush(&mut self) {
        self.inner.flush();
    }
    fn size(&self) -> (u16, u16) {
        self.inner.size()
    }
    fn poll(&mut self, timeout_ms: u64) -> Vec<InputEvent> {
        if self.inner.out.len() > self.consumed {
            let fresh = String::from_utf8_lossy(&self.inner.out[self.consumed..]).into_owned();
            self.consumed = self.inner.out.len();
            self.since.push_str(&strip_ansi(&fresh));
        }
        let ready = self
            .steps
            .front()
            .is_some_and(|s| s.wait.iter().all(|w| self.since.contains(w)));
        if ready {
            let s = self.steps.pop_front().expect("a step is ready");
            self.since.clear();
            self.raw_after.push((s.name, self.inner.out.len()));
            let mut events = Vec::new();
            if let Some((cols, rows)) = s.resize {
                self.inner.resize(cols, rows);
                events.push(InputEvent::Resize { cols, rows });
            }
            events.extend(s.events);
            return events;
        }
        if Instant::now() > self.deadline {
            let name = self.steps.front().map(|s| s.name).unwrap_or("(no quit)");
            let tail: String = self
                .since
                .chars()
                .rev()
                .take(2000)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            panic!("the script timed out at step `{name}`; text since the last step:\n{tail}");
        }
        std::thread::sleep(Duration::from_millis(timeout_ms.min(5)));
        Vec::new()
    }
    fn caps(&self) -> TermCaps {
        self.inner.caps()
    }
    fn set_mode(&mut self, mode: TermMode, on: bool) {
        self.inner.set_mode(mode, on);
    }
    fn suspend(&mut self) {
        self.inner.suspend();
    }
    fn resume(&mut self) {
        self.inner.resume();
    }
}

fn key(id: &str) -> InputEvent {
    InputEvent::Key(Key::press(id))
}

fn typed(text: &str, then: &[&str]) -> Vec<InputEvent> {
    let mut out: Vec<InputEvent> = text.chars().map(|c| InputEvent::Key(Key::char(c))).collect();
    out.extend(then.iter().map(|id| key(id)));
    out
}

fn deps(engine: Arc<dyn Engine>, store: Arc<dyn SessionStore>, root: &Path, work: &Path) -> Deps {
    Deps {
        engine,
        store,
        tools: oxa_tools::builtin_tools(),
        hooks: Vec::new(),
        policy: Box::new(AllowAll),
        sandbox: Arc::new(NoBash),
        settings: Settings::default(),
        dirs: Dirs::under(root),
        cwd: work.to_path_buf(),
        selection: ToolSelection::default(),
        resources: Resources::default(),
        model: None,
        thinking: None,
        mode: None,
        session_id: None,
    }
}

/// Run the app on a `cols` x `rows` terminal with the given settings and environment.
fn run(
    script: Vec<ScriptTurn>,
    settings: serde_json::Value,
    env_vars: &[(&str, &str)],
    size: (u16, u16),
    steps: Vec<Step>,
    prepare: impl FnOnce(&TestEnv),
) -> ScriptTerm {
    let env = TestEnv::new();
    prepare(&env);
    let server = FakeServer::start(script, FakeOpts::default()).expect("start the fake engine");
    let url = server.url();
    let dirs = Dirs::under(env.root());
    let store: Arc<dyn SessionStore> = Arc::new(FsStore::new(dirs.sessions_dir.clone()));
    let engine: Arc<dyn Engine> = Arc::new(HttpEngine::new(&url, None).expect("engine client"));
    let session =
        Session::open(deps(engine, store.clone(), env.root(), &env.work()), Open::New).expect("open");
    let (root, work, store2) = (env.root().to_path_buf(), env.work(), store.clone());
    let mut opts = InteractiveOpts {
        fullscreen: true,
        dirs,
        settings: Settings::from_values(settings, json!({})),
        keymap: Keymap::defaults(),
        store,
        hooks: Vec::new(),
        mcp: None,
        initial_prompt: None,
        initial_parts: Vec::new(),
        initial_follow_ups: Vec::new(),
        theme: None,
        notices: Vec::new(),
        env: env_vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        caps: Some(TermCaps::BASIC),
        reopen: Box::new(move |how| {
            let open = match how {
                Reopen::New => Open::New,
                Reopen::Resume(sref) => Open::Resume(sref),
                Reopen::Fork(sref) => Open::Fork(sref),
            };
            let engine: Arc<dyn Engine> = Arc::new(HttpEngine::new(&url, None)?);
            Session::open(deps(engine, store2.clone(), &root, &work), open)
        }),
        open_picker: false,
        save_trust: None,
        gated: Vec::new(),
        yolo: Some(oxa_proto::YoloSwitch::new(false)),
    };
    let mut term = ScriptTerm::new(size.0, size.1, steps);
    let (code, _) = run_session(&mut term, session, &mut opts);
    assert_eq!(code, 0);
    assert!(term.steps.is_empty(), "the app quit before the script ended");
    term
}

fn read_then_answer() -> Vec<ScriptTurn> {
    vec![
        ScriptTurn::tool("call_1", "read", r#"{"path": "notes.txt"}"#),
        ScriptTurn::text("ALPHA done.\n"),
    ]
}

fn write_notes(env: &TestEnv) {
    std::fs::write(env.work().join("notes.txt"), "first line\nsecond line\n").expect("notes");
}

const UTF8: &[(&str, &str)] = &[("LANG", "C.UTF-8")];

/// The colour-selecting SGR parameters in `raw` (30-38, 40-48, 90-97, 100-107).
fn colour_params(raw: &str) -> Vec<u32> {
    let mut found = Vec::new();
    let mut rest = raw;
    while let Some(at) = rest.find("\x1b[") {
        rest = &rest[at + 2..];
        let end = rest
            .find(|c: char| !(c.is_ascii_digit() || c == ';' || c == ':'))
            .unwrap_or(rest.len());
        if rest[end..].starts_with('m') {
            for p in rest[..end]
                .split([';', ':'])
                .filter_map(|p| p.parse::<u32>().ok())
            {
                if matches!(p, 30..=38 | 40..=48 | 90..=97 | 100..=107) {
                    found.push(p);
                }
            }
        }
        rest = &rest[end..];
    }
    found
}

#[test]
fn colour_params_finds_colours_only() {
    assert_eq!(
        colour_params("\x1b[1mx\x1b[22m\x1b[39m\x1b[0m\x1b[2J\x1b[7m"),
        Vec::<u32>::new()
    );
    assert_eq!(colour_params("\x1b[38;5;4mx\x1b[48;2;1;2;3m"), vec![38, 48]);
}

#[test]
fn the_oxa_skin_draws_chrome_and_panels() {
    let term = run(
        read_then_answer(),
        json!({}),
        UTF8,
        (110, 34),
        vec![
            step(
                "start",
                &["READY", "enter send"],
                typed("look at notes.txt", &["enter"]),
            ),
            step("answered", &["ALPHA done."], vec![key("ctrl+d")]),
        ],
        write_notes,
    );
    let text = term.all_text();
    // The splash: logo, identity and the suite, once and subtly.
    assert!(text.contains("OXA v"), "splash title");
    assert!(text.contains("▄▀▀▄ ▀▄ ▄▀ ▄▀▀▄"), "logo");
    assert!(text.contains("part of ox-llm"));
    // The HUD: title, labelled meters, state.
    assert!(text.contains(" OXA │ "), "HUD title");
    assert!(text.contains("CTX "), "context meter");
    assert!(text.contains("● READY"));
    // The hint bar teaches the keys of the moment.
    assert!(text.contains("enter send"));
    assert!(text.contains("/ commands"));
    assert!(
        text.contains("escape cancel"),
        "the running hints name the cancel key"
    );
    // The tool call is a framed panel with a status mark; the prompt has its rail.
    assert!(text.contains("┌─ ✓ read notes.txt"), "tool panel title");
    assert!(text.contains("│ first line"), "tool panel body");
    assert!(text.contains("▌ look at notes.txt"), "user rail");
    // pi's chrome is not drawn.
    assert!(!text.contains("Working..."));
    // The window title says OXA.
    assert!(term.raw().contains("OXA - "), "window title");
}

#[test]
fn ui_skin_pi_keeps_the_plain_look() {
    let term = run(
        read_then_answer(),
        json!({"ui": {"skin": "pi"}}),
        &[("LANG", "C.UTF-8")],
        (110, 34),
        vec![
            step("start", &["oxa v"], typed("look at notes.txt", &["enter"])),
            step("answered", &["ALPHA done."], vec![key("ctrl+d")]),
        ],
        write_notes,
    );
    let text = term.all_text();
    assert!(!text.contains("OXA"), "no OXA chrome under ui.skin = pi");
    assert!(!text.contains("READY"));
    assert!(!text.contains("┌─"));
    assert!(!text.contains("▌"));
    assert!(text.contains("read notes.txt"));
    assert!(term.raw().contains("oxa - "), "pi's window title");
}

#[test]
fn ui_skin_selects_the_look() {
    let plain = run(
        vec![ScriptTurn::text("ALPHA done.\n")],
        json!({"ui": {"skin": "pi"}}),
        UTF8,
        (100, 30),
        vec![
            step("start", &["oxa v"], typed("hello", &["enter"])),
            step("answered", &["ALPHA done."], vec![key("ctrl+d")]),
        ],
        |_| {},
    );
    let text = plain.all_text();
    assert!(!text.contains("READY") && !text.contains("OXA v"));

    let oxa = run(
        vec![ScriptTurn::text("ALPHA done.\n")],
        json!({"ui": {"skin": "oxa"}}),
        &[("LANG", "C.UTF-8")],
        (100, 30),
        vec![
            step("start", &["READY"], typed("hello", &["enter"])),
            step("answered", &["ALPHA done."], vec![key("ctrl+d")]),
        ],
        |_| {},
    );
    assert!(oxa.all_text().contains(" OXA │ "));
}

#[test]
fn resize_redraws_the_chrome_for_the_new_width() {
    // The width rules themselves are checked on the pure functions (u2, u3): here the app
    // must redraw with the new size: a narrow HUD without the dropped segments, then the
    // wide one again.
    let mut narrow = step("narrow", &["ALPHA done."], Vec::new());
    narrow.resize = Some((48, 20));
    let mut wide = step("wide", &["OXA", "CTX"], Vec::new());
    wide.resize = Some((160, 40));
    let term = run(
        read_then_answer(),
        json!({}),
        UTF8,
        (110, 34),
        vec![
            step("start", &["READY"], typed("look at notes.txt", &["enter"])),
            narrow,
            wide,
            step("quit", &["part of ox-llm", "workspace"], vec![key("ctrl+d")]),
        ],
        write_notes,
    );
    let after_narrow = term.raw_since("narrow");
    let until_wide = strip_ansi(&after_narrow[..after_narrow.len() - term.raw_since("wide").len()]);
    assert!(
        until_wide.contains("┌─ ✓ read notes.txt"),
        "the panels are redrawn narrow"
    );
    assert!(
        !until_wide.contains("part of ox-llm"),
        "the narrow splash drops the suite note"
    );
    assert!(!until_wide.contains("tok/s"), "the narrow HUD drops the speed");
}

#[test]
fn no_color_writes_no_colour_and_stays_readable() {
    let term = run(
        read_then_answer(),
        json!({}),
        &[("LANG", "C.UTF-8"), ("NO_COLOR", "1")],
        (110, 34),
        vec![
            step("start", &["READY"], typed("look at notes.txt", &["enter"])),
            step("answered", &["ALPHA done."], vec![key("ctrl+d")]),
        ],
        write_notes,
    );
    let raw = term.raw();
    assert_eq!(
        colour_params(&raw),
        Vec::<u32>::new(),
        "colour was written under NO_COLOR"
    );
    let text = term.all_text();
    assert!(text.contains("OXA") && text.contains("READY") && text.contains("┌─ ✓ read notes.txt"));
}

#[test]
fn ascii_locale_draws_ascii_frames() {
    let term = run(
        read_then_answer(),
        json!({}),
        &[("LANG", "C")],
        (110, 34),
        vec![
            step("start", &["READY"], typed("look at notes.txt", &["enter"])),
            step("answered", &["ALPHA done."], vec![key("ctrl+d")]),
        ],
        write_notes,
    );
    let text = term.all_text();
    assert!(text.contains("+- ok read notes.txt"), "ASCII tool panel");
    assert!(text.contains(" OXA | "), "ASCII HUD");
    for glyph in ["┌", "│", "═", "█", "▌", "●"] {
        assert!(
            !text.contains(glyph),
            "{glyph} was drawn under a non-UTF-8 locale"
        );
    }
}

#[test]
fn omarchy_palette_colours_the_ui_and_falls_back_without_one() {
    // With a palette file in the state directory the accent colour of that palette is used.
    let with = run(
        vec![ScriptTurn::text("ALPHA done.\n")],
        json!({}),
        UTF8,
        (110, 30),
        vec![
            step("start", &["READY"], typed("hello", &["enter"])),
            step("answered", &["ALPHA done."], vec![key("ctrl+d")]),
        ],
        |env| {
            let dir = env.home().join(".local/state/omarchy/current/theme");
            std::fs::create_dir_all(&dir).expect("theme dir");
            std::fs::write(
                dir.join("colors.toml"),
                "background = \"#101010\"\nforeground = \"#e0e0e0\"\naccent = \"#12ab34\"\n",
            )
            .expect("palette");
            std::fs::write(dir.join("../theme.name"), "test-theme\n").expect("name");
        },
    );
    assert!(
        with.all_text().contains("oxa-omarchy (test-theme)"),
        "the splash names the desktop theme"
    );
    // TermCaps::BASIC is 256 colours: the accent is its nearest palette index.
    let accent = oxa_tui::rgb_to_256(0x12, 0xab, 0x34);
    assert!(
        with.raw().contains(&format!("\x1b[38;5;{accent}m")),
        "the palette's accent is used"
    );

    // Without a palette the UI still starts (the `system` theme).
    let without = run(
        vec![ScriptTurn::text("ALPHA done.\n")],
        json!({}),
        UTF8,
        (110, 30),
        vec![
            step("start", &["READY"], typed("hello", &["enter"])),
            step("answered", &["ALPHA done."], vec![key("ctrl+d")]),
        ],
        |_| {},
    );
    assert!(without.all_text().contains(" OXA │ "));
}

#[test]
fn engine_error_is_one_error_panel() {
    let term = run(
        vec![ScriptTurn::fault(Fault::Http {
            status: 400,
            body: r#"{"error":{"message":"model is not loaded","type":"invalid_request_error","code":400}}"#
                .to_string(),
        })],
        json!({}),
        UTF8,
        (110, 30),
        vec![
            step("start", &["READY"], typed("hello", &["enter"])),
            step(
                "failed",
                &["ERROR", "model is not loaded", "ask again to retry"],
                vec![key("ctrl+d")],
            ),
        ],
        |_| {},
    );
    let text = term.all_text();
    assert!(text.contains("┌─ ✗ ERROR"));
    assert!(text.contains("not sent to the model; ask again"));
}

#[test]
fn selector_is_framed_and_the_hint_bar_shows_its_keys() {
    let term = run(
        Vec::new(),
        json!({}),
        UTF8,
        (100, 30),
        vec![
            step("start", &["READY"], typed("/theme", &["enter"])),
            step("open", &["oxa-omarchy", "enter select"], vec![key("escape")]),
            step("closed", &["enter send"], vec![key("ctrl+d")]),
        ],
        |_| {},
    );
    let text = term.all_text();
    assert!(
        text.contains("╔═") && text.contains("╚═"),
        "the selector has a frame"
    );
    assert!(text.contains("escape close"));
}

#[test]
fn yolo_toggle_shows_the_flame_the_banner_and_goes_away() {
    let term = run(
        Vec::new(),
        json!({}),
        UTF8,
        (120, 30),
        vec![
            step("start", &["READY"], typed("/yolo", &["enter"])),
            step("on", &["YOLO MODE", "🔥  YOLO  0"], typed("/yolo", &["enter"])),
            step("off", &["YOLO off"], vec![key("ctrl+d")]),
        ],
        |_| {},
    );
    let text = term.all_text();
    assert!(
        text.contains("the sandbox and the deny-list stay on"),
        "the banner says what stays"
    );
    assert!(
        text.contains("YOLO on: no permission prompts"),
        "a notice stays in the transcript"
    );
    // Nothing drawn after it was switched off carries the flame.
    assert!(
        !strip_ansi(&term.raw_since("off")).contains("🔥"),
        "the flame is gone after /yolo off"
    );
}

#[test]
fn yolo_key_toggles_and_no_color_shows_a_text_badge() {
    let term = run(
        Vec::new(),
        json!({}),
        &[("LANG", "C.UTF-8"), ("NO_COLOR", "1")],
        (120, 30),
        vec![
            step("start", &["READY"], vec![key("ctrl+alt+o")]),
            step("on", &["[YOLO MODE]", "[YOLO] 0"], vec![key("ctrl+d")]),
        ],
        |_| {},
    );
    assert_eq!(
        colour_params(&term.raw()),
        Vec::<u32>::new(),
        "no colour under NO_COLOR"
    );
    assert!(
        !term.all_text().contains("▁▃"),
        "no flame animation without colour"
    );
}

#[test]
fn yolo_under_the_plain_look_is_a_footer_status() {
    let term = run(
        Vec::new(),
        json!({"ui": {"skin": "pi"}}),
        &[("LANG", "C.UTF-8")],
        (100, 30),
        vec![
            step("start", &["oxa v"], typed("/yolo", &["enter"])),
            step("on", &["YOLO on", "YOLO"], vec![key("ctrl+d")]),
        ],
        |_| {},
    );
    let text = term.all_text();
    assert!(
        !text.contains("YOLO MODE") && !text.contains("🔥"),
        "the plain look gets no banner"
    );
}

#[test]
fn hud_counts_auto_approved_actions_and_reduced_motion_stills_the_flames() {
    use oxa_interactive::{oxa_hud, oxa_yolo_banner, Skin, StatusModel};
    let theme = oxa_tui::builtin_theme("dark", oxa_tui::ColorMode::TrueColor, None).expect("dark");
    let st = StatusModel {
        model: "m".to_string(),
        yolo: true,
        yolo_approved: 12,
        ..StatusModel::default()
    };
    for width in 20..=160u16 {
        let line = oxa_hud(&st, width, &theme, Skin::Oxa);
        assert!(visible_width(&line) <= width as usize);
        if width >= 40 {
            assert!(
                strip_ansi(&line).contains("YOLO  12"),
                "the count is never dropped ({width})"
            );
        }
    }
    let ascii = oxa_hud(&st, 100, &oxa_tui::Theme::plain(), Skin::OxaAscii);
    assert!(ascii.contains("^ [YOLO] 12") && ascii.is_ascii());
    let off = oxa_hud(&StatusModel::default(), 100, &theme, Skin::Oxa);
    assert!(!strip_ansi(&off).contains("YOLO"));

    let moving = strip_ansi(&oxa_yolo_banner(0, true, 120, &theme, Skin::Oxa));
    let later = strip_ansi(&oxa_yolo_banner(240, true, 120, &theme, Skin::Oxa));
    assert!(moving.contains("YOLO MODE") && moving != later, "the flames move");
    let still = oxa_yolo_banner(0, false, 120, &theme, Skin::Oxa);
    assert_eq!(
        still,
        oxa_yolo_banner(240, false, 120, &theme, Skin::Oxa),
        "reduced motion"
    );
    for width in 1..=200u16 {
        assert!(visible_width(&oxa_yolo_banner(360, true, width, &theme, Skin::Oxa)) <= width as usize);
    }
}
