//! The environment note (`oxa.envNote`, on by default): the date and the session
//! facts reach the model without touching the frozen prefix, and every request's history is a
//! byte prefix of the next one's (the cache stays exact, C3/W1). Isolation: `TestEnv` only.

use oxa_config::{Dirs, Settings};
use oxa_core::envnote::{strip_note, ENV_OPEN, TIME_NOTE};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{EntryBody, HeadlessUi, Part, Role, VecSink};
use oxa_resources::Resources;
use oxa_sandbox::NoSandbox;
use oxa_session::{decode_entry, encode_entry, FsStore, MemStore, SessionStore};
use serde_json::json;
use std::sync::Arc;

fn open(env: &TestEnv, eng: Arc<ScriptedEngine>, settings: Settings) -> Session {
    open_in(env, eng, settings, Arc::new(MemStore::new()), Open::New)
}

fn open_in(
    env: &TestEnv,
    eng: Arc<ScriptedEngine>,
    settings: Settings,
    store: Arc<dyn SessionStore>,
    how: Open,
) -> Session {
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
        jobs: oxa_core::fresh_jobs(),
    };
    Session::open(deps, how).expect("open")
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
fn notes_are_appended_and_history_bytes_never_change() {
    let reqs = run(Settings::default());
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
    assert_eq!(
        users[1].split_once("\n\n").map(|(typed, _)| typed),
        Some("and the weather?")
    );
    // The cost, printed for the record (bytes; the engine tokenizer is used in the PR notes).
    eprintln!(
        "env block {} bytes, time note {} bytes",
        users[0].len() - "what's the latest news in norway?\n\n".len(),
        users[1].len() - "and the weather?\n\n".len()
    );
}

#[test]
fn the_off_switch_sends_the_typed_text_only() {
    let reqs = run(Settings::from_values(
        json!({"oxa": {"envNote": false}}),
        json!({}),
    ));
    assert_eq!(
        user_texts(&reqs[2]),
        ["what's the latest news in norway?", "and the weather?", "thanks"]
    );
}

/// A user message typed to look like a note, ending in an `<env>` block or a time line.
const LITERAL_ENV: &str = "Explain this format:\n\n<env>\nexample\n</env>";
const LITERAL_TIME: &str = "what is this line?\n\n[local time 13:42 CET]";

/// The stored user messages of `s` (wire text, parts), in order.
fn stored_users(s: &Session) -> Vec<(String, Vec<Part>, String)> {
    s.inner()
        .tree
        .entries()
        .iter()
        .filter_map(|e| match &e.body {
            EntryBody::Msg { wire, parts, .. } if wire.role == Role::User => Some((
                wire.content.clone().unwrap_or_default(),
                parts.clone(),
                e.id.clone(),
            )),
            _ => None,
        })
        .collect()
}

/// Codex T2 review M1: the refill (fork before / `/tree` to a user message) gives back the
/// typed text. A generated note is recorded with its entry, survives the session file (JSONL
/// encode/decode, `--fork` of the file) and is the only text dropped; a typed text that merely
/// ends like a note keeps that ending.
#[test]
fn generated_note_round_trips_and_only_it_is_stripped() {
    let env = TestEnv::new();
    let typed = ["what's the latest news in norway?", LITERAL_ENV, LITERAL_TIME];
    let eng = Arc::new(ScriptedEngine::new(vec![
        ScriptTurn::text("one"),
        ScriptTurn::text("two"),
        ScriptTurn::text("three"),
    ]));
    let store: Arc<dyn SessionStore> = Arc::new(FsStore::new(env.root().join("sessions")));
    let local = Settings::default;
    let mut s = open_in(&env, eng.clone(), local(), store.clone(), Open::New);
    let (mut sink, mut ui) = (VecSink::default(), HeadlessUi::default());
    for p in typed {
        s.prompt(p.into(), Vec::new(), &mut sink, &mut ui)
            .expect("prompt");
    }
    // `/resume` reads SessionStore::list.
    let listed = store.list(Some(&env.work()));
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].first_message, typed[0]);
    assert_eq!(
        listed[0].all_text,
        format!("{}\none\n{}\ntwo\n{}\nthree", typed[0], typed[1], typed[2])
    );
    let users = stored_users(&s);
    assert_eq!(users.len(), 3);
    for ((content, parts, _), t) in users.iter().zip(typed) {
        let notes: Vec<&Part> = parts.iter().filter(|p| matches!(p, Part::EnvNote(_))).collect();
        assert_eq!(notes.len(), 1, "one recorded note per prompt: {parts:?}");
        let Part::EnvNote(note) = notes[0] else {
            unreachable!()
        };
        // The wire bytes are the typed text + the recorded note (unchanged by the record).
        assert_eq!(content, &format!("{t}\n\n{note}"));
        assert_eq!(strip_note(content, parts), t);
    }
    // The session file keeps the record: every entry decodes to itself.
    for e in s.inner().tree.entries() {
        let mut line = Vec::new();
        encode_entry(e, &mut line);
        assert_eq!(&decode_entry(&line).expect("decode"), e);
    }
    // A fork of the file (decoded from JSONL) refills the editor with the typed text.
    let sref = s.inner().sref.clone();
    drop(s);
    let mut forked = open_in(&env, eng, local(), store, Open::Fork(sref));
    for ((_, _, id), t) in stored_users(&forked).into_iter().zip(typed).rev() {
        let refill = forked
            .navigate(&id, false, None, &mut sink, &mut ui)
            .expect("navigate to the user message");
        assert_eq!(refill.as_deref(), Some(t));
    }
}

/// Codex T2 review M1, the other half: with the note off (here `envNote: false`; also sessions
/// written before the record existed) nothing is recorded, and a typed
/// text that ends like a note comes back whole.
#[test]
fn note_off_refills_note_shaped_text_unchanged() {
    let env = TestEnv::new();
    let eng = Arc::new(ScriptedEngine::new(vec![
        ScriptTurn::text("one"),
        ScriptTurn::text("two"),
    ]));
    let mut s = open(
        &env,
        eng,
        Settings::from_values(json!({"oxa": {"envNote": false}}), json!({})),
    );
    let (mut sink, mut ui) = (VecSink::default(), HeadlessUi::default());
    for p in [LITERAL_ENV, LITERAL_TIME] {
        s.prompt(p.into(), Vec::new(), &mut sink, &mut ui)
            .expect("prompt");
    }
    let users = stored_users(&s);
    assert_eq!(
        users.iter().map(|(c, _, _)| c.as_str()).collect::<Vec<_>>(),
        [LITERAL_ENV, LITERAL_TIME]
    );
    assert!(users.iter().all(|(_, parts, _)| parts.is_empty()));
    assert!(
        TIME_NOTE.starts_with("[local time "),
        "the literal mimics the real note"
    );
    for (content, parts, id) in users.into_iter().rev() {
        assert_eq!(strip_note(&content, &parts), content);
        let refill = s
            .navigate(&id, false, None, &mut sink, &mut ui)
            .expect("navigate to the user message");
        assert_eq!(refill.as_deref(), Some(content.as_str()));
    }
}
