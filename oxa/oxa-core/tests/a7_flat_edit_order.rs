//! A7 agent-loop coverage: dependent flat `edit` calls in one reply.
//!
//! Three flat calls edit the same file in one assistant reply. Each call is matched against the
//! file as the previous call left it, so they land in order. Isolation: every file lives under
//! `oxa_fake::TestEnv`.

use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{HeadlessUi, Role, ToolCall, VecSink};
use oxa_resources::{discover, DiscoverOpts, Resources};
use oxa_sandbox::NoSandbox;
use oxa_session::{MemStore, SessionStore};
use oxa_tools::descriptions;
use std::sync::Arc;

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

fn open(env: &TestEnv, engine: Arc<ScriptedEngine>, store: Arc<dyn SessionStore>, how: Open) -> Session {
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
            selection: ToolSelection::default(),
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

fn prompt(s: &mut Session, text: &str) {
    let mut sink = VecSink::default();
    let mut ui = HeadlessUi::default();
    s.prompt(text.to_string(), Vec::new(), &mut sink, &mut ui)
        .expect("prompt");
}

#[test]
fn three_flat_edits_of_one_file_in_one_reply_apply_in_order() {
    let env = TestEnv::new();
    write_settings(&env, "{}");
    env.write("work/f.txt", "v0\n");
    let calls: Vec<ToolCall> = [("v0", "v1"), ("v1", "v2"), ("v2", "v3")]
        .iter()
        .enumerate()
        .map(|(i, (old, new))| ToolCall {
            id: format!("c{i}"),
            name: "edit".to_string(),
            args_raw: serde_json::json!({"path": "f.txt", "oldText": old, "newText": new}).to_string(),
        })
        .collect();
    let mut turn = ScriptTurn::tool("x", "edit", "{}");
    turn.tool_calls = calls;
    let engine = Arc::new(ScriptedEngine::new(vec![turn, ScriptTurn::text("done")]));
    let mut s = open(&env, engine.clone(), Arc::new(MemStore::new()), Open::New);
    prompt(&mut s, "edit it");
    // Each call matched the file as the previous call left it.
    assert_eq!(std::fs::read_to_string(env.work().join("f.txt")).unwrap(), "v3\n");
    let reqs = engine.requests();
    let results: Vec<(&str, &str)> = reqs[1]
        .messages
        .iter()
        .filter(|m| m.role == Role::Tool)
        .map(|m| (m.tool_call_id.as_deref().unwrap(), m.content.as_deref().unwrap()))
        .collect();
    let ok = descriptions::errors::edit_ok(1, "f.txt");
    assert_eq!(
        results,
        vec![("c0", ok.as_str()), ("c1", ok.as_str()), ("c2", ok.as_str())]
    );
}
