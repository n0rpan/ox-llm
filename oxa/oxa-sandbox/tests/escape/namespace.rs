//! Mounts, capabilities and `/proc`: the command must not leave its namespace.

use crate::harness::{assert_refused, run, sandbox};
use oxa_fake::TestEnv;

/// Threat: a command mounts over a host path or creates a device node.
/// Attempt: `mount -t tmpfs` in `/tmp`, `mknod /dev/...`, and `test -w /sys`.
/// Expected refusal: `--cap-drop ALL` leaves no capability; the host mounts and devices are
/// unchanged.
#[test]
fn mounts_and_devices_are_not_available() {
    let env = TestEnv::new();
    let Some(sb) = sandbox(&env) else { return };
    let commands = [
        "mkdir -p /tmp/m && mount -t tmpfs tmpfs /tmp/m",
        "mknod /tmp/oxa-escape-dev b 8 0",
        "test -w /sys",
        "test -e /dev/mem",
        "test -e /dev/sda",
    ];
    for cmd in commands {
        let (exit, out) = run(&sb, &env.work(), cmd, true, false);
        assert_refused(exit, &out, cmd);
    }
    // Control: an ordinary file in the private /tmp is writable.
    let (exit, out) = run(&sb, &env.work(), "printf ok > /tmp/plain", true, false);
    assert_eq!(exit, Some(0), "{out}");
}

/// Threat: a command reads another process's `/proc/<pid>` or uses `/proc/self/root`.
/// Attempt: `test -e /proc/<host test pid>` and `cat /proc/<pid>/environ`.
/// Expected refusal: a new PID namespace and a private procfs expose only the sandbox.
#[test]
fn host_processes_are_not_in_proc() {
    let env = TestEnv::new();
    let Some(sb) = sandbox(&env) else { return };
    let host = std::process::id();
    let host_home = env.home();
    for cmd in [
        format!("test -e /proc/{host}"),
        format!("test -e /proc/{host}/environ"),
        format!("test -e /proc/1/root{}/.ssh", host_home.display()),
    ] {
        let (exit, out) = run(&sb, &env.work(), &cmd, true, false);
        assert_refused(exit, &out, &cmd);
    }
    // Control: the sandbox's own procfs is there.
    let (exit, out) = run(&sb, &env.work(), "test -e /proc/self/status", true, false);
    assert_eq!(exit, Some(0), "{out}");
    let (exit, out) = run(
        &sb,
        &env.work(),
        "grep -q '^CapEff:\\s*0*$' /proc/self/status",
        true,
        false,
    );
    assert_eq!(exit, Some(0), "no capabilities expected: {out}");
}

/// Threat: a command changes a host sysctl or kernel file through `/proc/sys` or `/sys`.
/// Attempt: write `/proc/sys/kernel/hostname` and `/proc/sys/kernel/core_pattern`.
/// Expected refusal: the procfs and sysfs are read-only mounts.
#[test]
fn kernel_tunables_are_read_only() {
    let env = TestEnv::new();
    let Some(sb) = sandbox(&env) else { return };
    for cmd in [
        "printf x > /proc/sys/kernel/hostname",
        "printf x > /proc/sys/kernel/core_pattern",
        "printf x > /sys/kernel/uevent_helper",
    ] {
        let (exit, out) = run(&sb, &env.work(), cmd, true, false);
        assert_refused(exit, &out, cmd);
    }
}
