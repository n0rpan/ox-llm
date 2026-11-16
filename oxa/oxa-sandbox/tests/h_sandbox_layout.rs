//! Wave H lane `harden`: the sandbox's view of the machine (pre-flight B2, B3, B4).
//!
//! Argv goldens per case (a project, `$HOME` as cwd with and without a home workspace
//! (O-199), `/` and a parent of `$HOME` (never a workspace), plan mode), and real-bwrap runs: a PATH tool and a
//! toolchain under `$HOME` run, a cache is writable and a toolchain is not, a denied file is
//! neither readable nor writable, the protected project files are read-only for bash.
//! Real-bwrap tests follow A5: with `OXA_REQUIRE_BWRAP=1` an unusable bwrap fails them.
//! Isolation: every path lives under `oxa_fake::TestEnv`.

use oxa_fake::TestEnv;
use oxa_proto::{CancelToken, ExecSpec, Sandbox};
use oxa_sandbox::{bwrap_args_with, Bwrap, Layout, LayoutInputs};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

fn spec(command: &str, cwd: &Path, writable: bool) -> ExecSpec {
    ExecSpec {
        command: command.to_string(),
        cwd: cwd.to_path_buf(),
        writable,
        net: false,
        timeout_s: Some(20),
        env: Vec::new(),
    }
}

fn exe(env: &TestEnv, rel: &str, body: &str) -> PathBuf {
    let p = env.write(rel, body);
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    p
}

/// A home with a toolchain (`~/.cargo/bin/cargo` reading `~/.rustup`), a PATH tool in
/// `~/.local/bin`, a cargo cache, secrets (`~/.ssh/id_test`, `~/.netrc`) and agent state.
fn machine(env: &TestEnv) -> Layout {
    exe(
        env,
        "home/.cargo/bin/cargo",
        "#!/bin/sh\necho \"cargo from $(cat \"$HOME/.rustup/toolchains/stable/version\")\"\n",
    );
    env.write("home/.rustup/toolchains/stable/version", "1.0.0-test");
    exe(env, "home/.local/bin/mytool", "#!/bin/sh\necho mytool-ran\n");
    env.write("home/.cargo/registry/index/keep", "");
    env.write("home/.ssh/id_test", "PRIVATE-KEY-MATERIAL");
    env.write("home/.netrc", "machine example.com password hunter2");
    env.write("home/.oxa/agent/trust.json", "{}");
    let home = env.home();
    let path_var = format!(
        "{}:{}:/usr/bin:/bin:relative/dir:{}",
        home.join(".cargo/bin").display(),
        home.join(".local/bin").display(),
        home.join("missing/bin").display()
    );
    Layout::build(&LayoutInputs {
        home: home.clone(),
        agent_dir: Some(env.agent_dir()),
        path_var,
        env: vec![
            ("CARGO_HOME".into(), home.join(".cargo").display().to_string()),
            ("MISE_GITHUB_TOKEN".into(), "ghp_secret".into()),
            (
                "MISE_DATA_DIR".into(),
                home.join(".local/share/mise").display().to_string(),
            ),
            ("BRAVE_SEARCH_API_KEY".into(), "secret".into()),
            ("UNRELATED".into(), "x".into()),
        ],
        read_extra: Vec::new(),
        write_extra: Vec::new(),
        allow_home_write: false,
        exe: None,
        exe_in_workspace: false,
    })
}

/// The test root replaced by `<root>` so the goldens do not depend on the temp directory.
fn shown(args: &[String], env: &TestEnv) -> Vec<String> {
    let root = env.root().display().to_string();
    args.iter().map(|a| a.replace(&root, "<root>")).collect()
}

fn strs(xs: &[&str]) -> Vec<String> {
    xs.iter().map(|x| (*x).to_string()).collect()
}

const HEAD: &[&str] = &[
    "--unshare-all",
    "--die-with-parent",
    "--new-session",
    "--cap-drop",
    "ALL",
    "--ro-bind",
    "/",
    "/",
    "--dev",
    "/dev",
    "--proc",
    "/proc",
    "--tmpfs",
    "/tmp",
    "--tmpfs",
    "<root>/home",
];

fn tail(cwd: &str, path: &str) -> Vec<String> {
    strs(&[
        "--remount-ro",
        "<root>/home",
        "--chdir",
        cwd,
        "--clearenv",
        "--setenv",
        "PATH",
        path,
        "--setenv",
        "HOME",
        "<root>/home",
        "--setenv",
        "LANG",
        "C.UTF-8",
        "--setenv",
        "TERM",
        "dumb",
        "--setenv",
        "AI_AGENT",
        "oxa",
        "--setenv",
        "CARGO_HOME",
        "<root>/home/.cargo",
        "--setenv",
        "MISE_DATA_DIR",
        "<root>/home/.local/share/mise",
        "--setenv",
        "MISE_STATE_DIR",
        "/tmp/mise-state",
        "--setenv",
        "TMPDIR",
        "/tmp",
        "--setenv",
        "XDG_CACHE_HOME",
        "/tmp/.cache",
        "--",
        "bash",
        "-c",
        "true",
    ])
}

const PATH_IN: &str = "<root>/home/.cargo/bin:<root>/home/.local/bin:/usr/bin:/bin:/usr/local/bin";

#[test]
fn layout_keeps_path_binds_toolchains_and_strips_secrets() {
    let env = TestEnv::new();
    let l = machine(&env);
    let home = env.home();
    assert_eq!(
        l.path,
        vec![
            home.join(".cargo/bin").display().to_string(),
            home.join(".local/bin").display().to_string(),
            "/usr/bin".to_string(),
            "/bin".to_string(),
            "/usr/local/bin".to_string(),
        ],
        "absolute existing entries in order, the base appended"
    );
    assert_eq!(
        l.read,
        vec![
            home.join(".cargo/bin"),
            home.join(".local/bin"),
            home.join(".rustup")
        ]
    );
    assert_eq!(l.write, vec![home.join(".cargo/registry")]);
    let names: Vec<&str> = l.env.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "CARGO_HOME",
            "MISE_DATA_DIR",
            "MISE_STATE_DIR",
            "TMPDIR",
            "XDG_CACHE_HOME"
        ],
        "no token, no key, nothing unlisted"
    );
}

/// #280 / review of #302: no bash command, git included, may write the project's `.oxa` or
/// `.agents/skills`: git can be made to run programs or check out content the model wrote.
#[test]
fn git_commands_keep_oxa_and_skills_read_only() {
    let env = TestEnv::new();
    let l = machine(&env);
    env.write("work/.oxa/settings.json", "{}");
    env.write("work/.agents/skills/x/SKILL.md", "");
    for cmd in [
        "git pull",
        "git pull --upload-pack=./p.sh .",
        "git merge -s evil main",
        "git checkout abc123 -- .oxa/extensions/x.js",
        "git rebase --exec evil main",
    ] {
        let a = bwrap_args_with(&spec(cmd, &env.work(), true), &env.home(), &l);
        for p in [".oxa", ".agents/skills"] {
            let want = env.work().join(p).display().to_string();
            assert!(
                a.windows(2).any(|w| w[0] == "--ro-bind" && w[1] == want),
                "{cmd}: {p}"
            );
        }
    }
}

#[test]
fn golden_project_cwd() {
    let env = TestEnv::new();
    let l = machine(&env);
    env.write("work/.git/config", "[core]\n");
    env.write("work/.git/hooks/pre-commit.sample", "");
    env.write("work/.oxa/settings.json", "{}");
    let a =
        oxa_sandbox::without_account_home(bwrap_args_with(&spec("true", &env.work(), true), &env.home(), &l));
    let mut want = strs(HEAD);
    want.extend(strs(&[
        "--bind",
        "<root>/work",
        "<root>/work",
        "--ro-bind",
        "<root>/home/.cargo/bin",
        "<root>/home/.cargo/bin",
        "--bind",
        "<root>/home/.cargo/registry",
        "<root>/home/.cargo/registry",
        "--ro-bind",
        "<root>/home/.local/bin",
        "<root>/home/.local/bin",
        "--ro-bind",
        "<root>/home/.rustup",
        "<root>/home/.rustup",
        "--bind",
        "<root>/work/.git",
        "<root>/work/.git",
        "--ro-bind",
        "<root>/work/.oxa",
        "<root>/work/.oxa",
        "--ro-bind",
        "<root>/work/.git/hooks",
        "<root>/work/.git/hooks",
        "--ro-bind",
        "<root>/work/.git/config",
        "<root>/work/.git/config",
        // #201: a missing commondir / gitdir is masked after the protected binds.
        "--ro-bind",
        "/dev/null",
        "<root>/work/.git/commondir",
        "--ro-bind",
        "/dev/null",
        "<root>/work/.git/gitdir",
    ]));
    want.extend(tail("<root>/work", PATH_IN));
    assert_eq!(shown(&a, &env), want);
}

/// The masks of the machine's secrets when the cwd shows the home directory.
fn masks() -> Vec<String> {
    strs(&[
        "--ro-bind",
        "/dev/null",
        "<root>/home/.netrc",
        "--tmpfs",
        "<root>/home/.oxa/agent",
        "--remount-ro",
        "<root>/home/.oxa/agent",
        "--tmpfs",
        "<root>/home/.ssh",
        "--remount-ro",
        "<root>/home/.ssh",
    ])
}

#[test]
fn golden_home_cwd_is_read_only_and_masked() {
    let env = TestEnv::new();
    let l = machine(&env);
    let home = env.home();
    let a = oxa_sandbox::without_account_home(bwrap_args_with(&spec("true", &home, true), &home, &l));
    let mut want = strs(HEAD);
    want.extend(strs(&["--ro-bind", "<root>/home", "<root>/home"]));
    // Every toolchain bind is inside the cwd: none is left out (they hold their own mode).
    want.extend(strs(&[
        "--ro-bind",
        "<root>/home/.cargo/bin",
        "<root>/home/.cargo/bin",
        "--bind",
        "<root>/home/.cargo/registry",
        "<root>/home/.cargo/registry",
        "--ro-bind",
        "<root>/home/.local/bin",
        "<root>/home/.local/bin",
        "--ro-bind",
        "<root>/home/.rustup",
        "<root>/home/.rustup",
    ]));
    want.extend(masks());
    want.extend(tail("<root>/home", PATH_IN));
    assert_eq!(shown(&a, &env), want);

    // O-199: a home workspace (the user confirmed, or the setting): the home root stays
    // read-only, its plain folders and files are bound read-write (never a dot-entry, a
    // symlink or `~/bin`), the deny-list still holds after them.
    env.write("home/project/readme", "");
    env.write("home/notes.txt", "");
    env.write("home/bin/tool", "");
    std::os::unix::fs::symlink(env.work(), home.join("linked")).expect("symlink");
    let allowed = Layout {
        allow_home_write: true,
        ..l
    };
    let a = oxa_sandbox::without_account_home(bwrap_args_with(&spec("true", &home, true), &home, &allowed));
    let s = shown(&a, &env);
    assert_eq!(
        s[HEAD.len()..HEAD.len() + 9],
        strs(&[
            "--ro-bind",
            "<root>/home",
            "<root>/home",
            "--bind",
            "<root>/home/notes.txt",
            "<root>/home/notes.txt",
            "--bind",
            "<root>/home/project",
            "<root>/home/project",
        ])[..],
        "{s:?}"
    );
    assert!(
        !s.iter()
            .any(|x| x.ends_with("/home/bin") || x.ends_with("/home/linked")),
        "{s:?}"
    );
    // Plan mode in the same layout: nothing writable.
    let ro = shown(
        &oxa_sandbox::without_account_home(bwrap_args_with(&spec("true", &home, false), &home, &allowed)),
        &env,
    );
    assert!(!ro.iter().any(|x| x.ends_with("/home/project")), "{ro:?}");
    let masked = masks();
    assert!(
        s.windows(masked.len()).any(|w| w == masked.as_slice()),
        "the masks come after the writable home: {s:?}"
    );
}

#[test]
fn golden_root_and_parent_cwd_are_read_only() {
    let env = TestEnv::new();
    let home = env.home();
    env.write("project/readme", "");
    // O-199: a parent of `$HOME` or `/` is never a workspace, even with the setting.
    let allowed = Layout {
        allow_home_write: true,
        ..Layout::default()
    };
    for layout in [Layout::default(), allowed] {
        for cwd in [PathBuf::from("/"), env.root().to_path_buf()] {
            let a = bwrap_args_with(&spec("true", &cwd, true), &home, &layout);
            let c = cwd.display().to_string();
            let at = a.iter().position(|x| x == &c).expect("the cwd is bound");
            assert_eq!(a[at - 1], "--ro-bind", "{cwd:?}: {a:?}");
            assert!(!a.iter().any(|x| x == "--bind"), "{cwd:?}: {a:?}");
        }
    }
}

#[test]
fn golden_a_symlink_to_home_is_a_home_root() {
    // A cwd that only resolves to the home directory is the home directory: read-only
    // without a workspace (on main it was bound read-write with no deny-list masks), and
    // sbx-symhome: bound at the real home, with its masks (never at the alias, unmasked).
    let env = TestEnv::new();
    env.write("home/.ssh/id_test", "KEY");
    let home = env.home();
    let link = env.root().join("homelink");
    std::os::unix::fs::symlink(&home, &link).expect("symlink");
    let a = bwrap_args_with(&spec("true", &link, true), &home, &Layout::default());
    let s = shown(&a, &env);
    assert!(
        s.windows(3)
            .any(|w| w == ["--ro-bind", "<root>/home", "<root>/home"]),
        "{s:?}"
    );
    assert!(!s.iter().any(|x| x.contains("homelink")), "{s:?}");
    assert!(
        s.windows(4)
            .any(|w| w == ["--tmpfs", "<root>/home/.ssh", "--remount-ro", "<root>/home/.ssh"]),
        "{s:?}"
    );
}

#[test]
fn golden_plan_mode_binds_no_protected_paths() {
    let env = TestEnv::new();
    env.write("work/.oxa/settings.json", "{}");
    let a = bwrap_args_with(&spec("true", &env.work(), false), &env.home(), &Layout::default());
    let s = shown(&a, &env);
    assert!(s
        .windows(3)
        .any(|w| w == ["--ro-bind", "<root>/work", "<root>/work"]));
    assert!(
        !s.iter().any(|x| x.ends_with("/.oxa")),
        "the whole cwd is read-only already: {s:?}"
    );
}

// ------------------------------------------------------------------------------- real bwrap

fn required() -> bool {
    std::env::var("OXA_REQUIRE_BWRAP").is_ok_and(|v| v == "1")
}

/// The machine's bwrap, or `None` (a skip notice cargo shows; a failure under
/// `OXA_REQUIRE_BWRAP=1`).
fn real(env: &TestEnv, layout: Layout) -> Option<Bwrap> {
    let path = std::env::var("PATH").unwrap_or_default();
    match Bwrap::detect(&path, &env.home()) {
        Ok(mut b) => {
            b.layout = layout;
            Some(b)
        }
        Err(e) => {
            assert!(!required(), "OXA_REQUIRE_BWRAP=1 but bwrap is not usable: {e}");
            use std::io::Write as _;
            let _ = writeln!(
                std::io::stderr(),
                "h_sandbox_layout SKIPPED: bwrap is not usable: {e}"
            );
            None
        }
    }
}

fn run(b: &Bwrap, command: &str, cwd: &Path) -> (Option<i32>, String) {
    let r = b.run(&spec(command, cwd, true), &CancelToken::new(), &mut |_| {});
    (r.exit, String::from_utf8_lossy(&r.output).into_owned())
}

#[test]
fn real_toolchains_under_home_run() {
    let env = TestEnv::new();
    let l = machine(&env);
    let Some(b) = real(&env, l) else { return };
    let (exit, out) = run(&b, "mytool && cargo", &env.work());
    assert_eq!(exit, Some(0), "{out}");
    assert!(
        out.contains("mytool-ran") && out.contains("cargo from 1.0.0-test"),
        "{out}"
    );
    // The host's own cargo (a system toolchain) runs too.
    if Path::new("/usr/bin/cargo").exists() {
        let (exit, out) = run(&b, "/usr/bin/cargo --version", &env.work());
        assert_eq!(exit, Some(0), "{out}");
    }
    // Caches are writable and persist; toolchains are not writable.
    let (exit, out) = run(&b, "touch \"$HOME/.cargo/registry/written\"", &env.work());
    assert_eq!(exit, Some(0), "{out}");
    assert!(env.home().join(".cargo/registry/written").exists());
    let (exit, _) = run(&b, "touch \"$HOME/.cargo/bin/evil\"", &env.work());
    assert_ne!(exit, Some(0));
    assert!(!env.home().join(".cargo/bin/evil").exists());
}

#[test]
fn real_denied_files_are_neither_readable_nor_writable() {
    let env = TestEnv::new();
    let l = Layout {
        allow_home_write: true,
        ..machine(&env)
    };
    let Some(b) = real(&env, l) else { return };
    let home = env.home();
    // Even with the home directory as a writable cwd.
    let (_, out) = run(
        &b,
        "cat ~/.ssh/id_test; cat ~/.netrc; ls ~/.oxa/agent; echo done",
        &home,
    );
    assert!(out.contains("done"), "{out}");
    assert!(
        !out.contains("PRIVATE-KEY-MATERIAL") && !out.contains("hunter2"),
        "{out}"
    );
    assert!(!out.contains("trust.json"), "{out}");
    let (_, out) = run(
        &b,
        "echo x > ~/.ssh/authorized_keys; echo y >> ~/.netrc; touch ~/.oxa/agent/planted; echo done",
        &home,
    );
    assert!(out.contains("done"), "{out}");
    assert!(!home.join(".ssh/authorized_keys").exists());
    assert_eq!(
        std::fs::read_to_string(home.join(".netrc")).unwrap(),
        "machine example.com password hunter2"
    );
    assert!(!home.join(".oxa/agent/planted").exists());
}

/// O-199: started in `$HOME` and confirmed (or the setting): the home's folders are the
/// workspace; the startup files, `~/.local/bin`, `~/.config/systemd`, `~/.ssh`, `~/.oxa` and
/// the home root itself stay as they are on the host, byte for byte.
#[test]
fn real_home_workspace_writes_projects_and_never_startup_files_or_keys() {
    let env = TestEnv::new();
    let l = Layout {
        allow_home_write: true,
        ..machine(&env)
    };
    env.write("home/project/keep", "");
    let originals: Vec<(&str, &str)> = vec![
        (".bashrc", "# rc\n"),
        (".profile", "# profile\n"),
        (".zshrc", "# zsh\n"),
        (".local/bin/mytool", "#!/bin/sh\necho mytool-ran\n"),
        (".config/systemd/user/a.service", "[Unit]\n"),
        (".config/autostart/a.desktop", "[Desktop Entry]\n"),
        (".ssh/id_test", "PRIVATE-KEY-MATERIAL"),
        (".oxa/settings.json", "{}"),
        (".oxa/agent/trust.json", "{}"),
        ("bin/tool", "#!/bin/sh\n"),
    ];
    for (rel, body) in &originals {
        env.write(&format!("home/{rel}"), body);
    }
    let Some(b) = real(&env, l) else { return };
    let home = env.home();
    let (exit, out) = run(
        &b,
        "mkdir -p ~/project/sub && echo hi > ~/project/x && echo deep > ~/project/sub/y",
        &home,
    );
    assert_eq!(exit, Some(0), "{out}");
    assert_eq!(std::fs::read_to_string(home.join("project/x")).unwrap(), "hi\n");
    assert_eq!(
        std::fs::read_to_string(home.join("project/sub/y")).unwrap(),
        "deep\n"
    );
    for cmd in [
        "echo evil >> ~/.bashrc",
        "echo evil > ~/.profile",
        "rm -f ~/.zshrc",
        "echo evil > ~/.bash_profile",
        "echo evil > ~/.local/bin/mytool",
        "touch ~/.local/bin/planted",
        "echo evil > ~/.config/systemd/user/a.service",
        "touch ~/.config/systemd/user/planted.service",
        "touch ~/.config/autostart/planted.desktop",
        "echo x > ~/.ssh/authorized_keys",
        "echo evil > ~/.oxa/settings.json",
        "echo evil > ~/bin/tool",
        "touch ~/planted-at-root",
        "mv ~/project ~/moved",
    ] {
        let (exit, out) = run(&b, cmd, &home);
        assert_ne!(exit, Some(0), "{cmd} must fail: {out}");
    }
    for (rel, body) in &originals {
        assert_eq!(
            std::fs::read_to_string(home.join(rel)).unwrap_or_default(),
            *body,
            "{rel} changed on the host"
        );
    }
    for planted in [
        ".bash_profile",
        ".local/bin/planted",
        ".config/systemd/user/planted.service",
        ".config/autostart/planted.desktop",
        ".ssh/authorized_keys",
        "planted-at-root",
        "moved",
    ] {
        assert!(!home.join(planted).exists(), "{planted} appeared on the host");
    }
    // Keys stay unreadable.
    let (_, out) = run(&b, "cat ~/.ssh/id_test; echo done", &home);
    assert!(
        out.contains("done") && !out.contains("PRIVATE-KEY-MATERIAL"),
        "{out}"
    );
}

/// O-199: `/` and a parent of `$HOME` give bash no writable directory, even with the setting.
#[test]
fn real_parent_of_home_is_never_writable() {
    let env = TestEnv::new();
    let l = Layout {
        allow_home_write: true,
        ..machine(&env)
    };
    env.write("home/project/keep", "");
    let Some(b) = real(&env, l) else { return };
    let parent = env.root().to_path_buf();
    for cwd in [parent.clone(), PathBuf::from("/")] {
        let (exit, out) = run(&b, &format!("touch {}/planted", parent.display()), &cwd);
        assert_ne!(exit, Some(0), "{cwd:?}: {out}");
        assert!(out.contains("Read-only file system"), "{cwd:?}: {out}");
        let (exit, out) = run(&b, "touch ~/project/planted", &cwd);
        assert_ne!(exit, Some(0), "{cwd:?}: {out}");
        assert!(out.contains("Read-only file system"), "{cwd:?}: {out}");
        // A harmless read works: setup must not fail (the home tmpfs is covered by the cwd bind).
        let (exit, out) = run(&b, "cat /etc/hostname >/dev/null; echo marker-ok", &cwd);
        assert_eq!(exit, Some(0), "{cwd:?}: {out}");
        assert!(out.contains("marker-ok"), "{cwd:?}: {out}");
    }
    assert!(!parent.join("planted").exists());
    assert!(!env.home().join("project/planted").exists());
}

#[test]
fn real_home_cwd_without_the_setting_is_read_only() {
    let env = TestEnv::new();
    let Some(b) = real(&env, machine(&env)) else {
        return;
    };
    let home = env.home();
    let (exit, _) = run(&b, "touch ~/notes.txt", &home);
    assert_ne!(exit, Some(0));
    assert!(!home.join("notes.txt").exists());
}

#[test]
fn real_protected_project_files_are_read_only() {
    let env = TestEnv::new();
    env.write("work/.git/config", "[core]\n");
    env.write("work/.git/hooks/README", "");
    env.write("work/.oxa/prompts/p.md", "hi");
    let Some(b) = real(&env, Layout::default()) else {
        return;
    };
    let work = env.work();
    let (_, out) = run(
        &b,
        "echo evil > .git/hooks/pre-commit; echo '[core] hooksPath=/x' >> .git/config; echo '{}' > .oxa/mcp.json; echo ok > src.txt; echo done",
        &work,
    );
    assert!(out.contains("done"), "{out}");
    assert!(!work.join(".git/hooks/pre-commit").exists());
    assert_eq!(
        std::fs::read_to_string(work.join(".git/config")).unwrap(),
        "[core]\n"
    );
    assert!(!work.join(".oxa/mcp.json").exists());
    assert!(
        work.join("src.txt").exists(),
        "the rest of the project is writable"
    );
}

// ------------------------------------------------------------------- sbx-symhome (T3, PR #8)

/// Credentials a home holds (fake bytes only), relative to the home.
const CREDS: &[(&str, &str)] = &[
    (".ssh/id_test", "SSH-KEY-SENTINEL"),
    (".gnupg/private-keys-v1.d/k.key", "GPG-KEY-SENTINEL"),
    (".netrc", "NETRC-SENTINEL"),
    (".codex/auth.json", "CODEX-TOKEN-SENTINEL"),
    (".claude/.credentials.json", "CLAUDE-TOKEN-SENTINEL"),
    (".dsh/.credentials.yaml", "DSH-TOKEN-SENTINEL"),
    (".gemini/oauth_creds.json", "GEMINI-TOKEN-SENTINEL"),
    (".config/gh/hosts.yml", "GH-TOKEN-SENTINEL"),
    (".oxa/agent/auth.json", "OXA-KEY-SENTINEL"),
];

fn plant_creds(env: &TestEnv) {
    for (rel, body) in CREDS {
        env.write(&format!("home/{rel}"), body);
    }
}

/// Every credential stays unreadable through `paths` (relative to the cwd or absolute) and
/// unwritable, and its host bytes stay as they were.
fn assert_creds_hidden(b: &Bwrap, cwd: &Path, home: &Path, paths: &[String]) {
    let reads: String = paths.iter().map(|p| format!("cat '{p}' 2>/dev/null; ")).collect();
    let (_, out) = run(b, &format!("{reads}echo done"), cwd);
    assert!(out.contains("done"), "{out}");
    for (rel, body) in CREDS {
        assert!(!out.contains(body), "{rel} readable from {cwd:?}: {out}");
    }
    let writes: String = paths
        .iter()
        .map(|p| format!("echo evil > '{p}' 2>/dev/null; "))
        .collect();
    let (_, out) = run(b, &format!("{writes}echo done"), cwd);
    assert!(out.contains("done"), "{out}");
    for (rel, body) in CREDS {
        assert_eq!(
            std::fs::read_to_string(home.join(rel)).unwrap_or_default(),
            *body,
            "{rel} changed on the host"
        );
    }
}

/// T3 finding on ab555057: started through a symlink to `$HOME`, the home was bound at the
/// alias and the deny-list masks (lexical `$HOME/...`) were skipped, so every credential was
/// readable through the alias. Also through a symlink inside the home (`~/link-to-ssh`).
#[test]
fn real_symlink_to_home_start_masks_credentials() {
    let env = TestEnv::new();
    let l = Layout {
        allow_home_write: true,
        ..machine(&env)
    };
    plant_creds(&env);
    env.write("home/project/keep", "");
    let home = env.home();
    std::os::unix::fs::symlink(home.join(".ssh"), home.join("link-to-ssh")).expect("symlink");
    let link = env.root().join("homelink");
    std::os::unix::fs::symlink(&home, &link).expect("symlink");
    let Some(b) = real(&env, l) else { return };
    let mut paths: Vec<String> = CREDS.iter().map(|(rel, _)| (*rel).to_string()).collect();
    paths.extend(CREDS.iter().map(|(rel, _)| format!("{}/{rel}", link.display())));
    paths.extend(CREDS.iter().map(|(rel, _)| format!("{}/{rel}", home.display())));
    paths.push("link-to-ssh/id_test".into());
    assert_creds_hidden(&b, &link, &home, &paths);
    // Still the home workspace of a normal start: projects writable, the root read-only.
    let (exit, out) = run(&b, "echo hi > project/x", &link);
    assert_eq!(exit, Some(0), "{out}");
    let (exit, _) = run(&b, "touch planted-at-root", &link);
    assert_ne!(exit, Some(0));
    assert!(!home.join("planted-at-root").exists());
}

/// The same through a symlinked subdirectory of the home (here `~/.config`, which holds a
/// deny-listed `gh`): a project cwd that resolves into the home keeps its masks.
#[test]
fn real_symlinked_subdir_of_home_masks_credentials() {
    let env = TestEnv::new();
    let l = machine(&env);
    plant_creds(&env);
    let home = env.home();
    let link = env.work().join("cfg");
    std::os::unix::fs::symlink(home.join(".config"), &link).expect("symlink");
    let Some(b) = real(&env, l) else { return };
    let paths = vec![
        "gh/hosts.yml".to_string(),
        format!("{}/gh/hosts.yml", link.display()),
        format!("{}/.config/gh/hosts.yml", home.display()),
    ];
    assert_creds_hidden(&b, &link, &home, &paths);
    let (exit, out) = run(&b, "echo ok > notes.txt", &link);
    assert_eq!(exit, Some(0), "{out}");
    assert!(home.join(".config/notes.txt").exists());
}

/// The normal start in `$HOME` (not through a symlink) is unchanged: the same masks hide
/// the same credentials.
#[test]
fn real_plain_home_start_still_masks_credentials() {
    let env = TestEnv::new();
    let l = Layout {
        allow_home_write: true,
        ..machine(&env)
    };
    plant_creds(&env);
    let home = env.home();
    let Some(b) = real(&env, l) else { return };
    let mut paths: Vec<String> = CREDS.iter().map(|(rel, _)| (*rel).to_string()).collect();
    paths.extend(CREDS.iter().map(|(rel, _)| format!("{}/{rel}", home.display())));
    assert_creds_hidden(&b, &home, &home, &paths);
}

/// Fail closed: a cwd that cannot be resolved runs nothing.
#[test]
fn real_unresolvable_cwd_is_refused() {
    let env = TestEnv::new();
    let Some(b) = real(&env, Layout::default()) else {
        return;
    };
    let gone = env.root().join("gone");
    let r = b.run(&spec("echo ran", &gone, true), &CancelToken::new(), &mut |_| {});
    let out = String::from_utf8_lossy(&r.output);
    assert_eq!(r.exit, None, "{out}");
    assert!(
        out.contains("cannot be resolved") && !out.contains("ran\n"),
        "{out}"
    );
}

/// Masks come last: in the argv, every credential mask follows every bind that could cover it
/// (the cwd bind of `/`, a parent of home, or home itself), and nothing is bound over home
/// after the masks.
#[test]
fn golden_masks_come_after_every_bind_that_covers_them() {
    let env = TestEnv::new();
    plant_creds(&env);
    let home = env.home();
    let layout = Layout {
        allow_home_write: true,
        ..machine(&env)
    };
    for cwd in [PathBuf::from("/"), env.root().to_path_buf(), home.clone()] {
        let a = bwrap_args_with(&spec("true", &cwd, true), &home, &layout);
        let first_mask = a
            .iter()
            .position(|x| x == "--tmpfs" && a.iter().any(|y| y.ends_with("/.ssh")))
            .and_then(|_| {
                a.windows(2)
                    .position(|w| w[0] == "--tmpfs" && w[1].ends_with("/.ssh"))
            })
            .expect("the .ssh mask");
        let last_mask = a
            .windows(3)
            .rposition(|w| w[0] == "--ro-bind" && w[1] == "/dev/null")
            .max(
                a.windows(2)
                    .rposition(|w| w[0] == "--tmpfs" && w[1].ends_with("/.ssh")),
            )
            .expect("masks");
        for (i, w) in a.windows(3).enumerate() {
            let covers = (w[0] == "--bind" || w[0] == "--ro-bind")
                && w[1] != "/dev/null"
                && w[1] == w[2]
                && home.starts_with(Path::new(&w[1]));
            if covers {
                assert!(i < first_mask, "{cwd:?}: bind of {} after a mask: {a:?}", w[1]);
            }
        }
        assert!(
            !a.windows(2)
                .skip(last_mask)
                .any(|w| (w[0] == "--bind" || w[0] == "--ro-bind") && Path::new(&w[1]).starts_with(&home)),
            "{cwd:?}: nothing binds home after the last mask: {a:?}"
        );
    }
}

/// With cwd = a parent of home (and `/`), the credentials in the real home stay hidden while an
/// ordinary file in home is readable (P1 on ddbd025: a late bind of home exposed them).
#[test]
fn real_parent_cwd_hides_credentials_but_reads_ordinary_files() {
    let env = TestEnv::new();
    let l = Layout {
        allow_home_write: true,
        ..machine(&env)
    };
    plant_creds(&env);
    env.write("home/ordinary.txt", "ORDINARY-SENTINEL");
    let home = env.home();
    let Some(b) = real(&env, l) else { return };
    for cwd in [env.root().to_path_buf(), PathBuf::from("/")] {
        let (exit, out) = run(&b, "cat ~/ordinary.txt", &cwd);
        assert_eq!(exit, Some(0), "{cwd:?}: {out}");
        assert!(out.contains("ORDINARY-SENTINEL"), "{cwd:?}: {out}");
        // Absolute paths, expanded here: no shell tilde inside the helper's single quotes.
        let abs: Vec<String> = CREDS
            .iter()
            .map(|(rel, _)| format!("{}/{rel}", home.display()))
            .collect();
        assert_creds_hidden(&b, &cwd, &home, &abs);
    }
}

/// Live regression of the gh token-store mask: `hosts.yml` must stay a readable regular file
/// (gh fails on an unreadable one) but its content (other hosts' tokens) must stay hidden, and
/// the benign config stays readable.
#[test]
fn real_gh_hosts_file_is_readable_empty_and_hides_the_token_store() {
    let env = TestEnv::new();
    env.write("home/.config/gh/hosts.yml", "SYNTH-HOST-TOKEN-SENTINEL");
    env.write("home/.config/gh/config.yml", "editor: synth-editor");
    let Some(b) = real(&env, machine(&env)) else {
        return;
    };
    let mut sp = spec(
        "cat \"$HOME/.config/gh/hosts.yml\" && test -f \"$HOME/.config/gh/hosts.yml\" && echo REGULAR; cat \"$HOME/.config/gh/config.yml\"",
        &env.work(),
        true,
    );
    sp.net = true;
    let r = b.run(&sp, &CancelToken::new(), &mut |_| {});
    let (exit, out) = (r.exit, String::from_utf8_lossy(&r.output).into_owned());
    assert_eq!(exit, Some(0), "{out}");
    assert!(out.contains("REGULAR") && out.contains("synth-editor"), "{out}");
    assert!(!out.contains("SYNTH-HOST-TOKEN-SENTINEL"), "{out}");
    assert_eq!(
        std::fs::read_to_string(env.home().join(".config/gh/hosts.yml")).unwrap(),
        "SYNTH-HOST-TOKEN-SENTINEL"
    );
}

/// #250: mise shims (`gh`, `claude`, ...) write their cache and tracked-config state; those
/// directories are writable when they exist, so the shim does not fail on a read-only home.
#[test]
fn mise_cache_and_state_are_writable() {
    let env = TestEnv::new();
    env.write("home/.cache/mise/gh/keep", "");
    env.write("home/.local/state/mise/tracked-configs/keep", "");
    env.write("home/.local/state/mise/trusted-configs/keep", "");
    let home = env.home();
    let l = Layout::build(&LayoutInputs {
        home: home.clone(),
        agent_dir: Some(env.agent_dir()),
        path_var: "/usr/bin".into(),
        env: Vec::new(),
        read_extra: Vec::new(),
        write_extra: Vec::new(),
        allow_home_write: false,
        exe: None,
        exe_in_workspace: false,
    });
    assert!(l.write.contains(&home.join(".cache/mise")), "{:?}", l.write);
    assert!(
        l.write.contains(&home.join(".local/state/mise/tracked-configs")),
        "{:?}",
        l.write
    );
    // Trust records stay unwritable: no writable bind may be, or contain, `trusted-configs`.
    let trusted = home.join(".local/state/mise/trusted-configs");
    assert!(!l.write.iter().any(|w| trusted.starts_with(w)), "{:?}", l.write);
}

/// #457 (supersedes #362): every tool gets one writable cache home, the session directory at
/// `/tmp`: `XDG_CACHE_HOME` and `TMPDIR` point there even when the user set them to a host
/// path (read-only inside); no bind of the real `~/.cache/gh`, no host directory created.
#[test]
fn cache_home_and_tmpdir_are_the_session_tmp() {
    let env = TestEnv::new();
    let home = env.home();
    let l = Layout::build(&LayoutInputs {
        home: home.clone(),
        agent_dir: Some(PathBuf::from("/nonexistent-agent")),
        path_var: "/usr/bin".into(),
        env: vec![
            ("XDG_CACHE_HOME".into(), home.join(".cache").display().to_string()),
            ("TMPDIR".into(), "/var/tmp/host".into()),
        ],
        ..LayoutInputs::default()
    });
    assert!(
        l.env.contains(&("XDG_CACHE_HOME".into(), "/tmp/.cache".into())),
        "{:?}",
        l.env
    );
    assert!(l.env.contains(&("TMPDIR".into(), "/tmp".into())), "{:?}", l.env);
    assert_eq!(l.env.iter().filter(|(k, _)| k == "XDG_CACHE_HOME").count(), 1);
    assert!(!l
        .env
        .iter()
        .any(|(k, _)| k == "GH_CACHE_DIR" || k == "MISE_CACHE_DIR"));
    assert!(!l.write.contains(&home.join(".cache/gh")));
    assert!(!home.join(".cache/gh").exists(), "build must not touch the host");
}

/// #457, N28: the session directory replaces `--tmpfs /tmp` at the same place, before the
/// home tmpfs and before every credential mask, so no mask can be covered by it.
#[test]
fn golden_session_dir_is_bound_at_tmp_before_every_mask() {
    let env = TestEnv::new();
    let l = machine(&env);
    let sc = oxa_sandbox::Scratch::create(&oxa_sandbox::scratch_root(&env.home())).expect("scratch");
    let dir = sc.path().to_path_buf();
    let l = l.with_scratch(sc);
    let a =
        oxa_sandbox::without_account_home(bwrap_args_with(&spec("true", &env.work(), true), &env.home(), &l));
    let s = shown(&a, &env);
    let mut head = strs(HEAD);
    head[12] = "--bind".into();
    head[13] = dir
        .display()
        .to_string()
        .replace(&env.root().display().to_string(), "<root>");
    head.insert(14, "/tmp".into());
    assert_eq!(s[..head.len()], head[..], "{s:?}");
    assert!(!s.windows(2).any(|w| w == ["--tmpfs", "/tmp"]), "{s:?}");
    // The home tmpfs, the binds and the masks all follow it (the prefix above).
    // Without a session directory nothing changes.
    let a = oxa_sandbox::without_account_home(bwrap_args_with(
        &spec("true", &env.work(), true),
        &env.home(),
        &machine(&env),
    ));
    assert_eq!(shown(&a, &env)[..HEAD.len()], strs(HEAD)[..]);
}

/// The links `with_scratch` made in the session directory, as (name, target).
fn cache_links(dir: &Path) -> Vec<(String, PathBuf)> {
    let mut v: Vec<(String, PathBuf)> = std::fs::read_dir(dir.join(".cache"))
        .unwrap()
        .flatten()
        .map(|e| {
            let t = std::fs::read_link(e.path()).expect("only links in .cache");
            (e.file_name().to_string_lossy().into_owned(), t)
        })
        .collect();
    v.sort();
    v
}

fn cache_layout(env: &TestEnv, user_env: Vec<(String, String)>, write_extra: Vec<PathBuf>) -> Layout {
    Layout::build(&LayoutInputs {
        home: env.home(),
        agent_dir: Some(env.agent_dir()),
        path_var: "/usr/bin".into(),
        env: user_env,
        write_extra,
        ..LayoutInputs::default()
    })
}

/// #457: without `XDG_CACHE_HOME` the user's cache home is `~/.cache`: each read-write bound
/// cache in it is linked from `/tmp/.cache/<name>`; an unbound one (`~/.cache/gh`) is not.
#[test]
fn default_cache_home_links_its_read_write_caches() {
    let env = TestEnv::new();
    let home = env.home();
    for d in [".cache/sccache", ".cache/pip", ".cache/gh"] {
        std::fs::create_dir_all(home.join(d)).unwrap();
    }
    let l = cache_layout(&env, Vec::new(), Vec::new());
    assert_eq!(l.cache_home, home.join(".cache"));
    let sc = oxa_sandbox::Scratch::create(&oxa_sandbox::scratch_root(&home)).expect("scratch");
    let dir = sc.path().to_path_buf();
    let _l = l.with_scratch(sc);
    assert_eq!(
        cache_links(&dir),
        vec![
            ("pip".to_string(), home.join(".cache/pip")),
            ("sccache".to_string(), home.join(".cache/sccache")),
        ]
    );
}

/// #457 review: a user-set `XDG_CACHE_HOME` keeps its shared caches. The rule: the shared
/// caches are the read-write bound entries of the user's cache home (`$XDG_CACHE_HOME`, else
/// `~/.cache`), linked from `/tmp/.cache/<name>`; everything else caches in the session.
/// `~/.cache/pip` is not the user's pip cache here, so it is not linked; an entry under a
/// bound cache home is (the whole home bound by `writePaths`).
#[test]
fn user_cache_home_keeps_its_read_write_caches() {
    let env = TestEnv::new();
    let home = env.home();
    let xdg = env.root().join("data/cache");
    for d in ["sccache", "pip", "unbound"] {
        std::fs::create_dir_all(xdg.join(d)).unwrap();
    }
    std::fs::create_dir_all(home.join(".cache/pip")).unwrap();
    let user = vec![("XDG_CACHE_HOME".to_string(), xdg.display().to_string())];
    let l = cache_layout(&env, user.clone(), vec![xdg.join("sccache"), xdg.join("pip")]);
    assert_eq!(l.cache_home, xdg);
    // Inside, the cache home is still the session's, never the read-only host path.
    assert!(
        l.env.contains(&("XDG_CACHE_HOME".into(), "/tmp/.cache".into())),
        "{:?}",
        l.env
    );
    let sc = oxa_sandbox::Scratch::create(&oxa_sandbox::scratch_root(&home)).expect("scratch");
    let dir = sc.path().to_path_buf();
    let _l = l.with_scratch(sc);
    assert_eq!(
        cache_links(&dir),
        vec![
            ("pip".to_string(), xdg.join("pip")),
            ("sccache".to_string(), xdg.join("sccache")),
        ]
    );

    // The whole cache home bound read-write: every entry in it is shared.
    let l = cache_layout(&env, user, vec![xdg.clone()]);
    let sc = oxa_sandbox::Scratch::create(&oxa_sandbox::scratch_root(&home)).expect("scratch");
    let dir = sc.path().to_path_buf();
    let _l = l.with_scratch(sc);
    assert_eq!(cache_links(&dir).len(), 3, "{:?}", cache_links(&dir));

    // A relative `XDG_CACHE_HOME` is ignored, as the XDG rule says: `~/.cache` again.
    let l = cache_layout(
        &env,
        vec![("XDG_CACHE_HOME".into(), "rel/cache".into())],
        Vec::new(),
    );
    assert_eq!(l.cache_home, home.join(".cache"));
}

/// #457 review: each session of one `Bwrap` gets its own directory at `/tmp`, removed when
/// that session's sandbox is dropped; the process sandbox keeps a per-command tmpfs. When no
/// directory can be made, the session falls back to the tmpfs and its notice says so.
#[test]
fn every_session_has_its_own_scratch() {
    let env = TestEnv::new();
    let home = env.home();
    let root = oxa_sandbox::scratch_root(&home);
    let base = Bwrap {
        bwrap: PathBuf::from("/usr/bin/bwrap"),
        home: home.clone(),
        layout: Layout {
            scratch_root: Some(root.clone()),
            ..Layout::default()
        },
    };
    let tmp = |b: &dyn Sandbox| {
        let a = b.bwrap_args(&spec("true", &env.work(), true));
        a.windows(3)
            .find(|w| w[0] == "--bind" && w[2] == "/tmp")
            .map(|w| PathBuf::from(&w[1]))
    };
    assert_eq!(tmp(&base), None, "the process sandbox has no session directory");
    let one = base.for_session().expect("a session sandbox");
    let two = base.for_session().expect("a session sandbox");
    let (a, b) = (tmp(one.as_ref()).unwrap(), tmp(two.as_ref()).unwrap());
    assert_ne!(a, b);
    assert!(a.is_dir() && b.is_dir() && a.starts_with(&root));
    assert_eq!(one.persistence_notice(), Some(oxa_sandbox::SCRATCH_NOTICE));
    let clone = one.clone();
    drop(one);
    assert!(a.is_dir(), "removed while the session still holds it");
    drop(clone);
    assert!(!a.exists(), "the session directory outlived its session");
    assert!(b.is_dir());
    drop(two);
    assert!(!b.exists());

    // No root: no session sandbox of its own.
    let plain = Bwrap {
        layout: Layout::default(),
        ..base.clone()
    };
    assert!(plain.for_session().is_none());
    // A root that cannot be created (a file is in the way): the per-command tmpfs.
    std::fs::write(env.root().join("blocker"), "").unwrap();
    let blocked = Bwrap {
        layout: Layout {
            scratch_root: Some(env.root().join("blocker/scratch")),
            ..Layout::default()
        },
        ..base
    };
    let s = blocked.for_session().expect("a session sandbox");
    assert_eq!(tmp(s.as_ref()), None);
    assert!(s
        .persistence_notice()
        .unwrap()
        .contains("discarded after each command"));
}
