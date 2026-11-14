//! The environment note (`oxa.envNote`, on under profile `local`): the date and the session
//! facts reach the model without touching the frozen prefix, and every request's history is a
//! byte prefix of the next one's (the cache stays exact, C3/W1). Isolation: `TestEnv` only.

use oxa_config::{Dirs, Settings};
use oxa_core::envnote::{strip_note, ENV_OPEN};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{HeadlessUi, Role, VecSink};
use oxa_resources::Resources;
use oxa_sandbox::NoSandbox;
use oxa_session::{MemStore, SessionStore};
use serde_json::json;
use std::sync::Arc;

fn open(env: &TestEnv, eng: Arc<ScriptedEngine>, settings: Settings) -> Session {
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    let deps = Deps {
        engine: eng,
        store,
        tools: Vec::new(),
        hooks: Vec::new(),
        policy: Box::new(ModePolicy::new()),
        sandbox: Arc::new(NoSandbox),
        settings,
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

fn run(settings: Settings) -> Vec<oxa_fake::Recorded> {
    let env = TestEnv::new();
    let eng = Arc::new(ScriptedEngine::new(vec![
        ScriptTurn::text("one"),
        ScriptTurn::text("two"),
        ScriptTurn::text("three"),
    ]));
    let mut s = open(&env, eng.clone(), settings);
    let (mut sink, mut ui) = (VecSink::default(), HeadlessUi::default());
    for p in ["what's the latest news in norway?", "and the weather?", "thanks"] {
        s.prompt(p.into(), Vec::new(), &mut sink, &mut ui)
            .expect("prompt");
    }
    eng.requests().into_iter().filter(|r| r.max_tokens > 0).collect()
}

fn user_texts(r: &oxa_fake::Recorded) -> Vec<String> {
    r.messages
        .iter()
        .filter(|m| m.role == Role::User)
        .map(|m| m.content.clone().unwrap_or_default())
        .collect()
}

#[test]
fn local_profile_notes_are_appended_and_history_bytes_never_change() {
    let reqs = run(Settings::from_values(
        json!({"oxa": {"profile": "local"}}),
        json!({}),
    ));
    assert_eq!(reqs.len(), 3);
    // The frozen prefix is the same bytes in every request and carries no clock.
    for r in &reqs {
        assert_eq!(r.system, reqs[0].system);
        assert_eq!(r.tools_json, reqs[0].tools_json);
        assert!(!r.system.contains(ENV_OPEN) && !r.system.contains("local time"));
    }
    // C3: each request's messages are a byte prefix of the next one's.
    for w in reqs.windows(2) {
        assert!(w[1].messages.len() > w[0].messages.len());
        assert_eq!(&w[1].messages[..w[0].messages.len()], &w[0].messages[..]);
    }
    let users = user_texts(&reqs[2]);
    assert_eq!(users.len(), 3);
    // The first prompt carries the full block after the typed text; later ones the time.
    assert!(users[0].starts_with("what's the latest news in norway?\n\n<env>\nDate: "));
    assert!(users[0].contains("Model: ") && users[0].ends_with("</env>"));
    for u in &users[1..] {
        let (_, note) = u.split_once("\n\n").expect("a note");
        assert!(note.starts_with("[local time ") && note.ends_with(']'), "{u}");
    }
    assert_eq!(strip_note(&users[1]), "and the weather?");
    // The cost, printed for the record (bytes; the engine tokenizer is used in the PR notes).
    eprintln!(
        "env block {} bytes, time note {} bytes",
        users[0].len() - "what's the latest news in norway?\n\n".len(),
        users[1].len() - "and the weather?\n\n".len()
    );
}

#[test]
fn pi_profile_and_the_off_switch_send_the_typed_text_only() {
    for settings in [
        Settings::default(),
        Settings::from_values(json!({"oxa": {"profile": "local", "envNote": false}}), json!({})),
    ] {
        let reqs = run(settings);
        assert_eq!(
            user_texts(&reqs[2]),
            ["what's the latest news in norway?", "and the weather?", "thanks"]
        );
    }
}
