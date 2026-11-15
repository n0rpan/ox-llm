//! Paths: `..`, absolute paths, hard links and symlinks out of the workspace.

use crate::harness::{assert_refused, dir, home_file, run, sandbox, sh, write};
use oxa_fake::TestEnv;
use std::path::Path;

/// Threat: a command writes outside the project with `..` or an absolute path.
/// Attempt: `../escape`, `/tmp/<unique>`, `/etc/<unique>`, and a symlink to `/`.
/// Expected refusal: the cwd bind is the only writable project path; the host is unchanged.
#[test]
fn dotdot_and_absolute_writes_stay_out() {
    let env = TestEnv::new();
    let Some(sb) = sandbox(&env) else { return };
    let outside = env.work().parent().expect("parent").join("dotdot-escape");
    let tmp_name = format!("oxa-escape-{}", std::process::id());
    let etc = Path::new("/etc").join(&tmp_name);
    let symlink_root = env.work().join("root");
    std::os::unix::fs::symlink("/", &symlink_root).expect("symlink");

    // `..` and `/tmp` land on the private tmpfs (the test root is under the host /tmp), so
    // they may succeed inside the sandbox: the assertion is that the host is untouched.
    for cmd in [
        format!("printf x > {}", sh("../dotdot-escape")),
        format!("printf x > /tmp/{}", sh(&tmp_name)),
    ] {
        let (exit, out) = run(&sb, &env.work(), &cmd, true, false);
        assert!(exit.is_some(), "the command must run inside the sandbox: {out}");
    }
    assert!(!outside.exists(), ".. wrote outside the cwd");
    assert!(
        !Path::new("/tmp").join(&tmp_name).exists(),
        "/tmp write reached the host"
    );
    // `/etc` and `/` are read-only mounts, so those writes are refused.
    for cmd in [
        format!("printf x > /etc/{}", sh(&tmp_name)),
        "printf x > root/etc/oxa-escape".to_string(),
    ] {
        let (exit, out) = run(&sb, &env.work(), &cmd, true, false);
        assert_refused(exit, &out, &cmd);
    }
    assert!(!etc.exists(), "/etc write reached the host");
    // Control: the same write inside the cwd succeeds.
    let (exit, out) = run(&sb, &env.work(), "printf ok > inside", true, false);
    assert_eq!(exit, Some(0), "{out}");
    assert_eq!(
        std::fs::read_to_string(env.work().join("inside")).expect("inside"),
        "ok"
    );
}

/// Threat: a hard link aliases a protected file into the writable project.
/// Attempt: `ln .git/config .git/config.alias`, then write the alias; also `ln /etc/passwd`.
/// Expected refusal: the protected entry is a separate mount, so the link fails (EXDEV) and
/// the host file is unchanged.
#[test]
fn hard_links_cannot_alias_a_protected_file() {
    let env = TestEnv::new();
    let work = env.work();
    dir(&work.join(".git/hooks"));
    dir(&work.join(".git/objects"));
    write(&work.join(".git/HEAD"), "ref: refs/heads/main\n");
    write(
        &work.join(".git/config"),
        "[core]\n\trepositoryformatversion = 0\n",
    );
    let Some(sb) = sandbox(&env) else { return };
    for cmd in [
        "ln .git/config .git/config.alias",
        "ln /etc/passwd ./passwd.alias",
        "ln ~/.netrc ./netrc.alias",
    ] {
        let (exit, out) = run(&sb, &work, cmd, true, false);
        assert_refused(exit, &out, cmd);
    }
    assert!(!work.join(".git/config.alias").exists());
    assert!(!work.join("passwd.alias").exists());
    assert!(!work.join("netrc.alias").exists());
    assert_eq!(
        std::fs::read_to_string(work.join(".git/config")).expect("config"),
        "[core]\n\trepositoryformatversion = 0\n"
    );
}

/// Threat: a symlink inside the project points at a writable cache outside the project.
/// Attempt: create a symlink to a layout write path, then write through it.
/// Expected refusal: the cache is the only writable part of that path; the test home stays
/// unchanged. (A cache is documented as shared with the user's builds; this only pins the
/// containment of the *project* test home.)
#[test]
fn symlink_to_a_cache_does_not_make_the_home_writable() {
    let env = TestEnv::new();
    write(&home_file(&env, ".cache/pip/keep"), "keep\n");
    let work = env.work();
    std::os::unix::fs::symlink(home_file(&env, ".cache/pip"), work.join("cache")).expect("symlink");
    let mut layout = oxa_sandbox::Layout::default();
    layout.write.push(home_file(&env, ".cache/pip"));
    let Some(sb) = crate::harness::sandbox_with_layout(&env, layout) else {
        return;
    };
    let (exit, out) = run(&sb, &work, "printf x > cache/keep", true, false);
    assert_eq!(exit, Some(0), "the cache is writable by design: {out}");
    // The home with the cache is still hidden; a cache symlink cannot be used to write home.
    let (exit, out) = run(&sb, &work, "printf x > ../home/.ssh/authorized_keys", true, false);
    assert_refused(exit, &out, "home write through the hidden home");
}
