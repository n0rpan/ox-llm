//! Wave H, lane cache (OXA-EXACT-COUNTS): where oxa needs a token count of a wire it has not
//! sent yet (the model-switch guard after a rebase and its post-compaction fit check, the
//! leaf-move notice), it asks the engine for the exact count (`Engine::count_prompt`) and falls
//! back to the estimate only when the engine cannot count.

use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{cache_bytes, ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{Engine, HeadlessUi, VecSink};
use oxa_resources::Resources;
use oxa_sandbox::NoSandbox;
use oxa_session::{MemStore, SessionStore};
use std::sync::Arc;

fn open(env: &TestEnv, engine: Arc<dyn Engine>) -> Session {
    let dirs = Dirs::under(env.root());
    let path = dirs.settings_file();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        r#"{"compaction": {"keepRecentTokens": 1}, "cacheWarming": "off"}"#,
    )
    .unwrap();
    let settings = Settings::load(&dirs, &env.work(), false);
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    Session::open(
        Deps {
            engine,
            store,
            tools: Vec::new(),
            hooks: Vec::new(),
            policy: Box::new(ModePolicy::new()),
            sandbox: Arc::new(NoSandbox),
            settings,
            dirs,
            cwd: env.work(),
            selection: ToolSelection::default(),
            resources: Resources::default(),
            model: None,
            thinking: None,
            mode: None,
            session_id: None,
        },
        Open::New,
    )
    .expect("open")
}

#[test]
fn switch_plan_after_a_rebase_uses_the_exact_count() {
    let env = TestEnv::new();
    let eng = Arc::new(ScriptedEngine::new(vec![
        ScriptTurn::text("first reply"),
        ScriptTurn::text("second reply"),
        ScriptTurn::text("SUMMARY"),
    ]));
    let engine: Arc<dyn Engine> = eng.clone();
    let mut s = open(&env, engine);
    let mut sink = VecSink::default();
    s.prompt(
        "one".to_string(),
        Vec::new(),
        &mut sink,
        &mut HeadlessUi::default(),
    )
    .unwrap();
    s.prompt(
        "two".to_string(),
        Vec::new(),
        &mut sink,
        &mut HeadlessUi::default(),
    )
    .unwrap();
    s.compact(None, &mut sink, &mut HeadlessUi::default())
        .expect("compact");
    assert_eq!(s.inner().transcript.context(), 0, "unknown until the next reply");
    let st = s.inner();
    let exact = cache_bytes(
        &st.prefix.system_text(),
        &st.prefix.tools_json,
        st.transcript.wire(),
    )
    .len() as u32;
    let plan = s.plan_model_switch("fake-model");
    assert_eq!(
        plan.context_tokens, exact,
        "the engine's own count, framing included"
    );
}
