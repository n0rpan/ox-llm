//! V022 (#884): duplicate startup loads of the user settings file are gone.
//!
//! `prepare` used to parse `<agent_dir>/settings.json` three times with the same inputs when the
//! project was untrusted: once for the user-only view, once inside the trust decision and once
//! more for the settings it then used. Two of those were the same read; they are now one.
//!
//! The count is not a fixed number, so it is measured against the trusted run: the readers other
//! than those loads (resource discovery's package declarations, the execution hooks' `httpProxy`
//! lookup) run in both, so an untrusted run must never read the user settings *more* often than a
//! trusted one, and before the fix it did (the two duplicate loads are extra). The trusted run
//! also proves the trace sees the project's file, which the untrusted run only peeks at for its
//! `done` key (`Settings::project_declares_done`) and never merges.
//!
//! `strace` (as in `package_list.rs`), because the read count is the behaviour under test; strace
//! is mandatory and its absence fails rather than skips.
#[cfg(target_os = "linux")]
#[path = "common/strace.rs"]
mod strace;
use oxa_cli::{run, Stdio};
use oxa_config::{Dirs, Env};
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
use std::io::{Cursor, Write};
#[cfg(target_os = "linux")]
use std::path::Path;
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The environment of a traced child, for a fixture root laid out like [`TestEnv`]. Kept in step
/// with [`TestEnv::envs`] by [`traced_environment_matches_testenv`].
#[cfg(target_os = "linux")]
fn pairs(root: &Path, engine_url: &str) -> Vec<(String, String)> {
    let home = root.join("home");
    vec![
        ("PATH".to_string(), std::env::var("PATH").unwrap_or_default()),
        ("HOME".to_string(), home.to_string_lossy().into_owned()),
        (
            "OXA_AGENT_DIR".to_string(),
            home.join(".oxa/agent").to_string_lossy().into_owned(),
        ),
        (
            "XDG_CONFIG_HOME".to_string(),
            home.join(".config").to_string_lossy().into_owned(),
        ),
        (
            "XDG_CACHE_HOME".to_string(),
            home.join(".cache").to_string_lossy().into_owned(),
        ),
        (
            "XDG_DATA_HOME".to_string(),
            home.join(".local/share").to_string_lossy().into_owned(),
        ),
        (
            "TMPDIR".to_string(),
            root.join("tmp").to_string_lossy().into_owned(),
        ),
        ("OXA_ENGINE_URL".to_string(), engine_url.to_string()),
        ("OXA_SYSTEM_SKILLS_DIR".to_string(), String::new()),
        ("OXA_TEST_ISOLATION".to_string(), "1".to_string()),
        ("TERM".to_string(), "dumb".to_string()),
        ("LANG".to_string(), "C.UTF-8".to_string()),
    ]
}

#[cfg(target_os = "linux")]
#[test]
fn traced_environment_matches_testenv() {
    let env = TestEnv::new();
    assert_eq!(
        pairs(env.root(), "http://127.0.0.1:1"),
        env.envs("http://127.0.0.1:1"),
        "the traced child's environment must match TestEnv::envs"
    );
}

/// One print-mode run against the fake engine, in the traced subprocess only.
#[cfg(target_os = "linux")]
#[test]
fn settings_read_count_worker() {
    let Some(root) = std::env::var_os("OXA_SETTINGS_REUSE_FIXTURE") else {
        return;
    };
    let root = std::path::PathBuf::from(root);
    let server = FakeServer::start(vec![ScriptTurn::text("ok")], FakeOpts::default()).expect("fake engine");
    let owned = pairs(&root, &server.url());
    let refs: Vec<(&str, &str)> = owned.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let env = Env::from_pairs(&refs);
    let io = Stdio {
        stdin: Box::new(Cursor::new(Vec::new())),
        stdout: Box::new(Sink::default()),
        stderr: Box::new(Sink::default()),
        stdin_is_tty: false,
        stdout_is_tty: false,
        cwd: root.join("work"),
    };
    let code = run(&["-p".to_string(), "hi".to_string()], &env, io);
    assert_eq!(code, 0, "the traced run must succeed");
}

/// The opens of `path` in a trace, as the trace lines themselves (for failure messages).
#[cfg(target_os = "linux")]
fn opens<'a>(trace: &'a str, path: &Path) -> Vec<&'a str> {
    let needle = path.to_str().unwrap();
    trace.lines().filter(|l| l.contains(needle)).collect()
}

/// Run one traced child over `env` and return its trace.
#[cfg(target_os = "linux")]
fn trace_run(env: &TestEnv) -> String {
    use std::process::Command;

    let trace = env.root().join("settings-reads.trace");
    let output = Command::new(strace::executable())
        .args(["-f", "-e", "trace=open,openat,openat2", "-o"])
        .arg(&trace)
        .arg(std::env::current_exe().unwrap())
        .args(["--exact", "settings_read_count_worker", "--nocapture"])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("OXA_SETTINGS_REUSE_FIXTURE", env.root())
        .current_dir(env.work())
        .output()
        .expect("strace is required for the startup settings read count");
    assert!(
        output.status.success(),
        "the traced run failed: {output:?}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    std::fs::read_to_string(&trace).unwrap()
}

/// The user settings and one trust-gated project resource (so the project is untrusted unless a
/// decision says otherwise).
#[cfg(target_os = "linux")]
fn fixture(env: &TestEnv) {
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"defaultProjectTrust":"ask"}"#,
    );
    env.write("work/.oxa/settings.json", r#"{"packages":[]}"#);
}

#[cfg(target_os = "linux")]
#[test]
fn untrusted_startup_reads_user_settings_no_more_than_a_trusted_one() {
    use oxa_perm::TrustStore;

    let untrusted = TestEnv::new();
    fixture(&untrusted);
    let trusted = TestEnv::new();
    fixture(&trusted);
    let dirs = Dirs::under(trusted.root());
    let mut store = TrustStore::load(&dirs.trust_file());
    store.save(&dirs.trust_file(), &trusted.work(), true).unwrap();
    store.approve(&dirs.trust_file(), &trusted.work()).unwrap();
    assert!(store.trusted_as_approved(&trusted.work()));

    let without = trace_run(&untrusted);
    let with = trace_run(&trusted);
    let dirs_without = Dirs::under(untrusted.root());
    let (user_without, user_with) = (
        opens(&without, &dirs_without.settings_file()),
        opens(&with, &dirs.settings_file()),
    );
    assert!(
        !user_without.is_empty(),
        "the untrusted run must read the user settings:\n{without}"
    );
    assert!(
        user_without.len() <= user_with.len(),
        "an untrusted run must not read the user settings more often than a trusted one \
         ({} vs {}): the duplicate startup loads are back\nuntrusted:\n{}\ntrusted:\n{}",
        user_without.len(),
        user_with.len(),
        user_without.join("\n"),
        user_with.join("\n")
    );
    // The trace sees the project's file: the trusted run merges it. The untrusted run is not
    // asserted to leave it alone — `Settings::project_declares_done` reads it whatever the trust,
    // to say that the checks were skipped — so a read count cannot express that boundary; the
    // merge itself is what trust gates, and `h_settings_accessors` covers it.
    let project_with = opens(&with, &dirs.project_settings_file(&trusted.work()));
    assert!(
        !project_with.is_empty(),
        "a trusted project's settings file is read:\n{with}"
    );
}
