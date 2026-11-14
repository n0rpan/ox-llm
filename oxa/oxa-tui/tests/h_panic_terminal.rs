//! Wave H lane `harden` (pre-flight A1/A2): the release binary unwinds, so a panic that a
//! boundary contains must leave the terminal as it is, and so must one on a thread other than
//! the one that drives the terminal. A panic of the terminal's own thread still restores it.
use oxa_tui::{install_panic_hook, restore_sequence, restores_on_panic};

const MARK: &str = "H-PANIC-MARK";

/// The child: hook installed on this thread, then a contained panic and a panic on a helper
/// thread (neither may restore), the marker, then a panic of this thread (must restore).
fn child() -> ! {
    oxa_proto::panic::install_hook();
    install_panic_hook();
    assert!(restores_on_panic());
    let r = oxa_proto::panic::contain(|| -> () { panic!("contained") });
    assert_eq!(r, Err("contained".to_string()));
    let helper = std::thread::spawn(|| {
        assert!(!restores_on_panic());
        panic!("helper thread");
    });
    assert!(helper.join().is_err());
    use std::io::Write as _;
    let mut out = std::io::stdout();
    let _ = write!(out, "{MARK}");
    let _ = out.flush();
    panic!("owner thread");
}

#[test]
fn contained_and_foreign_panics_leave_the_terminal_alone() {
    if std::env::var_os("OXA_H_PANIC_CHILD").is_some() {
        child();
    }
    let exe = std::env::current_exe().expect("test binary path");
    let out = std::process::Command::new(exe)
        .args([
            "--exact",
            "contained_and_foreign_panics_leave_the_terminal_alone",
            "--test-threads=1",
            "--nocapture",
        ])
        .env("OXA_H_PANIC_CHILD", "1")
        .output()
        .expect("run the child test");
    assert!(!out.status.success(), "the child was meant to panic at the end");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let at = stdout.find(MARK).expect("the child reached the marker");
    assert!(
        !stdout[..at].contains(restore_sequence()),
        "a contained / helper-thread panic restored the terminal: {stdout:?}"
    );
    assert!(
        stdout[at..].contains(restore_sequence()),
        "the owner thread's panic did not restore the terminal: {stdout:?}"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.lines().any(|l| l.trim() == "contained"),
        "a contained panic printed its message: {stderr}"
    );
}
