//! # oxa-parity (test-only)
//!
//! oxa's behavior regression suite, derived from pi. Supported contracts are indexed in
//! `PARITY.md`; P rows retain inherited behavior and A rows describe oxa adaptations.
//! Golden fixtures live in `fixtures/goldens/`. Retired IDs are never reused.
//!
//! Tests drive the real binaries inside an isolated `TestEnv`, or call crates directly
//! for library contracts. Build binaries first: `cargo build -p oxa-cli -p oxa-fake`.
//! Interactive tests use real pseudo-terminals and inspect the rendered screen. Sandbox,
//! permission, trust, offline, MCP and extension-hook behavior remain required contracts.
//! Run the whole workspace with oxa's defaults; no alternate profile is required.
#[cfg(target_os = "linux")]
pub mod pty;

use oxa_fake::TestEnv;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Once;
use std::time::SystemTime;

fn sibling(name: &str, var: &str) -> PathBuf {
    if let Ok(p) = std::env::var(var) {
        return PathBuf::from(p);
    }
    // target/<profile>/deps/<test exe> -> target/<profile>/<name>
    let mut p = std::env::current_exe().expect("current_exe");
    p.pop();
    if p.ends_with("deps") {
        p.pop();
    }
    p.join(name)
}

/// The `oxa` binary: `$OXA_BIN` or `target/<profile>/oxa` (rebuilt first when stale).
pub fn oxa_bin() -> PathBuf {
    ensure_fresh_bins();
    sibling("oxa", "OXA_BIN")
}

/// The `oxa-fake` binary: `$OXA_FAKE_BIN` or `target/<profile>/oxa-fake` (rebuilt first when
/// stale).
pub fn fake_bin() -> PathBuf {
    ensure_fresh_bins();
    sibling("oxa-fake", "OXA_FAKE_BIN")
}

/// The newest modification time of the `.rs` and `Cargo.toml` files below `dir` (skipping
/// `target` and hidden directories), and of `extra` files.
fn newest_source(dir: &Path, newest: &mut Option<SystemTime>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let name = e.file_name();
        let name = name.to_string_lossy();
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            if name != "target" && !name.starts_with('.') {
                newest_source(&p, newest);
            }
        } else if name.ends_with(".rs") || name == "Cargo.toml" || name == "Cargo.lock" {
            if let Ok(t) = e.metadata().and_then(|m| m.modified()) {
                if newest.is_none_or(|n| t > n) {
                    *newest = Some(t);
                }
            }
        }
    }
}

/// `true` when `bin` is missing or older than a Rust source or manifest of the workspace at
/// `workspace`: running it would test old code (a `cargo test -p oxa-parity` does not rebuild
/// the binaries of other packages).
pub fn bin_is_stale(bin: &Path, workspace: &Path) -> bool {
    let Ok(built) = std::fs::metadata(bin).and_then(|m| m.modified()) else {
        return true;
    };
    let mut newest = None;
    newest_source(workspace, &mut newest);
    newest.is_some_and(|n| n > built)
}

/// Rebuild `oxa` and `oxa-fake` once per test process when either is stale and neither
/// `$OXA_BIN` nor `$OXA_FAKE_BIN` points at a prebuilt binary (CI archives set their own).
fn ensure_fresh_bins() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        if std::env::var_os("OXA_BIN").is_some() || std::env::var_os("OXA_FAKE_BIN").is_some() {
            return;
        }
        let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        let oxa = sibling("oxa", "OXA_BIN");
        let fake = sibling("oxa-fake", "OXA_FAKE_BIN");
        if !bin_is_stale(&oxa, &workspace) && !bin_is_stale(&fake, &workspace) {
            return;
        }
        // target/<profile>/oxa: `debug` is cargo's `dev` profile.
        let profile = oxa
            .parent()
            .and_then(|d| d.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "debug".to_string());
        let profile = if profile == "debug" {
            "dev".to_string()
        } else {
            profile
        };
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let status = Command::new(cargo)
            .args(["build", "--profile", &profile, "-p", "oxa-cli", "-p", "oxa-fake"])
            .current_dir(&workspace)
            .status();
        assert!(
            matches!(status, Ok(s) if s.success()),
            "the oxa / oxa-fake binaries are stale and rebuilding them failed: {status:?}; \
             run `cargo build -p oxa-cli -p oxa-fake`"
        );
    });
}

/// The environment every PARITY `oxa` child gets after `TestEnv::envs`: the per-prompt
/// environment note is off (`OXA_ENV_NOTE=0`). PARITY compares the user messages with pi's
/// bytes; the note has its own tests (`oxa-core/tests/env_note_cache.rs`).
pub fn note_off_env() -> Vec<(String, String)> {
    vec![("OXA_ENV_NOTE".to_string(), "0".to_string())]
}

/// Run `oxa <args>` in `env.work()` with only `env.envs(engine_url)` and [`note_off_env`] as
/// environment, feeding `stdin`, and return its output.
pub fn run_oxa(env: &TestEnv, engine_url: &str, args: &[&str], stdin: &str) -> Output {
    use std::io::Write;
    let mut child = Command::new(oxa_bin())
        .args(args)
        .current_dir(env.work())
        .env_clear()
        .envs(env.envs(engine_url))
        .envs(crate::note_off_env())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn oxa (build it first: cargo build -p oxa-cli -p oxa-fake)");
    if let Some(mut si) = child.stdin.take() {
        let _ = si.write_all(stdin.as_bytes());
    }
    child.wait_with_output().expect("wait for oxa")
}

/// Path of a fixture below `oxa-parity/fixtures/`.
pub fn fixture(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(rel)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_or_older_binary_is_stale_newer_is_fresh() {
        let env = TestEnv::new();
        let ws = env.work();
        let src = env.write("work/src/lib.rs", "fn a() {}");
        let bin = env.root().join("bin");
        assert!(bin_is_stale(&bin, &ws), "missing binary");
        std::fs::write(&bin, "x").unwrap();
        let t = |s: u64| std::time::UNIX_EPOCH + std::time::Duration::from_secs(s);
        let set = |p: &Path, s: u64| {
            std::fs::File::options()
                .write(true)
                .open(p)
                .unwrap()
                .set_modified(t(s))
                .unwrap()
        };
        set(&bin, 1_000);
        set(&src, 2_000);
        assert!(bin_is_stale(&bin, &ws), "source newer than binary");
        set(&bin, 3_000);
        assert!(!bin_is_stale(&bin, &ws), "binary newer than every source");
        // Build output under `target` never counts as source.
        let out = env.write("work/target/debug/build.rs", "");
        set(&out, 4_000);
        assert!(!bin_is_stale(&bin, &ws));
    }
}
