//! #517: `grep`, `find` and `ls` never show what is under the deny-list (B3), also when the
//! search root is an allowed parent of it (cwd = the home directory, `~/.config`). Only the root
//! goes through the policy, so the walk itself must skip the denied entries.
//! Isolation: every file lives under `oxa_fake::TestEnv`.

use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::{ModePolicy, PathGuard};
use oxa_proto::{Autonomy, HeadlessUi, Mode, Role, ToolCall, VecSink, YoloSwitch};
use oxa_resources::Resources;
use oxa_sandbox::NoSandbox;
use oxa_session::MemStore;
use std::path::Path;
use std::sync::Arc;

const MARK: &str = "zq517marker";

/// Run one reply with `calls` (name, args) in a session whose cwd is `cwd`, with the guard of
/// the fake home; the tool results in call order.
fn results(env: &TestEnv, cwd: &Path, calls: &[(&str, serde_json::Value)]) -> Vec<String> {
    results_in_mode(env, cwd, calls, Autonomy::Standard, Mode::Workspace)
}

fn results_in_mode(
    env: &TestEnv,
    cwd: &Path,
    calls: &[(&str, serde_json::Value)],
    autonomy: Autonomy,
    mode: Mode,
) -> Vec<String> {
    let dirs = Dirs::under(env.root());
    let calls: Vec<ToolCall> = calls
        .iter()
        .enumerate()
        .map(|(i, (name, args))| ToolCall {
            id: format!("c{i}"),
            name: name.to_string(),
            args_raw: args.to_string(),
        })
        .collect();
    let mut turn = ScriptTurn::tool("x", "grep", "{}");
    turn.tool_calls = calls;
    let engine = Arc::new(ScriptedEngine::new(vec![turn, ScriptTurn::text("done")]));
    let guard = PathGuard::new(&dirs.home, &dirs.agent_dir, &dirs.tmp_dir);
    let mut s = Session::open(
        Deps {
            engine: engine.clone(),
            store: Arc::new(MemStore::new()),
            tools: oxa_tools::builtin_tools(),
            hooks: Vec::new(),
            policy: Box::new(
                ModePolicy::new()
                    .with_guard(guard)
                    .with_yolo(YoloSwitch::new(autonomy)),
            ),
            sandbox: Arc::new(NoSandbox),
            settings: Settings::default(),
            dirs,
            cwd: cwd.to_path_buf(),
            selection: ToolSelection::default(),
            resources: Resources::default(),
            model: None,
            thinking: None,
            mode: Some(mode),
            session_id: Some("s1".to_string()),
            jobs: oxa_core::fresh_jobs(),
        },
        Open::New,
    )
    .expect("open");
    let mut sink = VecSink::default();
    let mut ui = HeadlessUi::default();
    s.prompt("look".to_string(), Vec::new(), &mut sink, &mut ui)
        .expect("prompt");
    let reqs = engine.requests();
    reqs[1]
        .messages
        .iter()
        .filter(|m| m.role == Role::Tool)
        .map(|m| m.content.clone().unwrap_or_default())
        .collect()
}

fn secrets(env: &TestEnv) {
    env.write("home/.ssh/id_x", &format!("{MARK} ssh\n"));
    env.write("home/.config/gh/hosts.yml", &format!("{MARK} gh\n"));
    env.write("home/.netrc", &format!("{MARK} netrc\n"));
    env.write("home/.config/hypr/hyprland.conf", &format!("{MARK} hypr\n"));
    env.write("home/notes.txt", &format!("{MARK} notes\n"));
}

#[test]
fn grep_from_home_skips_denied_files_and_directories() {
    let env = TestEnv::new();
    secrets(&env);
    let out = results(
        &env,
        &env.home(),
        &[("grep", serde_json::json!({"pattern": MARK, "path": "."}))],
    );
    let text = &out[0];
    assert!(text.contains("notes.txt"), "{text}");
    assert!(text.contains("hyprland.conf"), "{text}");
    for leak in ["ssh", "gh", "netrc"] {
        assert!(!text.contains(&format!("{MARK} {leak}")), "{leak} leaked: {text}");
    }
}

#[test]
fn grep_from_config_skips_denied_directories() {
    let env = TestEnv::new();
    secrets(&env);
    let out = results(
        &env,
        &env.home().join(".config"),
        &[("grep", serde_json::json!({"pattern": MARK}))],
    );
    let text = &out[0];
    assert!(text.contains("hypr/hyprland.conf"), "{text}");
    assert!(!text.contains("gh"), "{text}");
}

#[test]
fn find_and_ls_do_not_list_denied_entries() {
    let env = TestEnv::new();
    secrets(&env);
    let out = results(
        &env,
        &env.home(),
        &[
            ("find", serde_json::json!({"pattern": "*"})),
            ("ls", serde_json::json!({"path": "."})),
            ("ls", serde_json::json!({"path": ".config"})),
        ],
    );
    let (find, ls_home, ls_config) = (&out[0], &out[1], &out[2]);
    assert!(
        find.contains("notes.txt") && find.contains(".config/hypr/hyprland.conf"),
        "{find}"
    );
    for leak in ["id_x", "hosts.yml", ".netrc", ".ssh", ".config/gh"] {
        assert!(!find.contains(leak), "{leak} listed: {find}");
    }
    assert!(
        ls_home.contains("notes.txt") && ls_home.contains(".config/"),
        "{ls_home}"
    );
    assert!(
        !ls_home.contains(".ssh") && !ls_home.contains(".netrc"),
        "{ls_home}"
    );
    assert!(
        ls_config.contains("hypr/") && !ls_config.contains("gh/"),
        "{ls_config}"
    );
}

#[test]
fn a_walk_inside_the_readable_builtin_skills_still_finds_them() {
    // #492: the built-in skills under the denied agent directory stay readable.
    let env = TestEnv::new();
    let skills = env.agent_dir().join("builtin-skills");
    env.write(
        "home/.oxa/agent/builtin-skills/demo/SKILL.md",
        &format!("{MARK} skill\n"),
    );
    let out = results(
        &env,
        &env.work(),
        &[
            (
                "grep",
                serde_json::json!({"pattern": MARK, "path": skills.to_string_lossy()}),
            ),
            (
                "find",
                serde_json::json!({"pattern": "*.md", "path": skills.to_string_lossy()}),
            ),
        ],
    );
    assert!(out[0].contains("demo/SKILL.md"), "{}", out[0]);
    assert!(out[1].contains("demo/SKILL.md"), "{}", out[1]);
}

#[test]
fn a_root_reached_through_a_symlink_still_hides_the_deny_list() {
    let env = TestEnv::new();
    secrets(&env);
    let link = env.work().join("home-link");
    std::os::unix::fs::symlink(env.home(), &link).unwrap();
    // The session's cwd is the link itself, so the policy allows the root.
    let out = results(&env, &link, &[("grep", serde_json::json!({"pattern": MARK}))]);
    let text = &out[0];
    assert!(text.contains("notes.txt"), "{text}");
    for leak in ["ssh", "gh", "netrc"] {
        assert!(!text.contains(&format!("{MARK} {leak}")), "{leak} leaked: {text}");
    }
}

#[test]
fn yolo_walks_keep_hiding_credentials_like_auto_and_plan() {
    let env = TestEnv::new();
    secrets(&env);
    let calls = [
        ("grep", serde_json::json!({"pattern": MARK, "path": "."})),
        ("find", serde_json::json!({"pattern": "*"})),
        ("ls", serde_json::json!({"path": "."})),
    ];
    // #1512 S4: yolo drops the prompts, not the walk filter: credentials stay hidden everywhere.
    for (autonomy, mode, full) in [
        (Autonomy::Yolo, Mode::Workspace, false),
        (Autonomy::Auto, Mode::Workspace, false),
        (Autonomy::Yolo, Mode::Plan, false),
    ] {
        let out = results_in_mode(&env, &env.home(), &calls, autonomy, mode);
        assert_eq!(out[0].contains(&format!("{MARK} ssh")), full, "{}", out[0]);
        assert_eq!(out[1].contains("id_x"), full, "{}", out[1]);
        assert_eq!(out[2].contains(".ssh"), full, "{}", out[2]);
        assert!(out[0].contains("notes.txt"), "{}", out[0]);
    }
}
