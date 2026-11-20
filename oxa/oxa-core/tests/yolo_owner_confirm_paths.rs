//! Yolo power plan S1: each session gets its own owner confirm door (a descriptor, no path), so
//! two sessions in one process never answer for each other's broker.
//!
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{Autonomy, YoloSwitch};
use oxa_resources::Resources;
use oxa_sandbox::NoSandbox;
use oxa_session::MemStore;
use std::sync::Arc;

/// Deps over a scripted engine with no turns, an empty store and the test's own directories.
fn deps(env: &TestEnv) -> Deps {
    Deps {
        engine: Arc::new(ScriptedEngine::new(Vec::new())),
        store: Arc::new(MemStore::new()),
        tools: Vec::new(),
        hooks: Vec::new(),
        policy: Box::new(ModePolicy::new()),
        sandbox: Arc::new(NoSandbox),
        settings: Settings::from_values(
            serde_json::json!({"oxa": {"envNote": false}}),
            serde_json::json!({}),
        ),
        dirs: Dirs::under(env.root()),
        cwd: env.work(),
        selection: ToolSelection::default(),
        resources: Resources::default(),
        model: None,
        thinking: None,
        mode: None,
        session_id: None,
        jobs: oxa_core::fresh_jobs(),
    }
}

/// One question through the session's door, answered by that session's broker.
fn ask(door: std::os::fd::RawFd) -> oxa_core::owner_confirm::Reply {
    let env = oxa_config::Env::from_pairs(&[(oxa_core::owner_confirm::FD_ENV, &door.to_string())]);
    oxa_core::owner_confirm::ask(&env, &oxa_core::owner_confirm::Request::ask("Push", "Push?"))
}

/// Two SDK sessions in one process each own their door: a standard session must never receive
/// the second session's yolo answer, and the second session's yolo answer is its own.
#[test]
fn two_sessions_in_one_process_keep_their_own_brokers() {
    let env = TestEnv::new();

    let mut first = Session::open(deps(&env), Open::New).expect("open");
    first
        .enable_owner_confirm(YoloSwitch::new(Autonomy::Standard))
        .expect("broker");
    let first_door = first.owner_confirm_door().expect("door");

    let mut second = Session::open(deps(&env), Open::New).expect("open");
    second
        .enable_owner_confirm(YoloSwitch::new(Autonomy::Yolo))
        .expect("broker");
    let second_door = second.owner_confirm_door().expect("door");
    assert_ne!(first_door, second_door, "each session has its own descriptor");

    // The standard session's own broker answers its own question: `none`, because nobody is
    // serving its dialog. It must not reach the second session's yolo broker and answer yes.
    let answer = ask(first_door);
    assert_ne!(
        answer.verdict,
        oxa_core::owner_confirm::Verdict::Yes,
        "a standard session must never inherit a second session's yolo answer"
    );
    assert_eq!(
        ask(second_door).verdict,
        oxa_core::owner_confirm::Verdict::Yes,
        "the yolo session answers for itself"
    );
}
