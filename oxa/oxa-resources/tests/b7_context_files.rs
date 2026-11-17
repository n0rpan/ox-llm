//! B7 acceptance tests: Context files.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_config::Dirs;
use oxa_fake::TestEnv;
use oxa_resources::{context_file_in, context_search_dirs, load_context, ContextFile, DiscoverOpts};
use std::path::{Path, PathBuf};

/// Context files found below the test root. Ancestors of the test root (e.g. `/tmp`) are not
/// under our control, so the order tests only look at files the test itself wrote.
fn under_root(env: &TestEnv, files: &[ContextFile]) -> Vec<(PathBuf, String)> {
    files
        .iter()
        .filter(|f| f.path.starts_with(env.root()))
        .map(|f| (f.path.clone(), f.content.clone()))
        .collect()
}

fn dirs(env: &TestEnv) -> Dirs {
    Dirs::under(env.root())
}

/// load order
#[test]
fn agent_dir_then_parents_then_cwd_order() {
    let env = TestEnv::new();
    env.write("home/.oxa/agent/AGENTS.md", "agent");
    env.write("AGENTS.md", "root");
    env.write("work/CLAUDE.md", "work");
    env.write("work/sub/AGENTS.md", "sub");
    let cwd = env.work().join("sub");

    let load = load_context(&dirs(&env), &cwd, &DiscoverOpts::default());
    let got: Vec<String> = under_root(&env, &load.files)
        .into_iter()
        .map(|(_, c)| c)
        .collect();
    assert_eq!(got, ["agent", "root", "work", "sub"]);
    assert_eq!(load.files[0].path, env.agent_dir().join("AGENTS.md"));
    assert_eq!(load.files[3].path, cwd.join("AGENTS.md"));
}

/// AGENTS.override.md
#[test]
fn agents_override_same_dir_rule() {
    let env = TestEnv::new();
    env.write("work/AGENTS.override.md", "override");
    env.write("work/AGENTS.md", "plain");
    env.write("work/CLAUDE.md", "claude");
    let work = env.work();

    assert_eq!(context_file_in(&work), Some(work.join("AGENTS.override.md")));
    let load = load_context(&dirs(&env), &work, &DiscoverOpts::default());
    let got: Vec<String> = under_root(&env, &load.files)
        .into_iter()
        .map(|(_, c)| c)
        .collect();
    assert_eq!(got, ["override"], "only the override is read in that directory");
}

/// CLAUDE.md fallback rule
#[test]
fn claude_md_read_as_pi() {
    let env = TestEnv::new();
    env.write("work/CLAUDE.md", "claude only");
    let work = env.work();
    assert_eq!(context_file_in(&work), Some(work.join("CLAUDE.md")));
    let load = load_context(&dirs(&env), &work, &DiscoverOpts::default());
    assert_eq!(
        under_root(&env, &load.files),
        vec![(work.join("CLAUDE.md"), "claude only".to_string())]
    );

    // AGENTS.md beats CLAUDE.md in the same directory.
    env.write("work/AGENTS.md", "agents");
    assert_eq!(context_file_in(&work), Some(work.join("AGENTS.md")));
}

/// project over user, flag over file
#[test]
fn system_md_and_append_system_precedence() {
    let env = TestEnv::new();
    env.write("home/.oxa/agent/SYSTEM.md", "user-system");
    env.write("work/.oxa/SYSTEM.md", "project-system");
    env.write("home/.oxa/agent/APPEND_SYSTEM.md", "user-append");
    env.write("work/.oxa/APPEND_SYSTEM.md", "project-append");
    let work = env.work();

    let trusted = DiscoverOpts {
        trusted: true,
        ..DiscoverOpts::default()
    };
    let load = load_context(&dirs(&env), &work, &trusted);
    assert_eq!(load.system_override.as_deref(), Some("project-system"));
    assert_eq!(load.append_system, ["user-append", "project-append"]);

    let flagged = DiscoverOpts {
        trusted: true,
        system_prompt: Some("flag-system".into()),
        append_system_prompt: vec!["flag-a".into(), "flag-b".into()],
        ..DiscoverOpts::default()
    };
    let load = load_context(&dirs(&env), &work, &flagged);
    assert_eq!(
        load.system_override.as_deref(),
        Some("flag-system"),
        "flag beats any SYSTEM.md"
    );
    assert_eq!(
        load.append_system,
        ["user-append", "project-append", "flag-a", "flag-b"]
    );
}

/// -nc loads none
#[test]
fn no_context_flag() {
    let env = TestEnv::new();
    env.write("home/.oxa/agent/AGENTS.md", "agent");
    env.write("work/AGENTS.md", "work");
    let opts = DiscoverOpts {
        no_context: true,
        ..DiscoverOpts::default()
    };
    let load = load_context(&dirs(&env), &env.work(), &opts);
    assert!(load.files.is_empty());
    assert!(load.diagnostics.is_empty());
}

/// SYSTEM.md gated, AGENTS.md not
#[test]
fn untrusted_project_skips_gated() {
    let env = TestEnv::new();
    env.write("home/.oxa/agent/SYSTEM.md", "user-system");
    env.write("work/.oxa/SYSTEM.md", "project-system");
    env.write("work/.oxa/APPEND_SYSTEM.md", "project-append");
    env.write("work/AGENTS.md", "work agents");
    let work = env.work();

    let load = load_context(
        &dirs(&env),
        &work,
        &DiscoverOpts {
            trusted: false,
            ..DiscoverOpts::default()
        },
    );
    let got: Vec<String> = under_root(&env, &load.files)
        .into_iter()
        .map(|(_, c)| c)
        .collect();
    assert_eq!(
        got,
        ["work agents"],
        "AGENTS.md is read from an untrusted project"
    );
    assert_eq!(load.system_override.as_deref(), Some("user-system"));
    assert!(load.append_system.is_empty());
}

// ---- extra cases ----

#[test]
fn content_is_byte_exact_and_lossy_only_for_invalid_utf8() {
    let env = TestEnv::new();
    env.write("work/AGENTS.md", "\u{feff}  padded \n\n");
    let load = load_context(&dirs(&env), &env.work(), &DiscoverOpts::default());
    assert_eq!(load.files.len(), 1);
    assert_eq!(load.files[0].content, "\u{feff}  padded \n\n");

    let bad = env.root().join("work/AGENTS.md");
    std::fs::write(&bad, [0xffu8, b'a']).expect("write bytes");
    let load = load_context(&dirs(&env), &env.work(), &DiscoverOpts::default());
    assert_eq!(load.files[0].content, "\u{fffd}a");
}

#[test]
fn agent_dir_as_cwd_is_listed_once() {
    let env = TestEnv::new();
    env.write("home/.oxa/agent/AGENTS.md", "agent");
    let load = load_context(&dirs(&env), &env.agent_dir(), &DiscoverOpts::default());
    let got: Vec<String> = under_root(&env, &load.files)
        .into_iter()
        .map(|(_, c)| c)
        .collect();
    assert_eq!(got, ["agent"]);
}

#[test]
fn search_dirs_outermost_first_ending_with_cwd() {
    let p = Path::new("/a/b/c");
    let dirs = context_search_dirs(p);
    assert_eq!(dirs.last().map(PathBuf::as_path), Some(p));
    assert_eq!(dirs.first().map(PathBuf::as_path), Some(Path::new("/")));
    assert_eq!(dirs.len(), 4);
}
