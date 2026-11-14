//! `oxa.profile` in the core (OXA-LOCAL-FIRST sec. 6, T1): read once per epoch and stored with
//! it; `pi` changes no byte; `local` declares `edit` flat; a resumed session keeps its profile.

use oxa_config::{Dirs, Settings};
use oxa_core::{
    build_prefix, build_prefix_with, declared_specs, resolve_toolset, Deps, Open, PrefixOpts, Session,
    ToolSelection, PROFILE_ENTRY,
};
use oxa_fake::{ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{EntryBody, HeadlessUi, Profile, Role, ToolCall, ToolSpec, VecSink};
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

fn markers(s: &Session) -> usize {
    let st = s.inner();
    st.store
        .load(&st.sref)
        .unwrap()
        .iter()
        .filter(|e| matches!(&e.body, EntryBody::Custom { custom_type, .. } if custom_type == PROFILE_ENTRY))
        .count()
}

/// The prefix the built-ins gave before profiles existed: the resolved specs, undeclared.
fn todays_prefix(env: &TestEnv) -> oxa_proto::FrozenPrefix {
    let dirs = Dirs::under(env.root());
    let settings = Settings::load(&dirs, &env.work(), true);
    let (active, _) = resolve_toolset(&oxa_tools::builtin_tools(), &ToolSelection::default());
    let specs: Vec<ToolSpec> = active.specs();
    build_prefix(&env.work(), &resources(env, &settings), &specs, &[])
}

#[test]
fn pi_first_request_bytes_are_todays() {
    for settings in [None, Some(r#"{"oxa": {"profile": "pi"}}"#)] {
        let env = TestEnv::new();
        if let Some(json) = settings {
            write_settings(&env, json);
        }
        let engine = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("ok")]));
        let mut s = open(&env, engine.clone(), Arc::new(MemStore::new()), Open::New);
        prompt(&mut s, "hi");
        let today = todays_prefix(&env);
        let r = &engine.requests()[0];
        assert_eq!(r.system, today.system_text(), "system bytes under pi");
        assert_eq!(r.tools_json, today.tools_json, "tools bytes under pi");
        assert_eq!(s.inner().prefix.sha256, today.sha256);
        assert!(r.tools_json.contains(descriptions::EDIT.schema_json));
        assert_eq!(s.state().profile, Profile::Pi);
        assert_eq!(s.stats().profile, Profile::Pi);
        assert_eq!(markers(&s), 0, "a pi epoch writes no profile entry");
    }
}

/// The prefix of a `local` epoch with the output diet on (the default under `local`): the
/// resolved specs with their profile `local` declarations.
fn local_prefix(env: &TestEnv) -> oxa_proto::FrozenPrefix {
    let dirs = Dirs::under(env.root());
    let settings = Settings::load(&dirs, &env.work(), true);
    let (active, _) = resolve_toolset(&oxa_tools::builtin_tools(), &ToolSelection::default());
    let specs = declared_specs(active.specs(), Profile::Local);
    let opts = PrefixOpts {
        profile: Profile::Local,
        diet_output: settings.diet_output(Profile::Local),
        child: false,
    };
    build_prefix_with(&env.work(), &resources(env, &settings), &specs, &[], &opts)
}

#[test]
fn local_declares_its_tools_flat_and_stores_the_profile() {
    let env = TestEnv::new();
    write_settings(&env, r#"{"oxa": {"profile": "local"}}"#);
    let engine = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("ok")]));
    let mut s = open(&env, engine.clone(), Arc::new(MemStore::new()), Open::New);
    prompt(&mut s, "hi");
    let r = &engine.requests()[0];
    let expected = local_prefix(&env);
    assert_eq!(r.system, expected.system_text(), "system bytes under local");
    assert_eq!(r.tools_json, expected.tools_json, "tools bytes under local");
    // The edit declaration is the flat one (T1), and no declaration has the `edits` array.
    assert!(r.tools_json.contains(descriptions::EDIT_FLAT.schema_json));
    assert!(!r.tools_json.contains("\"edits\""));
    assert!(!r.tools_json.contains("\"type\":\"number\""), "T3: integers only");
    assert_eq!(s.state().profile, Profile::Local);
    assert_eq!(markers(&s), 1);
    // The marker follows the prefix entry.
    let st = s.inner();
    let entries = st.store.load(&st.sref).unwrap();
    assert!(matches!(entries[0].body, EntryBody::Prefix(_)));
    assert!(
        matches!(&entries[1].body, EntryBody::Custom { custom_type, data } if custom_type == PROFILE_ENTRY && data == "local")
    );
}

#[test]
fn resume_keeps_the_profile_until_a_new_epoch() {
    let env = TestEnv::new();
    write_settings(&env, r#"{"oxa": {"profile": "local"}}"#);
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    let engine = Arc::new(ScriptedEngine::new(vec![
        ScriptTurn::text("ok"),
        ScriptTurn::text("ok2"),
    ]));
    let (sref, sha) = {
        let mut s = open(&env, engine.clone(), store.clone(), Open::New);
        prompt(&mut s, "hi");
        (s.inner().sref.clone(), s.inner().prefix.sha256)
    };
    // The user switches to pi; the session was created under local and keeps it.
    write_settings(&env, r#"{"oxa": {"profile": "pi"}}"#);
    let mut s = open(&env, engine.clone(), store.clone(), Open::Resume(sref));
    assert_eq!(s.state().profile, Profile::Local);
    assert_eq!(s.inner().prefix.sha256, sha, "the stored prefix is replayed");
    prompt(&mut s, "again");
    let reqs = engine.requests();
    assert_eq!(reqs[1].tools_json, reqs[0].tools_json);
    assert_eq!(reqs[1].system, reqs[0].system);
    // The next epoch reads the profile again.
    let mut sink = VecSink::default();
    let mut ui = HeadlessUi::default();
    assert!(s.reload(&mut sink, &mut ui).expect("reload"), "a new epoch");
    assert_eq!(s.state().profile, Profile::Pi);
    assert_eq!(s.inner().prefix.sha256, todays_prefix(&env).sha256);
}

#[test]
fn three_flat_edits_of_one_file_in_one_reply_apply_in_order() {
    let env = TestEnv::new();
    write_settings(&env, r#"{"oxa": {"profile": "local"}}"#);
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
