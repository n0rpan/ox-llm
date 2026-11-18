//! Isolate drop-and-reopen lock assertions from unrelated libtest forks.

/// Return after running this exact test in a fresh process, or continue in that child.
pub fn in_parent() -> bool {
    const CHILD: &str = "OXA_WORK_LOCK_TEST_CHILD";
    let thread = std::thread::current();
    let name = thread.name().expect("named libtest thread");
    if std::env::var(CHILD).as_deref() == Ok(name) {
        return false;
    }

    // CLOEXEC closes descriptors at exec, not at fork. A parallel test's
    // pre_exec child can retain an unrelated locked file description after its
    // last intended owner drops it. Keep the real drop/reacquire assertions,
    // but create their locks only after exec, with no other tests in this process.
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .env_clear()
        .env(CHILD, name)
        .args(["--exact", name, "--nocapture", "--test-threads=1"]);
    if let Some(directory) = std::env::var_os("TMPDIR") {
        command.env("TMPDIR", directory);
    }
    let output = command.output().expect("start isolated lock test");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed; 0 failed;"),
        "isolated {name}: {}\n{stdout}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    true
}
