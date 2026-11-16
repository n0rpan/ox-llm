//! #564/#567: headless tools can read their own truncated output, not shared temporary files.
//! All paths, tools and engine replies are synthetic and isolated under TestEnv.
use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::{ModePolicy, PathGuard};
use oxa_proto::{HeadlessUi, Mode, Role, VecSink};
use oxa_resources::Resources;
use oxa_sandbox::NoSandbox;
use oxa_session::{MemStore, SessionStore};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn open(env: &TestEnv, mode: Mode) -> (Session, Arc<ScriptedEngine>, Arc<MemStore>) {
    let mut dirs = Dirs::under(env.root());
    dirs.tmp_dir = dirs.agent_dir.join("tmp");
    let engine = Arc::new(ScriptedEngine::new(Vec::new()));
    let store = Arc::new(MemStore::new());
    let guard = PathGuard::new(&dirs.home, &dirs.agent_dir, &dirs.tmp_dir);
    let session = Session::open(
        Deps {
            engine: engine.clone(),
            store: store.clone(),
            tools: oxa_tools::builtin_tools(),
            hooks: Vec::new(),
            policy: Box::new(ModePolicy::new().with_guard(guard)),
            sandbox: Arc::new(NoSandbox),
            settings: Settings::default(),
            dirs,
            cwd: env.work(),
            selection: ToolSelection::default(),
            resources: Resources::default(),
            model: None,
            thinking: None,
            mode: Some(mode),
            session_id: None,
            jobs: oxa_core::fresh_jobs(),
        },
        Open::New,
    )
    .unwrap();
    (session, engine, store)
}

fn spill(env: &TestEnv, session: &Session) -> PathBuf {
    let dir = env.agent_dir().join("tmp").join(session.state().session_id);
    let raw = format!("spill-marker\n{}", "filler\n".repeat(3000));
    let shaped = oxa_tools::shape(raw.as_bytes(), 1024, false, &dir);
    let path = shaped
        .split("full output in ")
        .nth(1)
        .unwrap()
        .split(']')
        .next()
        .unwrap();
    let path = PathBuf::from(path);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), raw);
    path
}

fn call(session: &mut Session, engine: &ScriptedEngine, tool: &str, path: &Path) -> String {
    engine.push(ScriptTurn::tool(
        "inspect",
        tool,
        &serde_json::json!({"path": path, "pattern": "spill-marker", "limit": 1}).to_string(),
    ));
    engine.push(ScriptTurn::text("done"));
    session
        .prompt(
            "inspect".into(),
            Vec::new(),
            &mut VecSink::default(),
            &mut HeadlessUi::default(),
        )
        .unwrap();
    engine
        .requests()
        .last()
        .unwrap()
        .messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Tool)
        .unwrap()
        .content
        .clone()
        .unwrap()
}

#[test]
fn headless_spill_reads_are_limited_to_the_active_session() {
    for mode in [Mode::Plan, Mode::Workspace, Mode::Ask] {
        let env = TestEnv::new();
        let (mut session, engine, _) = open(&env, mode);
        let own = spill(&env, &session);
        for tool in ["read", "grep"] {
            let out = call(&mut session, &engine, tool, &own);
            assert!(out.contains("spill-marker"), "{mode:?} {tool}: {out}");
        }
        for rel in [
            "tmp/other-session/out.txt",
            "tmp/codemode/source.js",
            "tmp/loose.txt",
        ] {
            let path = env.agent_dir().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "spill-marker").unwrap();
            for tool in ["read", "grep"] {
                let out = call(&mut session, &engine, tool, &path);
                assert!(out.contains("declined"), "{mode:?} {tool} {rel}: {out}");
                assert!(!out.contains("spill-marker"), "{out}");
            }
        }
    }
}

#[test]
fn switching_and_resuming_replaces_the_spill_read_exception() {
    let env = TestEnv::new();
    let (mut session, engine, store) = open(&env, Mode::Ask);
    let first = spill(&env, &session);
    let first_ref = store.list(Some(&env.work())).into_iter().next().unwrap().sref;
    session
        .switch(Open::New, &mut VecSink::default(), &mut HeadlessUi::default())
        .unwrap();
    session.set_mode(Mode::Ask, &mut VecSink::default());
    let second = spill(&env, &session);
    assert_ne!(first, second);
    assert!(call(&mut session, &engine, "read", &first).contains("declined"));
    assert!(call(&mut session, &engine, "read", &second).contains("spill-marker"));
    session
        .switch(
            Open::Resume(first_ref),
            &mut VecSink::default(),
            &mut HeadlessUi::default(),
        )
        .unwrap();
    assert!(call(&mut session, &engine, "read", &second).contains("declined"));
    assert!(call(&mut session, &engine, "read", &first).contains("spill-marker"));
}
