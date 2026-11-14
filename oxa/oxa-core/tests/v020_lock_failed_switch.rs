//! A failed switch never unlocks the session still in use: hold A, fail to resume B (locked
//! elsewhere; unreadable), and A stays locked while B is untouched.
//! Isolation: `oxa_fake::TestEnv` for every file and directory.

use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{HeadlessUi, VecSink};
use oxa_resources::Resources;
use oxa_sandbox::NoSandbox;
use oxa_session::{FsStore, SessionStore, LOCKED};
use std::sync::Arc;

fn open(env: &TestEnv, store: Arc<dyn SessionStore>) -> Session {
    let deps = Deps {
        engine: Arc::new(ScriptedEngine::new(Vec::new())),
        store,
        tools: Vec::new(),
        hooks: Vec::new(),
        policy: Box::new(ModePolicy::new()),
        sandbox: Arc::new(NoSandbox),
        settings: Settings::default(),
        dirs: Dirs::under(env.root()),
        cwd: env.work(),
        selection: ToolSelection::default(),
        resources: Resources::default(),
        model: None,
        thinking: None,
        mode: None,
        session_id: None,
    };
    Session::open(deps, Open::New).expect("open")
}

fn fs(env: &TestEnv) -> FsStore {
    FsStore::new(env.root().join("sessions"))
}

fn switch(s: &mut Session, b: oxa_session::SessionRef) -> Result<(), oxa_proto::OxaError> {
    s.switch(
        Open::Resume(b),
        &mut VecSink::default(),
        &mut HeadlessUi::default(),
    )
}

#[test]
fn a_failed_switch_keeps_the_current_session_locked() {
    let env = TestEnv::new();
    let store = fs(&env);
    let mut s = open(&env, Arc::new(store.clone()));
    let a = store.list(None).pop().expect("session A").sref;

    // Case 1: B is locked by another holder.
    let b = store.create(&env.work(), Some("sessionb"), None).unwrap();
    let other = fs(&env);
    other.acquire(&b).unwrap();
    let err = switch(&mut s, b.clone()).expect_err("B is locked elsewhere");
    assert_eq!(err.code, LOCKED);
    assert_eq!(fs(&env).acquire(&a).expect_err("A still locked").code, LOCKED);

    // Case 2: B is unreadable (its directory is gone): the switch fails after B's lock is taken.
    let gone = env.root().join("gone");
    let c = store.create(&gone, Some("sessionc"), None).unwrap();
    assert!(switch(&mut s, c.clone()).is_err());
    assert_eq!(fs(&env).acquire(&a).expect_err("A still locked").code, LOCKED);
    // B (held by `other`) and C (never entered) are untouched: C is free, B still held.
    fs(&env).acquire(&c).expect("C's lock was released");
    assert_eq!(
        fs(&env).acquire(&b).expect_err("B still held by its owner").code,
        LOCKED
    );
}
