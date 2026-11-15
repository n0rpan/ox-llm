//! Host runtime IPC: Unix sockets under `/run` are not network sockets, so the network
//! namespace does not contain them. A command must not reach the user session bus, the
//! container control socket or the host resolver.

use crate::harness::{assert_refused, run, sandbox};
use oxa_fake::TestEnv;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

fn uid(env: &TestEnv) -> u32 {
    std::fs::metadata(env.home()).map(|m| m.uid()).unwrap_or(0)
}

fn skip(path: &str) {
    eprintln!("escape SKIPPED: {path} is not present on this host");
}

/// Threat: a sandboxed command talks to the user's session D-Bus, which can start units and
/// processes outside the sandbox.
/// Attempt: `busctl --user list` with the explicit socket address.
/// Expected refusal: the socket is masked, so the bus call fails.
#[test]
fn the_user_session_bus_is_masked() {
    let env = TestEnv::new();
    let bus = format!("/run/user/{}/bus", uid(&env));
    if !Path::new(&bus).exists() {
        skip(&bus);
        return;
    }
    let Some(sb) = sandbox(&env) else { return };
    let cmd = format!("DBUS_SESSION_BUS_ADDRESS=unix:path={bus} busctl --user --no-pager list");
    let (exit, out) = run(&sb, &env.work(), &cmd, true, false);
    assert_refused(exit, &out, "the user session bus");
    assert!(
        !out.contains("org.freedesktop.systemd1"),
        "the user bus answered: {out}"
    );
}

/// Threat: a sandboxed command asks the host's systemd-resolved to resolve a name, so DNS
/// still leaves the machine with `--offline` and `oxa.sandbox.net: off`.
/// Attempt: `resolvectl status` (a varlink call to the host resolver, no external DNS).
/// Expected refusal: `/run/systemd/resolve` is masked, so the call fails.
#[test]
fn the_host_resolver_is_masked() {
    let env = TestEnv::new();
    let socket = "/run/systemd/resolve/io.systemd.Resolve";
    if !Path::new(socket).exists() {
        skip(socket);
        return;
    }
    let Some(sb) = sandbox(&env) else { return };
    let (exit, out) = run(&sb, &env.work(), &format!("test ! -S {socket}"), true, true);
    crate::harness::assert_ok(exit, &out, "resolver IPC stays masked online");
    let (exit, out) = run(&sb, &env.work(), "resolvectl status", true, false);
    assert_refused(exit, &out, "systemd-resolved");
    assert!(
        !out.contains("resolv.conf mode"),
        "the host resolver answered: {out}"
    );
}

/// Threat: a sandboxed command uses the Docker/Podman control socket to start a container
/// with the user's rights (outside the sandbox).
/// Attempt: test the socket and make one harmless API request if it is present.
/// Expected refusal: the socket is masked.
#[test]
fn container_control_sockets_are_masked() {
    let env = TestEnv::new();
    let sockets: Vec<&str> = ["/run/docker.sock", "/run/podman/podman.sock"]
        .into_iter()
        .filter(|p| Path::new(p).exists())
        .collect();
    if sockets.is_empty() {
        skip("a container control socket");
        return;
    }
    let Some(sb) = sandbox(&env) else { return };
    for socket in sockets {
        let cmd = format!("test -S {socket}");
        let (exit, out) = run(&sb, &env.work(), &cmd, true, false);
        assert_refused(exit, &out, socket);
    }
}

/// Online commands may read DNS configuration without reaching resolver IPC or the network.
#[test]
fn online_commands_can_read_resolver_configuration() {
    let env = TestEnv::new();
    let expected = std::fs::read_to_string("/etc/resolv.conf").expect("host resolver configuration");
    let Some(sb) = sandbox(&env) else { return };
    let (exit, out) = run(&sb, &env.work(), "cat /etc/resolv.conf", true, true);
    crate::harness::assert_ok(exit, &out, "online resolver configuration");
    assert_eq!(out, expected);
}
