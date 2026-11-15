//! sbx-gitsym: a protected git entry that is not plain (a symlink, a symlink chain, a symlinked
//! `hooks` directory, a hard link, an `include`) must not let a writable sandbox change what git
//! outside reads or runs. Such a git directory is bound read-only (with a notice); a normal
//! worktree and a normal plain repository keep working as before.
//!
//! * Real bwrap (the negatives write through the alias and must fail with the host bytes
//!   unchanged; the positives commit). Skip with a notice when bwrap is unusable, a failure
//!   under `OXA_REQUIRE_BWRAP=1`.
//! * Argv goldens: the modes and the order of the binds, the per-command re-check.
//!
//! Isolation: every path lives under `oxa_fake::TestEnv`; git runs with no global config.

use oxa_fake::TestEnv;
use oxa_proto::{CancelToken, ExecSpec, Sandbox};
use oxa_sandbox::{resolve_git_binds, Bwrap, Layout};
use std::fs;
use std::os::unix::fs::symlink;
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
    fs::create_dir_all(path).expect("mkdir");
    assert_git(path, env, &["init", "-q", "-b", "main"]);
    assert_git(path, env, &["config", "user.name", "Oxa Test"]);
    assert_git(path, env, &["config", "user.email", "test@example.invalid"]);
    assert_git(path, env, &["commit", "-q", "--allow-empty", "-m", "init"]);
}

/// A main repository with one commit and a linked worktree on `feature`: (main, worktree,
/// canonical common dir).
fn linked_worktree(env: &TestEnv) -> (PathBuf, PathBuf, PathBuf) {
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
    let common = fs::canonicalize(main.join(".git")).expect("canon");
    (main, wt, common)
}

/// Move `<git>/config` to `<git>/config-data` and leave a relative symlink in its place (the
/// Codex import #3 fixture).
fn symlink_config(git: &Path) {
    fs::rename(git.join("config"), git.join("config-data")).expect("rename");
    symlink("config-data", git.join("config")).expect("symlink");
}

fn required() -> bool {
    std::env::var("OXA_REQUIRE_BWRAP").is_ok_and(|v| v == "1")
}

fn sandbox(env: &TestEnv, cwd: &Path) -> Option<Bwrap> {
    let path = std::env::var("PATH").unwrap_or_default();
    match Bwrap::detect(&path, &env.home()) {
        Ok(b) => Some(b.with_git_dirs(cwd)),
        Err(e) => {
            assert!(!required(), "OXA_REQUIRE_BWRAP=1 but bwrap is not usable: {e}");
            use std::io::Write as _;
            let _ = writeln!(std::io::stderr(), "sbx_gitsym SKIPPED: bwrap is not usable: {e}");
            None
        }
    }
}

fn run(b: &Bwrap, command: &str, cwd: &Path) -> (Option<i32>, String) {
    let r = b.run(&spec(command, cwd, true), &CancelToken::new(), &mut |_| {});
    (r.exit, String::from_utf8_lossy(&r.output).into_owned())
}

/// Run each write in a writable sandbox: every one must fail read-only, and `watched` must keep
/// its bytes; git outside still sees no `core.hooksPath`.
fn assert_writes_refused(env: &TestEnv, sb: &Bwrap, cwd: &Path, writes: &[String], watched: &[PathBuf]) {
    let before: Vec<Vec<u8>> = watched.iter().map(|p| fs::read(p).unwrap_or_default()).collect();
    for w in writes {
        let (exit, out) = run(sb, w, cwd);
        assert_ne!(exit, Some(0), "`{w}` must fail: {out}");
        assert!(out.contains("Read-only file system"), "`{w}`: {out}");
    }
    for (p, b) in watched.iter().zip(&before) {
        assert_eq!(
            &fs::read(p).unwrap_or_default(),
            b,
            "{} changed on the host",
            p.display()
        );
    }
    let o = git(cwd, env, &["config", "--get", "core.hooksPath"]);
    assert!(
        String::from_utf8_lossy(&o.stdout).trim().is_empty(),
        "hooksPath leaked"
    );
}

/// The sandbox refuses to run `command` at all (nothing ran: no exit code, a "sandbox refused"
/// message), and `watched` keeps its bytes; git outside still sees no `core.hooksPath`.
fn assert_command_refused(env: &TestEnv, sb: &Bwrap, cwd: &Path, command: &str, watched: &[PathBuf]) {
    let before: Vec<Vec<u8>> = watched.iter().map(|p| fs::read(p).unwrap_or_default()).collect();
    let (exit, out) = run(sb, command, cwd);
    assert_eq!(exit, None, "`{command}` must not run: {out}");
    assert!(out.contains("sandbox refused"), "`{command}`: {out}");
    for (p, b) in watched.iter().zip(&before) {
        assert_eq!(
            &fs::read(p).unwrap_or_default(),
            b,
            "{} changed on the host",
            p.display()
        );
    }
    let o = git(cwd, env, &["config", "--get", "core.hooksPath"]);
    assert!(
        String::from_utf8_lossy(&o.stdout).trim().is_empty(),
        "hooksPath leaked"
    );
    assert!(
        sb.layout.git.notes.iter().any(|n| n.contains("sandbox refused")),
        "the session notice says so: {:?}",
        sb.layout.git.notes
    );
}

/// The worktree's common `config` moved to `target` (anywhere), `config` an absolute symlink
/// to it.
fn config_to(common: &Path, target: &Path) {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).expect("mkdir");
    }
    fs::rename(common.join("config"), target).expect("move config");
    symlink(target, common.join("config")).expect("symlink");
}

fn append_hooks_path(p: &Path) -> String {
    format!(
        "printf '[core]\\n\\thooksPath = /tmp/oxa-evil-hooks\\n' >> '{}'",
        p.display().to_string().replace('\'', "'\\''")
    )
}

fn args_of(b: &Bwrap, cwd: &Path, env: &TestEnv) -> Vec<String> {
    let root = env.root().display().to_string();
    b.bwrap_args(&spec("true", cwd, true))
        .iter()
        .map(|a| a.replace(&root, "<root>"))
        .collect()
}

fn index_of_run(hay: &[String], needle: &[&str]) -> Option<usize> {
    let needle: Vec<String> = needle.iter().map(|x| (*x).to_string()).collect();
    hay.windows(needle.len()).position(|w| w == needle.as_slice())
}

fn bwrap_value(env: &TestEnv) -> Bwrap {
    Bwrap {
        bwrap: PathBuf::from("/usr/bin/bwrap"),
        home: env.home(),
        layout: Layout::default(),
    }
}

// ------------------------------------------------------------- real bwrap: negatives

#[test]
fn worktree_symlinked_config_target_inside_common_is_read_only() {
    let env = TestEnv::new();
    let (_main, wt, common) = linked_worktree(&env);
    symlink_config(&common);
    let Some(sb) = sandbox(&env, &wt) else { return };
    assert!(
        !sb.layout.git.notes.is_empty(),
        "a notice names the read-only git data"
    );
    assert_writes_refused(
        &env,
        &sb,
        &wt,
        &[
            append_hooks_path(&common.join("config-data")),
            append_hooks_path(&common.join("config")),
            format!(
                "rm -f {0}/config && printf '[core]\\n' > {0}/config",
                common.display()
            ),
        ],
        &[common.join("config-data")],
    );
    assert!(fs::symlink_metadata(common.join("config"))
        .expect("link")
        .file_type()
        .is_symlink());
}

#[test]
fn worktree_symlinked_info_dir_in_common_is_read_only_and_refused() {
    let env = TestEnv::new();
    let (_main, wt, common) = linked_worktree(&env);
    fs::remove_dir_all(common.join("info")).ok();
    fs::create_dir(common.join("info-data")).expect("mkdir");
    symlink("info-data", common.join("info")).expect("symlink");
    let binds = resolve_git_binds(&wt, &env.home(), None);
    assert!(
        binds.notes.iter().any(|n| n.contains("info is a symlink")),
        "resolution classifies it unsafe: {:?}",
        binds.notes
    );
    let Some(sb) = sandbox(&env, &wt) else { return };
    let ex = common.join("info-data/exclude");
    let (exit, out) = run(&sb, &format!("printf 'x\\n' > {}", ex.display()), &wt);
    assert_ne!(exit, Some(0), "{out}");
    assert!(out.contains("sandbox refused"), "early refusal: {out}");
    assert!(!ex.exists());
}

#[test]
fn worktree_info_files_symlinked_or_hardlinked_are_refused() {
    let env = TestEnv::new();
    let (_main, wt, common) = linked_worktree(&env);
    fs::create_dir_all(common.join("info")).expect("mkdir");
    let target = wt.join("alias-target");
    fs::write(&target, "").expect("write");
    let ex = common.join("info/exclude");
    fs::remove_file(&ex).ok();
    symlink(&target, &ex).expect("symlink");
    let binds = resolve_git_binds(&wt, &env.home(), None);
    assert!(
        binds.notes.iter().any(|n| n.contains("exclude is a symlink")),
        "symlinked info/exclude is unsafe: {:?}",
        binds.notes
    );
    fs::remove_file(&ex).ok();
    fs::hard_link(&target, common.join("info/attributes")).expect("hard link");
    let binds = resolve_git_binds(&wt, &env.home(), None);
    assert!(
        binds
            .notes
            .iter()
            .any(|n| n.contains("attributes has more than one hard link")),
        "hard-linked info/attributes is unsafe: {:?}",
        binds.notes
    );
    let Some(sb) = sandbox(&env, &wt) else { return };
    let (exit, out) = run(&sb, &format!("printf 'x\\n' > {}", target.display()), &wt);
    assert_ne!(exit, Some(0), "{out}");
    assert!(out.contains("sandbox refused"), "early refusal: {out}");
}

#[test]
fn worktree_symlinked_hooks_dir_inside_common_is_read_only() {
    let env = TestEnv::new();
    let (_main, wt, common) = linked_worktree(&env);
    fs::remove_dir_all(common.join("hooks")).ok();
    fs::create_dir(common.join("hooks-data")).expect("mkdir");
    symlink("hooks-data", common.join("hooks")).expect("symlink");
    let Some(sb) = sandbox(&env, &wt) else { return };
    let hook = common.join("hooks-data/pre-commit");
    assert_writes_refused(
        &env,
        &sb,
        &wt,
        &[
            format!("printf '#!/bin/sh\\ntouch /tmp/pwned\\n' > {}", hook.display()),
            format!(
                "printf '#!/bin/sh\\n' > {}",
                common.join("hooks/post-checkout").display()
            ),
        ],
        &[],
    );
    assert!(!hook.exists() && !common.join("hooks-data/post-checkout").exists());
}

#[test]
fn worktree_chained_symlink_config_with_dotdot_is_read_only() {
    let env = TestEnv::new();
    let (_main, wt, common) = linked_worktree(&env);
    fs::rename(common.join("config"), common.join("config-data")).expect("rename");
    fs::create_dir(common.join("sub")).expect("mkdir");
    symlink("sub/../config-data", common.join("link1")).expect("symlink");
    symlink("link1", common.join("config")).expect("symlink");
    let Some(sb) = sandbox(&env, &wt) else { return };
    assert_writes_refused(
        &env,
        &sb,
        &wt,
        &[
            append_hooks_path(&common.join("config-data")),
            format!("ln -sfn sub/evil {}", common.join("link1").display()),
        ],
        &[common.join("config-data")],
    );
}

#[test]
fn worktree_hard_linked_config_is_refused_and_included_config_is_read_only() {
    // Aliases without a symlink: a second name for the config's inode, and an `include` of a
    // file in the common dir.
    let env = TestEnv::new();
    let (_main, wt, common) = linked_worktree(&env);
    fs::hard_link(common.join("config"), common.join("config-alias")).expect("hard link");
    let Some(sb) = sandbox(&env, &wt) else { return };
    // The other names of a hard-linked file are unknown, so commands are refused outright.
    assert_command_refused(
        &env,
        &sb,
        &wt,
        &append_hooks_path(&common.join("config-alias")),
        &[common.join("config")],
    );

    let env = TestEnv::new();
    let (main, wt, common) = linked_worktree(&env);
    fs::write(common.join("extra"), "").expect("write");
    assert_git(&main, &env, &["config", "include.path", "extra"]);
    let Some(sb) = sandbox(&env, &wt) else { return };
    assert_writes_refused(
        &env,
        &sb,
        &wt,
        &[append_hooks_path(&common.join("extra"))],
        &[common.join("extra")],
    );
}

#[test]
fn plain_repository_symlinked_config_is_read_only() {
    let env = TestEnv::new();
    let plain = env.work().join("plain");
    init_repo(&plain, &env);
    let git_dir = plain.join(".git");
    symlink_config(&git_dir);
    let Some(sb) = sandbox(&env, &plain) else { return };
    assert!(!sb.layout.git.notes.is_empty());
    assert_writes_refused(
        &env,
        &sb,
        &plain,
        &[append_hooks_path(&git_dir.join("config-data"))],
        &[git_dir.join("config-data")],
    );
    // The working tree itself stays writable.
    let (exit, out) = run(&sb, "echo ok > file.txt", &plain);
    assert_eq!(exit, Some(0), "{out}");
}

// ------------------------------- real bwrap: aliases outside the git dirs (refused)

#[test]
fn config_symlink_to_a_file_in_the_cwd_workspace_is_refused() {
    let env = TestEnv::new();
    let (_main, wt, common) = linked_worktree(&env);
    let target = wt.join("notes/config-in-workspace");
    config_to(&common, &target);
    let Some(sb) = sandbox(&env, &wt) else { return };
    assert_command_refused(
        &env,
        &sb,
        &wt,
        &append_hooks_path(&target),
        std::slice::from_ref(&target),
    );
}

#[test]
fn chain_through_the_cwd_workspace_is_refused() {
    // The final target is in the read-only common dir, but one hop is a symlink in the cwd,
    // which the command could point elsewhere.
    let env = TestEnv::new();
    let (_main, wt, common) = linked_worktree(&env);
    fs::rename(common.join("config"), common.join("config-data")).expect("rename");
    symlink(common.join("config-data"), wt.join("hop")).expect("symlink");
    symlink(wt.join("hop"), common.join("config")).expect("symlink");
    let Some(sb) = sandbox(&env, &wt) else { return };
    let retarget = format!(
        "printf '[core]\\n\\thooksPath = /tmp/oxa-evil-hooks\\n' > evil && ln -sfn {}/evil {}",
        wt.display(),
        wt.join("hop").display()
    );
    assert_command_refused(&env, &sb, &wt, &retarget, &[common.join("config-data")]);
    assert_eq!(
        fs::read_link(wt.join("hop")).expect("hop"),
        common.join("config-data")
    );
}

#[test]
fn include_of_a_file_in_the_cwd_workspace_is_refused() {
    let env = TestEnv::new();
    let (main, wt, _common) = linked_worktree(&env);
    let inc = wt.join("included.cfg");
    fs::write(&inc, "").expect("write");
    assert_git(
        &main,
        &env,
        &["config", "include.path", inc.to_str().expect("utf8")],
    );
    let Some(sb) = sandbox(&env, &wt) else { return };
    assert_command_refused(
        &env,
        &sb,
        &wt,
        &append_hooks_path(&inc),
        std::slice::from_ref(&inc),
    );
}

#[test]
fn config_symlink_into_a_writable_cache_is_refused() {
    let env = TestEnv::new();
    let (_main, wt, common) = linked_worktree(&env);
    let cache = env.root().join("cache");
    let target = cache.join("cfg");
    config_to(&common, &target);
    let path = std::env::var("PATH").unwrap_or_default();
    let mut b = match Bwrap::detect(&path, &env.home()) {
        Ok(b) => b,
        Err(e) => {
            assert!(!required(), "OXA_REQUIRE_BWRAP=1 but bwrap is not usable: {e}");
            return;
        }
    };
    b.layout.write.push(cache.clone());
    let sb = b.with_git_dirs(&wt);
    assert_command_refused(
        &env,
        &sb,
        &wt,
        &append_hooks_path(&target),
        std::slice::from_ref(&target),
    );
}

#[test]
fn dangling_config_symlink_is_refused() {
    let env = TestEnv::new();
    let (_main, wt, common) = linked_worktree(&env);
    fs::remove_file(common.join("config")).expect("rm");
    symlink("config-data", common.join("config")).expect("symlink");
    let Some(sb) = sandbox(&env, &wt) else { return };
    let (exit, out) = run(&sb, "true", &wt);
    assert_eq!(exit, None, "{out}");
    assert!(out.contains("sandbox refused"), "{out}");
    assert!(!common.join("config-data").exists());
}

#[test]
fn quoted_include_names_are_checked_byte_for_byte() {
    // (value as written in the config, the file name git reads, the decoy a trimming parser
    // would check instead). The real name is a symlink to a file in the writable worktree; the
    // decoy is a benign file in the read-only common dir.
    let cases: &[(&str, &str, &str)] = &[
        ("\" target\"", " target", "target"),
        ("\"target \"", "target ", "target"),
        ("\"\\ttarget\"", "\ttarget", "target"),
        ("\"\ttarget \t\"", "\ttarget \t", "target"),
        ("\"tar\\\"get \"", "tar\"get ", "tar\"get"),
        ("\" tar\\\nget\"", " target", "target"),
    ];
    for (value, real, decoy) in cases {
        let env = TestEnv::new();
        let (_main, wt, common) = linked_worktree(&env);
        fs::write(common.join(decoy), "").expect("decoy");
        let data = wt.join("config-data");
        fs::write(&data, "[oxa]\n\tprobe = reached\n").expect("data");
        symlink(&data, common.join(real)).expect("symlink");
        let mut cfg = fs::read_to_string(common.join("config")).expect("config");
        cfg.push_str(&format!("[include]\n\tpath = {value}\n"));
        fs::write(common.join("config"), cfg).expect("config");
        // Real git follows the include to the worktree file, not to the decoy.
        let o = git(&wt, &env, &["config", "--get", "oxa.probe"]);
        assert_eq!(String::from_utf8_lossy(&o.stdout).trim(), "reached", "{value:?}");
        let Some(sb) = sandbox(&env, &wt) else { return };
        assert_command_refused(
            &env,
            &sb,
            &wt,
            &append_hooks_path(&data),
            std::slice::from_ref(&data),
        );
    }
}

#[test]
fn quoted_include_of_a_read_only_file_runs_and_stays_read_only() {
    // The control: the quoted name is a plain file in the common dir.
    let env = TestEnv::new();
    let (_main, wt, common) = linked_worktree(&env);
    let ok = common.join(" ok\t");
    fs::write(&ok, "[oxa]\n\tprobe = reached\n").expect("ok");
    let mut cfg = fs::read_to_string(common.join("config")).expect("config");
    cfg.push_str("[include]\n\tpath = \" ok\\t\"\n");
    fs::write(common.join("config"), cfg).expect("config");
    let o = git(&wt, &env, &["config", "--get", "oxa.probe"]);
    assert_eq!(String::from_utf8_lossy(&o.stdout).trim(), "reached");
    let Some(sb) = sandbox(&env, &wt) else { return };
    assert!(
        !sb.layout.git.notes.iter().any(|n| n.contains("sandbox refused")),
        "{:?}",
        sb.layout.git.notes
    );
    let (exit, out) = run(&sb, "git log -1 --format=%s", &wt);
    assert_eq!(exit, Some(0), "{out}");
    assert_writes_refused(
        &env,
        &sb,
        &wt,
        &[append_hooks_path(&ok)],
        std::slice::from_ref(&ok),
    );
}

// ------------------------------------------------------------- real bwrap: positives

#[test]
fn include_like_text_in_values_and_comments_keeps_the_worktree_writable() {
    let env = TestEnv::new();
    let (main, wt, common) = linked_worktree(&env);
    assert_git(
        &main,
        &env,
        &[
            "remote",
            "add",
            "origin",
            "https://example.invalid/include-lib.git",
        ],
    );
    let mut cfg = fs::read_to_string(common.join("config")).expect("config");
    cfg.push_str(
        "#include this one day
; [include]
[alias]
	inc = !echo include.path
",
    );
    fs::write(common.join("config"), cfg).expect("write");
    let Some(sb) = sandbox(&env, &wt) else { return };
    assert!(sb.layout.git.notes.is_empty(), "{:?}", sb.layout.git.notes);
    let (exit, out) = run(&sb, "git commit -q --allow-empty -m with-url", &wt);
    assert_eq!(exit, Some(0), "{out}");
    let log = git(&main, &env, &["log", "-1", "--format=%s", "feature"]);
    assert_eq!(String::from_utf8_lossy(&log.stdout).trim(), "with-url");
}

#[test]
fn normal_worktree_still_commits_indexes_and_updates_refs() {
    let env = TestEnv::new();
    let (main, wt, _common) = linked_worktree(&env);
    let Some(sb) = sandbox(&env, &wt) else { return };
    assert!(sb.layout.git.notes.is_empty(), "{:?}", sb.layout.git.notes);
    for cmd in [
        "echo hello > f.txt && git add f.txt",
        "git commit -q -m add-f",
        "git fetch -q . feature:refs/heads/fetched",
    ] {
        let (exit, out) = run(&sb, cmd, &wt);
        assert_eq!(exit, Some(0), "`{cmd}`: {out}");
    }
    let log = git(&main, &env, &["log", "-1", "--format=%s", "fetched"]);
    assert_eq!(String::from_utf8_lossy(&log.stdout).trim(), "add-f");
}

#[test]
fn normal_plain_repository_still_commits() {
    let env = TestEnv::new();
    let plain = env.work().join("plain");
    init_repo(&plain, &env);
    let Some(sb) = sandbox(&env, &plain) else { return };
    assert!(sb.layout.git.notes.is_empty());
    let (exit, out) = run(&sb, "echo x > f && git add f && git commit -q -m plain-x", &plain);
    assert_eq!(exit, Some(0), "{out}");
    let log = git(&plain, &env, &["log", "-1", "--format=%s"]);
    assert_eq!(String::from_utf8_lossy(&log.stdout).trim(), "plain-x");
}

// ------------------------------------------------------------------- argv goldens

#[test]
fn golden_unsafe_worktree_binds_its_git_dirs_read_only_after_the_cwd() {
    let env = TestEnv::new();
    let (_main, wt, common) = linked_worktree(&env);
    symlink_config(&common);
    let b = bwrap_value(&env).with_git_dirs(&wt);
    let args = args_of(&b, &wt, &env);
    let mc = "<root>/work/main/.git";
    let gd = "<root>/work/main/.git/worktrees/wt";
    let cwd_at = index_of_run(&args, &["--bind", "<root>/work/wt", "<root>/work/wt"]).expect("cwd");
    let mc_at = index_of_run(&args, &["--ro-bind", mc, mc]).expect("common ro");
    let gd_at = index_of_run(&args, &["--ro-bind", gd, gd]).expect("gitdir ro");
    let chdir_at = index_of_run(&args, &["--chdir", "<root>/work/wt"]).expect("chdir");
    // The common dir before the gitdir inside it, both after the cwd, before the chdir.
    assert!(cwd_at < mc_at && mc_at < gd_at && gd_at < chdir_at, "{args:?}");
    assert!(
        !args.windows(2).any(|w| w[0] == "--bind" && w[1].starts_with(mc)),
        "no read-write git bind: {args:?}"
    );
    assert!(
        !args
            .windows(2)
            .any(|w| (w[0] == "--tmpfs" || w[0] == "/dev/null") && w[1].starts_with(mc)),
        "nothing is masked inside a read-only git dir: {args:?}"
    );
}

#[test]
fn golden_unsafe_plain_dotgit_is_bound_read_only_after_the_cwd() {
    let env = TestEnv::new();
    let plain = env.work().join("plain");
    init_repo(&plain, &env);
    let normal = args_of(&bwrap_value(&env).with_git_dirs(&plain), &plain, &env);
    let g = "<root>/work/plain/.git";
    assert!(index_of_run(&normal, &["--bind", g, g]).is_some(), "{normal:?}");
    symlink_config(&plain.join(".git"));
    let args = args_of(&bwrap_value(&env).with_git_dirs(&plain), &plain, &env);
    let cwd_at = index_of_run(&args, &["--bind", "<root>/work/plain", "<root>/work/plain"]).expect("cwd");
    let g_at = index_of_run(&args, &["--ro-bind", g, g]).expect(".git ro");
    assert!(cwd_at < g_at, "{args:?}");
    assert!(index_of_run(&args, &["--bind", g, g]).is_none(), "{args:?}");
}

#[test]
fn a_symlink_that_appears_after_resolution_downgrades_the_next_command() {
    let env = TestEnv::new();
    let (_main, wt, common) = linked_worktree(&env);
    let b = bwrap_value(&env).with_git_dirs(&wt);
    let mc = "<root>/work/main/.git";
    assert!(index_of_run(&args_of(&b, &wt, &env), &["--bind", mc, mc]).is_some());
    symlink_config(&common);
    let args = args_of(&b, &wt, &env);
    assert!(index_of_run(&args, &["--ro-bind", mc, mc]).is_some(), "{args:?}");
    assert!(index_of_run(&args, &["--bind", mc, mc]).is_none(), "{args:?}");
    // A fresh resolution says why.
    let fresh = resolve_git_binds(&wt, &env.home(), Some(&env.agent_dir()));
    assert!(fresh.rw.is_empty(), "{fresh:?}");
    assert!(
        fresh.notes.iter().any(|n| n.contains("config is a symlink")),
        "{fresh:?}"
    );
}
