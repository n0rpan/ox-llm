//! L5 (`tune.rs`): the per-epoch tool tuning and the small-context adaptation.
//! `st.tune` is set on create, on resume and on an epoch; the kept tail is 2000 unless the user
//! sets it, and the seven built-ins are always kept.

use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{HeadlessUi, ToolTune, VecSink};
use oxa_resources::{discover, DiscoverOpts, Resources};
use oxa_sandbox::NoSandbox;
use oxa_session::{MemStore, SessionStore};
use std::sync::Arc;

const SEVEN: [&str; 7] = ["bash", "edit", "find", "grep", "ls", "read", "write"];

fn write_settings(env: &TestEnv, json: &str) {
    let path = Dirs::under(env.root()).settings_file();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, json).unwrap();
}

fn resources(env: &TestEnv, settings: &Settings) -> Resources {
    let opts = DiscoverOpts {
        trusted: true,
        ..Default::default()
    };
    discover(&Dirs::under(env.root()), &env.work(), settings, &opts)
}

fn open_with(
    env: &TestEnv,
    engine: Arc<ScriptedEngine>,
    store: Arc<dyn SessionStore>,
    how: Open,
    selection: ToolSelection,
) -> Session {
    let dirs = Dirs::under(env.root());
    let settings = Settings::load(&dirs, &env.work(), true);
    let resources = resources(env, &settings);
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
            selection,
            resources,
            model: None,
            thinking: None,
            mode: None,
            session_id: Some("s1".to_string()),
            jobs: oxa_core::fresh_jobs(),
        },
        how,
    )
    .expect("open")
}

fn open(env: &TestEnv, engine: Arc<ScriptedEngine>, store: Arc<dyn SessionStore>, how: Open) -> Session {
    open_with(env, engine, store, how, ToolSelection::default())
}

fn names(s: &Session) -> Vec<String> {
    let mut v: Vec<String> = s.inner().active.specs().iter().map(|t| t.name.clone()).collect();
    v.sort();
    v
}

/// The default tuning (diet on, bash timeout 120 s) with the given first page.
fn default_tune(read_page_lines: u32, read_page_bytes: u32) -> ToolTune {
    ToolTune {
        diet: true,
        read_page_lines,
        read_page_bytes,
        bash_timeout_s: 120,
        // L4: the output cap these windows give (a page under 8 KiB is the cap itself).
        output_cap_bytes: if read_page_bytes < 8192 {
            read_page_bytes
        } else {
            16_384
        },
    }
}

#[test]
fn create_small_context_tunes_and_keeps_seven() {
    let env = TestEnv::new();
    write_settings(&env, r#"{}"#);
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    let engine = Arc::new(ScriptedEngine::new(Vec::new()).with_limits(16_384, 1024));
    let s = open(&env, engine, store, Open::New);
    assert_eq!(s.inner().tune, default_tune(200, 5898));
    assert_eq!(s.inner().tune.read_page_lines, 200);
    assert_eq!(s.inner().tune.read_page_bytes, 5898);
    // T4 (four tools on a small context) is NOT applied: CLI-032 keeps the seven built-ins.
    assert_eq!(names(&s), SEVEN);
    assert_eq!(s.inner().compact.keep_tokens, 2000);
}

#[test]
fn create_large_context_keeps_seven_and_the_defaults() {
    let env = TestEnv::new();
    write_settings(&env, r#"{}"#);
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    let engine = Arc::new(ScriptedEngine::new(Vec::new()).with_limits(49_152, 1024));
    let s = open(&env, engine, store, Open::New);
    assert_eq!(s.inner().tune, default_tune(400, 8192));
    assert_eq!(
        (s.inner().tune.read_page_lines, s.inner().tune.read_page_bytes),
        (400, 8192)
    );
    assert_eq!(names(&s), SEVEN);
    assert_eq!(s.inner().compact.keep_tokens, 4000);
}

#[test]
fn an_explicit_keep_recent_tokens_wins() {
    let env = TestEnv::new();
    write_settings(&env, r#"{"compaction": {"keepRecentTokens": 3000}}"#);
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    let engine = Arc::new(ScriptedEngine::new(Vec::new()).with_limits(16_384, 1024));
    let s = open(&env, engine, store, Open::New);
    assert_eq!(s.inner().compact.keep_tokens, 3000);
}

#[test]
fn an_epoch_retunes_the_session() {
    let env = TestEnv::new();
    write_settings(&env, r#"{"oxa": {"diet": {"output": false, "tools": false}}}"#);
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    let engine = Arc::new(ScriptedEngine::new(Vec::new()).with_limits(16_384, 1024));
    let mut s = open(&env, engine, store, Open::New);
    assert!(!s.inner().tune.diet);
    write_settings(&env, r#"{}"#);
    let mut sink = VecSink::default();
    let mut ui = HeadlessUi::default();
    assert!(s.reload(&mut sink, &mut ui).expect("reload"), "a new epoch");
    assert_eq!(s.inner().tune, default_tune(200, 5898));
    // T4 (four tools on a small context) is NOT applied: CLI-032 keeps the seven built-ins.
    assert_eq!(names(&s), SEVEN);
    assert_eq!(s.inner().compact.keep_tokens, 2000);
}
