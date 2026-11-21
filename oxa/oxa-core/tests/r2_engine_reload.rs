//! R2 (wave H, FIT F4): the engine reloads the model mid-session, against the
//! HTTP fake. The unload between turns is the fake's `restart` (the engine forgets every
//! session's state) with `/props` switching from "no model loaded" to loaded; the visible
//! load is a 503 `model loading` before the first byte.

use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{FakeModel, FakeOpts, FakeServer, Fault, ScriptTurn, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{AgentEvent, Engine, HeadlessUi, Level, VecSink};
use oxa_resources::Resources;
use oxa_sandbox::NoSandbox;
use oxa_session::{MemStore, SessionStore};
use std::sync::Arc;

const FAST_RETRY: &str =
    r#"{"compaction": {"enabled": false}, "retry": {"baseDelayMs": 1, "maxAgentDelayMs": 5}}"#;

fn open(env: &TestEnv, server: &FakeServer) -> Session {
    let dirs = Dirs::under(env.root());
    let path = dirs.settings_file();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, FAST_RETRY).unwrap();
    let settings = Settings::load(&dirs, &env.work(), false);
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    let engine: Arc<dyn Engine> = Arc::new(oxa_engine::HttpEngine::new(&server.url(), None).unwrap());
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
            jobs: oxa_core::fresh_jobs(),
        },
        Open::New,
    )
    .unwrap()
}

fn prompt(s: &mut Session, text: &str) -> Vec<AgentEvent> {
    let mut sink = VecSink::default();
    s.prompt(
        text.to_string(),
        Vec::new(),
        &mut sink,
        &mut HeadlessUi::default(),
    )
    .unwrap();
    sink.0
}

fn info_notices(events: &[AgentEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Notice {
                level: Level::Info,
                text,
            } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn unloaded(script: Vec<ScriptTurn>) -> FakeServer {
    FakeServer::start(
        script,
        FakeOpts {
            models: vec![FakeModel {
                alias: "fake-model".to_string(),
                cap: 1 << 20,
                max_output: None,
                caps: None,
            }],
            props_unloaded: true,
            ..FakeOpts::default()
        },
    )
    .unwrap()
}

#[test]
fn a_silent_reload_between_turns_rereads_the_capabilities_with_one_notice() {
    let env = TestEnv::new();
    let server = unloaded(vec![
        ScriptTurn::text("a"),
        ScriptTurn::text("b"),
        ScriptTurn::text("c"),
    ]);
    let mut s = open(&env, &server);
    prompt(&mut s, "one");
    assert!(
        !s.inner().info.prefill_only,
        "nothing loaded at the first re-read"
    );
    // Between the turns the model is unloaded and loaded again by the controller.
    server.restart();
    server.set_props_unloaded(false);
    let ev = prompt(&mut s, "two");
    assert_eq!(
        info_notices(&ev),
        vec![oxa_core_reloaded_text()],
        "exactly one notice"
    );
    assert!(
        s.inner().info.prefill_only,
        "capabilities read again after the reload"
    );
    // The next turn is warm again: no notice.
    let ev = prompt(&mut s, "three");
    assert!(info_notices(&ev).is_empty(), "{:?}", info_notices(&ev));
}

fn oxa_core_reloaded_text() -> String {
    "Engine reloaded: it held nothing of this session, so the whole conversation was prefilled again (the model was unloaded and loaded, or the engine restarted)\nFix: none needed: oxa read the engine's capabilities and limits again".to_string()
}

#[test]
fn a_visible_load_gives_one_notice_retries_with_the_same_key_and_rereads() {
    let env = TestEnv::new();
    let loading = || {
        ScriptTurn::fault(Fault::Http {
            status: 503,
            body: r#"{"error":{"message":"model loading","type":"unavailable_error","code":503}}"#
                .to_string(),
        })
    };
    let server = unloaded(vec![
        ScriptTurn::text("a"),
        loading(),
        loading(),
        ScriptTurn::text("b"),
    ]);
    let mut s = open(&env, &server);
    prompt(&mut s, "one");
    server.set_props_unloaded(false);
    let ev = prompt(&mut s, "two");
    let notices = info_notices(&ev);
    let loading_notices: Vec<_> = notices
        .iter()
        .filter(|n| n.starts_with("Model loading:"))
        .collect();
    assert_eq!(
        loading_notices.len(),
        1,
        "one notice for two retries: {notices:?}"
    );
    assert!(
        loading_notices[0].ends_with(
            "\nFix: none needed: oxa sends the same request again when the engine is ready; Esc cancels"
        ),
        "{}",
        loading_notices[0]
    );
    let retries = ev
        .iter()
        .filter(|e| matches!(e, AgentEvent::AutoRetryStart { .. }))
        .count();
    assert_eq!(retries, 2);
    // Every attempt of the logical request carried the same Idempotency-Key.
    let reqs = server.requests();
    let keys: Vec<&str> = reqs[1..].iter().map(|r| r.idem_key.as_str()).collect();
    assert_eq!(keys.len(), 3);
    assert!(!keys[0].is_empty());
    assert!(keys.iter().all(|k| *k == keys[0]), "{keys:?}");
    assert!(
        s.inner().info.prefill_only,
        "capabilities read again after the load"
    );
}

/// #2256: a model switch is not an engine reload. The target model legitimately holds nothing
/// of the session's wire, so the reply reports one cache miss with its real cause (`epoch`),
/// and no "Engine reloaded ... the engine restarted" notice blames the engine.
#[test]
fn a_model_switch_cold_prefill_is_not_reported_as_a_reload() {
    let env = TestEnv::new();
    let cold = || ScriptTurn {
        text: Some("b".to_string()),
        fault: Some(Fault::Cold),
        ..ScriptTurn::text("")
    };
    let server = FakeServer::start(
        vec![ScriptTurn::text("a"), cold()],
        FakeOpts {
            model: "first-model".to_string(),
            models: vec![
                FakeModel::moe("first-model", 1 << 20),
                FakeModel::moe("second-model", 1 << 20),
            ],
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let mut s = open(&env, &server);
    prompt(&mut s, "one");
    let mut sink = VecSink::default();
    s.set_model("second-model", &mut sink).expect("switch");
    let ev = prompt(&mut s, "two");
    let notices = info_notices(&ev);
    assert!(
        !notices.iter().any(|n| n.contains("Engine reloaded")),
        "a switch is not a reload: {notices:?}"
    );
    let causes: Vec<&str> = ev
        .iter()
        .filter_map(|e| match e {
            AgentEvent::CacheMiss { cause, cached, .. } => {
                assert_eq!(*cached, 0, "the target really prefilled cold");
                Some(*cause)
            }
            _ => None,
        })
        .collect();
    assert_eq!(causes, vec![oxa_proto::miss_cause::EPOCH]);
}
