//! Yolo power plan S1: each session gets its own owner confirm socket directory, so two
//! sessions in one process never unlink or answer for each other's broker.
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
use std::path::PathBuf;
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

/// Every live `confirm-*/confirm.sock` under `tmp`, sorted.
fn sockets_under(tmp: &std::path::Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(tmp)
        .expect("tmp dir")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("confirm-"))
        .map(|entry| entry.path().join("confirm.sock"))
        .filter(|path| path.exists())
        .collect();
    found.sort();
    found
}

/// One question through `sock`, answered by whatever broker (if any) is bound there.
fn ask(sock: &std::path::Path) -> oxa_core::owner_confirm::Reply {
    oxa_core::owner_confirm::ask(sock, &oxa_core::owner_confirm::Request::ask("Push", "Push?"))
}

/// Two SDK sessions in one process share `Dirs`: their brokers must stay separate, and a
/// standard session must never receive the second session's yolo answer.
#[test]
fn two_sessions_in_one_process_keep_their_own_brokers() {
    let env = TestEnv::new();
    let dirs = Dirs::under(env.root());

    let mut first = Session::open(deps(&env), Open::New).expect("open");
    first
        .enable_owner_confirm(YoloSwitch::new(Autonomy::Standard))
        .expect("broker");
    let first_socks = sockets_under(&dirs.tmp_dir);
    assert_eq!(first_socks.len(), 1, "the first session owns one socket");
    let first_sock = first_socks[0].clone();

    let mut second = Session::open(deps(&env), Open::New).expect("open");
    second
        .enable_owner_confirm(YoloSwitch::new(Autonomy::Yolo))
        .expect("broker");

    let both = sockets_under(&dirs.tmp_dir);
    assert_eq!(both.len(), 2, "each session binds its own socket: {both:?}");
    assert!(
        both.contains(&first_sock),
        "the second broker never unlinks the first session's socket"
    );

    // The standard session's own broker answers its own question: `none`, because nobody is
    // serving its dialog. It must not reach the second session's yolo broker and answer yes.
    let answer = ask(&first_sock);
    assert_ne!(
        answer.verdict,
        oxa_core::owner_confirm::Verdict::Yes,
        "a standard session must never inherit a second session's yolo answer"
    );
}
