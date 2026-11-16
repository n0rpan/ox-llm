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
use std::path::Path;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;

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
    let bin = p.join(name);
    if !bin.exists() {
        build_binaries(&p);
    }
    bin
}

/// Build `oxa` and `oxa-fake` once when a bare `cargo test -p oxa-parity` finds them missing.
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

/// The `oxa` binary: `$OXA_BIN`, or a snapshot of `target/<profile>/oxa` (built first when out
/// of date).
pub fn oxa_bin() -> PathBuf {
    fresh_bins().0.clone()
}

/// The `oxa-fake` binary: `$OXA_FAKE_BIN`, or a snapshot of `target/<profile>/oxa-fake` (built
/// first when out of date).
pub fn fake_bin() -> PathBuf {
    fresh_bins().1.clone()
}

/// A private copy of `bin` in `dir` (a hard link, else a copy): the inode this process built.
/// A target directory shared with another checkout gets its binaries replaced by that
/// checkout's builds while this process still runs tests; cargo unlinks and relinks the file,
/// so the snapshot keeps testing this checkout's code.
fn snapshot(bin: &Path, dir: &Path) -> PathBuf {
    let _ = std::fs::create_dir_all(dir);
    let name = bin.file_name().expect("binary file name");
    let out = dir.join(name);
    let _ = std::fs::remove_file(&out);
    if std::fs::hard_link(bin, &out).is_err() {
        std::fs::copy(bin, &out).unwrap_or_else(|e| panic!("snapshot {}: {e}", bin.display()));
    }
    out
}

/// Remove the snapshots of test processes that have ended.
fn prune_snapshots(parent: &Path) {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for e in entries.flatten() {
        let pid = e.file_name().to_string_lossy().parse::<u32>().ok();
        if pid.is_some_and(|p| p != std::process::id() && !Path::new(&format!("/proc/{p}")).exists()) {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

fn fresh_bins() -> &'static (PathBuf, PathBuf) {
    static BINS: OnceLock<(PathBuf, PathBuf)> = OnceLock::new();
    BINS.get_or_init(|| {
        ensure_fresh_bins();
        let oxa = sibling("oxa", "OXA_BIN");
        let fake = sibling("oxa-fake", "OXA_FAKE_BIN");
        if std::env::var_os("OXA_BIN").is_some() || std::env::var_os("OXA_FAKE_BIN").is_some() {
            return (oxa, fake);
        }
        let parent = oxa.with_file_name("parity-bin");
        prune_snapshots(&parent);
        let dir = parent.join(std::process::id().to_string());
        (snapshot(&oxa, &dir), snapshot(&fake, &dir))
    })
}

/// Bring `oxa` and `oxa-fake` up to date once per test process, unless `$OXA_BIN` or
/// `$OXA_FAKE_BIN` points at a prebuilt binary (CI archives set their own). A `cargo test -p
/// oxa-parity` does not build the binaries of other packages, so a leftover binary would test
/// old code. Cargo decides what is stale: its fingerprints also catch a binary that is newer
/// than the sources but was built from another checkout sharing the target directory, which a
/// modification-time comparison takes for fresh. When nothing changed the build is a no-op.
fn ensure_fresh_bins() {
    {
        if std::env::var_os("OXA_BIN").is_some() || std::env::var_os("OXA_FAKE_BIN").is_some() {
            return;
        }
        let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        let oxa = sibling("oxa", "OXA_BIN");
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
            "the oxa / oxa-fake binaries could not be built: {status:?}; \
             run `cargo build -p oxa-cli -p oxa-fake`"
        );
    }
}

/// The environment every PARITY `oxa` child gets after `TestEnv::envs`: the per-prompt
/// environment note is off (`OXA_ENV_NOTE=0`). PARITY compares the user messages with pi's
/// bytes; the note has its own tests (`oxa-core/tests/env_note_cache.rs`).
pub fn note_off_env() -> Vec<(String, String)> {
    vec![("OXA_ENV_NOTE".to_string(), "0".to_string())]
}

/// Explicit private settings for HTTP-only behavior fixtures; native selectors are tested separately.
pub fn http_envs(env: &TestEnv, url: &str) -> Vec<(String, String)> {
    let path = env.agent_dir().join("settings.json");
    let mut settings: serde_json::Value = match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).expect("valid private HTTP fixture settings"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(error) => panic!("read private HTTP fixture settings: {error}"),
    };
    // Repeated launches must not rewrite an already configured fixture's settings.
    if settings
        .pointer("/oxa/engine/inProcessCloud")
        .and_then(serde_json::Value::as_bool)
        == Some(false)
        && settings
            .pointer("/oxa/engine/inProcessLocal")
            .and_then(serde_json::Value::as_bool)
            == Some(false)
    {
        return env.envs(url);
    }
    settings["oxa"]["engine"]["inProcessCloud"] = serde_json::json!(false);
    settings["oxa"]["engine"]["inProcessLocal"] = serde_json::json!(false);
    env.write("home/.oxa/agent/settings.json", &settings.to_string());
    env.envs(url)
}

/// Run `oxa <args>` in `env.work()` with only `env.envs(engine_url)` and [`note_off_env`] as
/// environment, feeding `stdin`, and return its output.
pub fn run_oxa(env: &TestEnv, engine_url: &str, args: &[&str], stdin: &str) -> Output {
    use std::io::Write;
    let mut child = Command::new(oxa_bin())
        .args(args)
        .current_dir(env.work())
        .env_clear()
        .envs(http_envs(env, engine_url))
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
mod snapshot_tests {
    use super::snapshot;

    /// A target directory shared with another checkout: its build replaces `target/<profile>/oxa`
    /// (cargo unlinks and relinks it) while this process still runs tests. The snapshot keeps
    /// the binary this process built.
    #[test]
    fn a_replaced_target_binary_does_not_change_the_snapshot() {
        let root = std::env::temp_dir().join(format!("oxa-parity-snap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let built = root.join("oxa");
        std::fs::write(&built, b"this checkout").unwrap();
        let snap = snapshot(&built, &root.join("snap"));
        let other = root.join("oxa.other");
        std::fs::write(&other, b"another checkout").unwrap();
        std::fs::rename(&other, &built).unwrap();
        assert_eq!(std::fs::read(&snap).unwrap(), b"this checkout");
        assert_ne!(snap, built);
        let _ = std::fs::remove_dir_all(&root);
    }
}
