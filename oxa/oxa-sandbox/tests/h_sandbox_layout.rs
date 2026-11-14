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
        vec!["CARGO_HOME", "MISE_DATA_DIR"],
        "no token, no key, nothing unlisted"
    );
}

#[test]
fn golden_project_cwd() {
    let env = TestEnv::new();
    let l = machine(&env);
    env.write("work/.git/config", "[core]\n");
    env.write("work/.git/hooks/pre-commit.sample", "");
    env.write("work/.oxa/settings.json", "{}");
    let a = bwrap_args_with(&spec("true", &env.work(), true), &env.home(), &l);
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
    let a = bwrap_args_with(&spec("true", &home, true), &home, &l);
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
    let a = bwrap_args_with(&spec("true", &home, true), &home, &allowed);
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
        &bwrap_args_with(&spec("true", &home, false), &home, &allowed),
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
    // without a workspace (on main it was bound read-write with no deny-list masks).
    let env = TestEnv::new();
    env.write("home/.ssh/id_test", "KEY");
    let home = env.home();
    let link = env.root().join("homelink");
    std::os::unix::fs::symlink(&home, &link).expect("symlink");
    let a = bwrap_args_with(&spec("true", &link, true), &home, &Layout::default());
    let c = link.display().to_string();
    let at = a.iter().position(|x| x == &c).expect("the cwd is bound");
    assert_eq!(a[at - 1], "--ro-bind", "{a:?}");
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
        let (exit, out) = run(&b, "touch ~/project/planted", &cwd);
        assert_ne!(exit, Some(0), "{cwd:?}: {out}");
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
