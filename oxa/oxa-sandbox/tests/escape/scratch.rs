//! #457: the session directory at `/tmp` (persistent scratch and a writable cache home).

use crate::harness::{assert_ok, assert_refused, home_file, run, run_result, sandbox_with_layout, sh, write};
use oxa_fake::TestEnv;
use oxa_sandbox::{scratch_root, Bwrap, Layout, LayoutInputs};
use std::path::Path;

const SECRET: &str = "SYNTHETIC-PRIVATE-MATERIAL-457";

/// The process sandbox the CLI builds: toolchain env from the user's (plus `user_env`), the
/// cargo registry and `write` read-write, and a root for session directories.
fn process(env: &TestEnv, user_env: Vec<(String, String)>, write: Vec<std::path::PathBuf>) -> Option<Bwrap> {
    let home = env.home();
    // The machine's toolchains (they live under the real home, which the sandbox hides), as
    // the user's own layout shows them: tool directories read-only, the cargo registry cache
    // read-write.
    let host = std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_default();
    // CI runs the tests with a cleared environment and HOME set to a scratch directory, so
    // neither the variables nor HOME locate the toolchains. Fall back to the layout the
    // `cargo` on PATH lives in: `<home>/.cargo/bin/cargo` beside `<home>/.rustup`.
    let on_path_home = tool_dir("cargo")
        .filter(|d| d.file_name().is_some_and(|n| n == "bin"))
        .and_then(|d| d.parent().map(Path::to_path_buf));
    let cargo = std::env::var("CARGO_HOME")
        .map(std::path::PathBuf::from)
        .ok()
        .or(on_path_home.clone())
        .unwrap_or(host.join(".cargo"));
    let rustup = std::env::var("RUSTUP_HOME")
        .map(std::path::PathBuf::from)
        .ok()
        .or(on_path_home
            .and_then(|c| c.parent().map(|p| p.join(".rustup")))
            .filter(|r| r.is_dir()))
        .unwrap_or(host.join(".rustup"));
    let mut read_extra = vec![cargo.join("bin"), rustup.clone()];
    read_extra.extend(
        ["cargo", "gh", "python3"]
            .iter()
            .filter_map(|t| {
                tool_dir(t).and_then(|d| d.join(t).canonicalize().ok()?.parent().map(Path::to_path_buf))
            })
            .filter(|d| !d.starts_with("/usr") && !d.starts_with("/bin")),
    );
    // A Python virtual environment first on PATH (its `pip` lives beside its `bin`).
    read_extra.extend(
        tool_dir("python3")
            .and_then(|d| d.parent().map(Path::to_path_buf))
            .filter(|v| v.join("pyvenv.cfg").is_file()),
    );
    let mut toolchain_env = vec![
        ("CARGO_HOME".to_string(), cargo.display().to_string()),
        ("RUSTUP_HOME".to_string(), rustup.display().to_string()),
    ];
    // CI selects the toolchain with RUSTUP_TOOLCHAIN and has no rustup default.
    if let Ok(t) = std::env::var("RUSTUP_TOOLCHAIN") {
        toolchain_env.push(("RUSTUP_TOOLCHAIN".to_string(), t));
    }
    toolchain_env.extend(user_env);
    let mut write_extra = vec![cargo.join("registry")];
    write_extra.extend(write);
    let l = Layout::build(&LayoutInputs {
        home: home.clone(),
        agent_dir: Some(env.agent_dir()),
        path_var: std::env::var("PATH").unwrap_or_default(),
        env: toolchain_env,
        read_extra,
        write_extra,
        ..LayoutInputs::default()
    });
    let l = Layout {
        mask_runtime_ipc: true,
        scratch_root: Some(scratch_root(&home)),
        ..l
    };
    sandbox_with_layout(env, l)
}

/// One session of [`process`] and its directory on the host.
fn session(env: &TestEnv) -> Option<(Bwrap, std::path::PathBuf)> {
    session_of(&process(env, Vec::new(), Vec::new())?)
}

fn session_of(process: &Bwrap) -> Option<(Bwrap, std::path::PathBuf)> {
    let sb = process.session();
    let dir = sb
        .layout
        .scratch
        .as_ref()
        .expect("a session directory")
        .path()
        .to_path_buf();
    Some((sb, dir))
}

fn tool_dir(tool: &str) -> Option<std::path::PathBuf> {
    std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .map(std::path::PathBuf::from)
        .find(|d| d.is_absolute() && d.join(tool).is_file())
}

/// The tool is a real binary on PATH (not a version-manager shim this test cannot set up).
fn on_path(tool: &str) -> bool {
    let real = tool_dir(tool).and_then(|d| d.join(tool).canonicalize().ok());
    let ok = real
        .as_deref()
        .and_then(|r| r.file_name())
        .is_some_and(|n| n.to_string_lossy().starts_with(tool));
    if !ok {
        eprintln!("escape SKIPPED: {tool} is not a plain binary on PATH");
    }
    ok
}

/// Two bash calls of one session share `/tmp`: a file, a directory tree and an executable
/// written by the first are there for the second (#191). Writes do not reach the host `/tmp`,
/// and the directory is removed when the session ends.
#[test]
fn tmp_persists_between_calls_and_is_removed_at_the_end() {
    let env = TestEnv::new();
    let Some((sb, dir)) = session(&env) else {
        return;
    };
    let name = format!("oxa-457-{}", std::process::id());
    let first = format!(
        "mkdir -p /tmp/{n}/bin && printf '#!/bin/sh\\necho built\\n' > /tmp/{n}/bin/t && chmod +x /tmp/{n}/bin/t && echo $TMPDIR",
        n = sh(&name)
    );
    let (exit, out) = run(&sb, &env.work(), &first, true, false);
    assert_ok(exit, &out, "first call");
    assert_eq!(out.trim(), "/tmp");
    let (exit, out) = run(
        &sb,
        &env.work(),
        &format!("/tmp/{}/bin/t", sh(&name)),
        true,
        false,
    );
    assert_ok(exit, &out, "second call");
    assert_eq!(out.trim(), "built");
    assert!(
        !Path::new("/tmp").join(&name).exists(),
        "/tmp write reached the host /tmp"
    );
    assert!(dir.join(&name).is_dir());
    drop(sb);
    assert!(!dir.exists(), "the session directory outlived the session");
}

/// The cache home is writable for every XDG tool (#362 gh, #400/#446 sccache, pip), and a
/// read-write package cache under `~/.cache` stays the shared one.
#[test]
fn cache_home_is_writable_and_shared_caches_stay_shared() {
    let env = TestEnv::new();
    std::fs::create_dir_all(home_file(&env, ".cache/sccache")).unwrap();
    let Some((sb, dir)) = session(&env) else {
        return;
    };
    let (exit, out) = run(
        &sb,
        &env.work(),
        "mkdir -p \"$XDG_CACHE_HOME/gh\" && touch \"$XDG_CACHE_HOME/gh/x\" && touch \"$XDG_CACHE_HOME/sccache/shared\"",
        true,
        false,
    );
    assert_ok(exit, &out, "cache home");
    assert!(
        home_file(&env, ".cache/sccache/shared").exists(),
        "sccache cache is not the shared one"
    );
    // pip's own choice of cache directory (no fallback that prints the variable back) must be
    // the session's, and writable: with a read-only cache home pip reports a path under the
    // hidden home or the host's, and the write fails.
    if pip_in(&sb, &env) {
        let (exit, out) = run(
            &sb,
            &env.work(),
            "d=$(python3 -m pip cache dir) && mkdir -p \"$d/http-v2\" && touch \"$d/http-v2/probe\" && echo \"$d\"",
            true,
            false,
        );
        assert_ok(exit, &out, "pip");
        assert_eq!(out.trim(), "/tmp/.cache/pip", "{out}");
        assert!(
            dir.join(".cache/pip/http-v2/probe").is_file(),
            "pip's cache is not the session's"
        );
    }
    if on_path("gh") {
        let (exit, out) = run(&sb, &env.work(), "gh --version", true, false);
        assert_ok(exit, &out, "gh");
    }
}

/// #995: the cache path tools hardcode is `$HOME/.cache`, not `$XDG_CACHE_HOME`. The host cache
/// home is read-only inside a session, so a helper (a git hook's log, `$CARGO_TARGET_DIR` under
/// it) writing there fails on the read-only home. The session binds its own empty directory
/// there: the write works, lands in the session directory and is deleted with it, a shared
/// package cache under `$HOME/.cache` stays the user's, and the scratch root (other sessions)
/// is not exposed through it.
#[test]
fn the_hardcoded_home_cache_is_writable_in_the_session() {
    let env = TestEnv::new();
    std::fs::create_dir_all(home_file(&env, ".cache/sccache")).unwrap();
    let Some((sb, dir)) = session(&env) else {
        return;
    };
    let (exit, out) = run(
        &sb,
        &env.work(),
        "mkdir -p \"$HOME/.cache/build\" && echo built > \"$HOME/.cache/build/out\" \
         && touch \"$HOME/.cache/sccache/shared\" && echo written",
        true,
        false,
    );
    assert_ok(exit, &out, "hardcoded $HOME/.cache");
    assert!(out.contains("written"), "{out}");
    // The writes are the session's; on the host only the shared package cache was written.
    assert!(dir.join(".cache-home/build/out").is_file(), "{dir:?}");
    assert!(!home_file(&env, ".cache/build").exists());
    assert!(home_file(&env, ".cache/sccache/shared").exists());
    // Other sessions' directories are not shown through the cache home.
    let (exit, out) = run(
        &sb,
        &env.work(),
        "test ! -e \"$HOME/.cache/oxa\" && echo absent",
        true,
        false,
    );
    assert_ok(exit, &out, "scratch root");
    assert!(out.contains("absent"), "{out}");
}

/// #530 / PR #539: custom cache paths must not expose other checkouts, but builds must
/// still get a writable cache (also on a fresh home, for wrappers with any name).
#[test]
fn sccache_fallback_is_writable_without_exposing_external_checkouts() {
    let env = TestEnv::new();
    let outside = env.root().join("data/sccache-work");
    let hook = outside.join("checkout/.git/hooks/pre-commit");
    write(&hook, "original hook");
    let user = vec![("SCCACHE_DIR".into(), outside.display().to_string())];
    let Some(p) = process(&env, user, Vec::new()) else {
        return;
    };
    let Some((sb, dir)) = session_of(&p) else {
        return;
    };
    let (exit, out) = run(
        &sb,
        &env.work(),
        "mkdir -p \"$SCCACHE_DIR/a/b\" && echo cached > \"$SCCACHE_DIR/a/b/object\" && echo \"$SCCACHE_DIR\"",
        true,
        false,
    );
    assert_ok(exit, &out, "fallback cache write");
    assert_eq!(out.trim(), "/tmp/.cache/sccache");
    assert!(dir.join(".cache/sccache/a/b/object").is_file());
    let (exit, out) = run(
        &sb,
        &env.work(),
        &format!("echo replaced > {}", sh(&hook.display().to_string())),
        true,
        false,
    );
    assert_refused(exit, &out, "external hook write");
    assert_eq!(std::fs::read_to_string(&hook).unwrap(), "original hook");

    let Some(p) = process(&env, Vec::new(), Vec::new()) else {
        return;
    };
    let Some((sb, dir)) = session_of(&p) else {
        return;
    };
    let (exit, out) = run(
        &sb,
        &env.work(),
        "mkdir -p \"$SCCACHE_DIR\" && touch \"$SCCACHE_DIR/fresh\"",
        true,
        false,
    );
    assert_ok(exit, &out, "fresh cache write");
    assert!(dir.join(".cache/sccache/fresh").is_file());
    assert!(!home_file(&env, ".cache/sccache").exists());
}

/// Both existing home caches and explicit outside writePaths keep sharing across layouts,
/// regardless of their name and whether an earlier build has populated them.
#[test]
fn sccache_shared_caches_stay_shared_after_population() {
    for (relative, custom, explicit) in [
        ("home/.cache/sccache", false, false),
        ("home/.cache/build-cache", true, false),
        ("data/build-cache", true, true),
    ] {
        let env = TestEnv::new();
        let cache = env.root().join(relative);
        std::fs::create_dir_all(&cache).unwrap();
        for marker in ["first", "second"] {
            let user = if custom {
                vec![("SCCACHE_DIR".into(), cache.display().to_string())]
            } else {
                Vec::new()
            };
            let writes = if explicit { vec![cache.clone()] } else { Vec::new() };
            let Some(p) = process(&env, user, writes) else {
                return;
            };
            let Some((sb, _)) = session_of(&p) else {
                return;
            };
            let (exit, out) = run(
                &sb,
                &env.work(),
                &format!("mkdir -p \"$SCCACHE_DIR/a/b\" && echo cached > \"$SCCACHE_DIR/a/b/{marker}\""),
                true,
                false,
            );
            assert_ok(exit, &out, "shared cache write");
            assert!(cache.join("a/b").join(marker).is_file(), "{relative}");
        }
        assert!(cache.join("a/b/first").is_file());
    }
}

/// pip runs inside the sandbox (python3 with its pip module visible there).
fn pip_in(sb: &Bwrap, env: &TestEnv) -> bool {
    let (exit, _) = run(sb, &env.work(), "python3 -m pip --version", true, false);
    if exit != Some(0) {
        eprintln!("escape SKIPPED: pip is not available to python3 in the sandbox");
    }
    exit == Some(0)
}

/// #457 review: a user-set `XDG_CACHE_HOME` keeps its shared caches. A cache bound read-write
/// in it (here sccache, as `writePaths` does) is reached through `$XDG_CACHE_HOME/<name>` and
/// writes land in the user's directory; an unbound one stays read-only and is not linked, so
/// the tool caches in the session instead.
#[test]
fn user_cache_home_keeps_its_shared_caches() {
    let env = TestEnv::new();
    let xdg = env.root().join("data/cache");
    std::fs::create_dir_all(xdg.join("sccache")).unwrap();
    std::fs::create_dir_all(xdg.join("other")).unwrap();
    let user = vec![("XDG_CACHE_HOME".to_string(), xdg.display().to_string())];
    let Some(p) = process(&env, user, vec![xdg.join("sccache")]) else {
        return;
    };
    let Some((sb, dir)) = session_of(&p) else {
        return;
    };
    let (exit, out) = run(
        &sb,
        &env.work(),
        "echo \"$XDG_CACHE_HOME\" && touch \"$XDG_CACHE_HOME/sccache/shared\" && mkdir -p \"$XDG_CACHE_HOME/other\" && touch \"$XDG_CACHE_HOME/other/x\"",
        true,
        false,
    );
    assert_ok(exit, &out, "user cache home");
    assert_eq!(out.lines().next(), Some("/tmp/.cache"), "{out}");
    assert!(
        xdg.join("sccache/shared").is_file(),
        "the user's sccache cache was not shared"
    );
    assert!(!xdg.join("other/x").exists(), "an unbound cache became writable");
    assert!(dir.join(".cache/other/x").is_file());
}

/// #457 review (Codex P2): two sessions of one process (`/new`, `/resume`) never share
/// `/tmp`: a file the first left is absent in the second, and ending the first removes only
/// its own directory.
#[test]
fn sessions_of_one_process_do_not_share_tmp() {
    let env = TestEnv::new();
    let Some(p) = process(&env, Vec::new(), Vec::new()) else {
        return;
    };
    let (Some((first, a)), Some((second, b))) = (session_of(&p), session_of(&p)) else {
        return;
    };
    let (exit, out) = run(&first, &env.work(), "echo kept > /tmp/from-first", true, false);
    assert_ok(exit, &out, "first session");
    let (exit, out) = run(&second, &env.work(), "cat /tmp/from-first", true, false);
    assert_ne!(
        exit,
        Some(0),
        "the second session read the first one's /tmp: {out}"
    );
    assert!(!out.contains("kept"), "{out}");
    drop(first);
    assert!(!a.exists(), "the first session's directory outlived it");
    let (exit, out) = run(
        &second,
        &env.work(),
        "echo still > /tmp/x && cat /tmp/x",
        true,
        false,
    );
    assert_ok(exit, &out, "second session after the first ended");
    assert!(b.join("x").is_file());
    drop(second);
    assert!(!b.exists());
}

/// cargo builds a project in `/tmp` in one call and runs it in the next.
#[test]
fn cargo_builds_in_tmp_across_calls() {
    if tool_dir("cargo").is_none() {
        eprintln!("escape SKIPPED: cargo is not on PATH");
        return;
    }
    let env = TestEnv::new();
    let Some((sb, _dir)) = session(&env) else {
        return;
    };
    let build = "printf 'fixture.phase=prepare\\n' && mkdir -p /tmp/p/src && printf '[package]\\nname=\"p\"\\nversion=\"0.1.0\"\\nedition=\"2021\"\\n' > /tmp/p/Cargo.toml && printf 'fn main(){println!(\"hi457\")}' > /tmp/p/src/main.rs && cd /tmp/p && printf 'fixture.phase=cargo-build\\n' && cargo build --offline -vv 2>&1";
    let result = run_result(&sb, &env.work(), build, true, false);
    assert_ok(
        result.exit,
        &format!(
            "fixture.phase=build-return timed_out={}\n{}",
            result.timed_out,
            String::from_utf8_lossy(&result.output)
        ),
        "cargo build",
    );
    let (exit, out) = run(&sb, &env.work(), "/tmp/p/target/debug/p", true, false);
    assert_ok(exit, &out, "run");
    assert_eq!(out.trim(), "hi457");
}

/// Threat: the session directory becomes a way around the deny-list.
/// Attempt: a symlink in `/tmp` to a key and to the agent directory, then read through it.
/// Expected refusal: the masks still hold; no byte of the secret is read.
#[test]
fn session_dir_does_not_show_credentials() {
    let env = TestEnv::new();
    write(&home_file(&env, ".ssh/id_test"), SECRET);
    write(&env.agent_dir().join("trust.json"), SECRET);
    let Some((sb, _dir)) = session(&env) else {
        return;
    };
    let cmd = format!(
        "ln -s {} /tmp/k && ln -s {} /tmp/a && cat /tmp/k /tmp/a/trust.json",
        sh(&home_file(&env, ".ssh/id_test").display().to_string()),
        sh(&env.agent_dir().display().to_string())
    );
    let (exit, out) = run(&sb, &env.work(), &cmd, true, false);
    assert_refused(exit, &out, "read through /tmp");
    assert!(!out.contains(SECRET), "leaked: {out}");
}
