//! #202: full access (`yolo`). The bash tool's command runs as plain `bash -c` with the user's
//! own rights: no bwrap in its argv, a file outside the workspace is readable and a file in a
//! temporary directory outside it is written for real. `auto` and plan mode keep the sandbox.

use oxa_fake::TestEnv;
use oxa_proto::{Autonomy, CancelToken, ExecSpec, Sandbox, YoloSwitch};
use oxa_sandbox::{FullAccess, NoSandbox};
use std::sync::Arc;

fn spec(env: &TestEnv, command: String, writable: bool) -> ExecSpec {
    ExecSpec {
        command,
        cwd: env.work(),
        writable,
        net: false,
        timeout_s: Some(30),
        env: Vec::new(),
    }
}

fn full(level: Autonomy, env: &TestEnv) -> (FullAccess, YoloSwitch) {
    let switch = YoloSwitch::new(level);
    // The wrapped sandbox refuses everything: whatever runs did not go through it.
    let sb = FullAccess::new(Arc::new(NoSandbox), switch.clone(), "", env.home(), false);
    (sb, switch)
}

#[test]
fn yolo_runs_unsandboxed_outside_the_workspace() {
    let env = TestEnv::new();
    std::fs::create_dir_all(env.work()).unwrap();
    let outside = env.write("elsewhere/secret-notes.txt", "outside-the-workspace\n");
    let out_dir = tempdir_outside(&env);
    let target = out_dir.join("written.txt");
    let (sb, _switch) = full(Autonomy::Yolo, &env);
    let s = spec(
        &env,
        format!(
            "cat '{}' && echo made > '{}'",
            outside.display(),
            target.display()
        ),
        true,
    );
    assert!(sb.check().is_ok(), "full access needs no bwrap");
    let argv = sb.launch_argv(&s).unwrap();
    assert_eq!(argv[..2], ["bash".to_string(), "-c".to_string()]);
    assert!(!argv.iter().any(|a| a.contains("bwrap")), "{argv:?}");
    assert!(sb.bwrap_args(&s).is_empty());
    let r = sb.run(&s, &CancelToken::new(), &mut |_| {});
    assert_eq!(r.exit, Some(0), "{}", String::from_utf8_lossy(&r.output));
    assert_eq!(String::from_utf8_lossy(&r.output), "outside-the-workspace\n");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "made\n");
    let _ = std::fs::remove_dir_all(&out_dir);
}

#[test]
fn auto_and_plan_keep_the_sandbox() {
    let env = TestEnv::new();
    let target = env.root().join("never.txt");
    for (level, writable) in [
        (Autonomy::Auto, true),
        (Autonomy::Standard, true),
        (Autonomy::Yolo, false),
    ] {
        let (sb, _switch) = full(level, &env);
        let s = spec(&env, format!("echo x > '{}'", target.display()), writable);
        if level != Autonomy::Yolo {
            assert!(sb.check().is_err(), "{level:?}: the sandbox decides");
        }
        assert_eq!(sb.launch_argv(&s).unwrap()[0], "bwrap");
        let r = sb.run(&s, &CancelToken::new(), &mut |_| {});
        assert_eq!(r.exit, None);
        assert!(
            !target.exists(),
            "{level:?} writable={writable} ran outside the sandbox"
        );
    }
}

#[test]
fn switching_takes_effect_at_the_next_command() {
    let env = TestEnv::new();
    std::fs::create_dir_all(env.work()).unwrap();
    let (sb, switch) = full(Autonomy::Auto, &env);
    let s = spec(&env, "echo hi".to_string(), true);
    assert_eq!(sb.run(&s, &CancelToken::new(), &mut |_| {}).exit, None);
    switch.set_level(Autonomy::Yolo);
    assert_eq!(sb.run(&s, &CancelToken::new(), &mut |_| {}).exit, Some(0));
}

/// A fresh directory under the system temp dir, outside the test's workspace.
fn tempdir_outside(env: &TestEnv) -> std::path::PathBuf {
    let base = std::env::temp_dir();
    let name = format!(
        "oxa-full-access-{}-{}",
        std::process::id(),
        env.root().display().to_string().len()
    );
    let dir = base.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    assert!(!dir.starts_with(env.work()));
    dir
}
