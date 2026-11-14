//! sbx-gitsym: the `git config` cross-check is bounded by one deadline, also when `git` exits
//! at once but leaves a process holding its stdout. A fake `git` first on `PATH` does exactly
//! that; the sandbox must refuse (fail closed) well within 7 s instead of waiting for the pipe.
//!
//! Its own test binary: it changes `PATH` for the whole process.

use oxa_fake::TestEnv;
use oxa_proto::{CancelToken, ExecSpec, Sandbox};
use oxa_sandbox::{Bwrap, Layout};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn write(p: &Path, body: &str) {
    fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
    fs::write(p, body).expect("write");
}

#[test]
fn git_that_leaves_its_stdout_held_open_is_refused_within_the_deadline() {
    let env = TestEnv::new();
    let root = env.root();
    // A hand-built linked worktree whose common config includes a file in the common dir.
    let main_git = root.join("main/.git");
    write(&main_git.join("HEAD"), "ref: refs/heads/main\n");
    fs::create_dir_all(main_git.join("objects/pack")).expect("objects");
    write(
        &main_git.join("config"),
        "[core]\n\tbare = false\n[include]\n\tpath = extra\n",
    );
    write(&main_git.join("extra"), "");
    let gitdir = main_git.join("worktrees/wt1");
    write(&gitdir.join("HEAD"), "ref: refs/heads/feature\n");
    write(&gitdir.join("commondir"), "../..\n");
    let wt = root.join("work/wt1");
    fs::create_dir_all(&wt).expect("wt");
    write(
        &gitdir.join("gitdir"),
        &format!("{}\n", wt.join(".git").display()),
    );
    write(&wt.join(".git"), &format!("gitdir: {}\n", gitdir.display()));

    // The fake git: a grandchild keeps stdout open and sleeps; git itself exits 0 at once.
    let bin = root.join("fakebin");
    let git = bin.join("git");
    write(&git, "#!/bin/sh\nsleep 20 &\nexit 0\n");
    fs::set_permissions(&git, fs::Permissions::from_mode(0o755)).expect("chmod");
    let path = std::env::var("PATH").unwrap_or_default();
    std::env::set_var("PATH", format!("{}:{path}", bin.display()));

    let started = Instant::now();
    let b = Bwrap {
        bwrap: PathBuf::from("/usr/bin/bwrap"),
        home: env.home(),
        layout: Layout::default(),
    }
    .with_git_dirs(&wt);
    let spec = ExecSpec {
        command: "true".into(),
        cwd: wt.clone(),
        writable: true,
        net: false,
        timeout_s: Some(30),
        env: Vec::new(),
    };
    let r = b.run(&spec, &CancelToken::new(), &mut |_| {});
    let took = started.elapsed();
    let out = String::from_utf8_lossy(&r.output).into_owned();
    assert!(took < Duration::from_secs(7), "took {took:?}: {out}");
    assert_eq!(r.exit, None, "{out}");
    assert!(out.contains("sandbox refused"), "{out}");
    // The command after the timeout was refused at once by the repository's cooldown.
    assert!(out.contains("git config check timed out; retry in"), "{out}");
    assert!(
        b.layout.git.notes.iter().any(|n| n.contains("git config check")),
        "{:?}",
        b.layout.git.notes
    );
}
