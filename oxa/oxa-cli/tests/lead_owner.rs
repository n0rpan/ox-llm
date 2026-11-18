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
        serde_json::json!({"oxa":{"lead":{"sessionId":id}}}).to_string(),
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
    let busy = with_lead_owner::<()>(&binding, &env, |_| panic!("locked Session opened")).unwrap_err();
    assert_eq!(busy.code, "busy");
    assert!(server.requests().is_empty());
    drop(writer);
    let offline = Env::from_pairs(&[("OXA_OFFLINE", "1")]);
    let error = with_lead_owner::<()>(&binding, &offline, |_| {
        panic!("uninstalled offline policy accepted")
    })
    .unwrap_err();
    assert_eq!(error.code, oxa_proto::codes::OFFLINE);
    assert!(server.requests().is_empty());

    let inputs = ["/reload", "!printf synthetic"];
    with_lead_owner(&binding, &env, |session| {
        assert_eq!(session.inner().sref, owner);
        assert!(session.goal().is_none());
        session.set_goal(Some(goal.clone()), &mut VecSink::default())
    })
    .unwrap();
    assert!(!cwd.join("trust-fired.txt").exists());
    with_lead_owner(&binding, &env, |session| {
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
    assert!(requests.iter().all(|request| request.tools_json == "[]"));

    let original = owner.path.with_extension("previous");
    fs::rename(&owner.path, &original).unwrap();
    fs::copy(original, &owner.path).unwrap();
    let error = with_lead_owner::<()>(&binding, &env, |_| panic!("replaced Session opened")).unwrap_err();
    assert_eq!(error.code, "lead_owner_unavailable");
    assert_eq!(server.requests().len(), requests.len());
}
