//! L5 (`tune.rs`): the per-epoch tool tuning and the small-context adaptation of profile `local`.
//! `st.tune` is set on create, on resume and on an epoch; the kept tail is 2000 unless the user sets it. Profile `pi` with no
//! new setting keeps today's values (the default tuning, seven tools, the setting's kept tail).

use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{HeadlessUi, Profile, ToolTune, VecSink};
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

/// The tuning of profile `local` (diet on, bash timeout 120 s) with the given first page.
fn local_tune(read_page_lines: u32, read_page_bytes: u32) -> ToolTune {
    ToolTune {
        diet: true,
        local: true,
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
fn create_local_small_context_tunes_and_keeps_seven() {
    let env = TestEnv::new();
    write_settings(&env, r#"{"oxa": {"profile": "local"}}"#);
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    let engine = Arc::new(ScriptedEngine::new(Vec::new()).with_limits(16_384, 1024));
    let s = open(&env, engine, store, Open::New);
    assert_eq!(s.inner().stats.profile, Profile::Local);
    assert_eq!(s.inner().tune, local_tune(200, 5898));
    assert_eq!(s.inner().tune.read_page_lines, 200);
    assert_eq!(s.inner().tune.read_page_bytes, 5898);
    // T4 (four tools on a small context) is NOT applied: CLI-032 keeps the seven built-ins.
    assert_eq!(names(&s), SEVEN);
    assert_eq!(s.inner().compact.keep_tokens, 2000);
}

#[test]
fn create_local_large_context_keeps_seven_and_the_defaults() {
    let env = TestEnv::new();
    write_settings(&env, r#"{"oxa": {"profile": "local"}}"#);
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    let engine = Arc::new(ScriptedEngine::new(Vec::new()).with_limits(49_152, 1024));
    let s = open(&env, engine, store, Open::New);
    assert_eq!(s.inner().tune, local_tune(400, 8192));
    assert_eq!(
        (s.inner().tune.read_page_lines, s.inner().tune.read_page_bytes),
        (400, 8192)
    );
    assert_eq!(names(&s), SEVEN);
    assert_eq!(s.inner().compact.keep_tokens, 4000);
}

#[test]
fn create_pi_changes_nothing() {
    let env = TestEnv::new();
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    let engine = Arc::new(ScriptedEngine::new(Vec::new()).with_limits(16_384, 1024));
    let s = open(&env, engine, store, Open::New);
    assert_eq!(s.inner().stats.profile, Profile::Pi);
    assert_eq!(s.inner().tune, ToolTune::default());
    assert_eq!(names(&s), SEVEN);
    assert_eq!(s.inner().compact.keep_tokens, 4000);
}

#[test]
fn an_explicit_keep_recent_tokens_wins() {
    let env = TestEnv::new();
    write_settings(
        &env,
        r#"{"oxa": {"profile": "local"}, "compaction": {"keepRecentTokens": 3000}}"#,
    );
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    let engine = Arc::new(ScriptedEngine::new(Vec::new()).with_limits(16_384, 1024));
    let s = open(&env, engine, store, Open::New);
    assert_eq!(s.inner().compact.keep_tokens, 3000);
}

#[test]
fn resume_takes_the_tune_of_the_branch_profile() {
    let env = TestEnv::new();
    write_settings(&env, r#"{"oxa": {"profile": "local"}}"#);
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    let engine = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("ok")]).with_limits(16_384, 1024));
    let sref = {
        let mut s = open(&env, engine.clone(), store.clone(), Open::New);
        let mut sink = VecSink::default();
        let mut ui = HeadlessUi::default();
        s.prompt("hi".to_string(), Vec::new(), &mut sink, &mut ui)
            .expect("prompt");
        s.inner().sref.clone()
    };
    // Settings now say pi: the session keeps its local epoch (and its tune, and its kept tail).
    write_settings(&env, r#"{"oxa": {"profile": "pi"}}"#);
    let s = open(&env, engine.clone(), store.clone(), Open::Resume(sref.clone()));
    assert_eq!(s.inner().stats.profile, Profile::Local);
    assert_eq!(s.inner().tune, local_tune(200, 5898));
    assert_eq!(s.inner().compact.keep_tokens, 2000);
    assert_eq!(names(&s), SEVEN);
}

#[test]
fn resume_of_a_pi_branch_keeps_the_pi_tune_under_local_settings() {
    let env = TestEnv::new();
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    let engine = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("ok")]).with_limits(16_384, 1024));
    let sref = {
        let mut s = open(&env, engine.clone(), store.clone(), Open::New);
        let mut sink = VecSink::default();
        let mut ui = HeadlessUi::default();
        s.prompt("hi".to_string(), Vec::new(), &mut sink, &mut ui)
            .expect("prompt");
        s.inner().sref.clone()
    };
    write_settings(&env, r#"{"oxa": {"profile": "local"}}"#);
    let s = open(&env, engine, store, Open::Resume(sref));
    assert_eq!(s.inner().stats.profile, Profile::Pi);
    assert_eq!(s.inner().tune, ToolTune::default());
    assert_eq!(s.inner().compact.keep_tokens, 4000);
}

#[test]
fn an_epoch_retunes_the_session() {
    let env = TestEnv::new();
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    let engine = Arc::new(ScriptedEngine::new(Vec::new()).with_limits(16_384, 1024));
    let mut s = open(&env, engine, store, Open::New);
    assert_eq!(s.inner().tune, ToolTune::default());
    write_settings(&env, r#"{"oxa": {"profile": "local"}}"#);
    let mut sink = VecSink::default();
    let mut ui = HeadlessUi::default();
    assert!(s.reload(&mut sink, &mut ui).expect("reload"), "a new epoch");
    assert_eq!(s.inner().tune, local_tune(200, 5898));
    // T4 (four tools on a small context) is NOT applied: CLI-032 keeps the seven built-ins.
    assert_eq!(names(&s), SEVEN);
    assert_eq!(s.inner().compact.keep_tokens, 2000);
}
