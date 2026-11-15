//! sbx-git: the git data of a worktree or a shared clone that lies outside the cwd.
//!
//! * Resolver cases on hand-built directory trees (no git binary): what is bound, and the
//!   forged or malformed cases that must bind nothing.
//! * Argv goldens: the binds, their modes (plan mode read-only), the deny-list masks after them,
//!   and "resolved once" (a later rewrite of `.git` changes nothing).
//! * Real-bwrap runs: git works in a worktree and in a shared clone, a write into the alternate
//!   object directory fails, a plain repository is unchanged. Real-bwrap tests skip with a notice
//!   when bwrap is unusable (a failure under `OXA_REQUIRE_BWRAP=1`), as in A9.
//!
//! Isolation: every path lives under `oxa_fake::TestEnv`; git runs with no global config.

use oxa_fake::TestEnv;
use oxa_proto::{CancelToken, ExecSpec, Sandbox};
use oxa_sandbox::{bwrap_args_with, resolve_git_binds, Bwrap, GitBinds, Layout, MAX_ALTERNATES_DEPTH};
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

fn sorted(mut xs: Vec<PathBuf>) -> Vec<PathBuf> {
    xs.sort();
    xs
}

/// The root of the test with `<root>` in place of its temp path, for readable goldens.
fn shown(args: &[String], env: &TestEnv) -> Vec<String> {
    let root = env.root().display().to_string();
    args.iter().map(|a| a.replace(&root, "<root>")).collect()
}

/// `true` when `needle` occurs in `hay` as a contiguous run.
fn contains_run(hay: &[String], needle: &[String]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn index_of_run(hay: &[String], needle: &[String]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn strs(xs: &[&str]) -> Vec<String> {
    xs.iter().map(|x| (*x).to_string()).collect()
}

/// A [`Bwrap`] value with the default layout, for argv tests (no bwrap is run).
fn bwrap_value(env: &TestEnv) -> Bwrap {
    Bwrap {
        bwrap: PathBuf::from("/usr/bin/bwrap"),
        home: env.home(),
        layout: Layout::default(),
    }
}

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

/// A repository with one commit ("init") on `main`, identity in its local config.
fn init_repo(path: &Path, env: &TestEnv) {
    dir(path);
    assert_git(path, env, &["init", "-q", "-b", "main"]);
    assert_git(path, env, &["config", "user.name", "Oxa Test"]);
    assert_git(path, env, &["config", "user.email", "test@example.invalid"]);
    assert_git(path, env, &["commit", "-q", "--allow-empty", "-m", "init"]);
}

fn required() -> bool {
    std::env::var("OXA_REQUIRE_BWRAP").is_ok_and(|v| v == "1")
}

fn skip_notice(text: &str) {
    use std::io::Write as _;
    let _ = writeln!(std::io::stderr(), "git_worktree_binds SKIPPED: {text}");
}

/// The machine's bwrap with the sbx-git binds of `cwd`, or `None` with a notice.
fn sandbox(env: &TestEnv, cwd: &Path) -> Option<Bwrap> {
    let path = std::env::var("PATH").unwrap_or_default();
    match Bwrap::detect(&path, &env.home()) {
        Ok(b) => Some(b.with_git_dirs(cwd)),
        Err(e) => {
            assert!(!required(), "OXA_REQUIRE_BWRAP=1 but bwrap is not usable: {e}");
            skip_notice(&format!("bwrap is not usable: {e}"));
            None
        }
    }
}

fn run(b: &Bwrap, command: &str, cwd: &Path, writable: bool) -> (Option<i32>, String) {
    let r = b.run(&spec(command, cwd, writable), &CancelToken::new(), &mut |_| {});
    (r.exit, String::from_utf8_lossy(&r.output).into_owned())
}

// ------------------------------------------------------- hand-built trees (resolver)

/// A worktree of a hand-built main repository: `<root>/main/.git` (HEAD, objects, config),
/// `worktrees/wt1` (HEAD, commondir, back-pointer), and `<root>/work/wt1` with its `.git` file.
struct Worktree {
    main_git: PathBuf,
    gitdir: PathBuf,
    wt: PathBuf,
}

fn build_worktree(env: &TestEnv) -> Worktree {
    let root = env.root();
    let main_git = dir(&root.join("main/.git"));
    file(&main_git.join("HEAD"), "ref: refs/heads/main\n");
    dir(&main_git.join("objects/pack"));
    file(
        &main_git.join("config"),
        "[core]\n\trepositoryformatversion = 0\n",
    );
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

/// A clone with its own `.git` directory (a plain repository) whose alternates list `lines`.
fn build_clone(root: &Path, name: &str, lines: &[&str]) -> PathBuf {
    let clone = dir(&root.join("work").join(name));
    let git = dir(&clone.join(".git"));
    file(&git.join("HEAD"), "ref: refs/heads/main\n");
    dir(&git.join("objects/pack"));
    file(
        &git.join("objects/info/alternates"),
        &format!("{}\n", lines.join("\n")),
    );
    clone
}

/// An object directory with a pack directory, at `<root>/<name>/objects`.
fn objects(root: &Path, name: &str) -> PathBuf {
    dir(&root.join(name).join("objects/pack"));
    canon(&root.join(name).join("objects"))
}

#[test]
fn worktree_binds_its_gitdir_and_common_dir() {
    let env = TestEnv::new();
    let w = build_worktree(&env);
    file(&w.main_git.join("hooks/pre-commit"), "#!/bin/sh\n");
    let got = resolve_git_binds(&w.wt, &env.home(), Some(&env.agent_dir()));
    assert_eq!(
        sorted(got.rw.clone()),
        sorted(vec![canon(&w.main_git), canon(&w.gitdir)]),
        "the common dir and the worktree's gitdir, read-write"
    );
    assert!(got.ro.is_empty());
    // Hooks and config of the common dir stay read-only: git runs them outside the sandbox;
    // #201: so do `commondir` and `gitdir` of both git directories (missing ones are masked).
    assert_eq!(
        sorted(got.protect.clone()),
        sorted(vec![
            canon(&w.main_git).join("commondir"),
            canon(&w.main_git.join("config")),
            canon(&w.main_git).join("gitdir"),
            canon(&w.main_git).join("config.worktree"),
            canon(&w.main_git.join("hooks")),
            canon(&w.main_git).join("info"),
            canon(&w.gitdir.join("commondir")),
            canon(&w.gitdir).join("config.worktree"),
            canon(&w.gitdir.join("gitdir")),
            canon(&w.gitdir).join("hooks"),
        ])
    );
}

#[test]
fn subdirectory_of_a_worktree_binds_the_same() {
    let env = TestEnv::new();
    let w = build_worktree(&env);
    let sub = dir(&w.wt.join("src/deep"));
    let got = resolve_git_binds(&sub, &env.home(), Some(&env.agent_dir()));
    assert_eq!(sorted(got.rw), sorted(vec![canon(&w.main_git), canon(&w.gitdir)]));
}

#[test]
fn plain_repository_binds_nothing() {
    let env = TestEnv::new();
    let clone = dir(&env.work().join("plain"));
    let git = dir(&clone.join(".git"));
    file(&git.join("HEAD"), "ref: refs/heads/main\n");
    dir(&git.join("objects/pack"));
    let got = resolve_git_binds(&clone, &env.home(), Some(&env.agent_dir()));
    assert!(got.is_empty(), "{got:?}");
}

/// Review of #203: the `commondir` holding "." that a writable command leaves on the host is
/// not a worktree link; the repository stays discoverable from a subdirectory.
#[test]
fn plain_repository_with_synthetic_commondir_is_still_found() {
    let env = TestEnv::new();
    let top = dir(&env.work().join("top"));
    let git = dir(&top.join(".git"));
    file(&git.join("HEAD"), "ref: refs/heads/main\n");
    file(&git.join("commondir"), ".\n");
    dir(&git.join("objects/pack"));
    let sub = dir(&top.join("sub"));
    let got = resolve_git_binds(&sub, &env.home(), Some(&env.agent_dir()));
    assert_eq!(got.rw, vec![canon(&git)]);
}

#[test]
fn plain_repository_below_a_repo_root_binds_the_root_git_dir() {
    // cwd is a subdirectory of a plain repository whose `.git` lies above it.
    let env = TestEnv::new();
    let top = dir(&env.work().join("top"));
    let git = dir(&top.join(".git"));
    file(&git.join("HEAD"), "ref: refs/heads/main\n");
    dir(&git.join("objects/pack"));
    let sub = dir(&top.join("sub"));
    let got = resolve_git_binds(&sub, &env.home(), Some(&env.agent_dir()));
    assert_eq!(got.rw, vec![canon(&git)]);
    assert!(got.ro.is_empty());
}

#[test]
fn absolute_alternates_bind_their_object_dirs_read_only() {
    let env = TestEnv::new();
    let other = objects(env.root(), "other");
    let clone = build_clone(env.root(), "clone", &[other.to_str().expect("utf8")]);
    let got = resolve_git_binds(&clone, &env.home(), Some(&env.agent_dir()));
    assert!(
        got.rw.is_empty(),
        "the clone's own .git is inside the cwd: {got:?}"
    );
    assert_eq!(got.ro, vec![other]);
}

#[test]
fn relative_alternates_resolve_against_the_objects_dir() {
    let env = TestEnv::new();
    let other = objects(env.root(), "other");
    // From <root>/work/clone/.git/objects, four levels up is <root>.
    let clone = build_clone(env.root(), "clone", &["../../../../other/objects"]);
    let got = resolve_git_binds(&clone, &env.home(), Some(&env.agent_dir()));
    assert_eq!(got.ro, vec![other]);
}

#[test]
fn nested_alternates_are_followed_and_comments_skipped() {
    let env = TestEnv::new();
    let root = env.root();
    let a1 = objects(root, "a1");
    let a2 = objects(root, "a2");
    file(
        &a1.join("info/alternates"),
        &format!("# shared\n\n{}\n", a2.display()),
    );
    let clone = build_clone(
        root,
        "clone",
        &["/nonexistent/objects", a1.to_str().expect("utf8")],
    );
    let got = resolve_git_binds(&clone, &env.home(), Some(&env.agent_dir()));
    assert_eq!(got.ro, sorted(vec![a1, a2]), "the missing path is dropped");
}

#[test]
fn alternate_cycles_terminate_and_bind_each_dir_once() {
    let env = TestEnv::new();
    let root = env.root();
    let a = objects(root, "a");
    let b = objects(root, "b");
    file(&a.join("info/alternates"), &format!("{}\n", b.display()));
    file(&b.join("info/alternates"), &format!("{}\n", a.display()));
    let clone = build_clone(root, "clone", &[a.to_str().expect("utf8")]);
    let got = resolve_git_binds(&clone, &env.home(), Some(&env.agent_dir()));
    assert_eq!(got.ro, sorted(vec![a, b]));
}

#[test]
fn alternates_are_followed_at_most_five_levels() {
    let env = TestEnv::new();
    let root = env.root();
    let chain: Vec<PathBuf> = (1..=MAX_ALTERNATES_DEPTH + 2)
        .map(|i| objects(&root.join("chain"), &format!("o{i}")))
        .collect();
    for pair in chain.windows(2) {
        file(
            &pair[0].join("info/alternates"),
            &format!("{}\n", pair[1].display()),
        );
    }
    let clone = build_clone(root, "clone", &[chain[0].to_str().expect("utf8")]);
    let got = resolve_git_binds(&clone, &env.home(), Some(&env.agent_dir()));
    assert_eq!(got.ro, chain[..MAX_ALTERNATES_DEPTH].to_vec());
}

#[test]
fn alternate_that_is_not_an_object_dir_is_dropped() {
    let env = TestEnv::new();
    let root = env.root();
    let not_objects = dir(&root.join("notobj"));
    file(&not_objects.join("readme.txt"), "not git data\n");
    let clone = build_clone(root, "clone", &[not_objects.to_str().expect("utf8")]);
    let got = resolve_git_binds(&clone, &env.home(), Some(&env.agent_dir()));
    assert!(got.ro.is_empty(), "{got:?}");
}

#[test]
fn forged_dotgit_file_without_back_pointer_binds_nothing() {
    // `.git` points at a directory that looks like git data but does not name us back.
    let env = TestEnv::new();
    let victim = dir(&env.root().join("victim"));
    file(&victim.join("HEAD"), "ref: refs/heads/main\n");
    dir(&victim.join("objects/pack"));
    let cwd = dir(&env.work().join("proj"));
    file(&cwd.join(".git"), &format!("gitdir: {}\n", victim.display()));
    let got = resolve_git_binds(&cwd, &env.home(), Some(&env.agent_dir()));
    assert!(got.is_empty(), "{got:?}");
}

#[test]
fn forged_dotgit_into_a_sensitive_dir_binds_nothing() {
    // A valid-looking worktree whose gitdir sits in `~/.gnupg`: the deny-list refuses it.
    let env = TestEnv::new();
    let home = env.home();
    let fake = dir(&home.join(".gnupg/fake"));
    file(&fake.join("HEAD"), "ref: refs/heads/main\n");
    dir(&fake.join("objects/pack"));
    file(&fake.join("commondir"), ".\n");
    let cwd = dir(&env.work().join("proj"));
    file(&fake.join("gitdir"), &format!("{}\n", cwd.join(".git").display()));
    file(&cwd.join(".git"), &format!("gitdir: {}\n", fake.display()));
    let got = resolve_git_binds(&cwd, &home, Some(&env.agent_dir()));
    assert!(got.is_empty(), "{got:?}");
}

#[test]
fn common_dir_inside_the_home_or_a_sensitive_dir_binds_nothing() {
    // The common dir is `~/.ssh` (deny-listed) and the worktree's gitdir lives under it.
    let env = TestEnv::new();
    let home = env.home();
    let ssh_git = dir(&home.join(".ssh/repo.git"));
    file(&ssh_git.join("HEAD"), "ref: refs/heads/main\n");
    dir(&ssh_git.join("objects/pack"));
    let gitdir = dir(&ssh_git.join("worktrees/wt1"));
    file(&gitdir.join("HEAD"), "ref: refs/heads/main\n");
    file(&gitdir.join("commondir"), "../..\n");
    let cwd = dir(&env.work().join("proj"));
    file(
        &gitdir.join("gitdir"),
        &format!("{}\n", cwd.join(".git").display()),
    );
    file(&cwd.join(".git"), &format!("gitdir: {}\n", gitdir.display()));
    let got = resolve_git_binds(&cwd, &home, Some(&env.agent_dir()));
    assert!(got.is_empty(), "{got:?}");
}

#[test]
fn alternate_into_a_sensitive_dir_is_dropped() {
    let env = TestEnv::new();
    let home = env.home();
    let secret_objects = objects(&home.join(".ssh"), "store");
    let clone = build_clone(env.root(), "clone", &[secret_objects.to_str().expect("utf8")]);
    let got = resolve_git_binds(&clone, &home, Some(&env.agent_dir()));
    assert!(got.ro.is_empty(), "{got:?}");
}

#[test]
fn back_pointer_mismatch_binds_nothing() {
    let env = TestEnv::new();
    let w = build_worktree(&env);
    file(
        &w.gitdir.join("gitdir"),
        &format!("{}\n", env.root().join("elsewhere/.git").display()),
    );
    let got = resolve_git_binds(&w.wt, &env.home(), Some(&env.agent_dir()));
    assert!(got.is_empty(), "{got:?}");
}

#[test]
fn missing_back_pointer_binds_nothing() {
    let env = TestEnv::new();
    let w = build_worktree(&env);
    fs::remove_file(w.gitdir.join("gitdir")).expect("remove back-pointer");
    let got = resolve_git_binds(&w.wt, &env.home(), Some(&env.agent_dir()));
    assert!(got.is_empty(), "{got:?}");
}

#[test]
fn gitdir_not_registered_in_the_common_dir_binds_nothing() {
    // The gitdir names the common dir, but the common dir has no `worktrees/wt2` for it.
    let env = TestEnv::new();
    let w = build_worktree(&env);
    let elsewhere = dir(&env.root().join("elsewhere/wt2"));
    file(&elsewhere.join("HEAD"), "ref: refs/heads/x\n");
    file(
        &elsewhere.join("commondir"),
        &format!("{}\n", w.main_git.display()),
    );
    let cwd = dir(&env.work().join("proj2"));
    file(
        &elsewhere.join("gitdir"),
        &format!("{}\n", cwd.join(".git").display()),
    );
    file(&cwd.join(".git"), &format!("gitdir: {}\n", elsewhere.display()));
    let got = resolve_git_binds(&cwd, &env.home(), Some(&env.agent_dir()));
    assert!(got.is_empty(), "{got:?}");
}

#[test]
fn dotgit_symlink_binds_nothing() {
    let env = TestEnv::new();
    let w = build_worktree(&env);
    let cwd = dir(&env.work().join("linked"));
    std::os::unix::fs::symlink(&w.main_git, cwd.join(".git")).expect("symlink");
    let got = resolve_git_binds(&cwd, &env.home(), Some(&env.agent_dir()));
    assert!(got.is_empty(), "{got:?}");
}

#[test]
fn cwd_outside_any_repository_binds_nothing() {
    let env = TestEnv::new();
    let cwd = dir(&env.work().join("no-repo/deeper"));
    let got = resolve_git_binds(&cwd, &env.home(), Some(&env.agent_dir()));
    assert!(got.is_empty(), "{got:?}");
}

#[test]
fn home_as_a_repository_is_never_bound() {
    // `$HOME` is itself a repository; a project below it must not bind it.
    let env = TestEnv::new();
    let home = env.home();
    let git = dir(&home.join(".git"));
    file(&git.join("HEAD"), "ref: refs/heads/main\n");
    dir(&git.join("objects/pack"));
    let proj = dir(&home.join("proj"));
    let got = resolve_git_binds(&proj, &home, Some(&env.agent_dir()));
    assert!(got.is_empty(), "{got:?}");
    let got = resolve_git_binds(&home, &home, Some(&env.agent_dir()));
    assert!(got.is_empty(), "{got:?}");
}

#[test]
fn cwd_inside_the_bound_tree_needs_no_extra_bind() {
    // A worktree whose gitdir and common dir are inside the cwd (the cwd holds the repo).
    let env = TestEnv::new();
    let cwd = dir(&env.work().join("both"));
    let git = dir(&cwd.join(".git"));
    file(&git.join("HEAD"), "ref: refs/heads/main\n");
    dir(&git.join("objects/pack"));
    let got = resolve_git_binds(&cwd, &env.home(), Some(&env.agent_dir()));
    assert!(got.is_empty(), "{got:?}");
}

// ------------------------------------------------------------------- argv goldens

#[test]
fn golden_worktree_argv_binds_read_write_then_protected_read_only() {
    let env = TestEnv::new();
    let w = build_worktree(&env);
    file(&w.main_git.join("config"), "[core]\n");
    let b = bwrap_value(&env).with_git_dirs(&w.wt);
    let args = shown(&sandbox_args(&b, &w.wt, true), &env);
    let mc = "<root>/main/.git";
    let gd = "<root>/main/.git/worktrees/wt1";
    let want = strs(&[
        "--bind",
        mc,
        mc,
        "--bind",
        gd,
        gd,
        "--ro-bind",
        "/dev/null",
        &format!("{mc}/commondir"),
        "--ro-bind",
        &format!("{mc}/config"),
        &format!("{mc}/config"),
    ]);
    assert!(contains_run(&args, &want), "{args:?}");
    // After the cwd bind, before the chdir (which ends the binds).
    let cwd_at =
        index_of_run(&args, &strs(&["--bind", "<root>/work/wt1", "<root>/work/wt1"])).expect("cwd bind");
    let git_at = index_of_run(&args, &want).expect("git binds");
    let chdir_at = index_of_run(&args, &strs(&["--chdir", "<root>/work/wt1"])).expect("chdir");
    assert!(cwd_at < git_at && git_at < chdir_at, "{args:?}");
}

#[test]
fn golden_plain_repository_argv_is_unchanged() {
    let env = TestEnv::new();
    let clone = dir(&env.work().join("plain"));
    dir(&clone.join(".git/objects/pack"));
    file(&clone.join(".git/HEAD"), "ref: refs/heads/main\n");
    let b = bwrap_value(&env).with_git_dirs(&clone);
    let s = spec("git status", &clone, true);
    assert_eq!(
        bwrap_args_with(&s, &env.home(), &b.layout),
        bwrap_args_with(&s, &env.home(), &Layout::default()),
        "a plain repository adds no bind"
    );
}

#[test]
fn golden_plan_mode_is_read_only_for_git_binds() {
    let env = TestEnv::new();
    let w = build_worktree(&env);
    let b = bwrap_value(&env).with_git_dirs(&w.wt);
    let args = shown(&sandbox_args(&b, &w.wt, false), &env);
    let mc = "<root>/main/.git";
    let gd = "<root>/main/.git/worktrees/wt1";
    assert!(contains_run(&args, &strs(&["--ro-bind", mc, mc])), "{args:?}");
    assert!(contains_run(&args, &strs(&["--ro-bind", gd, gd])), "{args:?}");
    assert!(
        !args
            .windows(3)
            .any(|w| w[0] == "--bind" && (w[1] == mc || w[1] == gd)),
        "{args:?}"
    );
}

#[test]
fn golden_deny_list_masks_come_after_the_git_binds() {
    // The agent directory inside a read-write common dir: the mask must win over the bind.
    let env = TestEnv::new();
    let common = dir(&env.root().join("repo/.git"));
    let agent = dir(&common.join("oxa-agent"));
    let layout = Layout {
        agent_dir: Some(agent.clone()),
        git: GitBinds {
            rw: vec![common.clone()],
            ro: Vec::new(),
            protect: Vec::new(),
            ..GitBinds::default()
        },
        ..Layout::default()
    };
    let cwd = env.work();
    let args = shown(
        &bwrap_args_with(&spec("true", &cwd, true), &env.home(), &layout),
        &env,
    );
    let bind = strs(&["--bind", "<root>/repo/.git", "<root>/repo/.git"]);
    let mask = strs(&[
        "--tmpfs",
        "<root>/repo/.git/oxa-agent",
        "--remount-ro",
        "<root>/repo/.git/oxa-agent",
    ]);
    let at_bind = index_of_run(&args, &bind).expect("git bind");
    let at_mask = index_of_run(&args, &mask).expect("deny mask");
    assert!(at_bind < at_mask, "the mask must follow the bind: {args:?}");
    let chdir_at = index_of_run(&args, &strs(&["--chdir"])).expect("chdir");
    assert!(at_mask < chdir_at, "{args:?}");
}

#[test]
fn golden_alternates_are_read_only() {
    let env = TestEnv::new();
    let other = objects(env.root(), "other");
    let clone = build_clone(env.root(), "clone", &[other.to_str().expect("utf8")]);
    let b = bwrap_value(&env).with_git_dirs(&clone);
    let args = shown(&sandbox_args(&b, &clone, true), &env);
    let o = "<root>/other/objects";
    assert!(contains_run(&args, &strs(&["--ro-bind", o, o])), "{args:?}");
    assert!(
        !args.windows(3).any(|w| w[0] == "--bind" && w[1] == o),
        "{args:?}"
    );
}

/// The argv of `b` for a command in `cwd` (writable or plan mode).
fn sandbox_args(b: &Bwrap, cwd: &Path, writable: bool) -> Vec<String> {
    b.bwrap_args(&spec("git status", cwd, writable))
}

#[test]
fn resolved_once_a_later_rewrite_of_dotgit_changes_nothing() {
    let env = TestEnv::new();
    let w = build_worktree(&env);
    let b = bwrap_value(&env).with_git_dirs(&w.wt);
    let before = sandbox_args(&b, &w.wt, true);
    // The model rewrites `.git` to point at a sensitive directory, then runs another command.
    let ssh = dir(&env.home().join(".ssh/evil"));
    file(&ssh.join("HEAD"), "ref: refs/heads/main\n");
    dir(&ssh.join("objects/pack"));
    file(&w.wt.join(".git"), &format!("gitdir: {}\n", ssh.display()));
    assert_eq!(
        sandbox_args(&b, &w.wt, true),
        before,
        "the binds are fixed at start"
    );
    // A fresh resolution (a new session) sees the rewritten file and binds nothing.
    assert!(resolve_git_binds(&w.wt, &env.home(), Some(&env.agent_dir())).is_empty());
}

// ------------------------------------------------------------------- real bwrap

fn shared_clone(env: &TestEnv) -> (PathBuf, PathBuf) {
    let origin = env.work().join("origin");
    init_repo(&origin, env);
    let clone = env.work().join("clone");
    assert_git(
        &env.work(),
        env,
        &[
            "clone",
            "-q",
            "--shared",
            origin.to_str().expect("utf8"),
            clone.to_str().expect("utf8"),
        ],
    );
    assert_git(&clone, env, &["config", "user.name", "Oxa Test"]);
    assert_git(&clone, env, &["config", "user.email", "test@example.invalid"]);
    (origin, clone)
}

#[test]
fn worktree_git_status_log_and_commit_work_in_the_sandbox() {
    let env = TestEnv::new();
    let main = env.work().join("main");
    init_repo(&main, &env);
    let wt = env.work().join("wt");
    assert_git(
        &main,
        &env,
        &[
            "worktree",
            "add",
            "-q",
            wt.to_str().expect("utf8"),
            "-b",
            "feature",
        ],
    );
    let Some(sb) = sandbox(&env, &wt) else { return };
    for cmd in [
        "git status --short",
        "git log -1 --format=%s",
        "git commit -q --allow-empty -m x",
    ] {
        let (exit, out) = run(&sb, cmd, &wt, true);
        assert_eq!(exit, Some(0), "`{cmd}`: {out}");
    }
    // The commit is in the main repository's history, on the worktree's branch.
    let log = git(&main, &env, &["log", "-1", "--format=%s", "feature"]);
    assert_eq!(String::from_utf8_lossy(&log.stdout).trim(), "x");
}

#[test]
fn worktree_in_plan_mode_cannot_commit() {
    let env = TestEnv::new();
    let main = env.work().join("main");
    init_repo(&main, &env);
    let wt = env.work().join("wt");
    assert_git(
        &main,
        &env,
        &[
            "worktree",
            "add",
            "-q",
            wt.to_str().expect("utf8"),
            "-b",
            "feature",
        ],
    );
    let Some(sb) = sandbox(&env, &wt) else { return };
    let (exit, _) = run(&sb, "git commit -q --allow-empty -m planned", &wt, false);
    assert_ne!(exit, Some(0), "plan mode must not commit");
    let log = git(&main, &env, &["log", "-1", "--format=%s", "feature"]);
    assert_eq!(String::from_utf8_lossy(&log.stdout).trim(), "init");
}

#[test]
fn worktree_sandbox_still_hides_the_main_checkout_and_the_deny_list() {
    let env = TestEnv::new();
    let main = env.work().join("main");
    init_repo(&main, &env);
    file(&main.join("main-only.txt"), "MAIN-CHECKOUT-SECRET\n");
    file(&env.home().join(".ssh/id_test"), "PRIVATE-KEY-MATERIAL\n");
    let wt = env.work().join("wt");
    assert_git(
        &main,
        &env,
        &[
            "worktree",
            "add",
            "-q",
            wt.to_str().expect("utf8"),
            "-b",
            "feature",
        ],
    );
    let Some(sb) = sandbox(&env, &wt) else { return };
    // The main checkout is not bound (the private /tmp hides it too; the argv goldens prove the
    // binds are only the git dirs).
    let (exit, out) = run(
        &sb,
        &format!("cat {}", main.join("main-only.txt").display()),
        &wt,
        true,
    );
    assert_ne!(exit, Some(0), "{out}");
    assert!(!out.contains("MAIN-CHECKOUT-SECRET"));
    // The deny-list is still masked.
    let (exit, out) = run(&sb, "cat ~/.ssh/id_test", &wt, true);
    assert_ne!(exit, Some(0), "{out}");
    assert!(!out.contains("PRIVATE-KEY-MATERIAL"));
    // Outside every bind the root is read-only.
    let target = format!("/var/oxa-sbx-git-probe-{}", std::process::id());
    let (exit, _) = run(&sb, &format!("echo leaked > {target}"), &wt, true);
    assert_ne!(exit, Some(0));
    assert!(!Path::new(&target).exists());
}

#[test]
fn shared_clone_log_and_status_work_and_the_alternate_is_read_only() {
    let env = TestEnv::new();
    let (origin, clone) = shared_clone(&env);
    assert!(
        clone.join(".git/objects/info/alternates").is_file(),
        "--shared writes alternates"
    );
    let Some(sb) = sandbox(&env, &clone) else { return };
    let (exit, out) = run(&sb, "git log -1 --format=%s", &clone, true);
    assert_eq!(exit, Some(0), "{out}");
    assert_eq!(out.trim(), "init");
    let (exit, out) = run(&sb, "git status --short", &clone, true);
    assert_eq!(exit, Some(0), "{out}");
    // The clone's own history is writable.
    let (exit, out) = run(&sb, "git commit -q --allow-empty -m local", &clone, true);
    assert_eq!(exit, Some(0), "{out}");
    // The alternate object directory is read-only.
    let probe = origin.join(".git/objects/oxa-sbx-git-probe");
    let (exit, out) = run(&sb, &format!("echo x > {}", probe.display()), &clone, true);
    assert_ne!(exit, Some(0), "{out}");
    assert!(!probe.exists());
}

#[test]
fn plain_repository_works_as_before_in_the_sandbox() {
    let env = TestEnv::new();
    let plain = env.work().join("plain");
    init_repo(&plain, &env);
    let Some(sb) = sandbox(&env, &plain) else { return };
    let (exit, out) = run(&sb, "git status --short && git log -1 --format=%s", &plain, true);
    assert_eq!(exit, Some(0), "{out}");
    assert_eq!(out.trim(), "init");
}

// ---------------------------------------------- issue #103: commit in a worktree, hooks stay

fn real_worktree(env: &TestEnv) -> (PathBuf, PathBuf) {
    let main = env.work().join("main");
    init_repo(&main, env);
    let wt = env.work().join("wt");
    assert_git(
        &main,
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
    (main, wt)
}

fn gitdir_of(main: &Path, name: &str) -> PathBuf {
    canon(&main.join(".git/worktrees").join(name))
}

#[test]
fn issue_103_worktree_commit_works_and_hooks_and_config_stay_read_only() {
    let env = TestEnv::new();
    let (main, wt) = real_worktree(&env);
    let common = canon(&main.join(".git"));
    let gitdir = gitdir_of(&main, "wt");
    let Some(sb) = sandbox(&env, &wt) else { return };
    // A real commit with content: index, objects, refs and logs are all written.
    let (exit, out) = run(
        &sb,
        "echo hi > f.txt && git add f.txt && git commit -q -m real && git log -1 --format=%s",
        &wt,
        true,
    );
    assert_eq!(exit, Some(0), "{out}");
    let log = git(&main, &env, &["log", "-1", "--format=%s", "feature"]);
    assert_eq!(String::from_utf8_lossy(&log.stdout).trim(), "real");
    // Nothing a tool call writes there may run or configure git outside the sandbox.
    let cfg_before = fs::read(common.join("config")).expect("config");
    for target in [
        common.join("hooks/pre-commit"),
        common.join("hooks/x"),
        common.join("config"),
        common.join("config.worktree"),
        common.join("info/exclude"),
        gitdir.join("hooks/pre-commit"),
        gitdir.join("hooks/x"),
        gitdir.join("config.worktree"),
    ] {
        let (exit, out) = run(&sb, &format!("echo '[core]' >> {}", target.display()), &wt, true);
        assert_ne!(exit, Some(0), "{} was writable: {out}", target.display());
        let (exit, out) = run(&sb, &format!("rm -rf {}", target.display()), &wt, true);
        let _ = (exit, out);
    }
    assert_eq!(fs::read(common.join("config")).expect("config"), cfg_before);
    assert!(!common.join("hooks/x").exists());
    assert!(!gitdir.join("hooks/x").exists());
    assert!(!gitdir.join("hooks/pre-commit").exists());
    assert!(
        !common.join("config.worktree").exists()
            || fs::read(common.join("config.worktree")).expect("r").is_empty()
    );
}

#[test]
fn issue_103_worktree_binds_only_what_a_commit_needs_writable() {
    let env = TestEnv::new();
    let (main, wt) = real_worktree(&env);
    let common = canon(&main.join(".git"));
    let gitdir = gitdir_of(&main, "wt");
    let b = resolve_git_binds(&wt, &env.home(), Some(&env.agent_dir()));
    for rel in ["hooks", "config", "info"] {
        assert!(b.protect.contains(&common.join(rel)), "{rel}: {:?}", b.protect);
    }
    assert!(b.protect.contains(&gitdir.join("hooks")), "{:?}", b.protect);
}

#[test]
fn issue_103_plain_clone_keeps_hooks_and_config_read_only() {
    let env = TestEnv::new();
    let plain = env.work().join("plain");
    init_repo(&plain, &env);
    let Some(sb) = sandbox(&env, &plain) else { return };
    let (exit, out) = run(&sb, "git commit -q --allow-empty -m again", &plain, true);
    assert_eq!(exit, Some(0), "{out}");
    for target in [".git/hooks/x", ".git/config"] {
        let (exit, out) = run(&sb, &format!("echo x >> {target}"), &plain, true);
        assert_ne!(exit, Some(0), "{target}: {out}");
    }
    assert!(!plain.join(".git/hooks/x").exists());
}

/// #322: a git directory that vanished after the session started (worktree pruned or removed)
/// must not make bwrap fail for every later command.
#[test]
fn vanished_git_dir_is_not_bound() {
    let env = TestEnv::new();
    let w = build_worktree(&env);
    let b = bwrap_value(&env).with_git_dirs(&w.wt);
    let g = w.gitdir.to_string_lossy().into_owned();
    assert!(sandbox_args(&b, &w.wt, true).contains(&g), "bound while present");
    std::fs::remove_dir_all(&w.gitdir).expect("remove gitdir");
    let args = sandbox_args(&b, &w.wt, true);
    assert!(!args.contains(&g), "a missing source must not be bound: {args:?}");
}

/// #322 review: with the gitdir gone, nothing under the surviving common dir may be writable
/// (the vanished gitdir could be recreated with its own `commondir`/hooks), and a dangling
/// symlink in its place is not bound either.
#[test]
fn vanished_git_dir_leaves_no_writable_bind_to_recreate_it() {
    let env = TestEnv::new();
    let w = build_worktree(&env);
    let b = bwrap_value(&env).with_git_dirs(&w.wt);
    std::fs::remove_dir_all(&w.gitdir).expect("remove gitdir");
    let args = sandbox_args(&b, &w.wt, true);
    let common = w.main_git.to_string_lossy().into_owned();
    for (i, x) in args.iter().enumerate() {
        if x == "--bind" || x == "--bind-try" {
            let src = &args[i + 1];
            assert!(
                src != &common && !common.starts_with(&format!("{src}/")) || src == "/",
                "writable bind over the common dir: {src}"
            );
        }
    }
    assert!(
        args.windows(3)
            .any(|t| t[0] == "--ro-bind" && t[1] == common && t[2] == common),
        "common dir read-only: {args:?}"
    );
    // A dangling symlink in place of the gitdir is not bound.
    std::os::unix::fs::symlink(env.root().join("nowhere"), &w.gitdir).expect("symlink");
    let g = w.gitdir.to_string_lossy().into_owned();
    assert!(!sandbox_args(&b, &w.wt, true).contains(&g));
}
