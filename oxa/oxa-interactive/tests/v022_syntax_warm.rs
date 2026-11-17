//! V022 (#884): the interactive loop warms the bundled syntax set after the first frame, so the
//! first code block in a session does not pay for loading it.
//!
//! Deliberately one test in its own binary: `oxa_tui`'s syntax set is a process-wide `OnceLock`,
//! so "not loaded at the first frame" is only a meaningful statement when nothing else in the
//! same process can load it first.
//!
//! The terminal samples the flag once per frame (`Terminal::flush` is called exactly once per
//! frame by the renderer), which is what makes the *ordering* checkable rather than just the end
//! state.

use oxa_config::{Dirs, Keymap, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_engine::HttpEngine;
use oxa_fake::{FakeOpts, FakeServer, TestEnv};
use oxa_interactive::{run_session, InteractiveOpts, Reopen};
use oxa_proto::{
    Access, CancelToken, Decision, Engine, ExecResult, ExecSpec, Mode, OxaError, Policy, Sandbox,
};
use oxa_session::FsStore;
use oxa_tui::{InputEvent, Key, TermCaps, TermMode, Terminal, TestTerminal};
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

/// A `TestTerminal` that samples `oxa_tui::syntaxes_loaded()` at every frame and keeps the app
/// running (no input) until the warm-up has run, then quits. No screen-text matching and no
/// sleeping on the test thread: the only thing it waits for is the load itself.
struct WarmTerm {
    inner: TestTerminal,
    /// One sample per rendered frame, in order.
    frames: Vec<bool>,
    /// The quit key is handed to the app once, after the load.
    quit_sent: bool,
    deadline: Instant,
}

impl WarmTerm {
    fn new(cols: u16, rows: u16) -> WarmTerm {
        WarmTerm {
            inner: TestTerminal::new(cols, rows),
            frames: Vec::new(),
            quit_sent: false,
            deadline: Instant::now() + Duration::from_secs(60),
        }
    }
    /// What the app had written by now, escape sequences removed (for failure messages only).
    fn text(&self) -> String {
        oxa_tui::strip_ansi(&String::from_utf8_lossy(&self.inner.out))
    }
}

impl Terminal for WarmTerm {
    fn write(&mut self, bytes: &[u8]) {
        self.inner.write(bytes);
    }
    fn flush(&mut self) {
        self.inner.flush();
        self.frames.push(oxa_tui::syntaxes_loaded());
    }
    fn size(&self) -> (u16, u16) {
        self.inner.size()
    }
    fn poll(&mut self, timeout_ms: u64) -> Vec<InputEvent> {
        if self.quit_sent {
            std::thread::sleep(Duration::from_millis(timeout_ms.min(5)));
            return Vec::new();
        }
        if oxa_tui::syntaxes_loaded() {
            self.quit_sent = true;
            return vec![InputEvent::Key(Key::press("ctrl+d"))];
        }
        assert!(
            Instant::now() < self.deadline,
            "the syntax set was never warmed after the first frame; frames so far: {:?}\n{}",
            self.frames,
            self.text()
        );
        std::thread::sleep(Duration::from_millis(2));
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

fn deps(env: &TestEnv, url: &str) -> Deps {
    let engine: Arc<dyn Engine> = Arc::new(HttpEngine::new(url, None).expect("engine client"));
    Deps {
        engine,
        store: Arc::new(FsStore::new(Dirs::under(env.root()).sessions_dir.clone())),
        tools: oxa_tools::builtin_tools(),
        hooks: Vec::new(),
        policy: Box::new(AllowAll),
        sandbox: Arc::new(NoBash),
        settings: Settings::from_values(
            serde_json::json!({"oxa": {"envNote": false}}),
            serde_json::json!({}),
        ),
        dirs: Dirs::under(env.root()),
        cwd: env.work(),
        selection: ToolSelection::default(),
        resources: oxa_resources::Resources::default(),
        model: None,
        thinking: None,
        mode: None,
        session_id: None,
        jobs: oxa_core::fresh_jobs(),
    }
}

#[test]
fn v022_syntax_set_is_warmed_after_the_first_frame() {
    let env = TestEnv::new();
    let server = FakeServer::start(vec![], FakeOpts::default()).expect("start the fake engine");
    let url = server.url();
    let store = Arc::new(FsStore::new(Dirs::under(env.root()).sessions_dir.clone()));
    let root = env.root().to_path_buf();
    let work = env.work();

    let session = Session::open(deps(&env, &url), Open::New).expect("open the session");
    let mut opts = InteractiveOpts {
        fullscreen: false,
        dirs: Dirs::under(env.root()),
        settings: Settings::from_values(
            serde_json::json!({"oxa": {"envNote": false}}),
            serde_json::json!({}),
        ),
        keymap: Keymap::defaults(),
        store: store.clone(),
        hooks: Vec::new(),
        mcp: None,
        initial_prompt: None,
        initial_parts: Vec::new(),
        initial_follow_ups: Vec::new(),
        theme: None,
        notices: Vec::new(),
        env: vec![("OXA_SKIN".to_string(), "pi".to_string())],
        caps: Some(TermCaps::BASIC),
        reopen: Box::new(move |how, _carry| {
            let open = match how {
                Reopen::New => Open::New,
                Reopen::Resume(sref) => Open::Resume(sref),
                Reopen::Fork(sref) => Open::Fork(sref),
            };
            let mut d = deps(&env, &url);
            d.engine = Arc::new(HttpEngine::new(&url, None)?);
            d.store = store.clone();
            d.dirs = Dirs::under(&root);
            d.cwd = work.clone();
            Session::open(d, open)
        }),
        open_picker: false,
        save_trust: None,
        gated: Vec::new(),
        yolo: None,
    };

    // The premise: nothing has loaded the set yet (this is a fresh process, one test).
    assert!(
        !oxa_tui::syntaxes_loaded(),
        "the syntax set must load lazily, not at startup"
    );

    let mut term = WarmTerm::new(90, 30);
    let (code, last) = run_session(&mut term, session, &mut opts);
    assert_eq!(code, 0);
    assert!(last.is_some(), "the worker ended cleanly");

    let first = term.frames.first().copied().expect("the app drew a first frame");
    assert!(
        !first,
        "the first editable frame must not wait for the syntax set (frames: {:?})",
        term.frames
    );
    assert!(
        term.frames.iter().any(|loaded| *loaded),
        "the loop must warm the syntax set after the first frame (frames: {:?})",
        term.frames
    );
    assert!(
        oxa_tui::syntaxes_loaded(),
        "the set stays loaded after the warm-up"
    );
    // The warm-up loaded the same, bundled set: a highlight on it still produces lines.
    let lines = oxa_tui::highlight_code("fn main() {}", Some("rust"), &oxa_tui::Theme::plain());
    assert!(!lines.is_empty(), "highlighting works on the warmed set");
}
