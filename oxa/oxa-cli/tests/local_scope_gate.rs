//! The internal child cannot exec the engine until the parent releases its scope gate.
#![cfg(target_os = "linux")]
use std::{
    io::{Read, Write},
    process::{Command, Stdio},
};
#[test]
fn scope_gate_waits_for_permission_and_eof_never_executes() {
    for allow in [false, true] {
        let binary = env!("CARGO_BIN_EXE_oxa");
        let mut child = Command::new(binary)
            .args([oxc_local::platform::CHILD_GATE, binary, "--version"])
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut ready = [0];
        child.stdout.as_mut().unwrap().read_exact(&mut ready).unwrap();
        assert_eq!(ready, [0x6b]);
        assert!(child.try_wait().unwrap().is_none());
        if allow {
            child.stdin.as_mut().unwrap().write_all(&[0xa7]).unwrap();
        }
        drop(child.stdin.take());
        assert_eq!(child.wait().unwrap().success(), allow);
    }
}
