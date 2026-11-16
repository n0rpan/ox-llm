//! O-167(b) (wave H): the bash tool's network per permission mode, per
//! `oxa.sandbox.net` and under the offline switch. The argv is what the real bwrap sandbox
//! would run (`bwrap_args` of the spec that reaches the inner sandbox).

use oxa_fake::TestEnv;
use oxa_proto::{CancelToken, ExecResult, ExecSpec, Mode, Sandbox, Tool, ToolCtx};
use oxa_sandbox::{bwrap_args, sandbox_net_setting, NetSandbox};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Records the argv of every spec it is asked to run.
struct Recording {
    home: PathBuf,
    argv: Mutex<Vec<Vec<String>>>,
}

impl Sandbox for Recording {
    fn check(&self) -> Result<(), oxa_proto::OxaError> {
        Ok(())
    }
    fn bwrap_args(&self, s: &ExecSpec) -> Vec<String> {
        oxa_sandbox::without_account_home(bwrap_args(s, &self.home))
    }
    fn run(&self, s: &ExecSpec, _c: &CancelToken, _live: &mut dyn FnMut(&[u8])) -> ExecResult {
        self.argv
            .lock()
            .unwrap()
            .push(oxa_sandbox::without_account_home(bwrap_args(s, &self.home)));
        ExecResult {
            output: Vec::new(),
            exit: Some(0),
            timed_out: false,
        }
    }
}

/// The argv of one bash call under `mode`, the setting and the offline switch.
fn bash_argv(env: &TestEnv, mode: Mode, setting: Option<&str>, offline: bool) -> Vec<String> {
    let inner = Arc::new(Recording {
        home: env.home(),
        argv: Mutex::new(Vec::new()),
    });
    let sb = NetSandbox::new(inner.clone(), sandbox_net_setting(setting), offline);
    let cwd = env.work();
    let cancel = CancelToken::new();
    let cx = ToolCtx {
        checked: &[],
        cwd: &cwd,
        spill_dir: &cwd,
        sandbox: &sb,
        mode,
        cancel: &cancel,
        env: &[],
        tune: Default::default(),
    };
    let out = oxa_tools::BashTool::new().run(&serde_json::json!({ "command": "true" }), &cx, &mut |_| {});
    assert!(!out.is_error, "{:?}", out.text);
    let argv = inner.argv.lock().unwrap().pop().expect("one run");
    argv
}

fn expected(home: &Path, cwd: &Path, writable: bool, net: bool) -> Vec<String> {
    let home = home.display().to_string();
    let cwd = cwd.display().to_string();
    let mut a: Vec<&str> = vec!["--unshare-all"];
    if net {
        a.push("--share-net");
    }
    a.extend([
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
        &home,
        if writable { "--bind" } else { "--ro-bind" },
        &cwd,
        &cwd,
        "--remount-ro",
        &home,
        "--chdir",
        &cwd,
        "--clearenv",
        "--setenv",
        "PATH",
        "/usr/local/bin:/usr/bin:/bin",
        "--setenv",
        "HOME",
        &home,
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
        "true",
    ]);
    a.into_iter().map(str::to_string).collect()
}

#[test]
fn argv_golden_per_mode_and_setting() {
    let env = TestEnv::new();
    let (home, cwd) = (env.home(), env.work());
    // (mode, setting, writable, network)
    let table = [
        (Mode::Workspace, None, true, true),
        (Mode::Workspace, Some("on"), true, true),
        (Mode::Workspace, Some("off"), true, false),
        (Mode::Ask, None, true, true),
        (Mode::Ask, Some("off"), true, false),
        (Mode::Plan, None, false, false),
        (Mode::Plan, Some("on"), false, false),
    ];
    for (mode, setting, writable, net) in table {
        assert_eq!(
            bash_argv(&env, mode, setting, false),
            expected(&home, &cwd, writable, net),
            "{mode:?} {setting:?}"
        );
    }
}

#[test]
fn offline_never_shares_the_network() {
    let env = TestEnv::new();
    for mode in Mode::ALL {
        for setting in [None, Some("on"), Some("off")] {
            let argv = bash_argv(&env, mode, setting, true);
            assert!(!argv.iter().any(|a| a == "--share-net"), "{mode:?} {setting:?}");
        }
    }
}

#[test]
fn a_spec_asking_for_the_network_is_clamped_when_it_is_off() {
    // An extension's `exec` with `net: true` goes through the same sandbox.
    let env = TestEnv::new();
    let inner = Arc::new(Recording {
        home: env.home(),
        argv: Mutex::new(Vec::new()),
    });
    let spec = ExecSpec {
        command: "true".into(),
        cwd: env.work(),
        writable: true,
        net: true,
        timeout_s: None,
        env: Vec::new(),
    };
    for (setting, offline, net) in [(true, false, true), (false, false, false), (true, true, false)] {
        let sb = NetSandbox::new(inner.clone(), setting, offline);
        assert_eq!(sb.network(), net);
        let argv = sb.bwrap_args(&spec);
        assert_eq!(argv.iter().any(|a| a == "--share-net"), net);
        sb.run(&spec, &CancelToken::new(), &mut |_| {});
        let ran = inner.argv.lock().unwrap().pop().unwrap();
        assert_eq!(ran.iter().any(|a| a == "--share-net"), net);
    }
}

#[test]
fn setting_values() {
    assert!(sandbox_net_setting(None));
    assert!(sandbox_net_setting(Some("on")));
    assert!(!sandbox_net_setting(Some("off")));
    // A sandbox that knows nothing of the setting keeps the old rule: no network.
    assert!(!oxa_sandbox::NoSandbox.network());
}
