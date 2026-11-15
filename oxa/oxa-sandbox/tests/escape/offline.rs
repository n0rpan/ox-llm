//! Offline bash: the hard switch and `oxa.sandbox.net: off` both remove the network from
//! every command, whatever the tool asked for.

use crate::harness::{assert_refused, sandbox};
use oxa_fake::TestEnv;
use oxa_proto::{CancelToken, ExecSpec, Sandbox};
use oxa_sandbox::{bwrap_args, sandbox_net_setting, NetSandbox};
use std::sync::Arc;

/// Threat: `--offline` (or network off) still lets a command reach a host.
/// Attempt: a raw TCP connect to a reserved address and a DNS lookup.
/// Expected refusal: no network interface except loopback; the connect and the lookup fail.
#[test]
fn offline_bash_has_no_network() {
    let env = TestEnv::new();
    let Some(inner) = sandbox(&env) else { return };
    let sb = NetSandbox::new(Arc::new(inner), true, true);
    for cmd in [
        "exec 3<>/dev/tcp/192.0.2.1/9",
        "getent hosts example.com",
        "curl -sS --max-time 2 http://192.0.2.1/ >/dev/null 2>&1",
    ] {
        let spec = ExecSpec {
            command: cmd.to_string(),
            cwd: env.work(),
            writable: true,
            net: true,
            timeout_s: Some(10),
            env: Vec::new(),
        };
        let r = sb.run(&spec, &CancelToken::new(), &mut |_| {});
        let out = String::from_utf8_lossy(&r.output).into_owned();
        assert_refused(r.exit, &out, cmd);
    }
    // Control: only loopback exists in the network namespace.
    let spec = ExecSpec {
        command: "cat /proc/net/dev".into(),
        cwd: env.work(),
        writable: true,
        net: true,
        timeout_s: Some(10),
        env: Vec::new(),
    };
    let r = sb.run(&spec, &CancelToken::new(), &mut |_| {});
    assert_eq!(r.exit, Some(0));
    let text = String::from_utf8_lossy(&r.output);
    let ifaces: Vec<&str> = text
        .lines()
        .filter_map(|l| l.split_once(':').map(|(name, _)| name.trim()))
        .collect();
    assert!(!ifaces.is_empty());
    assert!(ifaces.iter().all(|i| *i == "lo"), "{ifaces:?}");
}

/// Threat: an extension (or a spec) asks for `net: true` while the network is off.
/// Attempt: inspect the argv `NetSandbox` would run.
/// Expected refusal: the spec is clamped; no argv carries `--share-net`.
#[test]
fn network_off_clamps_every_spec() {
    let env = TestEnv::new();
    let Some(inner) = sandbox(&env) else { return };
    let sb = NetSandbox::new(Arc::new(inner), sandbox_net_setting(Some("off")), false);
    let spec = ExecSpec {
        command: "true".into(),
        cwd: env.work(),
        writable: true,
        net: true,
        timeout_s: None,
        env: Vec::new(),
    };
    let args = sb.bwrap_args(&spec);
    assert!(!args.iter().any(|a| a == "--share-net"), "{args:?}");
    // The inner sandbox's argc for the same spec would share the network; this is the control.
    let inner_args = bwrap_args(&spec, &env.home());
    assert!(inner_args.iter().any(|a| a == "--share-net"));
}

/// Threat: an offline command reads `/run/systemd/resolve` and resolves names through the
/// host resolver (the socket is not a network socket).
/// Attempt: `resolvectl status`.
/// Expected refusal: the runtime IPC mask removes the resolver even under offline.
#[test]
fn offline_cannot_use_the_host_resolver() {
    let env = TestEnv::new();
    if !std::path::Path::new("/run/systemd/resolve/io.systemd.Resolve").exists() {
        eprintln!("escape SKIPPED: no systemd-resolved on this host");
        return;
    }
    let Some(inner) = sandbox(&env) else { return };
    let sb = NetSandbox::new(Arc::new(inner), true, true);
    let spec = ExecSpec {
        command: "resolvectl status".into(),
        cwd: env.work(),
        writable: true,
        net: true,
        timeout_s: Some(10),
        env: Vec::new(),
    };
    let r = sb.run(&spec, &CancelToken::new(), &mut |_| {});
    assert_refused(r.exit, &String::from_utf8_lossy(&r.output), "resolved offline");
}
