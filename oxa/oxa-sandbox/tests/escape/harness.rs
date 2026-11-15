//! Shared setup for the escape suite. Every test says its threat, attempt and expected
//! refusal in a doc comment; this module only finds bwrap, skips when it is unusable
//! (`OXA_REQUIRE_BWRAP=1` makes a skip a failure, as CI sets it) and runs one command.

use oxa_fake::TestEnv;
use oxa_proto::{CancelToken, ExecResult, ExecSpec, Sandbox};
use oxa_sandbox::{Bwrap, Layout};
use std::path::{Path, PathBuf};

/// The process PATH, where bwrap is looked up.
pub fn path_var() -> String {
    std::env::var("PATH").unwrap_or_default()
}

/// A5: with `OXA_REQUIRE_BWRAP=1` (CI sets it) an unusable bwrap is a test failure.
pub fn required() -> bool {
    std::env::var("OXA_REQUIRE_BWRAP").is_ok_and(|v| v == "1")
}

fn skip(text: &str) {
    assert!(!required(), "OXA_REQUIRE_BWRAP=1 but bwrap is not usable: {text}");
    eprintln!("escape SKIPPED: {text}");
}

/// A sandbox for `env` (home = the test home, cwd = the test work dir), or `None` with a
/// notice. `Bwrap::detect` also enables the host runtime-IPC masks.
pub fn sandbox(env: &TestEnv) -> Option<Bwrap> {
    sandbox_at(env, &env.work())
}

/// [`sandbox`] with an explicit session cwd (`git` binds are resolved for it).
pub fn sandbox_at(env: &TestEnv, cwd: &Path) -> Option<Bwrap> {
    match Bwrap::detect(&path_var(), &env.home()) {
        Ok(b) => Some(b.with_git_dirs(cwd)),
        Err(e) => {
            skip(&format!("bwrap is not usable: {}", e.message));
            None
        }
    }
}

/// [`sandbox`] with a layout built by the test (home workspace, toolchain binds, ..).
pub fn sandbox_with_layout(env: &TestEnv, layout: Layout) -> Option<Bwrap> {
    let mut b = sandbox_at(env, &env.work())?;
    b.layout = layout;
    Some(b.with_git_dirs(&env.work()))
}

/// One command in `cwd`; the sandbox's own refusal text and exit are returned unchanged.
pub fn run(sb: &Bwrap, cwd: &Path, command: &str, writable: bool, net: bool) -> (Option<i32>, String) {
    let spec = ExecSpec {
        command: command.to_string(),
        cwd: cwd.to_path_buf(),
        writable,
        net,
        timeout_s: Some(30),
        env: Vec::new(),
    };
    let r: ExecResult = sb.run(&spec, &CancelToken::new(), &mut |_| {});
    (r.exit, String::from_utf8_lossy(&r.output).into_owned())
}

/// A command that must fail (the expected refusal) and leave no marker in `out`.
pub fn assert_refused(exit: Option<i32>, out: &str, what: &str) {
    assert_ne!(exit, Some(0), "{what}: the command must be refused: {out}");
}

/// A command that must succeed (the control: the sandbox is not simply refusing everything).
pub fn assert_ok(exit: Option<i32>, out: &str, what: &str) {
    assert_eq!(exit, Some(0), "{what}: {out}");
}

pub fn write(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("mkdir parent");
    }
    std::fs::write(path, body).expect("write");
}

pub fn dir(path: &Path) {
    std::fs::create_dir_all(path).expect("mkdir");
}

/// `s` as one shell word (tests build commands with names the suite chooses).
pub fn sh(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// A path under the test home, for a test that needs the layout to know about it.
pub fn home_file(env: &TestEnv, rel: &str) -> PathBuf {
    env.home().join(rel)
}
