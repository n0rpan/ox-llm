//! #202: full access (`yolo`). The bash tool's command runs as plain `bash -c` with the user's
//! own rights: no bwrap in its argv, a file outside the workspace is readable and a file in a
//! temporary directory outside it is written for real. `auto` and plan mode keep the sandbox.

use oxa_fake::TestEnv;
use oxa_proto::{Autonomy, CancelToken, ExecSpec, Sandbox, YoloSwitch};
use oxa_sandbox::{FullAccess, NetSandbox, NoSandbox};
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

/// Owner standard: yolo = no sandbox anywhere. Exec hooks (inline and with stdin) run with the
/// user's rights while the switch is at yolo, also live; outside yolo both stay in the wrapped
/// sandbox (the raw `FullAccess` stdin path always does).
#[test]
fn yolo_hooks_run_unsandboxed_and_non_yolo_hooks_stay_sandboxed() {
    let env = TestEnv::new();
    std::fs::create_dir_all(env.work()).unwrap();
    let inline = env.root().join("hook-inline.txt");
    let piped = env.root().join("hook-stdin.txt");
    let (sb, switch) = full(Autonomy::Standard, &env);
    let hooks = FullAccess::hook_sandbox(&Arc::new(sb));
    let run = |hooks: &Arc<dyn Sandbox>| {
        let s = spec(&env, format!("echo x > '{}'", inline.display()), true);
        let a = hooks.run(&s, &CancelToken::new(), &mut |_| {});
        let s = spec(&env, format!("cat > '{}'", piped.display()), true);
        let b = hooks.run_with_stdin(&s, b"payload", &CancelToken::new(), &mut |_| {});
        (a, b)
    };
    let (a, b) = run(&hooks);
    assert_eq!(a.exit, None, "{}", String::from_utf8_lossy(&a.output));
    assert_eq!(b.exit, None, "{}", String::from_utf8_lossy(&b.output));
    assert!(
        !inline.exists() && !piped.exists(),
        "a non-yolo hook left the sandbox"
    );

    switch.set_level(Autonomy::Yolo);
    assert!(hooks.check().is_ok(), "yolo hooks need no bwrap");
    let (a, b) = run(&hooks);
    assert_eq!(a.exit, Some(0), "{}", String::from_utf8_lossy(&a.output));
    assert_eq!(b.exit, Some(0), "{}", String::from_utf8_lossy(&b.output));
    assert_eq!(std::fs::read_to_string(&inline).unwrap(), "x\n");
    assert_eq!(std::fs::read_to_string(&piped).unwrap(), "payload");

    // The raw FullAccess stdin path is not the hooks' view.
    let (raw, _switch) = full(Autonomy::Yolo, &env);
    let _ = std::fs::remove_file(&piped);
    let s = spec(&env, format!("cat > '{}'", piped.display()), true);
    let r = raw.run_with_stdin(&s, b"payload", &CancelToken::new(), &mut |_| {});
    assert_eq!(r.exit, None, "{}", String::from_utf8_lossy(&r.output));
    assert!(!piped.exists());
    let _ = std::fs::remove_file(&inline);
    let _ = std::fs::remove_file(&piped);
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
        let full = FullAccess::new(inner.clone(), switch.clone(), "", env.home(), offline);
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

/// Yolo bash around a networked sandbox that knows the GitHub token `tok`.
fn full_with_token(env: &TestEnv, tok: &str) -> FullAccess {
    let inner = NetSandbox::new(Arc::new(NoSandbox), true, false).with_gh_token(Some(tok.to_string()));
    FullAccess::new(
        Arc::new(inner),
        YoloSwitch::new(Autonomy::Yolo),
        "",
        env.home(),
        false,
    )
}

#[test]
fn yolo_bash_output_redacts_gh_token() {
    let env = TestEnv::new();
    std::fs::create_dir_all(env.work()).unwrap();
    let sb = full_with_token(&env, "ghp_SYNTH_YOLO_77");
    let s = spec(&env, "echo token=ghp_SYNTH_YOLO_77; echo plain".to_string(), true);
    let mut live = Vec::new();
    let r = sb.run(&s, &CancelToken::new(), &mut |b| live.extend_from_slice(b));
    let want = "token=[redacted]\nplain\n";
    assert_eq!(String::from_utf8_lossy(&r.output), want);
    assert_eq!(String::from_utf8_lossy(&live), want);
    let streamed = sb.run_streaming(&s, &CancelToken::new(), &mut |_| {});
    assert!(streamed.output.is_empty());
}

/// Yolo with the network off (`oxa.sandbox.net: off`) still masks the token: `gh auth token`
/// reads the credential locally, so the output mask never depends on the network. Only the
/// `GH_TOKEN` injection is network-gated, and this run has none.
#[test]
fn yolo_bash_output_redacts_gh_token_with_the_network_off() {
    let env = TestEnv::new();
    std::fs::create_dir_all(env.work()).unwrap();
    let inner =
        NetSandbox::new(Arc::new(NoSandbox), false, false).with_gh_token(Some("ghp_SYNTH_OFF".into()));
    let sb = FullAccess::new(
        Arc::new(inner),
        YoloSwitch::new(Autonomy::Yolo),
        "",
        env.home(),
        false,
    );
    let s = spec(&env, "echo token=ghp_SYNTH_OFF; echo plain".to_string(), true);
    let mut live = Vec::new();
    let r = sb.run(&s, &CancelToken::new(), &mut |b| live.extend_from_slice(b));
    let want = "token=[redacted]\nplain\n";
    assert_eq!(String::from_utf8_lossy(&r.output), want);
    assert_eq!(String::from_utf8_lossy(&live), want);
}

#[test]
fn yolo_redaction_does_not_block() {
    let env = TestEnv::new();
    std::fs::create_dir_all(env.work()).unwrap();
    let out = env.root().join("yolo-wrote.txt");
    let sb = full_with_token(&env, "ghp_SYNTH_YOLO_77");
    // The command that prints the token still runs, writes its file and keeps its exit code.
    let s = spec(
        &env,
        format!("echo ghp_SYNTH_YOLO_77 > '{}'; exit 3", out.display()),
        true,
    );
    let r = sb.run(&s, &CancelToken::new(), &mut |_| {});
    assert_eq!(r.exit, Some(3), "{}", String::from_utf8_lossy(&r.output));
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "ghp_SYNTH_YOLO_77\n");
    // No token known: output passes through untouched.
    let (plain, _) = full(Autonomy::Yolo, &env);
    let r = plain.run(
        &spec(&env, "echo ghp_SYNTH_YOLO_77".into(), true),
        &CancelToken::new(),
        &mut |_| {},
    );
    assert_eq!(String::from_utf8_lossy(&r.output), "ghp_SYNTH_YOLO_77\n");
}

/// RP6: yolo bash has no sandbox, so a host socket (the docker socket) is reachable without
/// any `oxa.sandbox.sockets` entry.
#[test]
fn yolo_bash_sees_the_docker_socket() {
    let env = TestEnv::new();
    std::fs::create_dir_all(env.work()).unwrap();
    let dir = tempdir_outside(&env);
    let sock = dir.join("docker.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&sock).expect("bind socket");
    let (sb, _switch) = full(Autonomy::Yolo, &env);
    let s = spec(
        &env,
        format!("test -S '{}' && echo reachable", sock.display()),
        true,
    );
    let r = sb.run(&s, &CancelToken::new(), &mut |_| {});
    assert_eq!(r.exit, Some(0), "{}", String::from_utf8_lossy(&r.output));
    assert_eq!(String::from_utf8_lossy(&r.output), "reachable\n");
    let _ = std::fs::remove_dir_all(&dir);
}
