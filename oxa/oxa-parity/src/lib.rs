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
    // Cargo does not rebuild another package's executables for these tests. Refresh
    // even existing siblings so a bare test run cannot silently exercise stale code.
    build_binaries(&p);
    p.join(name)
}

/// Refresh default `oxa` and `oxa-fake` siblings once per test process.
/// Explicit `OXA_BIN` / `OXA_FAKE_BIN` overrides bypass this build (e.g. CI archives).
fn build_binaries(profile_dir: &std::path::Path) {
    use std::sync::Once;
    static BUILD: Once = Once::new();
    BUILD.call_once(|| {
        let mut cmd = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
        cmd.args(["build", "-p", "oxa-cli", "-p", "oxa-fake"]);
        if profile_dir.file_name().is_some_and(|n| n == "release") {
            cmd.arg("--release");
        }
        let ok = cmd.status().map(|s| s.success()).unwrap_or(false);
        assert!(ok, "building oxa and oxa-fake failed");
    });
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn existing_sibling_binaries_are_refreshed() {
        const CHILD: &str = "OXA_PARITY_REFRESH_TEST";
        if std::env::var_os(CHILD).is_some() {
            assert_eq!(std::fs::read_to_string(oxa_bin()).unwrap(), "fresh oxa");
            assert_eq!(std::fs::read_to_string(fake_bin()).unwrap(), "fresh fake");
            return;
        }

        let env = TestEnv::new();
        let profile = env.root().join("debug");
        let deps = profile.join("deps");
        std::fs::create_dir_all(&deps).unwrap();
        // Exercise discovery from a test executable next to existing, stale binaries.
        let test_bin = deps.join("refresh-test");
        std::fs::copy(std::env::current_exe().unwrap(), &test_bin).unwrap();
        std::fs::write(profile.join("oxa"), "stale oxa").unwrap();
        std::fs::write(profile.join("oxa-fake"), "stale fake").unwrap();
        let cargo = env.write(
            "cargo",
            "#!/bin/sh\nset -eu\n[ \"$*\" = 'build -p oxa-cli -p oxa-fake' ]\nprintf 'build\\n' >> \"$REFRESH_PROFILE/builds\"\nprintf 'fresh oxa' > \"$REFRESH_PROFILE/oxa\"\nprintf 'fresh fake' > \"$REFRESH_PROFILE/oxa-fake\"\n",
        );
        std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o755)).unwrap();
        let output = Command::new(test_bin)
            .args([
                "--exact",
                "tests::existing_sibling_binaries_are_refreshed",
                "--nocapture",
            ])
            .env_clear()
            .envs(env.envs("http://127.0.0.1:1"))
            .env(CHILD, "1")
            .env("CARGO", cargo)
            .env("REFRESH_PROFILE", &profile)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "child failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(profile.join("builds")).unwrap(),
            "build\n",
            "both sibling binaries share one build"
        );
    }
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
