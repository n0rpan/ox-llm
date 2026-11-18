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
#[cfg(not(target_os = "linux"))]
fn unsandboxed_run_sets_the_flag_and_runs_the_command() {
    let env = oxa_fake::TestEnv::new();
    let marker = env.root().join("ran");
    let s = spec(&env, format!("echo hi && touch '{}'", marker.display()));
    use oxa_proto::Sandbox;
    let sb = oxa_sandbox::Unsandboxed;
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
#[cfg(not(target_os = "linux"))]
fn unsandboxed_notice_never_claims_a_sandbox() {
    use oxa_proto::Sandbox;
    let notice = oxa_sandbox::Unsandboxed
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
