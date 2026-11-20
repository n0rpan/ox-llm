use oxa_cli::{with_lead_owner, LeadOwnerBinding};
use oxa_config::{Dirs, Env};
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
use oxa_proto::{HeadlessUi, VecSink};
use oxa_session::{FsStore, Goal, SessionStore};
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
};

fn setup(test: &TestEnv) -> (Dirs, u32) {
    let dirs = Dirs::under(test.root());
    fs::create_dir_all(&dirs.agent_dir).unwrap();
    fs::set_permissions(&dirs.agent_dir, fs::Permissions::from_mode(0o700)).unwrap();
    (dirs, fs::metadata(test.root()).unwrap().uid())
}

fn select(dirs: &Dirs, id: &str) {
    fs::write(
        dirs.settings_file(),
        serde_json::json!({"oxa":{"lead":{"sessionId":id},"goal":{"idlePause":true}}}).to_string(),
    )
    .unwrap();
    fs::set_permissions(dirs.settings_file(), fs::Permissions::from_mode(0o600)).unwrap();
}

#[test]
fn owner_binding_refuses_project_settings_prefixes_and_duplicate_ids() {
    let test = TestEnv::new();
    let (dirs, uid) = setup(&test);
    let cwd = test.work().canonicalize().unwrap();
    let store = FsStore::new(dirs.sessions_dir.clone());
    store.create(&cwd, Some("owner-exact"), None).unwrap();
    test.write(
        "work/.oxa/settings.json",
        r#"{"oxa":{"lead":{"sessionId":"owner-exact"}}}"#,
    );
    assert!(LeadOwnerBinding::load(&dirs, &cwd, uid).unwrap().is_none());
    for id in ["owner", "../owner-exact", "owner-exact"] {
        select(&dirs, id);
        assert!(LeadOwnerBinding::load(&dirs, &cwd, uid).is_err());
    }
    let id = LeadOwnerBinding::create(&dirs, &cwd, uid).unwrap();
    select(&dirs, &id);
    let owner = store.resolve(&id).unwrap();
    assert!(LeadOwnerBinding::load(&dirs, &cwd, uid.wrapping_add(1)).is_err());
    assert!(LeadOwnerBinding::load(&dirs, &cwd, uid).unwrap().is_some());
    let duplicate = owner.path.with_file_name("duplicate.jsonl");
    fs::copy(&owner.path, duplicate).unwrap();
    assert!(LeadOwnerBinding::load(&dirs, &cwd, uid).is_err());
}

#[test]
fn owner_turn_uses_the_selected_writer_and_sends_slash_and_shell_text_literally() {
    let test = TestEnv::new();
    let (dirs, uid) = setup(&test);
    let cwd = test.work().canonicalize().unwrap();
    let store = FsStore::new(dirs.sessions_dir.clone());
    let id = LeadOwnerBinding::create(&dirs, &cwd, uid).unwrap();
    let owner = store.resolve(&id).unwrap();
    store.create(&cwd, Some("other-session"), None).unwrap();
    let goal = Goal::new("Review the synthetic work.".into(), Default::default(), 1).unwrap();
    select(&dirs, &owner.id);
    let binding = LeadOwnerBinding::load(&dirs, &cwd, uid).unwrap().unwrap();
    test.write("work/.oxa/extensions/local.js", "export default function () {}");
    test.write(
        "home/.oxa/agent/extensions/trust.js",
        "export default function (oxa) { oxa.on('project_trust', (e, ctx) => { ctx.fs.writeFile('trust-fired.txt', 'fixture'); }); }",
    );
    let server = FakeServer::start(
        vec![
            ScriptTurn::text("First reply."),
            ScriptTurn::text("Second reply."),
        ],
        FakeOpts::default(),
    )
    .unwrap();
    let pairs = test.envs(&server.url());
    let env = Env::from_pairs(
        &pairs
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect::<Vec<_>>(),
    );
    let writer = FsStore::new(dirs.sessions_dir.clone());
    writer.acquire(&owner).unwrap();
    writer.commit_lock();
    let busy = with_lead_owner::<()>(&binding, &env, true, |_| panic!("locked Session opened")).unwrap_err();
    assert_eq!(busy.code, "busy");
    assert!(server.requests().is_empty());
    drop(writer);
    let offline = Env::from_pairs(&[("OXA_OFFLINE", "1")]);
    let error = with_lead_owner::<()>(&binding, &offline, true, |_| {
        panic!("uninstalled offline policy accepted")
    })
    .unwrap_err();
    assert_eq!(error.code, oxa_proto::codes::OFFLINE);
    assert!(server.requests().is_empty());

    let inputs = ["/reload", "!printf synthetic"];
    with_lead_owner(&binding, &env, true, |session| {
        assert_eq!(session.inner().sref, owner);
        assert!(session.goal().is_none());
        session.set_goal(Some(goal.clone()), &mut VecSink::default())
    })
    .unwrap();
    assert!(!cwd.join("trust-fired.txt").exists());
    with_lead_owner(&binding, &env, true, |session| {
        assert_eq!(session.goal(), Some(goal));
        for text in inputs {
            session.prompt_literal(text.into(), &mut VecSink::default(), &mut HeadlessUi::default())?;
        }
        Ok(())
    })
    .unwrap();
    let requests = server.requests();
    for text in inputs {
        assert!(requests.iter().any(|request| request.max_tokens > 0
            && request
                .messages
                .iter()
                .any(|message| message.content.as_deref() == Some(text))));
    }
    // M1: the live owner Lead session declares the worker tool and nothing else.
    assert!(requests.iter().all(|request| {
        let tools: serde_json::Value = serde_json::from_str(&request.tools_json).unwrap();
        let text = tools.to_string();
        text.contains("\"worker\"") && !text.contains("\"bash\"") && !text.contains("\"read\"")
    }));

    let original = owner.path.with_extension("previous");
    fs::rename(&owner.path, &original).unwrap();
    fs::copy(original, &owner.path).unwrap();
    let error =
        with_lead_owner::<()>(&binding, &env, true, |_| panic!("replaced Session opened")).unwrap_err();
    assert_eq!(error.code, "lead_owner_unavailable");
    assert_eq!(server.requests().len(), requests.len());
}

#[test]
fn owner_turn_resolves_the_user_only_explicit_lead_model_route() {
    let test = TestEnv::new();
    let (dirs, uid) = setup(&test);
    let cwd = test.work().canonicalize().unwrap();
    let store = FsStore::new(dirs.sessions_dir.clone());
    let id = LeadOwnerBinding::create(&dirs, &cwd, uid).unwrap();
    let owner = store.resolve(&id).unwrap();
    select(&dirs, &owner.id);
    let binding = LeadOwnerBinding::load(&dirs, &cwd, uid).unwrap().unwrap();
    let opts = oxa_fake::FakeOpts {
        models: vec![
            oxa_fake::FakeModel::moe("fake-model", 1 << 16),
            oxa_fake::FakeModel::moe("synthetic-go-route", 1 << 16),
        ],
        ..FakeOpts::default()
    };
    let server = FakeServer::start(
        vec![
            ScriptTurn::text("First."),
            ScriptTurn::text("Second."),
            ScriptTurn::text("Third."),
        ],
        opts,
    )
    .unwrap();
    let pairs = test.envs(&server.url());
    let env = Env::from_pairs(
        &pairs
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect::<Vec<_>>(),
    );
    let prompt = |binding: &LeadOwnerBinding| {
        with_lead_owner(binding, &env, true, |session| {
            session.prompt_literal(
                "route question".into(),
                &mut VecSink::default(),
                &mut HeadlessUi::default(),
            )
        })
    };

    // Unset: the ordinary startup model, unchanged.
    prompt(&binding).unwrap();
    assert_eq!(server.requests()[0].model, "fake-model");

    // A repository project file never supplies the route.
    test.write(
        "work/.oxa/settings.json",
        r#"{"oxa":{"lead":{"model":"project-injected-model"}}}"#,
    );
    prompt(&binding).unwrap();
    assert_eq!(server.requests()[1].model, "fake-model");

    // The user-only explicit route resolves exactly through the normal selector.
    fs::write(
        dirs.settings_file(),
        serde_json::json!({"oxa":{"lead":{"sessionId":owner.id,"model":"synthetic-go-route"}}}).to_string(),
    )
    .unwrap();
    prompt(&binding).unwrap();
    assert_eq!(server.requests()[2].model, "synthetic-go-route");

    fs::write(
        dirs.settings_file(),
        serde_json::json!({"oxa":{"lead":{"sessionId":owner.id,"model":"synthetic-go"}}}).to_string(),
    )
    .unwrap();
    let error = prompt(&binding).unwrap_err();
    assert_eq!(error.code, oxa_proto::codes::NOT_FOUND);
    assert_eq!(server.requests().len(), 3);

    // A route the selector cannot resolve is reported as itself: no request, no
    // silent fallback to the startup model.
    fs::write(
        dirs.settings_file(),
        serde_json::json!({"oxa":{"lead":{"sessionId":owner.id,"model":"absent-model"}}}).to_string(),
    )
    .unwrap();
    let error = prompt(&binding).unwrap_err();
    assert_eq!(error.code, oxa_proto::codes::NOT_FOUND);
    assert!(error.message.contains("absent-model"), "{}", error.message);
    assert!(
        error
            .hint
            .as_deref()
            .is_some_and(|hint| hint.contains("synthetic-go-route")),
        "{}",
        error.hint.unwrap_or_default()
    );
    assert_eq!(server.requests().len(), 3);
}
