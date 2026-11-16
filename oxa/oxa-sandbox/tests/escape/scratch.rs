//! #457: the session directory at `/tmp` (persistent scratch and a writable cache home).

use crate::harness::{assert_ok, assert_refused, home_file, run, sandbox_with_layout, sh, write};
use oxa_fake::TestEnv;
use oxa_sandbox::{scratch_root, Layout, LayoutInputs, Scratch};
use std::path::Path;

const SECRET: &str = "SYNTHETIC-PRIVATE-MATERIAL-457";

/// The layout the CLI builds: toolchain env from the user's, a session directory at `/tmp`.
fn session(env: &TestEnv) -> (Layout, std::path::PathBuf) {
    let home = env.home();
    // The machine's toolchains (they live under the real home, which the sandbox hides), as
    // the user's own layout shows them: tool directories read-only, the cargo registry cache
    // read-write.
    let host = std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_default();
    let var = |k: &str, d: &str| {
        std::env::var(k)
            .map(std::path::PathBuf::from)
            .unwrap_or(host.join(d))
    };
    let (cargo, rustup) = (var("CARGO_HOME", ".cargo"), var("RUSTUP_HOME", ".rustup"));
    let mut read_extra = vec![cargo.join("bin"), rustup.clone()];
    read_extra.extend(
        ["cargo", "gh", "python3"]
            .iter()
            .filter_map(|t| {
                tool_dir(t).and_then(|d| d.join(t).canonicalize().ok()?.parent().map(Path::to_path_buf))
            })
            .filter(|d| !d.starts_with("/usr") && !d.starts_with("/bin")),
    );
    let mut toolchain_env = vec![
        ("CARGO_HOME".to_string(), cargo.display().to_string()),
        ("RUSTUP_HOME".to_string(), rustup.display().to_string()),
    ];
    // CI selects the toolchain with RUSTUP_TOOLCHAIN and has no rustup default.
    if let Ok(t) = std::env::var("RUSTUP_TOOLCHAIN") {
        toolchain_env.push(("RUSTUP_TOOLCHAIN".to_string(), t));
    }
    let l = Layout::build(&LayoutInputs {
        home: home.clone(),
        agent_dir: Some(env.agent_dir()),
        path_var: std::env::var("PATH").unwrap_or_default(),
        env: toolchain_env,
        read_extra,
        write_extra: vec![cargo.join("registry")],
        ..LayoutInputs::default()
    });
    let sc = Scratch::create(&scratch_root(&home)).expect("scratch");
    let dir = sc.path().to_path_buf();
    let l = Layout {
        mask_runtime_ipc: true,
        ..l.with_scratch(&home, sc)
    };
    (l, dir)
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
    let (l, dir) = session(&env);
    let Some(sb) = sandbox_with_layout(&env, l) else {
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
    let (l, _dir) = session(&env);
    let Some(sb) = sandbox_with_layout(&env, l) else {
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
    if on_path("python3") {
        let (exit, out) = run(&sb, &env.work(), "python3 -m pip cache dir 2>&1 || python3 -c 'import os;print(os.environ[\"XDG_CACHE_HOME\"]+\"/pip\")'", true, false);
        assert_ok(exit, &out, "pip");
        assert!(out.contains("/tmp/.cache/pip"), "{out}");
    }
    if on_path("gh") {
        let (exit, out) = run(&sb, &env.work(), "gh --version", true, false);
        assert_ok(exit, &out, "gh");
    }
}

/// cargo builds a project in `/tmp` in one call and runs it in the next.
#[test]
fn cargo_builds_in_tmp_across_calls() {
    if tool_dir("cargo").is_none() {
        eprintln!("escape SKIPPED: cargo is not on PATH");
        return;
    }
    let env = TestEnv::new();
    let (l, _dir) = session(&env);
    let Some(sb) = sandbox_with_layout(&env, l) else {
        return;
    };
    let build = "mkdir -p /tmp/p/src && printf '[package]\\nname=\"p\"\\nversion=\"0.1.0\"\\nedition=\"2021\"\\n' > /tmp/p/Cargo.toml && printf 'fn main(){println!(\"hi457\")}' > /tmp/p/src/main.rs && cd /tmp/p && cargo build --offline -q 2>&1";
    let (exit, out) = run(&sb, &env.work(), build, true, false);
    assert_ok(exit, &out, "cargo build");
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
    let (l, _dir) = session(&env);
    let Some(sb) = sandbox_with_layout(&env, l) else {
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
