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
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

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

/// The `oxa` binary: `$OXA_BIN` or `target/<profile>/oxa`.
pub fn oxa_bin() -> PathBuf {
    sibling("oxa", "OXA_BIN")
}

/// The `oxa-fake` binary: `$OXA_FAKE_BIN` or `target/<profile>/oxa-fake`.
pub fn fake_bin() -> PathBuf {
    sibling("oxa-fake", "OXA_FAKE_BIN")
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
