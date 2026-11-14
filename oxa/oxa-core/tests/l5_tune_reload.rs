//! Item A of the tune-reload lane: the per-tool tuning (`st.tune`) follows the engine's context
//! window and the settings at the next budget refresh (`/reload`, a model switch) and at a
//! same-bytes reload, without a new prefix epoch. The `local` flag stays tied to the epoch in
//! force, and profile `pi` never changes its tune or its prefix bytes.

use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{
    AgentEvent, AssistantTurn, CancelToken, ChatRequest, Engine, EngineInfo, HeadlessUi, OxaError, Profile,
    StreamEvent, ToolTune, VecSink,
};
use oxa_resources::{discover, DiscoverOpts, Resources};
use oxa_sandbox::NoSandbox;
use oxa_session::{MemStore, SessionStore};
use std::sync::{Arc, Mutex};

/// A scripted engine whose published context window can be changed between requests (an engine
/// reload with another `n_ctx`, or a model switch). Chat is the scripted engine's.
struct Window {
    chat: Arc<ScriptedEngine>,
    n_ctx: Mutex<u32>,
}

impl Window {
    fn new(turns: Vec<ScriptTurn>, n_ctx: u32) -> Arc<Window> {
        Arc::new(Window {
            chat: Arc::new(ScriptedEngine::new(turns).with_limits(n_ctx, 1024)),
            n_ctx: Mutex::new(n_ctx),
        })
    }
    fn set_n_ctx(&self, n: u32) {
        *self.n_ctx.lock().unwrap() = n;
    }
}

impl Engine for Window {
    fn info(&self, target: Option<&str>) -> Result<EngineInfo, OxaError> {
        let mut info = self.chat.info(target)?;
        info.n_ctx = *self.n_ctx.lock().unwrap();
        Ok(info)
    }
    fn count_tokens(&self, text: &str) -> Result<u32, OxaError> {
        self.chat.count_tokens(text)
    }
    fn chat(
        &self,
        r: &ChatRequest,
        cancel: &CancelToken,
        on: &mut dyn FnMut(StreamEvent),
    ) -> Result<AssistantTurn, OxaError> {
        self.chat.chat(r, cancel, on)
    }
    fn clear_hint(&self, hint: &str) -> Result<(), OxaError> {
        self.chat.clear_hint(hint)
    }
    fn count_prompt(&self, req: &ChatRequest) -> Result<Option<u64>, OxaError> {
        self.chat.count_prompt(req)
    }
}

fn write_settings(env: &TestEnv, json: &str) {
    let path = Dirs::under(env.root()).settings_file();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, json).unwrap();
}

fn open(env: &TestEnv, engine: Arc<Window>) -> Session {
    let dirs = Dirs::under(env.root());
    let settings = Settings::load(&dirs, &env.work(), true);
    let opts = DiscoverOpts {
        trusted: true,
        ..Default::default()
    };
    let resources: Resources = discover(&dirs, &env.work(), &settings, &opts);
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    Session::open(
        Deps {
            engine,
            store,
            tools: oxa_tools::builtin_tools(),
            hooks: Vec::new(),
            policy: Box::new(ModePolicy::new()),
            sandbox: Arc::new(NoSandbox),
            settings,
            dirs,
            cwd: env.work(),
            selection: ToolSelection::default(),
            resources,
            model: None,
            thinking: None,
            mode: None,
            session_id: Some("s1".to_string()),
        },
        Open::New,
    )
    .expect("open")
}

/// Reload with the settings on disk now: returns whether a new prefix epoch opened.
fn reload(s: &mut Session) -> bool {
    let mut sink = VecSink::default();
    let mut ui = HeadlessUi::default();
    s.reload(&mut sink, &mut ui).expect("reload")
}

/// The epoch count and the prefix sha in force.
fn epoch_state(s: &Session) -> (u32, [u8; 32]) {
    (s.inner().stats.epochs, s.inner().prefix.sha256)
}

/// Run one `read` of `big.txt` (no offset, no limit) and return its text.
fn read_first_page(s: &mut Session, engine: &Window) -> String {
    engine
        .chat
        .push(ScriptTurn::tool("c1", "read", r#"{"path":"big.txt"}"#));
    engine.chat.push(ScriptTurn::text("done"));
    let mut sink = VecSink::default();
    let mut ui = HeadlessUi::default();
    s.prompt("read it".to_string(), Vec::new(), &mut sink, &mut ui)
        .expect("prompt");
    sink.0
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolExecutionEnd {
                tool_name, result, ..
            } if tool_name == "read" => Some(result.text.clone()),
            _ => None,
        })
        .expect("a read result")
}

/// A 1000-line file whose line N reads `row NNNN`, so the first page is checkable by its last row.
fn big_file(env: &TestEnv) {
    let body: String = (1..=1000).map(|n| format!("row {n:04}\n")).collect();
    env.write("work/big.txt", &body);
}

#[test]
fn a_smaller_window_on_reload_shrinks_the_read_page_without_an_epoch() {
    let env = TestEnv::new();
    big_file(&env);
    write_settings(&env, r#"{"oxa": {"profile": "local"}}"#);
    let engine = Window::new(Vec::new(), 64_000);
    let mut s = open(&env, engine.clone());
    assert_eq!(s.inner().tune.read_page_lines, 400);
    let before = epoch_state(&s);

    engine.set_n_ctx(16_000);
    assert!(!reload(&mut s), "same prefix bytes: no epoch");
    assert_eq!(epoch_state(&s), before, "epoch count and prefix sha unchanged");
    assert_eq!(s.inner().tune.read_page_lines, 200);
    assert_eq!(s.inner().tune.read_page_bytes, 5760);
    assert!(
        s.inner().tune.local,
        "the local flag stays with the epoch in force"
    );

    let text = read_first_page(&mut s, &engine);
    assert!(text.contains("row 0200"), "first page ends at line 200: {text:?}");
    assert!(!text.contains("row 0201"), "no line past the small page");
    assert_eq!(epoch_state(&s), before, "the read opened no epoch");
}

#[test]
fn a_larger_window_on_reload_widens_the_read_page_without_an_epoch() {
    let env = TestEnv::new();
    big_file(&env);
    write_settings(&env, r#"{"oxa": {"profile": "local"}}"#);
    let engine = Window::new(Vec::new(), 16_000);
    let mut s = open(&env, engine.clone());
    assert_eq!(s.inner().tune.read_page_lines, 200);
    let before = epoch_state(&s);

    engine.set_n_ctx(64_000);
    assert!(!reload(&mut s));
    assert_eq!(epoch_state(&s), before);
    assert_eq!(
        (s.inner().tune.read_page_lines, s.inner().tune.read_page_bytes),
        (400, 8192)
    );

    let text = read_first_page(&mut s, &engine);
    assert!(text.contains("row 0400"), "first page ends at line 400: {text:?}");
    assert!(!text.contains("row 0401"), "no line past the large page");
    assert_eq!(epoch_state(&s), before);
}

#[test]
fn a_settings_reload_of_timeout_and_diet_applies_without_an_epoch() {
    let env = TestEnv::new();
    write_settings(&env, r#"{"oxa": {"profile": "local"}}"#);
    let engine = Window::new(Vec::new(), 64_000);
    let mut s = open(&env, engine);
    assert!(s.inner().tune.diet);
    assert_eq!(s.inner().tune.bash_timeout_s, 120);
    let before = epoch_state(&s);

    write_settings(
        &env,
        r#"{"oxa": {"profile": "local", "diet": {"tools": false}, "bash": {"defaultTimeout": 30}}}"#,
    );
    assert!(
        !reload(&mut s),
        "the timeout and the diet do not change the prefix: no epoch"
    );
    assert_eq!(epoch_state(&s), before);
    assert!(!s.inner().tune.diet, "the diet setting is in force");
    assert_eq!(
        s.inner().tune.bash_timeout_s,
        30,
        "the timeout setting is in force"
    );
    assert!(s.inner().tune.local);
}

#[test]
fn profile_pi_keeps_the_default_tune_and_the_bytes_across_window_changes() {
    let env = TestEnv::new();
    big_file(&env);
    write_settings(&env, r#"{"oxa": {"profile": "pi"}}"#);
    let engine = Window::new(Vec::new(), 16_000);
    let mut s = open(&env, engine.clone());
    assert_eq!(s.inner().stats.profile, Profile::Pi);
    assert_eq!(s.inner().tune, ToolTune::default());
    let before = epoch_state(&s);

    engine.set_n_ctx(64_000);
    assert!(!reload(&mut s));
    assert_eq!(
        s.inner().tune,
        ToolTune::default(),
        "pi tune under a larger window"
    );
    engine.set_n_ctx(8_000);
    assert!(!reload(&mut s));
    assert_eq!(
        s.inner().tune,
        ToolTune::default(),
        "pi tune under a smaller window"
    );
    assert_eq!(epoch_state(&s), before, "pi bytes do not change with the window");

    let text = read_first_page(&mut s, &engine);
    assert!(
        text.contains("row 1000"),
        "pi reads the whole file under the 2000-line page"
    );
}
