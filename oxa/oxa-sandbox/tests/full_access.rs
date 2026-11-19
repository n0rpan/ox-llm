//! #202: full access (`yolo`). The bash tool's command runs as plain `bash -c` with the user's
//! own rights: no bwrap in its argv, a file outside the workspace is readable and a file in a
//! temporary directory outside it is written for real. `auto` and plan mode keep the sandbox.

use oxa_fake::TestEnv;
use oxa_proto::{Autonomy, CancelToken, ExecResult, ExecSpec, OxaError, Sandbox, YoloSwitch};
use oxa_sandbox::{FullAccess, Layout, NetSandbox, NoSandbox};
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
    let sb = FullAccess::new(Arc::new(NoSandbox), switch.clone(), "", env.home(), false, false);
    (sb, switch)
}

#[test]
fn attended_host_bash_uses_the_toolchain_environment_and_scrubs_output() {
    let env = TestEnv::new();
    let (sb, _) = full(Autonomy::Standard, &env);
    let cargo = env.home().join("cargo");
    let layout = Layout {
        path: vec!["/usr/bin".into(), "/bin".into()],
        env: vec![("CARGO_HOME".into(), cargo.display().to_string())],
        ..Layout::default()
    };
    let synthetic = "fixture-value-for-redaction";
    let net = NetSandbox::new(Arc::new(NoSandbox), true, false).with_gh_token(Some(synthetic.into()));
    let sb = sb
        .with_os_sandbox(false)
        .with_host_controls(layout, Arc::new(net));
    let mut spec = spec(
        &env,
        format!(
            "test -z \"${{GH_TOKEN+x}}\" && printf '%s|%s|%s|%s' \"$HOME\" \"$PATH\" \"$CARGO_HOME\" '{}'",
            synthetic,
        ),
        true,
    );
    spec.net = true;
    let expected = format!(
        "{}|/usr/bin:/bin|{}|[redacted]",
        env.home().display(),
        cargo.display()
    );
    let mut live = Vec::new();
    let result = sb.run(&spec, &CancelToken::new(), &mut |bytes| {
        live.extend_from_slice(bytes)
    });
    assert_eq!(
        result.exit,
        Some(0),
        "{}",
        String::from_utf8_lossy(&result.output)
    );
    assert_eq!(result.output, expected.as_bytes());
    assert_eq!(live, expected.as_bytes());

    live.clear();
    let result = sb.run_streaming(&spec, &CancelToken::new(), &mut |bytes| {
        live.extend_from_slice(bytes)
    });
    assert_eq!(result.exit, Some(0));
    assert!(result.output.is_empty());
    assert_eq!(live, expected.as_bytes());
}

#[cfg(target_os = "linux")]
#[test]
fn network_disabled_host_bash_uses_the_cleared_environment() {
    use std::os::unix::fs::PermissionsExt;
    let env = TestEnv::new();
    let fake = env.write(
        "bin/bwrap",
        "#!/bin/sh\n[ \"$1\" = --unshare-all ] && exit 0\n[ \"$1\" = --unshare-net ] || exit 1\n[ -z \"${GH_TOKEN+x}\" ] || exit 2\nprintf '%s|%s' \"$HOME\" \"$OXA_SESSION_ID\"\n",
    );
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let switch = YoloSwitch::new(Autonomy::Standard);
    let sb = FullAccess::new(
        Arc::new(NoSandbox),
        switch,
        fake.parent().unwrap().to_str().unwrap(),
        env.home(),
        false,
        true,
    )
    .with_os_sandbox(false);
    let mut spec = spec(&env, "true".into(), true);
    spec.env.push(("GH_TOKEN".into(), "fixture-value".into()));
    spec.env.push(("OXA_SESSION_ID".into(), "fixture-session".into()));
    let result = sb.run(&spec, &CancelToken::new(), &mut |_| {});
    assert_eq!(
        result.exit,
        Some(0),
        "{}",
        String::from_utf8_lossy(&result.output)
    );
    assert_eq!(
        result.output,
        format!("{}|fixture-session", env.home().display()).as_bytes()
    );
}

#[test]
fn requiring_sandbox_at_run_start_updates_existing_bash_views() {
    let env = TestEnv::new();
    let (sb, _) = full(Autonomy::Standard, &env);
    let sb = sb.with_os_sandbox(false);
    let view = sb.with_inner(Arc::new(NoSandbox));
    let command = spec(&env, "printf ran".into(), true);
    let result = view.run(&command, &CancelToken::new(), &mut |_| {});
    assert_eq!(result.exit, Some(0));

    sb.require_os_sandbox();
    let result = view.run(&command, &CancelToken::new(), &mut |_| {});
    assert_eq!(result.exit, None);
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

/// Review of #314: `run_with_stdin` is only used for exec hooks (`oxa-ext/src/exec_hooks.rs`),
/// which the user configured, not the model's bash. They stay in the wrapped sandbox under
/// every level, `yolo` included: the full-access bypass belongs to the bash tool alone.
#[test]
fn hooks_with_stdin_keep_the_sandbox_under_yolo() {
    let env = TestEnv::new();
    std::fs::create_dir_all(env.work()).unwrap();
    let target = env.root().join("hook-wrote.txt");
    let (sb, _switch) = full(Autonomy::Yolo, &env);
    let s = spec(&env, format!("cat > '{}'", target.display()), true);
    let r = sb.run_with_stdin(&s, b"payload", &CancelToken::new(), &mut |_| {});
    assert_eq!(r.exit, None, "{}", String::from_utf8_lossy(&r.output));
    assert!(!target.exists(), "the hook ran outside the wrapped sandbox");
}

/// #602: changing autonomy changes file identity, not exec-hook/extension confinement.
#[test]
fn file_view_tracks_live_yolo_but_keeps_plan_and_execution_sandboxed() {
    use oxa_proto::Mode;
    use oxa_sandbox::{Bwrap, Layout, Scratch};
    let env = TestEnv::new();
    let scratch = Scratch::create(&env.tmp()).unwrap();
    let inner: Arc<dyn Sandbox> = Arc::new(Bwrap {
        bwrap: "/usr/bin/bwrap".into(),
        home: env.home(),
        layout: Layout {
            read: vec![env.root().join("shared")],
            ..Layout::default()
        }
        .with_scratch(scratch),
    });
    for offline in [false, true] {
        let switch = YoloSwitch::new(Autonomy::Auto);
        let full = FullAccess::new(inner.clone(), switch.clone(), "", env.home(), offline, false);
        let files = full.file_view(inner.clone());
        for level in [Autonomy::Auto, Autonomy::Yolo, Autonomy::Standard] {
            switch.set_level(level);
            assert_eq!(files.scratch_dir(), inner.scratch_dir());
            assert_eq!(files.home(), inner.home());
            assert!(files.preserves_host_path(&env.root().join("shared/note.txt")));
            for mode in [Mode::Workspace, Mode::Plan] {
                let expected = if level == Autonomy::Yolo && mode == Mode::Workspace {
                    None
                } else {
                    inner.scratch_dir()
                };
                assert_eq!(files.file_scratch_dir(mode), expected);
            }
            let s = spec(&env, "true".into(), true);
            assert_eq!(files.bwrap_args(&s), inner.bwrap_args(&s));
        }
    }
    // An inner sandbox that refuses execution must keep refusing even under YOLO.
    let (full, _) = full(Autonomy::Yolo, &env);
    let files = full.file_view(Arc::new(NoSandbox));
    let s = spec(&env, "echo must-not-run".into(), true);
    assert!(files.check().is_err());
    let r = files.run(&s, &CancelToken::new(), &mut |_| {});
    assert_ne!(r.exit, Some(0));
    let r = files.run_streaming(&s, &CancelToken::new(), &mut |_| {});
    assert_ne!(r.exit, Some(0));
    let r = files.run_with_stdin(&s, b"test", &CancelToken::new(), &mut |_| {});
    assert_ne!(r.exit, Some(0));
}

/// #1512 S3: a wrapped sandbox that can run (its `check` passes) and says what it ran.
struct Wrapped {
    persistence: Option<&'static str>,
}

impl Sandbox for Wrapped {
    fn check(&self) -> Result<(), OxaError> {
        Ok(())
    }
    fn persistence_notice(&self) -> Option<&str> {
        self.persistence
    }
    fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
        vec!["--ro-bind".into(), "/".into(), "/".into()]
    }
    fn run(&self, _s: &ExecSpec, _c: &CancelToken, _live: &mut dyn FnMut(&[u8])) -> ExecResult {
        ExecResult {
            output: b"wrapped".to_vec(),
            exit: Some(42),
            timed_out: false,
        }
    }
}

/// #1512 S3: an interactive run without the OS sandbox: a writable command runs with the
/// user's rights (no bwrap), even though bwrap would be present.
#[test]
fn no_os_sandbox_bypasses_bwrap_for_writable_spec() {
    let env = TestEnv::new();
    std::fs::create_dir_all(env.work()).unwrap();
    let target = env.root().join("no-os-sandbox-wrote.txt");
    let switch = YoloSwitch::new(Autonomy::Standard);
    let sb =
        FullAccess::new(Arc::new(NoSandbox), switch, "", env.home(), false, false).with_os_sandbox(false);
    let s = spec(&env, format!("echo made > '{}'", target.display()), true);
    assert!(sb.check().is_ok(), "no OS sandbox needs no bwrap");
    assert_eq!(sb.launch_argv(&s).unwrap()[0], "bash");
    let r = sb.run(&s, &CancelToken::new(), &mut |_| {});
    assert_eq!(r.exit, Some(0), "{}", String::from_utf8_lossy(&r.output));
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "made\n");
    let _ = std::fs::remove_file(&target);
}

/// #1512 S3: a read-only (plan) spec always keeps bwrap. Missing bwrap fails closed rather
/// than treating the command classifier as filesystem confinement.
#[test]
fn no_os_sandbox_plan_spec_uses_bwrap_when_present() {
    let env = TestEnv::new();
    let switch = YoloSwitch::new(Autonomy::Standard);
    let inner: Arc<dyn Sandbox> = Arc::new(Wrapped { persistence: None });
    let sb = FullAccess::new(inner, switch, "", env.home(), false, false).with_os_sandbox(false);
    let s = spec(&env, "ls".into(), false);
    assert_eq!(sb.launch_argv(&s).unwrap()[0], "bwrap");
    let r = sb.run(&s, &CancelToken::new(), &mut |_| {});
    assert_eq!(r.exit, Some(42), "plan keeps the wrapped sandbox");

    let missing = FullAccess::new(
        Arc::new(NoSandbox),
        YoloSwitch::new(Autonomy::Standard),
        "",
        env.home(),
        false,
        false,
    )
    .with_os_sandbox(false);
    let r = missing.run(&s, &CancelToken::new(), &mut |_| {});
    assert_eq!(r.exit, None, "plan must fail closed without bwrap");
}

/// The raw FullAccess stdin path remains host-side when selected directly. Dispatch does not
/// give this view to exec hooks; `hook_sandbox` is tested below.
#[test]
fn no_os_sandbox_hooks_run_unsandboxed() {
    let env = TestEnv::new();
    std::fs::create_dir_all(env.work()).unwrap();
    let target = env.root().join("no-os-sandbox-hook.txt");
    let switch = YoloSwitch::new(Autonomy::Standard);
    let sb =
        FullAccess::new(Arc::new(NoSandbox), switch, "", env.home(), false, false).with_os_sandbox(false);
    let s = spec(&env, format!("cat > '{}'", target.display()), true);
    let r = sb.run_with_stdin(&s, b"payload", &CancelToken::new(), &mut |_| {});
    assert_eq!(r.exit, Some(0), "{}", String::from_utf8_lossy(&r.output));
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "payload");
    let _ = std::fs::remove_file(&target);
}

/// #1512 S3: with the OS sandbox on, exec hooks stay wrapped even under yolo.
#[test]
fn os_sandbox_on_hooks_stay_sandboxed_under_yolo() {
    let env = TestEnv::new();
    std::fs::create_dir_all(env.work()).unwrap();
    let target = env.root().join("hook-wrote-os-on.txt");
    let switch = YoloSwitch::new(Autonomy::Yolo);
    let sb = FullAccess::new(Arc::new(NoSandbox), switch, "", env.home(), false, false).with_os_sandbox(true);
    let s = spec(&env, format!("cat > '{}'", target.display()), true);
    let r = sb.run_with_stdin(&s, b"payload", &CancelToken::new(), &mut |_| {});
    assert_eq!(r.exit, None, "{}", String::from_utf8_lossy(&r.output));
    assert!(!target.exists(), "the hook ran outside the wrapped sandbox");
}

/// #1512 S3: `oxa.sandbox.net = off` holds without the OS sandbox: the command runs through
/// bwrap with the network unshared, never with the user's network.
#[test]
fn no_os_sandbox_net_off_uses_offline_argv() {
    let env = TestEnv::new();
    let path = std::env::var("PATH").unwrap_or_default();
    let switch = YoloSwitch::new(Autonomy::Standard);
    let sb =
        FullAccess::new(Arc::new(NoSandbox), switch, &path, env.home(), false, true).with_os_sandbox(false);
    let s = spec(&env, "true".into(), true);
    let argv = match sb.launch_argv(&s) {
        Ok(argv) => argv,
        // Without bwrap on this machine the owner's setting refuses instead (next test).
        Err(_) => return,
    };
    assert!(argv[0].ends_with("bwrap"), "{argv:?}");
    assert!(argv.iter().any(|a| a == "--unshare-net"), "{argv:?}");
}

/// #1512 S3: `oxa.sandbox.net = off` without bwrap is refused, never run with the network.
#[test]
fn no_os_sandbox_net_off_without_bwrap_refuses() {
    let env = TestEnv::new();
    let switch = YoloSwitch::new(Autonomy::Standard);
    let sb = FullAccess::new(Arc::new(NoSandbox), switch, "", env.home(), false, true).with_os_sandbox(false);
    let s = spec(&env, "echo must-not-run".into(), true);
    assert!(sb.launch_argv(&s).is_err());
    let r = sb.run(&s, &CancelToken::new(), &mut |_| {});
    assert_eq!(r.exit, None);
    assert!(
        String::from_utf8_lossy(&r.output).contains("needs the network"),
        "{}",
        String::from_utf8_lossy(&r.output)
    );
}

/// #1512 S3: the model's persistence note (the /tmp scratch claim) is not made when commands
/// run without the OS sandbox; it stays with the sandboxed run.
#[test]
fn no_os_sandbox_persistence_notice_is_none() {
    let env = TestEnv::new();
    let inner: Arc<dyn Sandbox> = Arc::new(Wrapped {
        persistence: Some("scratch"),
    });
    let off = FullAccess::new(
        inner.clone(),
        YoloSwitch::new(Autonomy::Standard),
        "",
        env.home(),
        false,
        false,
    )
    .with_os_sandbox(false);
    assert_eq!(off.persistence_notice(), None);
    let on = FullAccess::new(
        inner,
        YoloSwitch::new(Autonomy::Standard),
        "",
        env.home(),
        false,
        false,
    );
    assert_eq!(on.persistence_notice(), Some("scratch"));
}

/// #1512 S3: with the OS sandbox on, the exec hooks' sandbox keeps the inline (`run`) path
/// wrapped too when yolo is switched on live, including when interactive bash opted out.
#[test]
fn os_sandbox_on_inline_hooks_stay_sandboxed_under_yolo() {
    let env = TestEnv::new();
    std::fs::create_dir_all(env.work()).unwrap();
    let target = env.root().join("inline-hook-wrote-os-on.txt");
    let switch = YoloSwitch::new(Autonomy::Standard);
    let full = Arc::new(FullAccess::new(
        Arc::new(NoSandbox),
        switch.clone(),
        "",
        env.home(),
        false,
        false,
    ));
    let hooks = FullAccess::hook_sandbox(&full);
    switch.set_level(Autonomy::Yolo);
    let s = spec(&env, format!("echo x > '{}'", target.display()), true);
    let r = hooks.run(&s, &CancelToken::new(), &mut |_| {});
    assert_eq!(r.exit, None, "{}", String::from_utf8_lossy(&r.output));
    assert!(
        !target.exists(),
        "an inline hook ran outside the wrapped sandbox under yolo"
    );

    let off = Arc::new(
        FullAccess::new(Arc::new(NoSandbox), switch, "", env.home(), false, false).with_os_sandbox(false),
    );
    let hooks = FullAccess::hook_sandbox(&off);
    let r = hooks.run(&s, &CancelToken::new(), &mut |_| {});
    assert_eq!(r.exit, None, "{}", String::from_utf8_lossy(&r.output));
    assert!(
        !target.exists(),
        "interactive bash policy must not release exec hooks from their sandbox"
    );
    let _ = std::fs::remove_file(&target);
}
