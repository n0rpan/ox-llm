//! O-152: the identity section and the warm-start anchor (tools + preamble + identity + skills
//! index). The anchor bytes do not depend on the working directory, the date or the session;
//! the boot preload (H6, background) makes turn 1 hit them; a missing identity falls back.

use oxa_config::{Dirs, Settings};
use oxa_core::prompt::{identity_section, identity_text, IdentityFacts, IdentityModel, IDENTITY_FALLBACK};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{cache_bytes, FakeOpts, FakeServer, Recorded, ScriptTurn, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{AgentEvent, Engine, FrozenPrefix, HeadlessUi, VecSink};
use oxa_resources::{discover, DiscoverOpts};
use oxa_sandbox::NoSandbox;
use oxa_session::{MemStore, SessionStore};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn write_settings(env: &TestEnv, json: &str) {
    let path = Dirs::under(env.root()).settings_file();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, json).unwrap();
}

/// A session opened the way the binary opens one: resources discovered for `cwd` (trusted),
/// the built-in tools, the given session id.
fn open_in(env: &TestEnv, cwd: &Path, engine: Arc<dyn Engine>, session_id: &str) -> Session {
    std::fs::create_dir_all(cwd).unwrap();
    let dirs = Dirs::under(env.root());
    let settings = Settings::load(&dirs, cwd, true);
    let opts = DiscoverOpts {
        trusted: true,
        ..Default::default()
    };
    let resources = discover(&dirs, cwd, &settings, &opts);
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
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
            cwd: cwd.to_path_buf(),
            selection: ToolSelection::default(),
            resources,
            model: None,
            thinking: None,
            mode: None,
            session_id: Some(session_id.to_string()),
            jobs: oxa_core::fresh_jobs(),
        },
        Open::New,
    )
    .expect("open")
}

fn scripted() -> Arc<dyn Engine> {
    Arc::new(oxa_fake::ScriptedEngine::new(Vec::new()))
}

fn http(server: &FakeServer) -> Arc<dyn Engine> {
    Arc::new(oxa_engine::HttpEngine::new(&server.url(), None).expect("engine"))
}

fn facts() -> IdentityFacts {
    IdentityFacts {
        oxa_version: "0.2.0".to_string(),
        machine: Some("Test Laptop 7x".to_string()),
        chip: Some("Snapdragon X Elite (X1E80100)".to_string()),
        memory_gib: Some(31),
        os: Some("Omarchy".to_string()),
        omarchy: true,
        models: vec![
            IdentityModel {
                alias: "flash".to_string(),
                context: 16384,
                max_output: 4096,
                remote: false,
                remote_provider: None,
            },
            IdentityModel {
                alias: "moe".to_string(),
                context: 131072,
                max_output: 32768,
                remote: false,
                remote_provider: None,
            },
        ],
    }
}

/// The anchor as the engine sees it: its system text, then the tools block.
fn anchor_bytes(p: &FrozenPrefix) -> Vec<u8> {
    let mut v = p.anchor_text().into_bytes();
    v.push(0);
    v.extend_from_slice(p.tools_json.as_bytes());
    v
}

fn names(p: &FrozenPrefix) -> Vec<&str> {
    p.sections.iter().map(|(n, _)| n.as_str()).collect()
}

/// A rough current year, for the "no date" check.
fn this_year() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    (1970 + secs / 31_556_952).to_string()
}

fn no_date_like_text(bytes: &str) {
    let chars: Vec<char> = bytes.chars().collect();
    for w in chars.windows(10) {
        let s: String = w.iter().collect();
        let date_like = s.chars().enumerate().all(|(i, c)| match i {
            4 | 7 => c == '-' || c == '/' || c == '.',
            _ => c.is_ascii_digit(),
        });
        assert!(!date_like, "W3: date-like text {s:?} in the anchor");
    }
    assert!(
        !bytes.contains(&this_year()),
        "W3: the current year in the anchor"
    );
}

#[test]
fn anchor_bytes_are_identical_across_cwd_date_and_session() {
    let env = TestEnv::new();
    let dirs = Dirs::under(env.root());
    std::fs::create_dir_all(&dirs.agent_dir).unwrap();
    std::fs::write(dirs.agent_dir.join("identity.md"), identity_text(&facts()) + "\n").unwrap();
    env.write(
        "home/.oxa/agent/skills/deploy/SKILL.md",
        "---\nname: deploy\ndescription: Deploy the site.\n---\nSteps.\n",
    );
    // Project B has its own context file; project A has none.
    let a: PathBuf = env.work().join("project-a");
    let b: PathBuf = env.root().join("elsewhere/project-b");
    env.write("elsewhere/project-b/AGENTS.md", "Use tabs.\n");

    let s1 = open_in(&env, &a, scripted(), "first-session");
    // Another process, later, in another directory, another session id.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let s2 = open_in(&env, &b, scripted(), "second-session");
    let s3 = open_in(&env, &a, scripted(), "third-session");
    let (p1, p2, p3) = (&s1.inner().prefix, &s2.inner().prefix, &s3.inner().prefix);

    assert_eq!(&names(p1)[..3], ["preamble", "identity", "skills"]);
    assert_eq!(
        anchor_bytes(p1),
        anchor_bytes(p2),
        "same anchor in another project"
    );
    assert_eq!(
        anchor_bytes(p1),
        anchor_bytes(p3),
        "same anchor in another session"
    );
    assert_eq!(p1.sha256, p3.sha256, "same directory = the same whole prefix");
    assert_ne!(p1.system_text(), p2.system_text(), "the project part differs");

    let anchor = p1.anchor_text();
    for p in [p1, p2, p3] {
        assert!(
            p.system_text().starts_with(&anchor),
            "the anchor is a byte prefix of the system text"
        );
        assert!(
            p.system_text().ends_with("</cwd>"),
            "cwd stays last, after the anchor"
        );
    }
    assert!(!anchor.contains("project-a") && !anchor.contains("project-b") && !anchor.contains("Use tabs."));
    assert!(
        p2.system_text()[anchor.len()..].contains("Use tabs."),
        "the context comes after the anchor"
    );
    // What the anchor holds: the identity, the skills index (names and descriptions; bodies
    // stay on disk) and the tool schemas.
    assert!(anchor.contains("<identity>\noxa is the agent of the ox suite"));
    assert!(
        anchor.contains("Snapdragon X Elite (X1E80100)") && anchor.contains("  - moe: context 131072 tokens")
    );
    for skill in ["deploy", "ox-llm", "oxa-extend", "pull-request", "report-issue"] {
        assert!(
            anchor.contains(&format!("<name>{skill}</name>")),
            "{skill} indexed"
        );
    }
    assert!(
        !anchor.contains("Steps.") && !anchor.contains("## Status"),
        "no skill bodies in the prefix"
    );
    assert!(p1.tools_json.contains("\"name\":\"bash\""));
    no_date_like_text(&anchor);
    assert!(!anchor.contains("first-session") && !anchor.contains("second-session"));
}

#[test]
fn missing_identity_falls_back_to_the_generic_text() {
    let env = TestEnv::new();
    let s = open_in(&env, &env.work(), scripted(), "s");
    let p = &s.inner().prefix;
    let identity = p
        .sections
        .iter()
        .find(|(n, _)| n == "identity")
        .map(|(_, t)| t.clone());
    assert_eq!(identity, Some(identity_section(None)));
    assert!(identity.unwrap().contains(IDENTITY_FALLBACK));
    // An identity file that is blank is the same as a missing one.
    env.write("home/.oxa/agent/identity.md", "\n");
    let s = open_in(&env, &env.work(), scripted(), "t");
    assert!(s.inner().prefix.system_text().contains(IDENTITY_FALLBACK));
}

/// Turn 1 after the boot preload: the chat request's cached tokens are the whole preloaded
/// prefix (the fake: one token = one byte), and the preload carried the same prefix (W3).
#[test]
fn turn_one_hits_the_boot_preload_anchor() {
    let env = TestEnv::new();
    let server = FakeServer::start(vec![ScriptTurn::text("hello")], FakeOpts::default()).unwrap();
    let mut s = open_in(&env, &env.work(), http(&server), "warm");
    assert!(s.inner().info.prefill_only, "the fake advertises H6");
    let mut sink = VecSink::default();
    assert!(s.preload_prefix(&mut sink, &mut HeadlessUi::default()));
    assert!(
        !s.preload_prefix(&mut sink, &mut HeadlessUi::default()),
        "once per session"
    );
    s.prompt(
        "hi".to_string(),
        Vec::new(),
        &mut sink,
        &mut HeadlessUi::default(),
    )
    .expect("prompt");
    assert!(
        !sink.0.iter().any(|e| matches!(e, AgentEvent::CacheMiss { .. })),
        "no miss on turn 1"
    );

    let reqs: Vec<Recorded> = server.requests();
    assert_eq!(reqs.len(), 2, "{reqs:?}");
    let (pre, chat) = (&reqs[0], &reqs[1]);
    assert_eq!(
        (pre.kind.as_str(), pre.max_tokens, pre.background),
        ("prefill", 0, true)
    );
    assert!(pre.messages.is_empty(), "the preload sends the prefix only");
    assert!(
        pre.snapshot_keep,
        "H4: the engine advertises checkpoints, so the preload keeps one"
    );
    assert_eq!(pre.session, chat.session);
    assert_eq!(
        (pre.system.as_str(), pre.tools_json.as_str()),
        (chat.system.as_str(), chat.tools_json.as_str())
    );
    let p = &s.inner().prefix;
    let prefix_len = cache_bytes(&p.system_text(), &p.tools_json, &[]).len() as u32;
    assert!(prefix_len as usize > p.anchor_text().len());
    let usage = chat.usage.expect("chat usage");
    assert_eq!(
        usage.cached, prefix_len,
        "turn 1 reuses the whole preloaded prefix"
    );
}

#[test]
fn boot_preload_is_generic_and_opt_out() {
    // `oxa.bootPreload: false`.
    let env = TestEnv::new();
    write_settings(&env, r#"{"oxa": {"bootPreload": false}}"#);
    let server = FakeServer::start(Vec::new(), FakeOpts::default()).unwrap();
    let mut s = open_in(&env, &env.work(), http(&server), "off");
    assert!(!s.preload_prefix(&mut VecSink::default(), &mut HeadlessUi::default()));
    assert!(server.requests().is_empty());

    // `cacheWarming: off` turns it off too.
    let env = TestEnv::new();
    write_settings(&env, r#"{"cacheWarming": "off"}"#);
    let server = FakeServer::start(Vec::new(), FakeOpts::default()).unwrap();
    let mut s = open_in(&env, &env.work(), http(&server), "cold");
    assert!(!s.preload_prefix(&mut VecSink::default(), &mut HeadlessUi::default()));
    assert!(server.requests().is_empty());

    // An engine without H6 gets no zero-token request.
    let env = TestEnv::new();
    let server = FakeServer::start(
        Vec::new(),
        FakeOpts {
            hints: false,
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let mut s = open_in(&env, &env.work(), http(&server), "plain");
    assert!(!s.preload_prefix(&mut VecSink::default(), &mut HeadlessUi::default()));
    assert!(server.requests().is_empty());

    // A session that already talked has nothing to preload.
    let env = TestEnv::new();
    let server = FakeServer::start(vec![ScriptTurn::text("r")], FakeOpts::default()).unwrap();
    let mut s = open_in(&env, &env.work(), http(&server), "busy");
    s.prompt(
        "q".to_string(),
        Vec::new(),
        &mut VecSink::default(),
        &mut HeadlessUi::default(),
    )
    .expect("prompt");
    let before = server.requests().len();
    assert!(
        !s.preload_prefix_if(&mut VecSink::default(), &mut HeadlessUi::default(), || panic!(
            "an inapplicable preload must not claim the session"
        ))
    );
    assert!(!s.handle().is_busy());
    assert_eq!(server.requests().len(), before);
}

/// F2: the identity bytes depend on the providers, not on the cloud catalog, so a catalog change
/// does not replace the cached anchor; local models keep their lines.
#[test]
fn cloud_models_are_one_provider_line_and_a_catalog_change_keeps_the_bytes() {
    let model = |alias: &str| IdentityModel {
        alias: alias.to_string(),
        context: 8192,
        max_output: 1024,
        remote: alias.contains('/'),
        remote_provider: alias.split_once('/').map(|(provider, _)| provider.to_string()),
    };
    let mut a = facts();
    a.models.extend([model("openrouter/y"), model("opencode/x")]);
    let mut b = facts();
    b.models.extend([
        model("opencode/other"),
        model("opencode/more"),
        model("openrouter/z"),
        model("openrouter/w"),
    ]);
    let (ta, tb) = (identity_text(&a), identity_text(&b));
    assert_eq!(ta, tb, "same providers = same bytes");
    assert!(ta.contains("\n- Cloud models: from opencode, openrouter ("));
    assert!(!ta.contains("  - opencode/") && !ta.contains("  - openrouter/"));
    assert!(ta.contains("  - moe: context 131072 tokens") && ta.contains("  - flash: context 16384 tokens"));
    let mut local = facts();
    local.models.clear();
    local.models.push(model("moe"));
    assert!(!identity_text(&local).contains("Cloud models"));

    let mut unprefixed_cloud = facts();
    unprefixed_cloud.models.push(IdentityModel {
        alias: "cloud-model".to_string(),
        context: 64_000,
        max_output: 4_096,
        remote: true,
        remote_provider: Some("openrouter".to_string()),
    });
    let text = identity_text(&unprefixed_cloud);
    assert!(text.contains("- Cloud models: from openrouter ("));
    assert!(!text.contains("  - cloud-model"));
}
