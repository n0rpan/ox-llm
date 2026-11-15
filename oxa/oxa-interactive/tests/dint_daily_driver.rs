//! D-INT acceptance tests: the daily driver, end to end. Owner: the architect.
//!
//! Every test runs the real app (`run_session`: worker thread + UI loop) with a real `Session`
//! against `oxa-fake` over HTTP, the built-in tools and a file session store. Input is a script
//! on an `oxa_tui::TestTerminal`: each step waits until the screen shows something, then sends
//! keys. Nothing here needs a real terminal, a model or the real home directory.

use oxa_config::{Dirs, Keymap, Settings};
use oxa_core::{Deps, Handle, Open, Session, ToolSelection};
use oxa_engine::HttpEngine;
use oxa_fake::{check_all, FakeOpts, FakeServer, Fault, Recorded, ScriptTurn, TestEnv};
use oxa_interactive::{run_session, InteractiveOpts, Reopen, SessionInfo};
use oxa_proto::{
    Access, CancelToken, Decision, Engine, ExecResult, ExecSpec, Mode, OxaError, Policy, Role, Sandbox,
};
use oxa_resources::Resources;
use oxa_session::{FsStore, SessionStore};
use oxa_tui::{
    strip_ansi, InputEvent, Key, Mouse, MouseButton, MouseKind, TermCaps, TermMode, Terminal, TestTerminal,
};
use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------------------------

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

/// One step of a script: wait until the text written to the terminal since the previous step
/// contains every string of `wait` (escape sequences removed) and `until` (if any) holds, then
/// send `events`.
struct Step {
    name: &'static str,
    wait: Vec<&'static str>,
    until: Option<Box<dyn Fn() -> bool>>,
    events: Vec<InputEvent>,
}

fn step(name: &'static str, wait: &[&'static str], events: Vec<InputEvent>) -> Step {
    Step {
        name,
        wait: wait.to_vec(),
        until: None,
        events,
    }
}

impl Step {
    /// Also wait until `cond` holds (a state the screen does not show, such as what the fake
    /// engine has received).
    fn until(mut self, cond: impl Fn() -> bool + 'static) -> Step {
        self.until = Some(Box::new(cond));
        self
    }

    /// Whether this step is ready to fire, given the text accumulated since the last step.
    fn is_ready(&self, since: &str) -> bool {
        self.wait.iter().all(|w| since.contains(w)) && self.until.as_ref().is_none_or(|f| f())
    }
}

/// A `TestTerminal` whose input is a script of [`Step`]s. `poll` waits (it does not return a
/// step's keys before the screen shows what the step waits for), so the test follows the real
/// timing of the worker thread and the HTTP engine without sleeping blindly.
struct ScriptTerm {
    inner: TestTerminal,
    steps: VecDeque<Step>,
    /// Text written since the last step fired, escape sequences removed.
    since: String,
    /// What each fired step had seen: `(step name, text)`.
    seen: Vec<(&'static str, String)>,
    consumed: usize,
    deadline: Instant,
    /// A barrier for race tests: once the script has ended, every `flush` waits (bounded)
    /// until this returns `true`. The app flushes between its last input batch and its
    /// shutdown, so the gate holds the quit back until the worker has reached a chosen point.
    gate: Option<Box<dyn Fn() -> bool>>,
}

impl ScriptTerm {
    fn new(cols: u16, rows: u16, steps: Vec<Step>) -> ScriptTerm {
        ScriptTerm {
            inner: TestTerminal::new(cols, rows),
            steps: steps.into(),
            since: String::new(),
            seen: Vec::new(),
            consumed: 0,
            deadline: Instant::now() + Duration::from_secs(60),
            gate: None,
        }
    }
    fn absorb(&mut self) {
        if self.inner.out.len() > self.consumed {
            let fresh = String::from_utf8_lossy(&self.inner.out[self.consumed..]).into_owned();
            self.consumed = self.inner.out.len();
            self.since.push_str(&strip_ansi(&fresh));
        }
    }
    /// The text a fired step had waited on.
    fn seen(&self, name: &str) -> &str {
        self.seen
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, t)| t.as_str())
            .unwrap_or("")
    }
    /// Everything written, escape sequences removed.
    fn all_text(&self) -> String {
        strip_ansi(&String::from_utf8_lossy(&self.inner.out))
    }
}

impl Terminal for ScriptTerm {
    fn write(&mut self, bytes: &[u8]) {
        self.inner.write(bytes);
    }
    fn flush(&mut self) {
        self.inner.flush();
        if self.steps.is_empty() {
            if let Some(gate) = self.gate.take() {
                let until = Instant::now() + Duration::from_secs(10);
                while !gate() {
                    assert!(Instant::now() < until, "the barrier was never reached");
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
        }
    }
    fn size(&self) -> (u16, u16) {
        self.inner.size()
    }
    fn poll(&mut self, timeout_ms: u64) -> Vec<InputEvent> {
        self.absorb();
        let ready = self.steps.front().is_some_and(|s| s.is_ready(&self.since));
        if ready {
            let s = self.steps.pop_front().expect("a step is ready");
            self.seen.push((s.name, std::mem::take(&mut self.since)));
            return s.events;
        }
        if Instant::now() > self.deadline {
            let name = self
                .steps
                .front()
                .map(|s| s.name)
                .unwrap_or("(script ended without a quit)");
            let tail: String = self
                .since
                .chars()
                .rev()
                .take(1500)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            let mut history = String::new();
            for (step, text) in &self.seen {
                let end: String = text
                    .chars()
                    .rev()
                    .take(400)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                history.push_str(&format!("--- step `{step}` fired after seeing (tail):\n{end}\n"));
            }
            panic!("the script timed out at step `{name}`\n{history}--- text since the last step:\n{tail}");
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

/// The keys that type `text`, then the extra keys.
fn typed(text: &str, then: &[&str]) -> Vec<InputEvent> {
    let mut out: Vec<InputEvent> = text.chars().map(|c| InputEvent::Key(Key::char(c))).collect();
    out.extend(then.iter().map(|id| key(id)));
    out
}

fn wheel(up: bool) -> InputEvent {
    InputEvent::Mouse(Mouse {
        kind: if up {
            MouseKind::WheelUp
        } else {
            MouseKind::WheelDown
        },
        button: MouseButton::None,
        col: 5,
        row: 2,
        shift: false,
        alt: false,
        ctrl: false,
    })
}

struct World {
    env: TestEnv,
    server: Arc<FakeServer>,
    store: Arc<dyn SessionStore>,
}

impl World {
    fn new(script: Vec<ScriptTurn>) -> World {
        World::with(script, FakeOpts::default())
    }

    fn with(script: Vec<ScriptTurn>, fake: FakeOpts) -> World {
        let env = TestEnv::new();
        let server = Arc::new(FakeServer::start(script, fake).expect("start the fake engine"));
        let dirs = Dirs::under(env.root());
        let store: Arc<dyn SessionStore> = Arc::new(FsStore::new(dirs.sessions_dir.clone()));
        World { env, server, store }
    }

    fn deps(&self) -> Deps {
        deps(&self.server.url(), self.store.clone(), &self.env)
    }

    fn opts(&self, fullscreen: bool) -> InteractiveOpts {
        let url = self.server.url();
        let store = self.store.clone();
        let root = self.env.root().to_path_buf();
        let work = self.env.work();
        InteractiveOpts {
            fullscreen,
            dirs: Dirs::under(self.env.root()),
            // Exact prompt-byte fixtures select note-free queues/RPC/replay.
            settings: Settings::from_values(
                serde_json::json!({"oxa": {"envNote": false}}),
                serde_json::json!({}),
            ),
            keymap: Keymap::defaults(),
            store: self.store.clone(),
            hooks: Vec::new(),
            mcp: None,
            initial_prompt: None,
            initial_parts: Vec::new(),
            initial_follow_ups: Vec::new(),
            theme: None,
            notices: Vec::new(),
            // These tests exercise behaviour (streams, errors, keys), not the skin: pin the
            // plain skin so the assertions on the header stay stable (OXA is the default).
            env: vec![("OXA_SKIN".to_string(), "pi".to_string())],
            caps: Some(TermCaps::BASIC),
            reopen: Box::new(move |how, _carry| {
                let open = match how {
                    Reopen::New => Open::New,
                    Reopen::Resume(sref) => Open::Resume(sref),
                    Reopen::Fork(sref) => Open::Fork(sref),
                };
                let engine: Arc<dyn Engine> = Arc::new(HttpEngine::new(&url, None)?);
                Session::open(deps_with(engine, store.clone(), &root, &work), open)
            }),
            open_picker: false,
            save_trust: None,
            gated: Vec::new(),
            yolo: None,
        }
    }

    fn requests(&self) -> Vec<Recorded> {
        self.server.requests()
    }
}

fn deps(url: &str, store: Arc<dyn SessionStore>, env: &TestEnv) -> Deps {
    let engine: Arc<dyn Engine> = Arc::new(HttpEngine::new(url, None).expect("engine client"));
    deps_with(engine, store, env.root(), &env.work())
}

fn deps_with(engine: Arc<dyn Engine>, store: Arc<dyn SessionStore>, root: &Path, work: &Path) -> Deps {
    Deps {
        engine,
        store,
        tools: oxa_tools::builtin_tools(),
        hooks: Vec::new(),
        policy: Box::new(AllowAll),
        sandbox: Arc::new(NoBash),
        // Exact prompt-byte fixtures select note-free queues/RPC/replay.
        settings: Settings::from_values(
            serde_json::json!({"oxa": {"envNote": false}}),
            serde_json::json!({}),
        ),
        dirs: Dirs::under(root),
        cwd: work.to_path_buf(),
        selection: ToolSelection::default(),
        resources: Resources::default(),
        model: None,
        thinking: None,
        mode: None,
        session_id: None,
        jobs: oxa_core::fresh_jobs(),
    }
}

/// Run the app until the script quits. Returns the exit code, the session the worker ended
/// with, the terminal and the control handle of the FIRST session.
fn run_app(
    world: &World,
    fullscreen: bool,
    size: (u16, u16),
    steps: Vec<Step>,
) -> (i32, Option<SessionInfo>, ScriptTerm, Handle) {
    let session = Session::open(world.deps(), Open::New).expect("open the first session");
    run_with(world, session, world.opts(fullscreen), size, steps)
}

fn run_with(
    _world: &World,
    session: Session,
    mut opts: InteractiveOpts,
    size: (u16, u16),
    steps: Vec<Step>,
) -> (i32, Option<SessionInfo>, ScriptTerm, Handle) {
    let first = session.handle();
    let mut term = ScriptTerm::new(size.0, size.1, steps);
    let (code, last) = run_session(&mut term, session, &mut opts);
    assert!(
        term.steps.is_empty(),
        "the app quit before the script ended: {:?} left",
        term.steps.len()
    );
    (code, last, term, first)
}

/// The chat requests (not the zero-token prefill ones) of one session, in order.
fn chats<'a>(reqs: &'a [Recorded], session: &str) -> Vec<&'a Recorded> {
    reqs.iter()
        .filter(|r| r.session == session && r.max_tokens > 0)
        .collect()
}

fn last_user(r: &Recorded) -> String {
    r.messages
        .iter()
        .rev()
        .find(|m| m.role == Role::User)
        .and_then(|m| m.content.clone())
        .unwrap_or_default()
}

/// W1-W6 (B15) over everything the fake recorded. The one rebase of these flows is the
/// `/tree` navigation: the first request after it is marked `leaf_move`.
fn assert_invariants(reqs: &[Recorded], leaf_moves: &[usize]) {
    let rebases: Vec<(usize, &'static str)> = leaf_moves
        .iter()
        .map(|s| (*s, oxa_proto::rebase_why::LEAF_MOVE))
        .collect();
    let violations = check_all(reqs, &rebases);
    assert!(
        violations.is_empty(),
        "wire invariants W1-W6 hold on the recorded session: {violations:#?}"
    );
}

fn long_reply(tag: &str) -> String {
    let mut text = format!("{tag} first line.\n\n");
    for i in 1..=40 {
        text.push_str(&format!("{tag} paragraph {i}.\n\n"));
    }
    text.push_str(&format!("{tag} last line."));
    text
}

// ---------------------------------------------------------------------------------------------
// The daily-driver flow, fullscreen
// ---------------------------------------------------------------------------------------------

/// The strings the "tree open" step waits for: the transcript text that is already on screen
/// *and* the overlay-only marker "Session Tree" rendered by `selectors/tree.rs`.
const TREE_OPEN_WAIT: &[&str] = &["second question", "BETA answer.", "Session Tree"];

/// open -> prompt -> tool call -> `/model` selector -> `/tree` navigate -> `/new` ->
/// `/resume` picker -> fullscreen scroll + search -> exit, against the fake over HTTP.
#[test]
fn fullscreen_daily_driver_flow() {
    let world = World::with(
        vec![
            ScriptTurn::tool("call_1", "read", r#"{"path":"notes.md"}"#),
            ScriptTurn::text(&long_reply("ALPHA")),
            ScriptTurn::text("BETA answer."),
            ScriptTurn::text("GAMMA answer."),
            ScriptTurn::text("DELTA answer."),
        ],
        FakeOpts {
            done_gap_ms: 250,
            ..FakeOpts::default()
        },
    );
    std::fs::write(world.env.work().join("notes.md"), "the secret word is OMEGA-42\n")
        .expect("write the notes");

    let steps = vec![
        // The app is up: header, editor, footer.
        step(
            "open",
            &["oxa v", "fake-model"],
            typed("read the notes", &["enter"]),
        ),
        // The tool ran (its result is on screen) and the long reply ended.
        // The tool ran and the long reply ended (the tool result itself has scrolled out of the
        // viewport by then; the search below finds it).
        step(
            "tool and reply",
            &["ALPHA last line."],
            typed("/export alpha-idle.html", &["enter"]),
        ),
        step(
            "alpha settled",
            &["Exported the session to", "alpha-idle.html"],
            typed("/model", &["enter"]),
        ),
        // The model selector lists the engine's model; Enter on the current one closes it.
        step(
            "model selector",
            &["Select model", "fake-model"],
            vec![key("enter")],
        ),
        step(
            "second prompt",
            &["ALPHA last line."],
            typed("second question", &["enter"]),
        ),
        step(
            "second reply",
            &["BETA answer."],
            typed("/export beta-idle.html", &["enter"]),
        ),
        step(
            "second reply settled",
            &["Exported the session to", "beta-idle.html"],
            typed("/tree", &["enter"]),
        ),
        // The navigator opens on the leaf (the BETA reply); Up selects the user message.
        step("tree open", TREE_OPEN_WAIT, vec![key("up"), key("enter")]),
        // pi's question before the leaf moves; Enter = no summary.
        step(
            "summary question",
            &["Summarize branch?", "No summary"],
            vec![key("enter")],
        ),
        // The leaf moved above the user message, whose text is back in the editor; send it again.
        step(
            "navigated",
            &["Navigated to selected point", "second question"],
            vec![key("enter")],
        ),
        step(
            "branch reply",
            &["GAMMA answer."],
            typed("/export gamma-idle.html", &["enter"]),
        ),
        step(
            "branch reply settled",
            &["Exported the session to", "gamma-idle.html"],
            typed("/new", &["enter"]),
        ),
        // A fresh session: the old transcript is gone from the screen, the header is back.
        step(
            "new session",
            &["New session started"],
            typed("fresh start", &["enter"]),
        ),
        step(
            "fresh reply",
            &["DELTA answer."],
            typed("/export delta-idle.html", &["enter"]),
        ),
        step(
            "fresh reply settled",
            &["Exported the session to", "delta-idle.html"],
            typed("/resume", &["enter"]),
        ),
        // The picker lists both sessions of this directory; search for the first and open it.
        step("picker", &["read the notes", "fresh start"], typed("notes", &[])),
        step("picker filtered", &["notes"], vec![key("enter")]),
        // The first session is back, on the branch the navigation left it on.
        step(
            "resumed",
            &["Resumed session", "GAMMA answer."],
            vec![key("pageUp"), key("pageUp")],
        ),
        // Scrolling up shows older lines; the wheel scrolls too, and Ctrl+Home goes to the top.
        step(
            "paged up",
            &["ALPHA paragraph"],
            vec![wheel(true), key("ctrl+home")],
        ),
        step(
            "at the top",
            &["oxa v", "read the notes"],
            vec![key("ctrl+shift+f")],
        ),
        step("search bar", &["Search:"], typed("omega", &[])),
        // One match (the tool result), shown as 1/1 and scrolled into view.
        step(
            "search match",
            &["Search: omega", "1/1"],
            vec![key("enter"), key("escape")],
        ),
        step("search closed", &[], vec![key("ctrl+end")]),
        step("at the bottom", &["GAMMA answer."], vec![key("ctrl+d")]),
    ];
    let (code, last, term, _first) = run_app(&world, true, (100, 24), steps);
    assert_eq!(code, 0, "Ctrl+D on an empty editor quits");

    // Fullscreen: the alternate screen and the mouse were switched on, and off again at exit.
    assert_eq!(term.inner.modes.first(), Some(&(TermMode::AltScreen, true)));
    assert!(term.inner.modes.contains(&(TermMode::Mouse, true)));
    assert!(!term.inner.mode_on(TermMode::AltScreen) && !term.inner.mode_on(TermMode::Mouse));

    // The session the user left is the resumed first one, not the one the app started `/new`.
    let reqs = world.requests();
    let first_session = reqs.first().expect("the fake saw requests").session.clone();
    let last = last.expect("the worker reports its last session");
    assert_eq!(last.id, first_session, "the exit lines name the resumed session");
    assert!(last.file.is_some(), "a file-backed session");
    assert!(
        last.commands.iter().any(|c| c.name == "model"),
        "the session command list reaches the UI"
    );

    // What the engine saw: the tool result went back, the navigation dropped the BETA turn.
    let first = chats(&reqs, &first_session);
    assert_eq!(
        first.len(),
        4,
        "prompt, tool follow-up, second prompt, re-sent prompt: {:?}",
        first.len()
    );
    assert!(first[1]
        .messages
        .iter()
        .any(|m| m.role == Role::Tool && m.content.as_deref().unwrap_or("").contains("OMEGA-42")));
    assert_eq!(last_user(first[3]), "second question");
    assert!(
        !first[3]
            .messages
            .iter()
            .any(|m| m.content.as_deref() == Some("BETA answer.")),
        "the abandoned reply is not on the new branch"
    );
    let other: Vec<&Recorded> = reqs
        .iter()
        .filter(|r| r.session != first_session && r.max_tokens > 0)
        .collect();
    assert_eq!(other.len(), 1, "the /new session got exactly its own prompt");
    assert_eq!(last_user(other[0]), "fresh start");
    assert_eq!(
        other[0].messages.len(),
        1,
        "a new session starts with an empty wire"
    );

    // The first request of the session after the navigation (the parked prefill, or the
    // re-sent prompt itself) is the one that follows the rebase.
    let moved = reqs
        .iter()
        .filter(|r| r.session == first_session && r.seq > first[2].seq)
        .map(|r| r.seq)
        .min()
        .expect("a request after the navigation");
    assert_invariants(&reqs, &[moved]);

    // The picker showed the search text, and the search hit is the tool result.
    assert!(term.seen("search match").is_empty() || !term.seen("search match").contains("0/0"));
    assert!(term.all_text().contains("Select model"));
}

// ---------------------------------------------------------------------------------------------
// The same core flow in regular mode
// ---------------------------------------------------------------------------------------------

#[test]
fn regular_mode_flow() {
    let world = World::with(
        vec![
            ScriptTurn::tool("call_1", "read", r#"{"path":"notes.md"}"#),
            ScriptTurn::text("ALPHA answer."),
            ScriptTurn::text("BETA answer."),
        ],
        FakeOpts {
            done_gap_ms: 250,
            ..FakeOpts::default()
        },
    );
    std::fs::write(world.env.work().join("notes.md"), "the secret word is OMEGA-42\n")
        .expect("write the notes");

    let steps = vec![
        step("open", &["oxa v"], typed("read the notes", &["enter"])),
        step(
            "reply",
            &["OMEGA-42", "ALPHA answer."],
            typed("/export alpha-idle.html", &["enter"]),
        ),
        // Reply text can precede [DONE]; the export notice follows AgentSettled in the worker FIFO.
        step(
            "alpha settled",
            &["Exported the session to", "alpha-idle.html"],
            typed("/model", &["enter"]),
        ),
        step(
            "model selector",
            &["Select model", "fake-model"],
            vec![key("escape")],
        ),
        step("thinking selector", &[], typed("/thinking", &["enter"])),
        step("thinking open", &["off"], vec![key("escape")]),
        step("session info", &[], typed("/session", &["enter"])),
        step(
            "info shown",
            &["Session id:", "Messages: user 1, assistant 2, tool calls 1"],
            typed("/export info-idle.html", &["enter"]),
        ),
        step(
            "info settled",
            &["Exported the session to", "info-idle.html"],
            typed("/tree", &["enter"]),
        ),
        step(
            "tree open",
            &["read the notes", "ALPHA answer."],
            vec![key("escape")],
        ),
        step("second prompt", &[], typed("again", &["enter"])),
        step("second reply", &["BETA answer."], typed("/exit", &["enter"])),
    ];
    let (code, last, term, _first) = run_app(&world, false, (90, 30), steps);
    assert_eq!(code, 0, "/exit quits (it never reaches the session as a prompt)");
    assert!(
        !term.inner.modes.contains(&(TermMode::AltScreen, true)),
        "regular mode stays on the main screen"
    );

    let reqs = world.requests();
    let session = last.expect("the last session").id;
    let chat = chats(&reqs, &session);
    assert_eq!(
        chat.len(),
        3,
        "two prompts and one tool follow-up; no slash command was sent as a prompt"
    );
    assert!(
        reqs.iter().all(|r| !last_user(r).starts_with('/')),
        "built-in commands never reach the wire"
    );
    assert_invariants(&reqs, &[]);

    let text = term.all_text();
    assert!(text.contains("read the notes") && text.contains("OMEGA-42") && text.contains("BETA answer."));
}

/// Wave H (pi interactive-mode.ts showTreeSelector): selecting the current point is a no-op
/// ("Already at this point"); Escape on the summary question re-opens the navigator; cancelling
/// the custom prompt goes back to the question; the user message refills the editor.
#[test]
fn tree_navigator_follows_pis_dialog_rules() {
    let world = World::with(
        vec![ScriptTurn::text("ONE answer."), ScriptTurn::text("TWO answer.")],
        FakeOpts {
            done_gap_ms: 250,
            ..FakeOpts::default()
        },
    );
    let steps = vec![
        step("open", &["oxa v"], typed("first question", &["enter"])),
        // Reply text can precede [DONE]. HTML export is a read-only worker fence:
        // its notice follows AgentSettled in the same FIFO, without adding a tree entry.
        step(
            "first reply",
            &["ONE answer."],
            typed("/export first-idle.html", &["enter"]),
        ),
        step(
            "first run idle",
            &["Exported the session to", "first-idle.html"],
            typed("second question", &["enter"]),
        ),
        step(
            "second reply",
            &["TWO answer."],
            typed("/export second-idle.html", &["enter"]),
        ),
        step(
            "second run idle",
            &["Exported the session to", "second-idle.html"],
            typed("/tree", &["enter"]),
        ),
        // The selection starts at the leaf (the TWO reply): Enter there moves nothing.
        step(
            "tree on the leaf",
            &["Type to search", "TWO answer."],
            vec![key("enter")],
        ),
        step("no-op", &["Already at this point"], typed("/tree", &["enter"])),
        step("tree again", &["Type to search"], vec![key("up"), key("enter")]),
        step(
            "question",
            &["Summarize branch?", "Summarize with custom prompt"],
            vec![key("escape")],
        ),
        // Escape: the navigator is back with the user message still selected.
        step(
            "tree back",
            &["Type to search", "second question"],
            vec![key("enter")],
        ),
        step(
            "question again",
            &["Summarize branch?"],
            vec![key("down"), key("down"), key("enter")],
        ),
        step(
            "custom prompt",
            &["Custom summarization instructions"],
            vec![key("escape")],
        ),
        // Cancelling the custom prompt goes back to the question; Enter = no summary.
        step("question once more", &["Summarize branch?"], vec![key("enter")]),
        step(
            "navigated",
            &["Navigated to selected point", "second question"],
            {
                let mut ev = vec![key("ctrl+c")];
                ev.extend(typed("/exit", &["enter"]));
                ev
            },
        ),
    ];
    let (code, _last, _term, _first) = run_app(&world, false, (90, 30), steps);
    assert_eq!(code, 0);
    let reqs = world.requests();
    assert!(
        reqs.iter()
            .all(|r| r.max_tokens == 0 || !last_user(r).contains("Summar")),
        "no summary was asked for"
    );
}

/// Wave H (TUI-019): `/mcp` opens the MCP server list (F7's selector), here without a manager:
/// the empty list says so; Escape closes it.
#[test]
fn mcp_command_opens_the_server_list() {
    let world = World::new(vec![ScriptTurn::text("unused")]);
    let steps = vec![
        step("open", &["oxa v"], typed("/mcp", &["enter"])),
        step(
            "list",
            &["MCP servers", "No MCP servers configured."],
            vec![key("escape")],
        ),
        step("closed", &[], typed("/exit", &["enter"])),
    ];
    let (code, _last, _term, _first) = run_app(&world, false, (90, 30), steps);
    assert_eq!(code, 0);
}

/// Wave H (TUI-016): pi `stopInteractiveTui`: leaving fullscreen with
/// `fullscreenExitOutput: "transcript"` (the default) prints the transcript to the normal
/// screen; `"resume-hint"` does not.
#[test]
fn fullscreen_exit_prints_the_transcript() {
    for (settings, printed) in [
        (serde_json::json!({}), true),
        (serde_json::json!({"fullscreenExitOutput": "resume-hint"}), false),
    ] {
        let world = World::new(vec![ScriptTurn::text("EXIT-ANSWER")]);
        let mut opts = world.opts(true);
        opts.settings = Settings::from_values(settings, serde_json::json!({}));
        let steps = vec![
            step("open", &["oxa v"], typed("EXIT-QUESTION", &["enter"])),
            step("reply", &["EXIT-ANSWER"], typed("/exit", &["enter"])),
        ];
        let session = Session::open(world.deps(), Open::New).expect("open");
        let (code, _last, term, _first) = run_with(&world, session, opts, (90, 30), steps);
        assert_eq!(code, 0);
        let raw = String::from_utf8_lossy(&term.inner.out).into_owned();
        let after = &raw[raw.rfind("\x1b[2J").expect("the last clear")..];
        let text = strip_ansi(after);
        assert_eq!(
            text.contains("EXIT-QUESTION") && text.contains("EXIT-ANSWER"),
            printed,
            "after leaving the alternate screen: {text:?}"
        );
    }
}

/// Wave H (USE-010): a dropped image file arrives as a pasted path; it is attached to the next
/// prompt (the wire carries the placeholder), a pasted text path stays text.
#[test]
fn dropped_image_path_is_attached() {
    let world = World::new(vec![ScriptTurn::text("SEEN.")]);
    let png = world.env.work().join("drop.png");
    std::fs::write(
        &png,
        [
            0x89u8, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
            0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
            0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8, 0xcf, 0xc0, 0xf0,
            0x1f, 0x00, 0x05, 0x00, 0x01, 0xff, 0x89, 0x99, 0x3d, 0x1d, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
            0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
        ],
    )
    .unwrap();
    let steps = vec![
        step(
            "open",
            &["oxa v"],
            vec![InputEvent::Paste(png.display().to_string())],
        ),
        step(
            "attached",
            &["1 image(s) attached"],
            typed("what is it", &["enter"]),
        ),
        step("reply", &["SEEN."], typed("/exit", &["enter"])),
    ];
    let (code, _last, _term, _first) = run_app(&world, false, (90, 30), steps);
    assert_eq!(code, 0);
    let reqs = world.requests();
    let sent = reqs
        .iter()
        .map(last_user)
        .find(|t| t.contains("what is it"))
        .expect("the prompt");
    assert!(sent.contains("[image"), "a placeholder on the wire: {sent}");
}

/// A paste while a list selector (no text field) is open is not dropped silently: a notice says so.
#[test]
fn paste_into_thinking_selector_shows_a_notice() {
    let world = World::new(vec![ScriptTurn::text("OK.")]);
    let steps = vec![
        step("selector", &[], typed("/thinking", &["enter"])),
        step("paste", &["off"], vec![InputEvent::Paste("hello".to_string())]),
        step("notice", &["Paste ignored"], vec![key("escape")]),
        step("exit", &[], typed("/exit", &["enter"])),
    ];
    let (code, _l, _t, _f) = run_app(&world, false, (90, 30), steps);
    assert_eq!(code, 0);
}

// ---------------------------------------------------------------------------------------------
// X5: controls follow the session; quit is a stop signal
// ---------------------------------------------------------------------------------------------

/// After `/new` the controls belong to the new session: a steering message is queued on it,
/// Escape aborts ITS request, and the old session's handle sees none of it.
#[test]
fn controls_are_rebound_to_the_new_session() {
    let hold_ms = 20_000;
    let world = World::with(
        vec![
            ScriptTurn::text("ALPHA answer."),
            ScriptTurn {
                fault: Some(Fault::HoldHeaders { ms: hold_ms }),
                ..ScriptTurn::text("LATE answer.")
            },
        ],
        FakeOpts {
            done_gap_ms: 250,
            ..FakeOpts::default()
        },
    );
    let steps = vec![
        step("open", &["oxa v"], typed("first", &["enter"])),
        // A final content delta does not mean the worker is idle. The queued,
        // read-only export notice proves AgentSettled has reached the UI first.
        step(
            "first reply",
            &["ALPHA answer."],
            typed("/export controls-idle.html", &["enter"]),
        ),
        step(
            "first run idle",
            &["Exported the session to", "controls-idle.html"],
            typed("/new", &["enter"]),
        ),
        step(
            "new session",
            &["New session started"],
            typed("slow question", &["enter"]),
        ),
        // The request of the new session hangs in the engine. Enter while it runs = steer.
        step(
            "running",
            &["slow question", "Working..."],
            typed("steer me", &["enter"]),
        ),
        // The message waits in the new session's queue, shown above the editor. Escape aborts
        // the run and brings the queued message back into the editor.
        step("steered", &["Steering: steer me"], vec![key("escape")]),
        // The aborted reply is a Shown block (never sent); the text is in the editor again.
        step(
            "aborted",
            &["partial reply dropped; your prompt stays", "steer me"],
            vec![key("ctrl+c")],
        ),
        step("cleared", &[], vec![key("ctrl+d")]),
    ];
    let started = Instant::now();
    let (code, last, term, first) = run_app(&world, false, (90, 30), steps);
    let took = started.elapsed();
    assert_eq!(code, 0);
    assert!(
        took < Duration::from_millis(hold_ms / 2),
        "Escape cancelled the new session's request (the engine would have held it {hold_ms} ms): {took:?}"
    );
    assert!(
        !term.all_text().contains("LATE answer."),
        "the aborted request never produced a reply"
    );

    // The old session's controls were not touched by anything done after the switch.
    let (steering, follow_up) = first.queued();
    assert!(
        steering.is_empty() && follow_up.is_empty(),
        "nothing was queued on the old session"
    );
    assert!(!first.is_busy(), "the old session is not running");

    let reqs = world.requests();
    let old = reqs.first().expect("requests").session.clone();
    let new = last.expect("the last session").id;
    assert_ne!(old, new, "the app ended in the /new session");
    assert_eq!(
        chats(&reqs, &old).len(),
        1,
        "the old session got only its own prompt"
    );
    assert_eq!(
        chats(&reqs, &new).len(),
        1,
        "the slow request went to the new session"
    );
    assert_eq!(last_user(chats(&reqs, &new)[0]), "slow question");
    assert!(
        reqs.iter().all(|r| last_user(r) != "steer me"),
        "the steering message was handed back, never sent"
    );
}

/// Messages still queued for the old session when it is swapped are handed back to the user
/// with a notice; they are not sent to either session.
#[test]
fn queued_work_of_the_old_session_is_handed_back_on_a_switch() {
    let world = World::new(vec![
        ScriptTurn::text("ALPHA answer."),
        ScriptTurn::text("BETA answer."),
    ]);
    let session = Session::open(world.deps(), Open::New).expect("open");
    // A follow-up that is still queued on the first session when the user switches.
    session.handle().follow_up("left over".to_string());
    let steps = vec![
        step("open", &["oxa v"], typed("/new", &["enter"])),
        step(
            "handed back",
            &[
                "New session started",
                "queued message(s) were not sent",
                "left over",
            ],
            vec![key("ctrl+c")],
        ),
        step("cleared", &[], typed("hello", &["enter"])),
        step("reply", &["ALPHA answer."], vec![key("ctrl+d")]),
    ];
    let (code, last, _term, first) = run_with(&world, session, world.opts(false), (90, 30), steps);
    assert_eq!(code, 0);
    let (steering, follow_up) = first.queued();
    assert!(
        steering.is_empty() && follow_up.is_empty(),
        "the old queue was drained"
    );
    let reqs = world.requests();
    let new = last.expect("the last session").id;
    assert_eq!(chats(&reqs, &new).len(), 1);
    assert!(reqs.iter().all(|r| !r
        .messages
        .iter()
        .any(|m| m.content.as_deref() == Some("left over"))));
}

/// Quit while a request runs and more prompts wait: the run is cancelled, the queued prompts
/// never start, and `run_session` returns with the worker gone.
#[test]
fn quit_stops_the_run_and_never_starts_queued_prompts() {
    let hold_ms = 20_000;
    let world = World::new(vec![
        ScriptTurn {
            fault: Some(Fault::HoldHeaders { ms: hold_ms }),
            ..ScriptTurn::text("LATE one.")
        },
        ScriptTurn::text("LATE two."),
        ScriptTurn::text("LATE three."),
    ]);
    let session = Session::open(world.deps(), Open::New).expect("open");
    let mut opts = world.opts(true);
    opts.initial_prompt = Some("first".to_string());
    opts.initial_follow_ups = vec!["second".to_string(), "third".to_string()];
    let server = world.server.clone();
    let steps = vec![
        // The first prompt is in flight (its request has reached the engine); the other two
        // wait in the command channel behind it.
        step("running", &["first", "Working..."], vec![key("ctrl+d")]).until(move || {
            server
                .requests()
                .iter()
                .any(|r| r.max_tokens > 0 && last_user(r) == "first")
        }),
    ];
    let started = Instant::now();
    let (code, last, term, first) = run_with(&world, session, opts, (90, 24), steps);
    let took = started.elapsed();
    assert_eq!(code, 0);
    assert!(
        took < Duration::from_millis(hold_ms / 2),
        "quit did not wait for the engine: {took:?}"
    );
    assert!(
        last.is_some(),
        "the worker ended cleanly and reported its session"
    );
    assert!(!first.is_busy(), "nothing is left running");
    let (steering, follow_up) = first.queued();
    assert!(
        steering.is_empty() && follow_up.is_empty(),
        "nothing is left queued"
    );

    let reqs = world.requests();
    let users: Vec<String> = reqs.iter().filter(|r| r.max_tokens > 0).map(last_user).collect();
    assert_eq!(
        users,
        vec!["first".to_string()],
        "the queued prompts were never started"
    );
    assert!(
        !term.all_text().contains("LATE"),
        "no reply was produced after the quit"
    );
    assert!(
        !term.inner.mode_on(TermMode::AltScreen),
        "the alternate screen was left"
    );
}

// ---------------------------------------------------------------------------------------------
// Replies that do not complete: the core's real event sequence, through the app
// ---------------------------------------------------------------------------------------------

/// The last stored entry of the session the app ended with.
fn last_entry(world: &World, last: &Option<SessionInfo>) -> oxa_proto::EntryBody {
    let info = last.as_ref().expect("the last session");
    let sref = world
        .store
        .resolve(&info.id)
        .expect("the session is in the store");
    world
        .store
        .load(&sref)
        .expect("load the session")
        .last()
        .expect("entries")
        .body
        .clone()
}

/// Escape while the reply streams: the core stores `Shown { why: aborted }` with the partial
/// text and sends no `message_end`. The app must show that block, not an ordinary reply.
#[test]
fn aborted_stream_is_shown_not_a_reply() {
    // The stream delivers its text and then stays open: the run is still in progress.
    let world = World::with(
        vec![ScriptTurn {
            chunk: 6,
            ..ScriptTurn::text("PARTIAL words that arrived")
        }],
        FakeOpts {
            done_gap_ms: 20_000,
            ..FakeOpts::default()
        },
    );
    let steps = vec![
        step("open", &["oxa v"], typed("go", &["enter"])),
        step("streaming", &["PARTIAL words that arrived"], vec![key("escape")]),
        step(
            "aborted",
            &["aborted: partial reply dropped; your prompt stays"],
            typed("/session", &["enter"]),
        ),
        // The Shown block is not counted as an assistant message.
        step(
            "counted",
            &["Messages: user 1, assistant 0, tool calls 0"],
            vec![key("ctrl+d")],
        ),
    ];
    let started = Instant::now();
    let (code, last, term, _first) = run_app(&world, false, (90, 30), steps);
    assert_eq!(code, 0);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the abort did not wait for the stream to end"
    );
    match last_entry(&world, &last) {
        oxa_proto::EntryBody::Shown { text, why } => {
            assert_eq!(why, "aborted");
            assert!(
                text.starts_with("PARTIAL"),
                "the partial text is what was stored: {text:?}"
            );
        }
        other => panic!("the core stored a Shown entry at the leaf, got {other:?}"),
    }
    assert!(
        term.all_text().contains("PARTIAL words that arrived"),
        "the partial text stays on screen"
    );
}

/// After Escape, one Ctrl+C on the empty editor arms the exit and the hint bar says so; a second
/// Ctrl+C exits. Drives the real loop, so the `armed` wiring into the hint bar is covered.
#[test]
fn ctrl_c_after_abort_shows_the_exit_hint() {
    let world = World::with(
        vec![ScriptTurn {
            chunk: 6,
            ..ScriptTurn::text("PARTIAL words that arrived")
        }],
        FakeOpts {
            done_gap_ms: 20_000,
            ..FakeOpts::default()
        },
    );
    let steps = vec![
        step("open", &[], typed("go", &["enter"])),
        step("streaming", &["PARTIAL words that arrived"], vec![key("escape")]),
        step(
            "aborted",
            &["partial reply dropped; your prompt stays"],
            vec![key("ctrl+c")],
        ),
        step("armed", &["ctrl+c exit"], vec![key("ctrl+c")]),
    ];
    // The hint bar belongs to the oxa skin; the other tests pin the plain one.
    let mut opts = world.opts(false);
    opts.env = vec![("OXA_SKIN".to_string(), "oxa".to_string())];
    let session = Session::open(world.deps(), Open::New).expect("open the session");
    let (code, _last, _term, _first) = run_with(&world, session, opts, (90, 30), steps);
    assert_eq!(code, 0);
}

/// A stream cut by the engine mid-reply: `Shown { why: cut }`, shown with the warning.
#[test]
fn cut_stream_is_shown_not_a_reply() {
    let world = World::new(vec![ScriptTurn {
        chunk: 8,
        fault: Some(Fault::DropAt { bytes: 600 }),
        ..ScriptTurn::text(&"CUTTEXT words keep coming and coming. ".repeat(40))
    }]);
    let steps = vec![
        step("open", &["oxa v"], typed("go", &["enter"])),
        step(
            "cut",
            &[
                "CUTTEXT",
                "cut: not sent to the model",
                "The reply did not finish",
            ],
            typed("/session", &["enter"]),
        ),
        step(
            "counted",
            &["Messages: user 1, assistant 0, tool calls 0"],
            vec![key("ctrl+d")],
        ),
    ];
    let (code, last, _term, _first) = run_app(&world, false, (90, 40), steps);
    assert_eq!(code, 0);
    match last_entry(&world, &last) {
        oxa_proto::EntryBody::Shown { why, .. } => assert_eq!(why, "cut"),
        other => panic!("the core stored a Shown entry at the leaf, got {other:?}"),
    }
    assert_eq!(
        world.requests().iter().filter(|r| r.max_tokens > 0).count(),
        1,
        "a cut reply is never resent"
    );
}

/// The engine refuses the request: `Shown { why: failed }` with the error text.
#[test]
fn engine_error_is_shown_not_a_reply() {
    let world = World::new(vec![ScriptTurn::fault(Fault::Http {
        status: 400,
        body: "{\"error\":{\"message\":\"nope\"}}".to_string(),
    })]);
    let steps = vec![
        step("open", &["oxa v"], typed("go", &["enter"])),
        step(
            "failed",
            &[
                "failed: not sent to the model",
                "The request failed; no reply was received",
            ],
            typed("/session", &["enter"]),
        ),
        step(
            "counted",
            &["Messages: user 1, assistant 0, tool calls 0"],
            vec![key("ctrl+d")],
        ),
    ];
    let (code, last, _term, _first) = run_app(&world, true, (90, 30), steps);
    assert_eq!(code, 0);
    match last_entry(&world, &last) {
        oxa_proto::EntryBody::Shown { why, .. } => assert_eq!(why, "failed"),
        other => panic!("the core stored a Shown entry at the leaf, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------------------------
// `-r` picker, `/trust`, the trust question before the session
// ---------------------------------------------------------------------------------------------

/// `oxa -r`: the session picker is open at start; choosing a session resumes it.
#[test]
fn resume_flag_opens_the_picker_at_start() {
    let world = World::new(vec![
        ScriptTurn::text("EARLIER answer."),
        ScriptTurn::text("LATER answer."),
    ]);
    let steps = vec![
        step("open", &["oxa v"], typed("earlier talk", &["enter"])),
        step("reply", &["EARLIER answer."], vec![key("ctrl+d")]),
    ];
    let (_, first, _, _) = run_app(&world, false, (90, 30), steps);
    let first = first.expect("the first run's session");

    let session = Session::open(world.deps(), Open::New).expect("open");
    let mut opts = world.opts(true);
    opts.open_picker = true;
    let steps = vec![
        // The picker lists the earlier session without any command being typed.
        step("picker at start", &["earlier talk"], vec![key("enter")]),
        step(
            "resumed",
            &["Resumed session", "EARLIER answer."],
            typed("and then", &["enter"]),
        ),
        step("reply", &["LATER answer."], vec![key("ctrl+d")]),
    ];
    let (code, last, _, _) = run_with(&world, session, opts, (90, 24), steps);
    assert_eq!(code, 0);
    assert_eq!(
        last.expect("the last session").id,
        first.id,
        "the picked session is the one resumed"
    );
    let reqs = world.requests();
    let chat = chats(&reqs, &first.id);
    assert_eq!(chat.len(), 2);
    assert_eq!(
        chat[1].messages.len(),
        3,
        "the resumed wire continues: user, assistant, user"
    );
    assert_invariants(&reqs, &[]);
}

/// `/trust` asks through the dialog host (the run is reported as blocked meanwhile) and saves
/// the answer through the CLI's store.
#[test]
fn trust_command_saves_the_decision() {
    use oxa_interactive::TrustChoice;
    use std::sync::Mutex;
    let world = World::new(Vec::new());
    let saved: Arc<Mutex<Vec<TrustChoice>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = saved.clone();
    let session = Session::open(world.deps(), Open::New).expect("open");
    let mut opts = world.opts(false);
    opts.gated = vec!["settings.json".to_string()];
    opts.save_trust = Some(Box::new(move |choice| {
        sink.lock().expect("lock").push(choice);
        Ok(())
    }));
    let steps = vec![
        step("open", &["oxa v"], typed("/trust", &["enter"])),
        step(
            "question",
            &["Project trust", "settings.json", "Trust this project"],
            vec![key("enter")],
        ),
        step("saved", &["This project is trusted"], vec![key("ctrl+d")]),
    ];
    let (code, _, _, _) = run_with(&world, session, opts, (100, 30), steps);
    assert_eq!(code, 0);
    assert_eq!(*saved.lock().expect("lock"), vec![TrustChoice::Trust]);
}

/// The trust question drawn on a bare terminal, before any session exists (what `oxa` shows
/// at start in an untrusted project with gated resources).
#[test]
fn trust_question_before_the_session() {
    use oxa_interactive::{ask_trust_on, TrustChoice};
    let keymap = Keymap::defaults();
    let theme = oxa_tui::Theme::plain();
    let gated = vec!["hooks".to_string()];
    // Down twice from "Trust this project": parent, then "Do not trust".
    let mut term = ScriptTerm::new(
        100,
        20,
        vec![step(
            "question",
            &["Project trust", "hooks"],
            vec![key("down"), key("down"), key("enter")],
        )],
    );
    assert_eq!(
        ask_trust_on(&mut term, &keymap, &theme, "/work/proj", &gated),
        Some(TrustChoice::NoTrust)
    );
    // Ctrl+C leaves it unanswered: untrusted for this run, asked again next time.
    let mut term = ScriptTerm::new(
        100,
        20,
        vec![step("question", &["Project trust"], vec![key("ctrl+c")])],
    );
    assert_eq!(
        ask_trust_on(&mut term, &keymap, &theme, "/work/proj", &gated),
        None
    );
}

/// The late-Rebind quit race, with a barrier instead of luck: `/new`, a prompt and the quit
/// arrive in ONE input batch, so the UI quits still holding the old session's handle, and the
/// quit is held back until the new session's request has reached the engine. The worker's
/// `Rebind` is then still unread. Shutdown must stop the run of the NEW session.
#[test]
fn quit_with_a_pending_rebind_stops_the_new_sessions_run() {
    let hold_ms = 20_000;
    let world = World::new(vec![
        ScriptTurn {
            fault: Some(Fault::HoldHeaders { ms: hold_ms }),
            ..ScriptTurn::text("LATE answer.")
        },
        ScriptTurn::text("NEVER answer."),
    ]);
    let session = Session::open(world.deps(), Open::New).expect("open");
    let first = session.handle();
    let old_id = session.state().session_id;
    let mut opts = world.opts(false);
    let mut batch = typed("/new", &["enter"]);
    batch.extend(typed("slow question", &["enter"]));
    batch.push(key("ctrl+d"));
    let mut term = ScriptTerm::new(90, 30, vec![step("open", &["oxa v"], batch)]);
    let server = world.server.clone();
    term.gate = Some(Box::new(move || {
        server
            .requests()
            .iter()
            .any(|r| r.max_tokens > 0 && last_user(r) == "slow question")
    }));

    let started = Instant::now();
    let (code, last) = run_session(&mut term, session, &mut opts);
    let took = started.elapsed();
    assert_eq!(code, 0);
    let last = last.expect("the worker ended cleanly");
    assert_ne!(last.id, old_id, "the worker had switched to the new session");
    // The premise of the race: the new session's request was in flight when the quit ran.
    let reqs = world.requests();
    let slow: Vec<&Recorded> = reqs.iter().filter(|r| r.max_tokens > 0).collect();
    assert_eq!(
        slow.len(),
        1,
        "exactly the new session's prompt reached the engine"
    );
    assert_eq!(slow[0].session, last.id);
    assert!(
        took < Duration::from_millis(hold_ms / 2),
        "the quit stopped the new session's run instead of waiting for it: {took:?}"
    );
    assert!(
        !term.all_text().contains("LATE") && !term.all_text().contains("NEVER"),
        "no reply after the quit"
    );
    assert!(!first.is_busy(), "the old session is not running either");
    // What the new session stored: the user message and an aborted reply, nothing completed.
    match last_entry(&world, &Some(last)) {
        oxa_proto::EntryBody::Shown { why, .. } => assert_eq!(why, "aborted"),
        other => panic!("the interrupted run ends in a Shown entry, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------------------------
// Step::is_ready unit tests
// ---------------------------------------------------------------------------------------------

#[test]
fn tree_open_not_ready_on_transcript_only() {
    let s = step("tree open", TREE_OPEN_WAIT, vec![]);
    // The transcript text is there but the overlay marker is missing.
    let since = "second question\nBETA answer.\n";
    assert!(!s.is_ready(since));
}

#[test]
fn tree_open_ready_with_overlay() {
    let s = step("tree open", TREE_OPEN_WAIT, vec![]);
    // All three markers are present, including the overlay-only "Session Tree".
    let since = "second question\nBETA answer.\n  Session Tree\n";
    assert!(s.is_ready(since));
}
