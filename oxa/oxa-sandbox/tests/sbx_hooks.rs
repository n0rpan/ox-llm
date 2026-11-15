//! sbx-hooks: git's hooks and config stay as they are in a writable sandbox, also when they do
//! not exist yet.
//!
//! * Argv: a missing `hooks` / `config` / `config.worktree` of a real git directory (the cwd's
//!   `.git`, the common dir and gitdir of a worktree) is masked; plan mode masks nothing; a repo
//!   whose hooks and config exist keeps its argv; symlinks are never bound over; a worktree's
//!   `.git` file is read-only.
//! * Real bwrap: the command cannot create a hook or config, git still works inside and outside
//!   afterwards, and the control: git in a worktree fails without the sbx-git binds. Real-bwrap
//!   tests skip with a notice when bwrap is unusable (a failure under `OXA_REQUIRE_BWRAP=1`).
//!
//! Isolation: every path lives under `oxa_fake::TestEnv`; git runs with no global config.

mod common {
    pub mod host_watch;
}
use common::host_watch::HostWatch;
use oxa_fake::TestEnv;
use oxa_proto::{CancelToken, ExecSpec, Sandbox};
use oxa_sandbox::{bwrap_args_with, resolve_git_binds, Bwrap, Layout};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

// ------------------------------------------------------------------------------- helpers

fn spec(command: &str, cwd: &Path, writable: bool) -> ExecSpec {
    ExecSpec {
        command: command.to_string(),
        cwd: cwd.to_path_buf(),
        writable,
        net: false,
        timeout_s: Some(30),
        env: Vec::new(),
    }
}

fn dir(p: &Path) -> PathBuf {
    fs::create_dir_all(p).expect("mkdir");
    p.to_path_buf()
}

fn file(p: &Path, body: &str) {
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).expect("mkdir parent");
    }
    fs::write(p, body).expect("write");
}

fn canon(p: &Path) -> PathBuf {
    fs::canonicalize(p).expect("canonicalize")
}

fn shown(args: &[String], env: &TestEnv) -> Vec<String> {
    let root = env.root().display().to_string();
    args.iter().map(|a| a.replace(&root, "<root>")).collect()
}

fn contains_run(hay: &[String], needle: &[String]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn index_of_run(hay: &[String], needle: &[String]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn strs(xs: &[&str]) -> Vec<String> {
    xs.iter().map(|x| (*x).to_string()).collect()
}

fn tmpfs_mask(p: &str) -> Vec<String> {
    strs(&["--tmpfs", p, "--remount-ro", p])
}

fn null_mask(p: &str) -> Vec<String> {
    strs(&["--ro-bind", "/dev/null", p])
}

fn ro(p: &str) -> Vec<String> {
    strs(&["--ro-bind", p, p])
}

fn bwrap_value(env: &TestEnv) -> Bwrap {
    Bwrap {
        bwrap: PathBuf::from("/usr/bin/bwrap"),
        home: env.home(),
        layout: Layout::default(),
    }
}

fn args_of(b: &Bwrap, cwd: &Path, writable: bool, env: &TestEnv) -> Vec<String> {
    shown(
        &oxa_sandbox::without_account_home(b.bwrap_args(&spec("git status", cwd, writable))),
        env,
    )
}

/// Every path a `/dev/null` bind or a tmpfs lands on, without the `commondir` / `gitdir` masks
/// every writable git directory gets (#201; checked in their own tests).
fn masked(args: &[String]) -> Vec<String> {
    masked_all(args)
        .into_iter()
        .filter(|p| !p.ends_with("/commondir") && !p.ends_with("/gitdir"))
        .collect()
}

/// `protect` without the `commondir` / `gitdir` entries (#201; checked in their own tests).
fn no_pointers(v: Vec<PathBuf>) -> Vec<PathBuf> {
    v.into_iter()
        .filter(|p| !p.ends_with("commondir") && !p.ends_with("gitdir"))
        .collect()
}

/// Every path a `/dev/null` bind or a tmpfs lands on.
fn masked_all(args: &[String]) -> Vec<String> {
    args.windows(3)
        .filter_map(|w| {
            if w[0] == "--ro-bind" && w[1] == "/dev/null" {
                Some(w[2].clone())
            } else if w[0] == "--tmpfs" && w[2] == "--remount-ro" {
                Some(w[1].clone())
            } else {
                None
            }
        })
        .collect()
}

/// A hand-made plain repository at `<work>/<name>`: `.git` with HEAD and objects only.
fn bare_plain(env: &TestEnv, name: &str) -> PathBuf {
    let repo = dir(&env.work().join(name));
    file(&repo.join(".git/HEAD"), "ref: refs/heads/main\n");
    dir(&repo.join(".git/objects/pack"));
    repo
}

/// A worktree of a hand-built main repository (as in `git_worktree_binds.rs`), with a `config`
/// of `config_body` and no `hooks` directory.
struct Worktree {
    main_git: PathBuf,
    gitdir: PathBuf,
    wt: PathBuf,
}

fn build_worktree(env: &TestEnv, config_body: &str) -> Worktree {
    let root = env.root();
    let main_git = dir(&root.join("main/.git"));
    file(&main_git.join("HEAD"), "ref: refs/heads/main\n");
    dir(&main_git.join("objects/pack"));
    file(&main_git.join("config"), config_body);
    let gitdir = dir(&main_git.join("worktrees/wt1"));
    file(&gitdir.join("HEAD"), "ref: refs/heads/feature\n");
    file(&gitdir.join("commondir"), "../..\n");
    let wt = dir(&root.join("work/wt1"));
    file(
        &gitdir.join("gitdir"),
        &format!("{}\n", wt.join(".git").display()),
    );
    file(&wt.join(".git"), &format!("gitdir: {}\n", gitdir.display()));
    Worktree { main_git, gitdir, wt }
}

const WORKTREE_CONFIG_ON: &str =
    "[core]\n\trepositoryformatversion = 1\n[extensions]\n\tworktreeConfig = true\n";

// ------------------------------------------------------------------------- argv: cwd .git

#[test]
fn plain_repo_without_hooks_masks_the_hooks_dir() {
    let env = TestEnv::new();
    let repo = bare_plain(&env, "plain");
    file(&repo.join(".git/config"), "[core]\n");
    let args = args_of(&bwrap_value(&env), &repo, true, &env);
    let cfg = ro("<root>/work/plain/.git/config");
    let mask = tmpfs_mask("<root>/work/plain/.git/hooks");
    let cfg_at = index_of_run(&args, &cfg).expect("config read-only");
    let mask_at = index_of_run(&args, &mask).expect("hooks masked");
    let chdir_at = index_of_run(&args, &strs(&["--chdir"])).expect("chdir");
    assert!(cfg_at < mask_at && mask_at < chdir_at, "{args:?}");
    assert_eq!(
        masked(&args),
        vec!["<root>/work/plain/.git/hooks".to_string()],
        "{args:?}"
    );
}

#[test]
fn plain_repo_without_config_masks_the_config_file() {
    let env = TestEnv::new();
    let repo = bare_plain(&env, "plain");
    dir(&repo.join(".git/hooks"));
    let args = args_of(&bwrap_value(&env), &repo, true, &env);
    assert!(
        contains_run(&args, &ro("<root>/work/plain/.git/hooks")),
        "{args:?}"
    );
    assert!(
        contains_run(&args, &null_mask("<root>/work/plain/.git/config")),
        "{args:?}"
    );
    // A missing config turns no extension on: config.worktree stays unmasked.
    assert_eq!(masked(&args), vec!["<root>/work/plain/.git/config".to_string()]);
}

#[test]
fn plain_repo_with_worktree_config_protects_config_worktree() {
    let env = TestEnv::new();
    let repo = bare_plain(&env, "plain");
    file(&repo.join(".git/config"), WORKTREE_CONFIG_ON);
    dir(&repo.join(".git/hooks"));
    let args = args_of(&bwrap_value(&env), &repo, true, &env);
    assert!(
        contains_run(&args, &null_mask("<root>/work/plain/.git/config.worktree")),
        "{args:?}"
    );
    // Once it exists (the mount point stays on the host), it is bound read-only.
    file(&repo.join(".git/config.worktree"), "");
    let args = args_of(&bwrap_value(&env), &repo, true, &env);
    assert!(
        contains_run(&args, &ro("<root>/work/plain/.git/config.worktree")),
        "{args:?}"
    );
    assert!(masked(&args).is_empty(), "{args:?}");
}

#[test]
fn existing_hooks_and_config_keep_the_argv_byte_for_byte() {
    // The B4 segment of a repository whose hooks and config exist is what it was before:
    // `.oxa`, `.git/hooks`, `.git/config` read-only, nothing more.
    let env = TestEnv::new();
    let repo = bare_plain(&env, "full");
    file(&repo.join(".git/config"), "[core]\n");
    file(&repo.join(".git/hooks/pre-commit.sample"), "");
    dir(&repo.join(".oxa"));
    let args = args_of(&bwrap_value(&env), &repo, true, &env);
    let mut want = strs(&["--bind", "<root>/work/full", "<root>/work/full"]);
    // The rename guard (the `.git` directory bound onto itself) is the one addition.
    want.extend(strs(&[
        "--bind",
        "<root>/work/full/.git",
        "<root>/work/full/.git",
    ]));
    want.extend(ro("<root>/work/full/.oxa"));
    want.extend(ro("<root>/work/full/.git/hooks"));
    want.extend(ro("<root>/work/full/.git/config"));
    // #201: the missing commondir / gitdir are masked.
    want.extend(null_mask("<root>/work/full/.git/commondir"));
    want.extend(null_mask("<root>/work/full/.git/gitdir"));
    want.extend(strs(&[
        "--remount-ro",
        "<root>/home",
        "--chdir",
        "<root>/work/full",
    ]));
    let at = index_of_run(&args, &strs(&["--bind", "<root>/work/full"])).expect("cwd bind");
    // The deny-list masks of an empty TestEnv home are not shown (the cwd is outside home).
    assert_eq!(args[at..at + want.len()].to_vec(), want, "{args:?}");
}

#[test]
fn read_only_sandbox_masks_nothing() {
    let env = TestEnv::new();
    let repo = bare_plain(&env, "plain");
    let args = args_of(&bwrap_value(&env), &repo, false, &env);
    assert!(masked(&args).is_empty(), "{args:?}");
    assert!(!args.iter().any(|a| a.contains("/.git")), "{args:?}");
    let w = build_worktree(&env, "[core]\n");
    let b = bwrap_value(&env).with_git_dirs(&w.wt);
    let args = args_of(&b, &w.wt, false, &env);
    assert!(masked(&args).is_empty(), "{args:?}");
    assert!(!contains_run(&args, &ro("<root>/work/wt1/.git")), "{args:?}");
}

#[test]
fn symlinks_are_never_bound_over_or_masked_through() {
    let env = TestEnv::new();
    // `.git/hooks` is a symlink: neither bound nor masked.
    let repo = bare_plain(&env, "linked-hooks");
    file(&repo.join(".git/config"), "[core]\n");
    let elsewhere = dir(&env.root().join("hooks-elsewhere"));
    std::os::unix::fs::symlink(&elsewhere, repo.join(".git/hooks")).expect("symlink");
    let args = args_of(&bwrap_value(&env), &repo, true, &env);
    assert!(!args.iter().any(|a| a.ends_with("/.git/hooks")), "{args:?}");
    // `.git` itself is a symlink to a git directory: nothing below it is bound or masked.
    let real = bare_plain(&env, "real");
    let cwd = dir(&env.work().join("linked-dotgit"));
    std::os::unix::fs::symlink(real.join(".git"), cwd.join(".git")).expect("symlink");
    let args = args_of(&bwrap_value(&env), &cwd, true, &env);
    assert!(!args.iter().any(|a| a.contains("linked-dotgit/.git")), "{args:?}");
}

#[test]
fn worktree_dotgit_file_is_read_only() {
    let env = TestEnv::new();
    let w = build_worktree(&env, "[core]\n");
    let b = bwrap_value(&env).with_git_dirs(&w.wt);
    let args = args_of(&b, &w.wt, true, &env);
    assert!(contains_run(&args, &ro("<root>/work/wt1/.git")), "{args:?}");
}

// ------------------------------------------------------------------- argv: worktree

#[test]
fn worktree_common_dir_without_hooks_masks_them() {
    let env = TestEnv::new();
    let w = build_worktree(&env, "[core]\n");
    let got = resolve_git_binds(&w.wt, &env.home(), Some(&env.agent_dir()));
    assert_eq!(
        no_pointers(got.protect),
        vec![
            canon(&w.main_git).join("config"),
            canon(&w.main_git).join("config.worktree"),
            canon(&w.main_git).join("hooks"),
            canon(&w.main_git).join("info"),
            canon(&w.gitdir).join("config.worktree"),
            canon(&w.gitdir).join("hooks"),
        ],
        "issue #103: a worktree also protects info, config.worktree and its gitdir's hooks"
    );
    let b = bwrap_value(&env).with_git_dirs(&w.wt);
    let args = args_of(&b, &w.wt, true, &env);
    assert!(contains_run(&args, &ro("<root>/main/.git/config")), "{args:?}");
    assert!(
        contains_run(&args, &tmpfs_mask("<root>/main/.git/hooks")),
        "{args:?}"
    );
    let m = masked(&args);
    assert!(m.contains(&"<root>/main/.git/hooks".to_string()), "{args:?}");
    assert!(m.contains(&"<root>/main/.git/info".to_string()), "{args:?}");
    assert!(
        m.contains(&"<root>/main/.git/worktrees/wt1/hooks".to_string()),
        "{args:?}"
    );
}

#[test]
fn worktree_with_worktree_config_masks_both_config_worktree_files() {
    let env = TestEnv::new();
    let w = build_worktree(&env, WORKTREE_CONFIG_ON);
    dir(&w.main_git.join("hooks"));
    let got = resolve_git_binds(&w.wt, &env.home(), Some(&env.agent_dir()));
    let mc = canon(&w.main_git);
    let gd = canon(&w.gitdir);
    let mut want = vec![
        mc.join("config"),
        mc.join("config.worktree"),
        mc.join("hooks"),
        mc.join("info"),
        gd.join("config.worktree"),
        gd.join("hooks"),
    ];
    want.sort();
    assert_eq!(no_pointers(got.protect), want);
    let b = bwrap_value(&env).with_git_dirs(&w.wt);
    let args = args_of(&b, &w.wt, true, &env);
    assert_eq!(
        masked(&args),
        vec![
            "<root>/main/.git/config.worktree".to_string(),
            "<root>/main/.git/info".to_string(),
            "<root>/main/.git/worktrees/wt1/config.worktree".to_string(),
            "<root>/main/.git/worktrees/wt1/hooks".to_string(),
        ],
        "{args:?}"
    );
}

#[test]
fn worktree_with_existing_hooks_and_config_is_unchanged() {
    // The protect set of the previous lane: existing entries only, all read-only.
    let env = TestEnv::new();
    let w = build_worktree(&env, "[core]\n");
    file(&w.main_git.join("hooks/pre-commit"), "#!/bin/sh\n");
    let got = resolve_git_binds(&w.wt, &env.home(), Some(&env.agent_dir()));
    let mc = canon(&w.main_git);
    let gd = canon(&w.gitdir);
    assert_eq!(
        no_pointers(got.protect),
        vec![
            mc.join("config"),
            mc.join("config.worktree"),
            mc.join("hooks"),
            mc.join("info"),
            gd.join("config.worktree"),
            gd.join("hooks"),
        ]
    );
    let b = bwrap_value(&env).with_git_dirs(&w.wt);
    let args = args_of(&b, &w.wt, true, &env);
    assert!(contains_run(&args, &ro("<root>/main/.git/config")), "{args:?}");
    assert!(contains_run(&args, &ro("<root>/main/.git/hooks")), "{args:?}");
    assert!(
        !masked(&args).contains(&"<root>/main/.git/hooks".to_string()),
        "{args:?}"
    );
}

#[test]
fn plain_repository_above_the_cwd_masks_its_missing_hooks() {
    let env = TestEnv::new();
    let top = bare_plain(&env, "top");
    file(&top.join(".git/config"), "[core]\n");
    let sub = dir(&top.join("sub"));
    let b = bwrap_value(&env).with_git_dirs(&sub);
    let args = args_of(&b, &sub, true, &env);
    assert!(
        contains_run(&args, &tmpfs_mask("<root>/work/top/.git/hooks")),
        "{args:?}"
    );
}

// ------------------------------------------------------------------------- real bwrap

fn git(dir: &Path, env: &TestEnv, args: &[&str]) -> Output {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", env.home())
        .output()
        .expect("run git")
}

fn assert_git(dir: &Path, env: &TestEnv, args: &[&str]) {
    let o = git(dir, env, args);
    assert!(
        o.status.success(),
        "git {args:?} in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&o.stderr)
    );
}

fn init_repo(path: &Path, env: &TestEnv) {
    dir(path);
    assert_git(path, env, &["init", "-q", "-b", "main"]);
    assert_git(path, env, &["config", "user.name", "Oxa Test"]);
    assert_git(path, env, &["config", "user.email", "test@example.invalid"]);
    assert_git(path, env, &["commit", "-q", "--allow-empty", "-m", "init"]);
}

fn add_worktree(main: &Path, env: &TestEnv) -> PathBuf {
    let wt = env.work().join("wt");
    assert_git(
        main,
        env,
        &[
            "worktree",
            "add",
            "-q",
            wt.to_str().expect("utf8"),
            "-b",
            "feature",
        ],
    );
    wt
}

fn required() -> bool {
    std::env::var("OXA_REQUIRE_BWRAP").is_ok_and(|v| v == "1")
}

fn detect(env: &TestEnv) -> Option<Bwrap> {
    let path = std::env::var("PATH").unwrap_or_default();
    match Bwrap::detect(&path, &env.home()) {
        Ok(b) => Some(b),
        Err(e) => {
            assert!(!required(), "OXA_REQUIRE_BWRAP=1 but bwrap is not usable: {e}");
            use std::io::Write as _;
            let _ = writeln!(std::io::stderr(), "sbx_hooks SKIPPED: bwrap is not usable: {e}");
            None
        }
    }
}

fn sandbox(env: &TestEnv, cwd: &Path) -> Option<Bwrap> {
    detect(env).map(|b| b.with_git_dirs(cwd))
}

/// Runs `command`; #318: no entry appears in (or vanishes from) the fake home or the cwd's
/// ancestors on the host while it runs, also not for a moment.
fn run(b: &Bwrap, command: &str, cwd: &Path, writable: bool) -> (Option<i32>, String) {
    let root = b.home.parent().expect("home below the test root");
    let watch = HostWatch::new(root, &b.home, cwd);
    let r = b.run(&spec(command, cwd, writable), &CancelToken::new(), &mut |_| {});
    let changed = watch.finish();
    assert!(changed.is_empty(), "`{command}` touched the host: {changed:?}");
    (r.exit, String::from_utf8_lossy(&r.output).into_owned())
}

fn is_empty_dir(p: &Path) -> bool {
    fs::read_dir(p).is_ok_and(|mut d| d.next().is_none())
}

#[test]
fn real_plain_repo_without_hooks_cannot_get_a_hook() {
    let env = TestEnv::new();
    let repo = env.work().join("plain");
    init_repo(&repo, &env);
    fs::remove_dir_all(repo.join(".git/hooks")).expect("rm hooks");
    let Some(sb) = sandbox(&env, &repo) else { return };
    let (exit, out) = run(
        &sb,
        "mkdir -p .git/hooks && printf '#!/bin/sh\\nexit 1\\n' > .git/hooks/pre-commit && chmod +x .git/hooks/pre-commit",
        &repo,
        true,
    );
    assert_ne!(exit, Some(0), "creating a hook must fail: {out}");
    for cmd in ["git status --short", "git commit -q --allow-empty -m inside"] {
        let (exit, out) = run(&sb, cmd, &repo, true);
        assert_eq!(exit, Some(0), "`{cmd}`: {out}");
    }
    // The host: no hook, an empty hooks directory (the mask's mount point), git works.
    assert!(!repo.join(".git/hooks/pre-commit").exists());
    assert!(
        is_empty_dir(&repo.join(".git/hooks")),
        "the mount point stays, empty"
    );
    assert_git(&repo, &env, &["commit", "-q", "--allow-empty", "-m", "outside"]);
    let log = git(&repo, &env, &["log", "--format=%s"]);
    assert_eq!(String::from_utf8_lossy(&log.stdout), "outside\ninside\ninit\n");
    // The next command binds the now existing (empty) directory read-only: still no hook.
    let (exit, _) = run(&sb, "echo x > .git/hooks/post-commit", &repo, true);
    assert_ne!(exit, Some(0));
    assert!(is_empty_dir(&repo.join(".git/hooks")));
}

#[test]
fn real_plain_repo_without_config_works_and_cannot_write_one() {
    let env = TestEnv::new();
    let repo = env.work().join("noconfig");
    init_repo(&repo, &env);
    fs::remove_file(repo.join(".git/config")).expect("rm config");
    let Some(sb) = sandbox(&env, &repo) else { return };
    let mut s = spec(
        "git status --short && git commit -q --allow-empty -m inside",
        &repo,
        true,
    );
    s.env = [
        ("GIT_AUTHOR_NAME", "Oxa Test"),
        ("GIT_AUTHOR_EMAIL", "test@example.invalid"),
        ("GIT_COMMITTER_NAME", "Oxa Test"),
        ("GIT_COMMITTER_EMAIL", "test@example.invalid"),
    ]
    .iter()
    .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
    .collect();
    let r = sb.run(&s, &CancelToken::new(), &mut |_| {});
    assert_eq!(r.exit, Some(0), "{}", String::from_utf8_lossy(&r.output));
    let (exit, out) = run(&sb, "git config core.hooksPath /tmp/evil", &repo, true);
    assert_ne!(exit, Some(0), "{out}");
    // The host: an empty config file (created before the run), git works outside.
    assert_eq!(fs::read(repo.join(".git/config")).expect("config"), b"");
    assert_git(&repo, &env, &["status", "--short"]);
    let log = git(&repo, &env, &["log", "-1", "--format=%s"]);
    assert_eq!(String::from_utf8_lossy(&log.stdout).trim(), "inside");
}

#[test]
fn real_worktree_without_common_hooks_cannot_get_a_hook() {
    let env = TestEnv::new();
    let main = env.work().join("main");
    init_repo(&main, &env);
    fs::remove_dir_all(main.join(".git/hooks")).expect("rm hooks");
    let wt = add_worktree(&main, &env);
    let Some(sb) = sandbox(&env, &wt) else { return };
    let hooks = main.join(".git/hooks");
    let (exit, out) = run(
        &sb,
        &format!(
            "mkdir -p {h} && echo 'exit 1' > {h}/pre-commit",
            h = hooks.display()
        ),
        &wt,
        true,
    );
    assert_ne!(exit, Some(0), "{out}");
    // The worktree's `.git` file cannot be pointed at a gitdir of the command's making.
    let (exit, out) = run(&sb, "echo 'gitdir: /tmp/x' > .git", &wt, true);
    assert_ne!(exit, Some(0), "{out}");
    for cmd in ["git status --short", "git commit -q --allow-empty -m inside"] {
        let (exit, out) = run(&sb, cmd, &wt, true);
        assert_eq!(exit, Some(0), "`{cmd}`: {out}");
    }
    assert!(!hooks.join("pre-commit").exists());
    assert!(is_empty_dir(&hooks));
    assert_git(&wt, &env, &["commit", "-q", "--allow-empty", "-m", "outside"]);
    let log = git(&main, &env, &["log", "--format=%s", "feature"]);
    assert_eq!(String::from_utf8_lossy(&log.stdout), "outside\ninside\ninit\n");
}

#[test]
fn real_worktree_config_cannot_be_created_when_the_extension_is_on() {
    let env = TestEnv::new();
    let main = env.work().join("main");
    init_repo(&main, &env);
    assert_git(&main, &env, &["config", "extensions.worktreeConfig", "true"]);
    let wt = add_worktree(&main, &env);
    let gitdir = main.join(".git/worktrees/wt");
    assert!(!gitdir.join("config.worktree").exists());
    let Some(sb) = sandbox(&env, &wt) else { return };
    let (exit, out) = run(&sb, "git config --worktree core.hooksPath /tmp/evil", &wt, true);
    assert_ne!(exit, Some(0), "{out}");
    let (exit, out) = run(&sb, "git status --short", &wt, true);
    assert_eq!(exit, Some(0), "{out}");
    assert_eq!(
        fs::read(gitdir.join("config.worktree")).expect("mount point"),
        b""
    );
    let o = git(&wt, &env, &["config", "--worktree", "--get", "core.hooksPath"]);
    assert!(!o.status.success(), "no hooksPath outside either");
}

#[test]
fn real_control_worktree_git_fails_without_the_git_binds() {
    // The sbx-git binds are necessary: the same worktree without them cannot run git.
    let env = TestEnv::new();
    let main = env.work().join("main");
    init_repo(&main, &env);
    let wt = add_worktree(&main, &env);
    if !env.root().starts_with("/tmp") {
        // The main repository is hidden only by the private /tmp (or the hidden home).
        use std::io::Write as _;
        let _ = writeln!(
            std::io::stderr(),
            "sbx_hooks control SKIPPED: test root not under /tmp"
        );
        return;
    }
    let Some(bare) = detect(&env) else { return };
    let (exit, out) = run(&bare, "git status --short", &wt, true);
    assert_ne!(exit, Some(0), "git must fail without the binds: {out}");
    let with = bare.with_git_dirs(&wt);
    let (exit, out) = run(&with, "git status --short", &wt, true);
    assert_eq!(exit, Some(0), "{out}");
}

// ---------------------------------------------------------------- rename of an ancestor

#[test]
fn cwd_dotgit_is_bound_onto_itself_before_its_protected_entries() {
    let env = TestEnv::new();
    let repo = bare_plain(&env, "plain");
    file(&repo.join(".git/config"), "[core]\n");
    let args = args_of(&bwrap_value(&env), &repo, true, &env);
    let selfbind = strs(&["--bind", "<root>/work/plain/.git", "<root>/work/plain/.git"]);
    let at = index_of_run(&args, &selfbind).expect("self-bind");
    let cfg = index_of_run(&args, &ro("<root>/work/plain/.git/config")).expect("config");
    let hooks = index_of_run(&args, &tmpfs_mask("<root>/work/plain/.git/hooks")).expect("hooks");
    assert!(at < cfg && at < hooks, "{args:?}");
    // Not in plan mode, not for a `.git` file or symlink.
    let args = args_of(&bwrap_value(&env), &repo, false, &env);
    assert!(!contains_run(&args, &selfbind), "{args:?}");
    let w = build_worktree(&env, "[core]\n");
    let args = args_of(&bwrap_value(&env).with_git_dirs(&w.wt), &w.wt, true, &env);
    assert!(
        !args
            .windows(2)
            .any(|x| x[0] == "--bind" && x[1].ends_with("wt1/.git")),
        "{args:?}"
    );
}

/// Rename `.git` away, copy it back, and plant a hook: the attack the self-bind blocks.
const RENAME_ATTACK: &str =
    "mv .git g && cp -a g .git && printf '#!/bin/sh\\nexit 1\\n' > .git/hooks/pre-commit && chmod +x .git/hooks/pre-commit";

#[test]
fn real_control_rename_of_dotgit_plants_a_hook_without_the_self_bind() {
    // Control: the same argv without the self-bind lets the command plant a hook on the host.
    let env = TestEnv::new();
    let repo = env.work().join("plain");
    init_repo(&repo, &env);
    // What the sandbox creates on the host before a writable command (#201's masks).
    file(&repo.join(".git/commondir"), ".\n");
    file(&repo.join(".git/gitdir"), "");
    let Some(sb) = detect(&env) else { return };
    let mut argv = bwrap_args_with(&spec(RENAME_ATTACK, &repo, true), &env.home(), &sb.layout);
    let dotgit = repo.join(".git").to_string_lossy().into_owned();
    let at = index_of_run(&argv, &strs(&["--bind", &dotgit, &dotgit])).expect("self-bind present");
    argv.drain(at..at + 3);
    let out = Command::new(&sb.bwrap).args(&argv).output().expect("run bwrap");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(
        repo.join(".git/hooks/pre-commit").is_file(),
        "the bypass works without the fix"
    );
}

#[test]
fn real_rename_of_dotgit_is_refused() {
    let env = TestEnv::new();
    let repo = env.work().join("plain");
    init_repo(&repo, &env);
    let Some(sb) = sandbox(&env, &repo) else { return };
    let (exit, out) = run(&sb, RENAME_ATTACK, &repo, true);
    assert_ne!(exit, Some(0), "{out}");
    assert!(!repo.join(".git/hooks/pre-commit").exists());
    assert!(!repo.join("g").exists());
    // Replacing it by deletion fails too: the mount point stays.
    let (exit, _) = run(&sb, "rm -rf .git; mkdir .git", &repo, true);
    assert_ne!(exit, Some(0));
    assert!(repo.join(".git/hooks").is_dir());
    assert!(!repo.join(".git/hooks/pre-commit").exists());
}

#[test]
fn real_git_operations_work_with_the_self_bound_dotgit() {
    let env = TestEnv::new();
    let repo = env.work().join("plain");
    init_repo(&repo, &env);
    file(&repo.join("a.txt"), "a\n");
    let Some(sb) = sandbox(&env, &repo) else { return };
    for cmd in [
        "git add a.txt && git commit -q -m add",
        "git gc -q",
        "git worktree add -q sub/wt -b side",
        "git -C sub/wt commit -q --allow-empty -m side",
        "git branch -q tmp && git branch -q -D tmp",
        "git status --short",
    ] {
        let (exit, out) = run(&sb, cmd, &repo, true);
        assert_eq!(exit, Some(0), "`{cmd}`: {out}");
    }
    let log = git(&repo, &env, &["log", "-1", "--format=%s", "side"]);
    assert_eq!(String::from_utf8_lossy(&log.stdout).trim(), "side");
}

#[test]
fn real_worktree_common_dir_cannot_be_renamed() {
    // The common dir outside the cwd is its own bind mount: renaming it fails; renaming its
    // parent only moves the sandbox's private view (the parent lives in the private /tmp).
    let env = TestEnv::new();
    let main = env.work().join("main");
    init_repo(&main, &env);
    let wt = add_worktree(&main, &env);
    let Some(sb) = sandbox(&env, &wt) else { return };
    let common = main.join(".git");
    let (exit, out) = run(&sb, &format!("mv {c} {c}.x", c = common.display()), &wt, true);
    assert_ne!(exit, Some(0), "{out}");
    let m = main.display();
    let (_, out) = run(
        &sb,
        &format!(
            "mv {m} {m}.x && mkdir -p {m}/.git/hooks && echo 'exit 1' > {m}/.git/hooks/pre-commit; echo done"
        ),
        &wt,
        true,
    );
    assert!(out.contains("done"), "{out}");
    assert!(!common.join("hooks/pre-commit").exists());
    assert!(!Path::new(&format!("{m}.x")).exists());
    assert_git(&wt, &env, &["status", "--short"]);
}

// ------------------------------------------------------------------- #201: commondir, gitdir

fn commit_env(s: &mut ExecSpec) {
    s.env = [
        ("GIT_AUTHOR_NAME", "Oxa Test"),
        ("GIT_AUTHOR_EMAIL", "test@example.invalid"),
        ("GIT_COMMITTER_NAME", "Oxa Test"),
        ("GIT_COMMITTER_EMAIL", "test@example.invalid"),
    ]
    .iter()
    .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
    .collect();
}

fn git_work_inside(sb: &Bwrap, cwd: &Path) {
    let mut s = spec(
        "echo x >> f.txt && git status --short && git add f.txt && git commit -q -m inside",
        cwd,
        true,
    );
    commit_env(&mut s);
    let r = sb.run(&s, &CancelToken::new(), &mut |_| {});
    assert_eq!(r.exit, Some(0), "{}", String::from_utf8_lossy(&r.output));
}

#[test]
fn plain_repo_masks_missing_commondir_and_gitdir() {
    let env = TestEnv::new();
    let repo = bare_plain(&env, "plain");
    file(&repo.join(".git/config"), "[core]\n");
    dir(&repo.join(".git/hooks"));
    let args = args_of(&bwrap_value(&env), &repo, true, &env);
    let m = masked_all(&args);
    for name in ["commondir", "gitdir"] {
        let p = format!("<root>/work/plain/.git/{name}");
        assert!(m.contains(&p), "{name} masked: {args:?}");
    }
}

#[test]
fn real_plain_repo_cannot_get_a_commondir() {
    let env = TestEnv::new();
    let repo = env.work().join("plain");
    init_repo(&repo, &env);
    let evil = dir(&env.work().join("evil"));
    let Some(sb) = sandbox(&env, &repo) else { return };
    for cmd in [
        format!("echo {} > .git/commondir", evil.display()),
        format!("rm -f .git/commondir; echo {} > .git/commondir", evil.display()),
        "mv .git/commondir .git/cd-old".to_string(),
        "echo x > .git/gitdir".to_string(),
    ] {
        let (exit, out) = run(&sb, &cmd, &repo, true);
        assert_ne!(exit, Some(0), "`{cmd}` must fail: {out}");
    }
    git_work_inside(&sb, &repo);
    // The host: git still uses its own directory.
    let o = git(&repo, &env, &["rev-parse", "--git-common-dir"]);
    assert_eq!(
        canon(Path::new(String::from_utf8_lossy(&o.stdout).trim())),
        canon(&repo.join(".git"))
    );
    assert_git(&repo, &env, &["commit", "-q", "--allow-empty", "-m", "outside"]);
    let log = git(&repo, &env, &["log", "--format=%s"]);
    assert_eq!(String::from_utf8_lossy(&log.stdout), "outside\ninside\ninit\n");
}

#[test]
fn real_worktree_cannot_change_its_commondir_or_gitdir() {
    let env = TestEnv::new();
    let main = env.work().join("main");
    init_repo(&main, &env);
    let wt = add_worktree(&main, &env);
    let gitdir = main.join(".git/worktrees/wt");
    let before_cd = fs::read(gitdir.join("commondir")).expect("commondir");
    let before_gd = fs::read(gitdir.join("gitdir")).expect("gitdir");
    let Some(sb) = sandbox(&env, &wt) else { return };
    for cmd in [
        format!("echo /tmp > {}/commondir", gitdir.display()),
        format!("echo /tmp > {}/gitdir", gitdir.display()),
        format!("echo /tmp > {}/.git/commondir", main.display()),
    ] {
        let (exit, out) = run(&sb, &cmd, &wt, true);
        assert_ne!(exit, Some(0), "`{cmd}` must fail: {out}");
    }
    git_work_inside(&sb, &wt);
    assert_eq!(fs::read(gitdir.join("commondir")).expect("commondir"), before_cd);
    assert_eq!(fs::read(gitdir.join("gitdir")).expect("gitdir"), before_gd);
    assert_git(&wt, &env, &["commit", "-q", "--allow-empty", "-m", "outside"]);
    let log = git(&main, &env, &["log", "--format=%s", "feature"]);
    assert_eq!(String::from_utf8_lossy(&log.stdout), "outside\ninside\ninit\n");
}
