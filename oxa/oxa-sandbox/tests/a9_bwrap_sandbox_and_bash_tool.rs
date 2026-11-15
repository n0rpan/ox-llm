//! A9 acceptance tests: bwrap sandbox and bash tool. Owner: task A9.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.
//!
//! Tests that need bwrap skip with a printed notice when `bwrap` is missing or cannot create a
//! user namespace.

use oxa_fake::TestEnv;
use oxa_proto::{CancelToken, ExecSpec, Sandbox};
use oxa_sandbox::{bwrap_args, run_argv, run_unsandboxed, Bwrap};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

fn spec(command: &str, cwd: &Path, writable: bool, net: bool) -> ExecSpec {
    ExecSpec {
        command: command.to_string(),
        cwd: cwd.to_path_buf(),
        writable,
        net,
        timeout_s: None,
        env: Vec::new(),
    }
}

fn argv(xs: &[&str]) -> Vec<String> {
    xs.iter().map(|x| (*x).to_string()).collect()
}

/// `bwrap` from the process PATH, if it exists and can create a user namespace.
fn bwrap_path() -> Option<PathBuf> {
    let path = std::env::var("PATH").unwrap_or_default();
    let found = path
        .split(':')
        .filter(|d| d.starts_with('/'))
        .map(|d| Path::new(d).join("bwrap"))
        .find(|p| p.is_file())?;
    let probe = Command::new(&found)
        .args(["--unshare-all", "--ro-bind", "/", "/", "true"])
        .output();
    match probe {
        Ok(o) if o.status.success() => Some(found),
        Ok(o) => {
            skip_notice(&format!(
                "bwrap cannot create a sandbox: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            ));
            None
        }
        Err(e) => {
            skip_notice(&format!("cannot run bwrap: {e}"));
            None
        }
    }
}

/// A5 (wave H lane `harden`): with `OXA_REQUIRE_BWRAP=1` (CI sets it) an unusable bwrap is a
/// test failure with the reason; without it the test is skipped with a notice written straight
/// to stderr (not captured, so cargo shows it), and [`bwrap_skips_are_reported`] sums them up.
fn required() -> bool {
    std::env::var("OXA_REQUIRE_BWRAP").is_ok_and(|v| v == "1")
}

fn skip_notice(text: &str) {
    use std::io::Write as _;
    let _ = writeln!(std::io::stderr(), "a9 SKIPPED: {text}");
}

/// A [`Bwrap`] for `env` (home = the test home), or `None` with a notice when bwrap is unusable
/// (a failure under `OXA_REQUIRE_BWRAP=1`).
fn sandbox_for(env: &TestEnv) -> Option<Bwrap> {
    let bwrap = match bwrap_path() {
        Some(b) => b,
        None => {
            assert!(
                !required(),
                "OXA_REQUIRE_BWRAP=1 but bwrap is not usable on this machine (see the reason above)"
            );
            skip_notice("bwrap is not usable on this machine");
            return None;
        }
    };
    Some(Bwrap {
        bwrap,
        home: env.home(),
        layout: oxa_sandbox::Layout::default(),
    })
}

/// A5: one line that cargo shows when the bwrap tests of this file were skipped (how many, and
/// why); a failure under `OXA_REQUIRE_BWRAP=1`.
#[test]
fn bwrap_skips_are_reported() {
    // The tests that need bwrap, counted in this file (minus this line's own pattern).
    let needing = include_str!("a9_bwrap_sandbox_and_bash_tool.rs")
        .matches("sandbox_for(&env) else")
        .count()
        - 1;
    if bwrap_path().is_none() {
        assert!(
            !required(),
            "OXA_REQUIRE_BWRAP=1 but bwrap is not usable: {needing} sandbox tests cannot run"
        );
        skip_notice(&format!(
            "{needing} sandbox tests did not run (bwrap is not usable); set OXA_REQUIRE_BWRAP=1 to make this a failure"
        ));
    }
}

/// The process tree probe: a pid is alive unless it is gone or a zombie.
fn alive(pid: i32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(s) => {
            let state = s.rfind(')').and_then(|i| s[i + 1..].trim_start().chars().next());
            state != Some('Z') && state != Some('X')
        }
        Err(_) => false,
    }
}

/// four goldens
#[test]
fn argv_golden_per_writable_net() {
    let cwd = Path::new("/w");
    let home = Path::new("/h");
    let base = |writable: bool, net: bool| {
        let mut s = spec("ls", cwd, writable, net);
        s.env.clear();
        oxa_sandbox::without_account_home(bwrap_args(&s, home))
    };
    // writable, no network
    assert_eq!(
        base(true, false),
        argv(&[
            "--unshare-all",
            "--die-with-parent",
            "--new-session",
            "--cap-drop",
            "ALL",
            "--ro-bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            "--tmpfs",
            "/tmp",
            "--tmpfs",
            "/h",
            "--bind",
            "/w",
            "/w",
            "--remount-ro",
            "/h",
            "--chdir",
            "/w",
            "--clearenv",
            "--setenv",
            "PATH",
            "/usr/local/bin:/usr/bin:/bin",
            "--setenv",
            "HOME",
            "/h",
            "--setenv",
            "LANG",
            "C.UTF-8",
            "--setenv",
            "TERM",
            "dumb",
            "--setenv",
            "AI_AGENT",
            "oxa",
            "--",
            "bash",
            "-c",
            "ls",
        ])
    );
    // writable, network
    assert_eq!(
        base(true, true),
        argv(&[
            "--unshare-all",
            "--share-net",
            "--die-with-parent",
            "--new-session",
            "--cap-drop",
            "ALL",
            "--ro-bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            "--tmpfs",
            "/tmp",
            "--tmpfs",
            "/h",
            "--bind",
            "/w",
            "/w",
            "--remount-ro",
            "/h",
            "--chdir",
            "/w",
            "--clearenv",
            "--setenv",
            "PATH",
            "/usr/local/bin:/usr/bin:/bin",
            "--setenv",
            "HOME",
            "/h",
            "--setenv",
            "LANG",
            "C.UTF-8",
            "--setenv",
            "TERM",
            "dumb",
            "--setenv",
            "AI_AGENT",
            "oxa",
            "--",
            "bash",
            "-c",
            "ls",
        ])
    );
    // read-only (plan), no network
    assert_eq!(
        base(false, false),
        argv(&[
            "--unshare-all",
            "--die-with-parent",
            "--new-session",
            "--cap-drop",
            "ALL",
            "--ro-bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            "--tmpfs",
            "/tmp",
            "--tmpfs",
            "/h",
            "--ro-bind",
            "/w",
            "/w",
            "--remount-ro",
            "/h",
            "--chdir",
            "/w",
            "--clearenv",
            "--setenv",
            "PATH",
            "/usr/local/bin:/usr/bin:/bin",
            "--setenv",
            "HOME",
            "/h",
            "--setenv",
            "LANG",
            "C.UTF-8",
            "--setenv",
            "TERM",
            "dumb",
            "--setenv",
            "AI_AGENT",
            "oxa",
            "--",
            "bash",
            "-c",
            "ls",
        ])
    );
    // read-only (plan), network
    assert_eq!(
        base(false, true),
        argv(&[
            "--unshare-all",
            "--share-net",
            "--die-with-parent",
            "--new-session",
            "--cap-drop",
            "ALL",
            "--ro-bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            "--tmpfs",
            "/tmp",
            "--tmpfs",
            "/h",
            "--ro-bind",
            "/w",
            "/w",
            "--remount-ro",
            "/h",
            "--chdir",
            "/w",
            "--clearenv",
            "--setenv",
            "PATH",
            "/usr/local/bin:/usr/bin:/bin",
            "--setenv",
            "HOME",
            "/h",
            "--setenv",
            "LANG",
            "C.UTF-8",
            "--setenv",
            "TERM",
            "dumb",
            "--setenv",
            "AI_AGENT",
            "oxa",
            "--",
            "bash",
            "-c",
            "ls",
        ])
    );
}

#[test]
fn argv_extra_env_follows_base_env_in_order() {
    let mut s = spec("env", Path::new("/w"), true, false);
    s.env = vec![
        ("OXA_SESSION_ID".to_string(), "s1".to_string()),
        ("PATH".to_string(), "/x".to_string()),
    ];
    let a = bwrap_args(&s, Path::new("/h"));
    let n = a.len();
    // The extra pairs come after the fixed base environment, in order, then the `--` separator.
    assert_eq!(
        &a[n - 13..],
        argv(&[
            "--setenv",
            "AI_AGENT",
            "oxa",
            "--setenv",
            "OXA_SESSION_ID",
            "s1",
            "--setenv",
            "PATH",
            "/x",
            "--",
            "bash",
            "-c",
            "env",
        ])
        .as_slice()
    );
}

/// needs bwrap; skipped with a notice when absent
#[test]
fn write_outside_cwd_fails() {
    let env = TestEnv::new();
    let Some(sb) = sandbox_for(&env) else { return };
    // Outside the cwd, the home and the private /tmp: the read-only root.
    let target = format!("/var/oxa-a9-probe-{}", std::process::id());
    let cmd = format!("echo leaked > {target}");
    let res = sb.run(
        &spec(&cmd, &env.work(), true, false),
        &CancelToken::new(),
        &mut |_| {},
    );
    let host_wrote = Path::new(&target).exists();
    if host_wrote {
        let _ = std::fs::remove_file(&target);
    }
    assert_ne!(
        res.exit,
        Some(0),
        "write outside the cwd must fail: {:?}",
        String::from_utf8_lossy(&res.output)
    );
    assert!(!host_wrote, "the host file must not be created");
    // Inside the cwd the same kind of write works.
    let inside = sb.run(
        &spec("echo ok > inside.txt", &env.work(), true, false),
        &CancelToken::new(),
        &mut |_| {},
    );
    assert_eq!(
        inside.exit,
        Some(0),
        "{:?}",
        String::from_utf8_lossy(&inside.output)
    );
    assert!(env.work().join("inside.txt").exists());
}

/// `ls ~/.ssh` fails inside
#[test]
fn home_ssh_absent() {
    let env = TestEnv::new();
    env.write("home/.ssh/id_test", "PRIVATE");
    let Some(sb) = sandbox_for(&env) else { return };
    let res = sb.run(
        &spec("ls ~/.ssh", &env.work(), true, false),
        &CancelToken::new(),
        &mut |_| {},
    );
    assert_ne!(
        res.exit,
        Some(0),
        "~/.ssh must be absent: {:?}",
        String::from_utf8_lossy(&res.output)
    );
    let cat = sb.run(
        &spec("cat ~/.ssh/id_test", &env.work(), true, false),
        &CancelToken::new(),
        &mut |_| {},
    );
    assert_ne!(cat.exit, Some(0));
    assert!(!String::from_utf8_lossy(&cat.output).contains("PRIVATE"));
}

/// #52: a secret env var (`GH_TOKEN`) reaches the command through `--args <fd>`, not argv.
#[test]
fn gh_token_reaches_the_command_but_not_the_bwrap_command_line() {
    let env = TestEnv::new();
    let Some(sb) = sandbox_for(&env) else { return };
    let mut sp = spec("echo \"[$GH_TOKEN]\"", &env.work(), true, false);
    sp.env.push(("GH_TOKEN".into(), "ghp_SECRET123".into()));
    let res = sb.run(&sp, &CancelToken::new(), &mut |_| {});
    let out = String::from_utf8_lossy(&res.output).into_owned();
    assert!(out.contains("[ghp_SECRET123]"), "{out}");
}

/// #53: with another `$HOME` the account's real home is an empty directory inside the sandbox,
/// in every mode (plan: read-only cwd; with and without network).
#[test]
fn a_different_home_hides_the_account_home() {
    let env = TestEnv::new();
    let Some(sb) = sandbox_for(&env) else { return };
    let Some(real) = oxa_sandbox::account_home()
        .filter(|r| std::fs::read_dir(r).is_ok_and(|mut d| d.next().is_some()) && !env.work().starts_with(r))
    else {
        return;
    };
    for (writable, net) in [(true, true), (true, false), (false, false)] {
        let cmd = format!(
            "ls -A '{}' | wc -l; test -e '{0}/.ssh' && echo SSH_VISIBLE",
            real.display()
        );
        let res = sb.run(
            &spec(&cmd, &env.work(), writable, net),
            &CancelToken::new(),
            &mut |_| {},
        );
        let out = String::from_utf8_lossy(&res.output).into_owned();
        assert_eq!(out.trim(), "0", "writable={writable} net={net}: {out}");
    }
}

/// needs bwrap
#[test]
fn no_network_by_default() {
    let env = TestEnv::new();
    let Some(sb) = sandbox_for(&env) else { return };
    let res = sb.run(
        &spec("cat /proc/net/dev", &env.work(), true, false),
        &CancelToken::new(),
        &mut |_| {},
    );
    assert_eq!(res.exit, Some(0), "{:?}", String::from_utf8_lossy(&res.output));
    let text = String::from_utf8_lossy(&res.output).into_owned();
    let ifaces: Vec<&str> = text
        .lines()
        .filter_map(|l| l.split_once(':').map(|(name, _)| name.trim()))
        .collect();
    assert!(!ifaces.is_empty());
    assert!(
        ifaces.iter().all(|i| *i == "lo"),
        "only loopback may exist without net: {ifaces:?}"
    );
}

/// child of child is gone
#[test]
fn timeout_kills_process_tree() {
    let env = TestEnv::new();
    let pidfile = env.tmp().join("grandchild.pid");
    // bash (the run) -> bash (child) -> sleep (grandchild, the process that must die).
    let script = format!(
        "bash -c 'sleep 60 & echo $! > {}; wait' & wait",
        pidfile.display()
    );
    let started = Instant::now();
    let res = run_argv(
        &argv(&["bash", "-c", &script]),
        &[],
        Some(1),
        &CancelToken::new(),
        &mut |_| {},
    );
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "the timeout must return promptly"
    );
    assert!(res.timed_out);
    assert_eq!(res.exit, None);
    let pid: i32 = std::fs::read_to_string(&pidfile)
        .expect("pid file")
        .trim()
        .parse()
        .expect("pid");
    std::thread::sleep(Duration::from_millis(200));
    assert!(!alive(pid), "grandchild {pid} must be gone after the timeout");
}

/// within 3 s
#[test]
fn cancel_kills_process_tree() {
    let env = TestEnv::new();
    let pidfile = env.tmp().join("cancel-grandchild.pid");
    let script = format!(
        "bash -c 'sleep 60 & echo $! > {}; wait' & wait",
        pidfile.display()
    );
    let cancel = CancelToken::new();
    let trigger = cancel.clone();
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(400));
        let at = Instant::now();
        trigger.cancel();
        at
    });
    let res = run_argv(&argv(&["bash", "-c", &script]), &[], None, &cancel, &mut |_| {});
    let cancelled_at = canceller.join().expect("canceller");
    assert!(
        cancelled_at.elapsed() < Duration::from_secs(3),
        "cancel must return within 3 s"
    );
    assert!(!res.timed_out);
    assert_eq!(res.exit, None);
    let pid: i32 = std::fs::read_to_string(&pidfile)
        .expect("pid file")
        .trim()
        .parse()
        .expect("pid");
    std::thread::sleep(Duration::from_millis(200));
    assert!(!alive(pid), "grandchild {pid} must be gone after the cancel");
}

/// `sleep 100 &` returns promptly
#[test]
fn background_writer_does_not_hang() {
    let started = Instant::now();
    let mut seen = Vec::new();
    let res = run_argv(
        &argv(&["bash", "-c", "sleep 100 & echo $!"]),
        &[],
        None,
        &CancelToken::new(),
        &mut |b| seen.extend_from_slice(b),
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "a background writer must not hang the call"
    );
    assert_eq!(res.exit, Some(0));
    // Stop the background sleep this test started.
    let pid: i32 = String::from_utf8_lossy(&res.output)
        .trim()
        .parse()
        .expect("background pid");
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
    assert_eq!(seen, res.output, "live chunks match the collected output");
}

/// detect with an empty PATH
#[test]
fn no_bwrap_is_no_sandbox_error() {
    let env = TestEnv::new();
    let err = Bwrap::detect("", &env.home()).expect_err("no bwrap on an empty PATH");
    assert_eq!(err.code, oxa_proto::codes::NO_SANDBOX);
    assert!(err.hint.is_some());
}

/// NoSandbox -> is_error with the fixed text
#[test]
fn bash_tool_refuses_without_sandbox() {
    use oxa_proto::Tool;
    let env = TestEnv::new();
    let home = env.home();
    let cancel = CancelToken::new();
    let sb = oxa_sandbox::NoSandbox;
    let cx = oxa_proto::ToolCtx {
        cwd: &home,
        spill_dir: &home,
        sandbox: &sb,
        mode: oxa_proto::Mode::Workspace,
        cancel: &cancel,
        env: &[],
        tune: Default::default(),
    };
    let marker = home.join("ran");
    let cmd = format!("touch {}", marker.display());
    let out = oxa_tools::BashTool::new().run(&serde_json::json!({ "command": cmd }), &cx, &mut |_| {});
    assert!(out.is_error);
    assert_eq!(
        out.text,
        "bwrap is not available; bash is disabled\ninstall bubblewrap and make sure unprivileged user namespaces are enabled"
    );
    assert!(!marker.exists(), "nothing may run without a sandbox");
}

/// pi's texts, no timings
#[test]
fn bash_tool_exit_code_and_timeout_lines() {
    use oxa_proto::Tool;
    let env = TestEnv::new();
    let Some(sb) = sandbox_for(&env) else { return };
    let cwd = env.home().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let cancel = CancelToken::new();
    let cx = oxa_proto::ToolCtx {
        cwd: &cwd,
        spill_dir: &cwd,
        sandbox: &sb,
        mode: oxa_proto::Mode::Workspace,
        cancel: &cancel,
        env: &[],
        tune: Default::default(),
    };
    let tool = oxa_tools::BashTool::new();
    let run = |args: serde_json::Value| tool.run(&args, &cx, &mut |_| {});

    let out = run(serde_json::json!({ "command": "echo hi" }));
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(out.text, "hi\n");

    let out = run(serde_json::json!({ "command": "echo hi; exit 3" }));
    assert!(out.is_error);
    assert_eq!(out.text, "hi\n\n\nCommand exited with code 3");
    assert_eq!(out.ui, serde_json::json!({ "exit": 3, "timedOut": false }));

    let out = run(serde_json::json!({ "command": "true" }));
    assert_eq!(out.text, "(no output)");

    let notice = sb.persistence_notice().unwrap();
    for command in ["echo TMPDIR", "echo /tmp"] {
        let out = run(serde_json::json!({ "command": command }));
        assert!(!out.is_error, "{}", out.text);
        assert!(out.text.ends_with(notice), "{}", out.text);
    }
    for error in [
        "Read-only file system",
        "Permission denied",
        "Operation not permitted",
    ] {
        let command = format!("echo '{error}' >&2; exit 1");
        let out = run(serde_json::json!({ "command": command }));
        assert!(out.is_error);
        assert!(out.text.contains(notice), "{}", out.text);
        let command = format!("echo '{error}'");
        let out = run(serde_json::json!({ "command": command }));
        assert!(!out.text.contains(notice), "successful output is not an error");
    }
    let out = run(serde_json::json!({ "command": "echo tmpdir /TMP" }));
    assert_eq!(out.text, "tmpdir /TMP\n");

    // Scratch succeeds, but the model sees its lifetime; streaming process bytes stay raw.
    let mut live = String::new();
    let out = tool.run(
        &serde_json::json!({ "command": "echo scratch > /tmp/oxa-98-tool; cat /tmp/oxa-98-tool" }),
        &cx,
        &mut |chunk| live.push_str(chunk),
    );
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(live, "scratch\n");
    assert!(out.text.contains("discarded after each command"));
    let out = run(serde_json::json!({ "command": "test ! -e /tmp/oxa-98-tool" }));
    assert!(!out.is_error, "{}", out.text);
    let out = run(serde_json::json!({ "command": "echo lost > ~/oxa-98-tool" }));
    assert!(out.is_error, "{}", out.text);
    assert!(out.text.contains("Read-only file system"));
    assert!(out.text.contains(notice));

    // Two runs of a timeout give identical bytes: no timing text (W4).
    let a = run(serde_json::json!({ "command": "echo a; sleep 30", "timeout": 1 }));
    let b = run(serde_json::json!({ "command": "echo a; sleep 30", "timeout": 1 }));
    assert!(a.is_error);
    assert_eq!(a.text, "a\n\n\nCommand timed out after 1 seconds");
    assert_eq!(a.text, b.text);
    assert_eq!(a.ui, serde_json::json!({ "exit": null, "timedOut": true }));
}

/// writable = false
#[test]
fn plan_mode_is_read_only() {
    // The argv: cwd is mounted read-only and never with `--bind`.
    let s = spec("touch x", Path::new("/w"), false, false);
    let a = bwrap_args(&s, Path::new("/h"));
    assert!(a.windows(3).any(|w| w == ["--ro-bind", "/w", "/w"]));
    assert!(!a.windows(3).any(|w| w == ["--bind", "/w", "/w"]));

    // Behaviour: a write in the cwd fails in plan mode.
    let env = TestEnv::new();
    let Some(sb) = sandbox_for(&env) else { return };
    let res = sb.run(
        &spec("echo x > planned.txt", &env.work(), false, false),
        &CancelToken::new(),
        &mut |_| {},
    );
    assert_ne!(res.exit, Some(0));
    assert!(!env.work().join("planned.txt").exists());
}

/// `!cmd` runs unsandboxed in the cwd (pi's behaviour), with the given environment.
#[test]
fn user_bash_runs_in_cwd_without_sandbox() {
    let env = TestEnv::new();
    let mut s = spec("pwd; echo $OXA_MARK", &env.work(), false, false);
    s.env = vec![("OXA_MARK".to_string(), "marked".to_string())];
    let res = run_unsandboxed(&s, &CancelToken::new(), &mut |_| {});
    assert_eq!(res.exit, Some(0));
    let text = String::from_utf8_lossy(&res.output).into_owned();
    let work = env.work().canonicalize().expect("work dir");
    assert!(text.contains(&*work.to_string_lossy()), "{text}");
    assert!(text.contains("marked"), "{text}");
}

/// Issue #98: the hidden home must not accept writes that silently disappear.
#[test]
fn hidden_home_writes_fail_but_tmp_is_scratch() {
    let env = TestEnv::new();
    let Some(sb) = sandbox_for(&env) else { return };
    let home = sb.run(
        &spec("touch ~/probe", &env.work(), true, false),
        &CancelToken::new(),
        &mut |_| {},
    );
    assert_ne!(home.exit, Some(0), "hidden home writes must fail, not disappear");
    assert!(String::from_utf8_lossy(&home.output).contains("Read-only file system"));
    assert!(!env.home().join("probe").exists());

    let scratch = sb.run(
        &spec(
            "touch /tmp/oxa-98-scratch; test -f /tmp/oxa-98-scratch",
            &env.work(),
            true,
            false,
        ),
        &CancelToken::new(),
        &mut |_| {},
    );
    assert_eq!(scratch.exit, Some(0));
    let next = sb.run(
        &spec("test ! -e /tmp/oxa-98-scratch", &env.work(), true, false),
        &CancelToken::new(),
        &mut |_| {},
    );
    assert_eq!(next.exit, Some(0), "scratch must not persist between calls");
}
