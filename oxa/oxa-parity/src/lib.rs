//! W0-FROZEN (architect-owned, implemented).
//!
//! # oxa-parity (test-only)
//! The PARITY suite: one test per `PARITY.md` row (or a recorded omission), written in wave G
//! from pi's docs by agents that did not write the code under test.
//!
//! Rules for G tasks
//! * Test name = the PARITY id lowercased with `_` (`CLI-012` -> `fn cli_012()`).
//! * P rows compare with pi's documented output or a fixture recorded from pi 1.5.2
//!   (`fixtures/pi/`); A rows test the adapted behaviour and name the invariant in a comment;
//!   O rows assert the `unsupported_in_oxa` error.
//! * Tests drive the real binaries ([`oxa_bin`], [`fake_bin`]) inside a `TestEnv`; nothing
//!   touches the real home. Build them first: `cargo build -p oxa-cli -p oxa-fake`.
//! * A test may be `#[ignore = "<PARITY id>: <reason>"]` only with the architect's
//!   reclassification recorded in PARITY.md.
//! * Wave FG checkpoint (architect): a row that is red or cannot be driven after wave G is
//!   kept with its assertions, either failing with a message that starts `RED <id>:` or
//!   `#[ignore]`d with a reason that says `RED`, `BLOCKED` or names the missing harness (pty).
//!   These are the H wave's input, not reclassifications: H fixes the product or the harness
//!   and removes the marker, or the architect records an omission in PARITY.md.
//! * Wave H: the pty harness exists ([`pty`]: the real binary in a real pseudo-terminal, a
//!   `vt100` screen model, both key encodings, terminal profiles that answer the binary's
//!   queries). "Needs a pty" is no longer a reason to ignore a row or to check it by grepping
//!   the source: a TUI/USE/keybinding/tree-navigator/session-picker row drives the screen. An
//!   `#[ignore]` now needs a real reason (a product bug with `RED <id>:`, or an omission the
//!   architect recorded in PARITY.md).
//! * Rows may also call the crates directly (`oxa-rpc`'s `RpcClient`, `oxa-mcp`, `oxa-session`,
//!   `oxa-tui`, `oxa-config`, `oxa-core`, `oxa-interactive`, `oxa-ext`, `oxa-tools`,
//!   `oxa-codemode` are dev-dependencies) when the behaviour is a library contract.
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
