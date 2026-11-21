//! #318: a session's sandboxed commands leave the host's home and the cwd's parent directories
//! as they were, also while they run: no `.git` (or any other entry) appears for a moment in
//! the account home. Real bwrap; a plain repository, a worktree and a plain directory in the
//! home, several commands each (git included). A control shows the watch catches a mount point
//! that bwrap creates on the host.
//!
//! Real-bwrap tests skip with a notice when bwrap is unusable (a failure under
//! `OXA_REQUIRE_BWRAP=1`). Isolation: every path lives under `oxa_fake::TestEnv`.

mod common {
    pub mod host_watch;
}
use common::host_watch::HostWatch;
use oxa_fake::TestEnv;
use oxa_proto::{CancelToken, ExecSpec, Sandbox};
use oxa_sandbox::Bwrap;
use std::path::Path;
use std::process::Command;

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .expect("git runs")
        .success();
    assert!(ok, "git {args:?}");
}

fn detect(env: &TestEnv) -> Option<Bwrap> {
    let path = std::env::var("PATH").unwrap_or_default();
    match Bwrap::detect(&path, &env.home()) {
        Ok(b) => Some(b),
        Err(e) => {
            assert!(
                std::env::var("OXA_REQUIRE_BWRAP").map_or(true, |v| v != "1"),
                "OXA_REQUIRE_BWRAP=1 but bwrap is not usable: {e}"
            );
            eprintln!("host_untouched SKIPPED: bwrap is not usable: {e}");
            None
        }
    }
}

/// A session's worth of commands in `cwd`, watched as a whole.
fn session(env: &TestEnv, cwd: &Path) -> Vec<String> {
    let Some(b) = detect(env) else {
        return Vec::new();
    };
    let b = b.with_git_dirs(cwd);
    let watch = HostWatch::new(env.root(), &env.home(), cwd);
    for (cmd, writable) in [
        ("git status >/dev/null 2>&1; ls -a .. >/dev/null", true),
        ("echo x > f.txt; git add -A >/dev/null 2>&1; true", true),
        ("sleep 0.2; git log --oneline >/dev/null 2>&1; true", true),
        ("ls >/dev/null", false),
    ] {
        let spec = ExecSpec {
            command: cmd.into(),
            cwd: cwd.to_path_buf(),
            writable,
            net: false,
            timeout_s: Some(30),
            env: Vec::new(),
        };
        let r = b.run(&spec, &CancelToken::new(), &mut |_| {});
        assert_eq!(r.exit, Some(0), "{cmd}: {}", String::from_utf8_lossy(&r.output));
    }
    watch.finish()
}

#[test]
fn a_repository_in_the_home_leaves_the_home_untouched() {
    let env = TestEnv::new();
    let proj = env.home().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    git(&proj, &["init", "-q"]);
    assert_eq!(session(&env, &proj), Vec::<String>::new());
}

#[test]
fn a_worktree_in_the_home_leaves_the_home_untouched() {
    let env = TestEnv::new();
    let main = env.home().join("lab/main");
    std::fs::create_dir_all(&main).unwrap();
    git(&main, &["init", "-q"]);
    git(&main, &["commit", "-q", "--allow-empty", "-m", "c"]);
    let wt = env.home().join("lab/wt/one");
    git(&main, &["worktree", "add", "-q", wt.to_str().unwrap()]);
    assert_eq!(session(&env, &wt), Vec::<String>::new());
}

#[test]
fn a_plain_directory_in_the_home_leaves_the_home_untouched() {
    let env = TestEnv::new();
    let dir = env.home().join("notes");
    std::fs::create_dir_all(&dir).unwrap();
    assert_eq!(session(&env, &dir), Vec::<String>::new());
}

/// Control: a mask whose parent is missing in a writable bind of the home makes bwrap create
/// that parent on the host; the watch reports it.
#[test]
fn control_the_watch_sees_a_mount_point_created_in_the_home() {
    let env = TestEnv::new();
    if detect(&env).is_none() {
        return;
    }
    let home = env.home();
    let watch = HostWatch::new(env.root(), &home, &home.join("proj"));
    let h = home.to_string_lossy().into_owned();
    let mask = home.join(".git/commondir").to_string_lossy().into_owned();
    let st = Command::new("bwrap")
        .args([
            "--ro-bind",
            "/",
            "/",
            "--bind",
            &h,
            &h,
            "--ro-bind",
            "/dev/null",
            &mask,
            "true",
        ])
        .status()
        .unwrap();
    assert!(st.success());
    let seen = watch.finish();
    assert!(
        seen.iter()
            .any(|s| s.ends_with("/home/.git") && s.starts_with("appeared")),
        "{seen:?}"
    );
}

// ------------------------------------------------------------------ #276: markers are removed

/// A session sandbox with a scratch root (so the marker ledger exists), like the CLI's.
fn session_sandbox(env: &TestEnv, cwd: &Path) -> Option<Bwrap> {
    let mut b = detect(env)?;
    b.layout.scratch_root = Some(env.root().join("scratch"));
    Some(b.with_git_dirs(cwd).session())
}

fn git_listing(dot_git: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dot_git)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn git_status(dir: &Path) -> String {
    let out = Command::new("git")
        .args(["status", "--porcelain", "--ignored"])
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn run_in(b: &Bwrap, cwd: &Path, command: &str) {
    let spec = ExecSpec {
        command: command.into(),
        cwd: cwd.to_path_buf(),
        writable: true,
        net: false,
        timeout_s: Some(30),
        env: Vec::new(),
    };
    let r = b.run(&spec, &CancelToken::new(), &mut |_| {});
    assert_eq!(
        r.exit,
        Some(0),
        "{command}: {}",
        String::from_utf8_lossy(&r.output)
    );
}

fn plain_repo(env: &TestEnv) -> std::path::PathBuf {
    let repo = env.work().join("plain");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    // No hooks directory: the sandbox needs a mount point for its mask.
    std::fs::remove_dir_all(repo.join(".git/hooks")).unwrap();
    repo
}

#[test]
fn a_session_leaves_the_git_directory_as_it_found_it() {
    let env = TestEnv::new();
    let repo = plain_repo(&env);
    let dot_git = repo.join(".git");
    // An entry that existed before is never removed.
    std::fs::write(dot_git.join("config.worktree"), "").unwrap();
    let (listing, status) = (git_listing(&dot_git), git_status(&repo));
    {
        let Some(b) = session_sandbox(&env, &repo) else {
            return;
        };
        run_in(&b, &repo, "git status >/dev/null 2>&1; true");
        assert!(
            dot_git.join("hooks").is_dir(),
            "the mount point exists during the session"
        );
    }
    assert_eq!(git_listing(&dot_git), listing);
    assert_eq!(git_status(&repo), status);
}

#[test]
fn a_marker_changed_on_the_host_is_left_alone() {
    let env = TestEnv::new();
    let repo = plain_repo(&env);
    let dot_git = repo.join(".git");
    {
        let Some(b) = session_sandbox(&env, &repo) else {
            return;
        };
        run_in(&b, &repo, "true");
        assert!(dot_git.join("gitdir").is_file());
        std::fs::write(dot_git.join("gitdir"), "written by the owner\n").unwrap();
    }
    assert_eq!(
        std::fs::read_to_string(dot_git.join("gitdir")).unwrap(),
        "written by the owner\n"
    );
    assert!(
        !dot_git.join("commondir").exists(),
        "the untouched marker is removed"
    );
}

#[test]
fn markers_stay_until_the_last_session_of_a_repository_ends() {
    let env = TestEnv::new();
    let repo = plain_repo(&env);
    let dot_git = repo.join(".git");
    let before = git_listing(&dot_git);
    let (Some(a), Some(b)) = (session_sandbox(&env, &repo), session_sandbox(&env, &repo)) else {
        return;
    };
    run_in(&a, &repo, "true");
    run_in(&b, &repo, "true");
    assert!(dot_git.join("commondir").exists());
    drop(a);
    for name in ["commondir", "gitdir", "hooks"] {
        assert!(
            dot_git.join(name).exists(),
            "{name} removed under the second session"
        );
    }
    run_in(&b, &repo, "git status >/dev/null 2>&1; true");
    drop(b);
    assert_eq!(git_listing(&dot_git), before);
}

#[test]
fn a_marker_left_by_a_killed_session_is_swept_by_the_next_one() {
    let env = TestEnv::new();
    let repo = plain_repo(&env);
    let dot_git = repo.join(".git");
    let before = git_listing(&dot_git);
    // What a killed oxa leaves: the marker and its ledger entry, with nobody holding a lock.
    std::fs::write(dot_git.join("commondir"), ".\n").unwrap();
    let ledger = env.root().join("scratch/markers");
    std::fs::create_dir_all(&ledger).unwrap();
    std::fs::write(
        ledger.join("stale"),
        format!("f\n{}\n.\n", dot_git.join("commondir").display()),
    )
    .unwrap();
    if session_sandbox(&env, &repo).is_none() {
        return;
    }
    assert_eq!(git_listing(&dot_git), before);
    assert!(!ledger.join("stale").exists());
}
