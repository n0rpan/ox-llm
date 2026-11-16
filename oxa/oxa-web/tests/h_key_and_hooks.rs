//! Lane `search`: key loading (precedence, the 0600 rule, symlink, empty, isolation), the
//! redaction of every type holding the key, the switches, and the per-epoch declaration.
mod common;

use common::*;
use oxa_fake::TestEnv;
use oxa_proto::{HeadlessUi, Hooks};
use oxa_web::{
    enabled, load_key, ApiKey, Brave, BraveOpts, KeyInputs, KeyOrigin, KeyStatus, WebConfig, WebSearchHooks,
};
use std::os::unix::fs::PermissionsExt;

fn inputs(env: &TestEnv, env_key: Option<&str>) -> KeyInputs {
    KeyInputs {
        env_key: env_key.and_then(ApiKey::new),
        agent_dir: env.agent_dir(),
        isolation: true,
        real_home: oxa_web::real_home(),
    }
}

fn write_key(env: &TestEnv, text: &str, mode: u32) -> std::path::PathBuf {
    let p = env.write("home/.oxa/agent/brave-search.key", text);
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
    p
}

#[test]
fn no_key_is_missing() {
    let env = TestEnv::new();
    assert_eq!(load_key(&inputs(&env, None)), KeyStatus::Missing);
}

#[test]
fn the_file_is_read_trimmed_and_the_env_wins() {
    let env = TestEnv::new();
    write_key(&env, "file-key-not-real\n", 0o600);
    match load_key(&inputs(&env, None)) {
        KeyStatus::Found(k, KeyOrigin::File) => assert_eq!(k, ApiKey::new("file-key-not-real").unwrap()),
        other => panic!("{other:?}"),
    }
    match load_key(&inputs(&env, Some(FAKE_KEY))) {
        KeyStatus::Found(k, KeyOrigin::Env) => assert_eq!(k, key()),
        other => panic!("{other:?}"),
    }
    // 0400 is fine too.
    write_key(&env, "file-key-not-real", 0o400);
    assert!(load_key(&inputs(&env, None)).key().is_some());
}

#[test]
fn a_readable_file_a_symlink_and_an_empty_file_are_refused() {
    let env = TestEnv::new();
    let p = write_key(&env, "file-key-not-real\n", 0o644);
    match load_key(&inputs(&env, None)) {
        KeyStatus::Refused(t) => {
            assert!(t.contains("readable by group or others (mode 0644)"), "{t}");
            assert!(t.contains("\nFix: chmod 600 "), "{t}");
            assert!(!t.contains("file-key-not-real"));
        }
        other => panic!("{other:?}"),
    }
    write_key(&env, "file-key-not-real\n", 0o640);
    assert!(matches!(load_key(&inputs(&env, None)), KeyStatus::Refused(_)));
    write_key(&env, " \n", 0o600);
    match load_key(&inputs(&env, None)) {
        KeyStatus::Refused(t) => assert!(t.contains("it is empty"), "{t}"),
        other => panic!("{other:?}"),
    }
    std::fs::remove_file(&p).unwrap();
    let target = env.write("tmp/real.key", "file-key-not-real");
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::os::unix::fs::symlink(&target, &p).unwrap();
    match load_key(&inputs(&env, None)) {
        KeyStatus::Refused(t) => assert!(t.contains("symbolic link"), "{t}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn every_type_holding_the_key_redacts_it() {
    let env = TestEnv::new();
    let k = key();
    assert_eq!(format!("{k}"), "<redacted>");
    assert_eq!(format!("{k:?}"), "ApiKey(<redacted>)");
    let i = inputs(&env, Some(FAKE_KEY));
    let status = load_key(&i);
    let brave = Brave::new(key(), BraveOpts::default());
    let cfg = WebConfig {
        key: i.clone(),
        opts: BraveOpts::default(),
    };
    let hooks = WebSearchHooks::new(cfg.clone());
    let tool = hooks.tools();
    for text in [
        format!("{i:?}"),
        format!("{i:#?}"),
        format!("{status:?}"),
        format!("{brave:?}"),
        format!("{cfg:?}"),
        format!("{hooks:?}"),
        hooks.status_lines().join("\n"),
        format!("{:?}", tool.iter().map(|t| t.spec().clone()).collect::<Vec<_>>()),
    ] {
        assert!(!text.contains(FAKE_KEY), "{text}");
    }
}

#[test]
fn isolation_refuses_an_existing_key_file_under_the_real_home() {
    // The "real home" is a stand-in directory holding a key file: it is refused unread.
    let env = TestEnv::new();
    write_key(&env, "file-key-not-real", 0o600);
    let i = KeyInputs {
        real_home: Some(env.root().to_path_buf()),
        ..inputs(&env, None)
    };
    match load_key(&i) {
        KeyStatus::Refused(t) => assert!(t.contains("OXA_TEST_ISOLATION=1"), "{t}"),
        other => panic!("a key file under the real home must never be read under isolation: {other:?}"),
    }
    // Without a known real home, isolation refuses every existing file.
    let i = KeyInputs {
        real_home: None,
        ..inputs(&env, None)
    };
    assert!(matches!(load_key(&i), KeyStatus::Refused(_)));
}

/// Issue #495: an isolated HOME below the real home (a worktree) with no key file must not
/// warn about a key that does not exist.
#[test]
fn isolation_under_the_real_home_without_a_key_file_is_missing() {
    let env = TestEnv::new();
    for real_home in [Some(env.root().to_path_buf()), None] {
        let i = KeyInputs {
            real_home,
            ..inputs(&env, None)
        };
        assert_eq!(load_key(&i), KeyStatus::Missing);
    }
}

#[test]
fn from_env_reads_the_marker_and_only_a_loopback_base_url() {
    let env = TestEnv::new();
    let vars = [
        ("OXA_TEST_ISOLATION", "1"),
        ("BRAVE_SEARCH_API_KEY", FAKE_KEY),
        ("OXA_BRAVE_BASE_URL", "http://127.0.0.1:1234"),
    ];
    let get = |n: &str| vars.iter().find(|(k, _)| *k == n).map(|(_, v)| v.to_string());
    let (cfg, warnings) = WebConfig::from_env(&get, &env.agent_dir(), Some(4096));
    assert!(warnings.is_empty());
    assert!(cfg.opts.isolation && cfg.key.isolation);
    assert_eq!(cfg.opts.base_url, "http://127.0.0.1:1234");
    assert_eq!(cfg.opts.max_tokens, 4096);
    assert_eq!(cfg.key.env_key, Some(key()));
    let vars = [("OXA_BRAVE_BASE_URL", "https://evil.example.com")];
    let get = |n: &str| vars.iter().find(|(k, _)| *k == n).map(|(_, v)| v.to_string());
    let (cfg, warnings) = WebConfig::from_env(&get, &env.agent_dir(), None);
    assert_eq!(cfg.opts.base_url, oxa_web::DEFAULT_BASE_URL);
    assert_eq!(cfg.opts.max_tokens, oxa_web::DEFAULT_MAX_TOKENS);
    assert_eq!(warnings.len(), 1);
    assert!(!cfg.opts.isolation);
}

#[test]
fn the_switches() {
    assert!(enabled(false, None, true, None));
    assert!(enabled(false, Some("1"), true, Some("brave")));
    assert!(!enabled(true, None, true, None), "--no-web-search");
    for off in ["0", "false", "NO", "off"] {
        assert!(!enabled(false, Some(off), true, None), "OXA_WEB_SEARCH={off}");
    }
    assert!(!enabled(false, None, false, None), "oxa.webSearch.enabled: false");
    assert!(
        !enabled(false, None, true, Some("none")),
        "oxa.webSearch.provider: none"
    );
}

fn hooks_for(env: &TestEnv, env_key: Option<&str>) -> WebSearchHooks {
    WebSearchHooks::new(WebConfig {
        key: inputs(env, env_key),
        opts: opts("http://127.0.0.1:9"),
    })
}

#[test]
fn the_tool_is_declared_only_with_a_key_and_follows_the_key_per_epoch() {
    let env = TestEnv::new();
    let hooks = hooks_for(&env, None);
    assert!(hooks.tools().is_empty(), "no key: no tool");
    assert!(hooks.commands().is_empty(), "no key: no command");
    // A key file added mid-session counts from the next epoch (the next `tools()` read).
    let p = write_key(&env, "file-key-not-real\n", 0o600);
    let tools = hooks.tools();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].spec().name, "web_search");
    assert_eq!(tools[0].spec().exposure, oxa_proto::Exposure::Direct);
    assert_eq!(hooks.commands()[0].name, "web-search");
    std::fs::remove_file(&p).unwrap();
    assert!(hooks.tools().is_empty());
    assert_eq!(hooks_for(&env, Some(FAKE_KEY)).tools().len(), 1);
    // A refused file is a startup notice, not a tool.
    write_key(&env, "file-key-not-real\n", 0o644);
    let h = hooks_for(&env, None);
    assert!(h.tools().is_empty());
    assert!(h.startup_notice().unwrap().contains("refused"));
}

#[test]
fn the_status_command_names_the_key_source_never_the_key() {
    let env = TestEnv::new();
    let hooks = hooks_for(&env, Some(FAKE_KEY));
    let mut ui = HeadlessUi::default();
    hooks.run_command("web-search", "", &mut ui).unwrap();
    let text = &ui.notes[0].1;
    assert!(
        text.starts_with("web_search: on (Brave Search, key from BRAVE_SEARCH_API_KEY)"),
        "{text}"
    );
    assert!(text.contains("endpoint: LLM Context"));
    assert!(!text.contains(FAKE_KEY));
    assert!(hooks.run_command("other", "", &mut ui).is_err());
}

/// O-170 guard (c): with exactly the environment `TestEnv::envs` gives a child, the provider
/// is isolated: the marker is set, no key variable is passed, and a key file under the real
/// home is refused unread, so a mis-wired test cannot pick up the owner's key.
#[test]
fn guard_testenv_environment_isolates_the_provider() {
    let env = TestEnv::new();
    let vars = env.envs("http://127.0.0.1:1");
    let get = |n: &str| vars.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone());
    assert_eq!(get("OXA_TEST_ISOLATION").as_deref(), Some("1"));
    assert_eq!(get("BRAVE_SEARCH_API_KEY"), None);
    write_key(&env, "file-key-not-real", 0o600);
    let (mut cfg, _) = WebConfig::from_env(&get, &env.agent_dir(), None);
    assert!(cfg.opts.isolation);
    cfg.key.real_home = Some(env.root().to_path_buf());
    assert!(matches!(load_key(&cfg.key), KeyStatus::Refused(_)));
    assert!(WebSearchHooks::new(cfg).tools().is_empty());
}
