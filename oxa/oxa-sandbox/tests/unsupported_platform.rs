//! Until a native backend exists, unsupported platforms must refuse even a fake bwrap.
#![cfg(not(target_os = "linux"))]

use oxa_proto::{CancelToken, ExecSpec, Sandbox};
use oxa_sandbox::{Bwrap, Layout};

#[test]
fn sandbox_refuses_without_launching_or_emitting_command_output() {
    let env = oxa_fake::TestEnv::new();
    let sandbox = Bwrap {
        bwrap: "/bin/sh".into(),
        home: env.home(),
        layout: Layout::default(),
    };
    assert!(Bwrap::detect("/bin:/usr/bin", &env.home())
        .unwrap_err()
        .to_string()
        .contains("only on Linux"));
    assert!(sandbox.check().unwrap_err().to_string().contains("only on Linux"));
    let marker = env.root().join("must-not-exist");
    let spec = ExecSpec {
        command: format!("touch '{}'", marker.display()),
        cwd: env.root().to_path_buf(),
        writable: true,
        net: true,
        timeout_s: Some(1),
        env: Vec::new(),
    };
    let cancel = CancelToken::new();
    let result = sandbox.run(&spec, &cancel, &mut |_| panic!("command output"));
    assert_eq!(result.exit, None);
    assert!(String::from_utf8_lossy(&result.output).contains("only on Linux"));
    let result = sandbox.run_with_stdin(&spec, b"input", &cancel, &mut |_| panic!("command output"));
    assert_eq!(result.exit, None);
    let mut live = Vec::new();
    let result = sandbox.run_streaming(&spec, &cancel, &mut |b| live.extend_from_slice(b));
    assert_eq!(result.exit, None);
    assert!(String::from_utf8_lossy(&live).contains("only on Linux"));
    assert!(!marker.exists());
    assert!(
        oxa_sandbox::offline_argv("/bin:/usr/bin", &env.home(), env.root(), &[], &["true".into()]).is_err()
    );
}

#[test]
fn parent_death_supervision_refuses_before_exec() {
    let mut command = std::process::Command::new("true");
    oxa_sandbox::die_with_parent(&mut command);
    assert!(command.spawn().is_err());
}
