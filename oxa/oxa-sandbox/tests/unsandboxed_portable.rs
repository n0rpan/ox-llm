//! Off Linux there is no OS sandbox: commands run unsandboxed and the result says so.
use oxa_proto::{CancelToken, ExecSpec};

fn spec(env: &oxa_fake::TestEnv, command: String) -> ExecSpec {
    ExecSpec {
        command,
        cwd: env.root().to_path_buf(),
        writable: true,
        net: false,
        timeout_s: Some(30),
        env: Vec::new(),
    }
}

#[test]
fn unsandboxed_flag_is_false_on_linux_and_true_elsewhere() {
    let env = oxa_fake::TestEnv::new();
    let r = oxa_sandbox::run_unsandboxed(&spec(&env, "true".into()), &CancelToken::new(), &mut |_| {});
    assert_eq!(r.exit, Some(0));
    assert_eq!(r.unsandboxed, !cfg!(target_os = "linux"));
}

#[test]
fn unsandboxed_run_sets_the_flag_and_runs_the_command() {
    let env = oxa_fake::TestEnv::new();
    let marker = env.root().join("ran");
    let s = spec(&env, format!("echo hi && touch '{}'", marker.display()));
    use oxa_proto::Sandbox;
    let sb = oxa_sandbox::Unsandboxed::default();
    assert!(sb.check().is_ok());
    let cancel = CancelToken::new();
    let r = sb.run(&s, &cancel, &mut |_| {});
    assert_eq!(r.exit, Some(0), "{}", String::from_utf8_lossy(&r.output));
    assert!(r.unsandboxed);
    assert!(String::from_utf8_lossy(&r.output).contains("hi"));
    assert!(marker.exists());
    let r = sb.run_streaming(&s, &cancel, &mut |_| {});
    assert!(r.unsandboxed && r.output.is_empty());
    let r = sb.run_with_stdin(&spec(&env, "cat".into()), b"piped", &cancel, &mut |_| {});
    assert!(r.unsandboxed);
    assert_eq!(String::from_utf8_lossy(&r.output), "piped");
}

#[test]
fn portable_hook_stdin_masks_known_values_in_capture_and_live_output() {
    use oxa_proto::Sandbox;
    use oxa_sandbox::{NetSandbox, Unsandboxed};
    use std::sync::Arc;

    let env = oxa_fake::TestEnv::new();
    let sb = NetSandbox::new(Arc::new(Unsandboxed::default()), false, false)
        .with_host_secrets_lazy(|| vec!["known-worker-value".into()]);
    let mut s = spec(&env, "cat".into());
    s.env.push(("API_KEY".into(), "hook-fixture-value".into()));
    let mut live = Vec::new();
    let r = sb.run_with_stdin(
        &s,
        b"before known-worker-value hook-fixture-value after",
        &CancelToken::new(),
        &mut |bytes| live.extend_from_slice(bytes),
    );
    assert_eq!(r.exit, Some(0));
    assert_eq!(live, b"before [redacted] [redacted] after");
    assert_eq!(r.output, live);
}

#[test]
#[cfg(not(target_os = "linux"))]
fn unsandboxed_notice_never_claims_a_sandbox() {
    use oxa_proto::Sandbox;
    let notice = oxa_sandbox::Unsandboxed::default()
        .persistence_notice()
        .unwrap()
        .to_lowercase();
    assert!(notice.contains("no os sandbox"), "{notice}");
    assert!(notice.contains("full user rights"), "{notice}");
}

#[test]
#[cfg(not(target_os = "linux"))]
fn bwrap_never_launches_off_linux() {
    use oxa_proto::Sandbox;
    use oxa_sandbox::{Bwrap, Layout};
    let env = oxa_fake::TestEnv::new();
    assert!(Bwrap::detect("/bin:/usr/bin", &env.home()).is_err());
    let sandbox = Bwrap {
        bwrap: "/bin/sh".into(),
        home: env.home(),
        layout: Layout::default(),
    };
    let marker = env.root().join("must-not-exist");
    let s = spec(&env, format!("touch '{}'", marker.display()));
    let r = sandbox.run(&s, &CancelToken::new(), &mut |_| panic!("command output"));
    assert_eq!(r.exit, None);
    assert!(!r.unsandboxed);
    assert!(!marker.exists());
}

#[test]
#[cfg(not(target_os = "linux"))]
fn die_with_parent_is_a_noop_off_linux() {
    let mut command = std::process::Command::new("true");
    oxa_sandbox::die_with_parent(&mut command);
    assert!(command.status().unwrap().success());
}

/// The model's bash gets bwrap's cleared base environment, not the process's: a credential
/// variable of the process is unreadable, and the variables are the ones bwrap sets.
#[test]
fn unsandboxed_env_matches_bwrap_base_env_and_hides_credentials() {
    use oxa_proto::Sandbox;
    use oxa_sandbox::{bwrap_args_with, Layout, Unsandboxed};
    std::env::set_var("OXA_TEST_UNSANDBOXED_API_KEY", "sk-must-not-leak");
    let env = oxa_fake::TestEnv::new();
    let layout = Layout {
        env: vec![("CARGO_HOME".into(), "/opt/cargo".into())],
        ..Layout::default()
    };
    let home = env.home();
    let sb = Unsandboxed {
        layout: layout.clone(),
        home: home.clone(),
    };
    let mut s = spec(&env, "env".into());
    s.env = vec![("OXA_TEST_SPEC_VAR".into(), "kept".into())];
    let r = sb.run(&s, &CancelToken::new(), &mut |_| {});
    let out = String::from_utf8_lossy(&r.output).into_owned();
    assert_eq!(r.exit, Some(0), "{out}");
    assert!(
        !out.contains("sk-must-not-leak") && !out.contains("OXA_TEST_UNSANDBOXED_API_KEY"),
        "{out}"
    );
    let mut got: Vec<(String, String)> = out
        .lines()
        .filter_map(|l| l.split_once('='))
        .filter(|(k, _)| !matches!(*k, "PWD" | "SHLVL" | "_" | "OLDPWD"))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    got.sort();
    let argv = bwrap_args_with(&s, &home, &layout);
    let mut want: Vec<(String, String)> = argv
        .windows(3)
        .filter(|w| w[0] == "--setenv")
        .map(|w| (w[1].clone(), w[2].clone()))
        .collect();
    want.sort();
    assert_eq!(got, want);
}

/// Off Linux a refused bash network cannot be enforced: the sandbox says so, so the model's
/// environment note and the start notice do not claim "off".
#[test]
fn unsandboxed_does_not_enforce_the_network() {
    use oxa_proto::Sandbox;
    assert!(!oxa_sandbox::Unsandboxed::default().network_enforced());
    let ns = oxa_sandbox::NetSandbox::new(
        std::sync::Arc::new(oxa_sandbox::Unsandboxed::default()),
        false,
        true,
    );
    assert!(!ns.network() && !ns.network_enforced());
}

/// #2130: a command with the user's rights (no OS sandbox) gets the host credentials scrubbed
/// from its output at the one sink, in every run path, for a read-only spec too.
#[test]
fn host_credentials_are_scrubbed_on_every_unsandboxed_path() {
    use oxa_proto::Sandbox;
    let env = oxa_fake::TestEnv::new();
    let secret = "SYNTHETIC-HOST-CREDENTIAL-2130";
    let ns = oxa_sandbox::NetSandbox::new(
        std::sync::Arc::new(oxa_sandbox::Unsandboxed::default()),
        true,
        false,
    )
    .with_host_secrets_lazy(|| vec!["SYNTHETIC-HOST-CREDENTIAL-2130".to_string()]);
    let mut read_only = spec(&env, format!("printf '%s' {secret}"));
    read_only.writable = false;
    let cancel = CancelToken::new();
    let mut live = Vec::new();
    let r = ns.run(&read_only, &cancel, &mut |b| live.extend_from_slice(b));
    assert_eq!(String::from_utf8_lossy(&r.output), "[redacted]", "{r:?}");
    assert_eq!(String::from_utf8_lossy(&live), "[redacted]");
    live.clear();
    let r = ns.run_streaming(&read_only, &cancel, &mut |b| live.extend_from_slice(b));
    assert!(r.output.is_empty());
    assert_eq!(String::from_utf8_lossy(&live), "[redacted]");
    let mut cat = spec(&env, "cat".into());
    cat.writable = false;
    let r = ns.run_with_stdin(&cat, secret.as_bytes(), &cancel, &mut |_| {});
    assert_eq!(String::from_utf8_lossy(&r.output), "[redacted]");
}
