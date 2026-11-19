//! E5 acceptance tests: Exec hooks.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_ext::{hook_stdin, parse_exec_hooks, reply_from_exit, ExecHooks, EXEC_HOOK_TIMEOUT_S};
use oxa_fake::TestEnv;
use oxa_proto::{
    codes, CancelToken, ExecResult, ExecSpec, HeadlessUi, HookPoint, HookReply, Hooks, Level, OxaError,
    Sandbox, Value,
};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A sandbox double that records every spec it is given and runs `bash -c` directly (no bwrap),
/// with stdout going to a file so that a killed hook cannot hold a pipe open.
struct RecordingSandbox {
    specs: Mutex<Vec<ExecSpec>>,
    usable: bool,
    scratch: PathBuf,
    counter: Mutex<usize>,
}

impl RecordingSandbox {
    fn new(scratch: &Path, usable: bool) -> Arc<RecordingSandbox> {
        std::fs::create_dir_all(scratch).unwrap();
        Arc::new(RecordingSandbox {
            specs: Mutex::new(Vec::new()),
            usable,
            scratch: scratch.to_path_buf(),
            counter: Mutex::new(0),
        })
    }
    fn specs(&self) -> Vec<ExecSpec> {
        self.specs.lock().unwrap().clone()
    }
}

impl Sandbox for RecordingSandbox {
    fn check(&self) -> Result<(), OxaError> {
        if self.usable {
            Ok(())
        } else {
            Err(OxaError::new(codes::NO_SANDBOX, "bwrap is not installed"))
        }
    }
    fn bwrap_args(&self, s: &ExecSpec) -> Vec<String> {
        vec![
            "--unshare-all".to_string(),
            "--die-with-parent".to_string(),
            "--chdir".to_string(),
            s.cwd.display().to_string(),
            "--".to_string(),
            "bash".to_string(),
            "-c".to_string(),
            s.command.clone(),
        ]
    }
    fn run(&self, s: &ExecSpec, _c: &CancelToken, live: &mut dyn FnMut(&[u8])) -> ExecResult {
        self.specs.lock().unwrap().push(s.clone());
        let n = {
            let mut c = self.counter.lock().unwrap();
            *c += 1;
            *c
        };
        let out_path = self.scratch.join(format!("out-{n}.bin"));
        let out = File::create(&out_path).unwrap();
        let mut child = Command::new("bash")
            .arg("-c")
            .arg(&s.command)
            .current_dir(&s.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::from(out))
            .spawn()
            .unwrap();
        let deadline = s
            .timeout_s
            .map(|t| Instant::now() + Duration::from_secs(u64::from(t)));
        let mut timed_out = false;
        let status = loop {
            if let Some(st) = child.try_wait().unwrap() {
                break Some(st);
            }
            if deadline.is_some_and(|d| Instant::now() >= d) {
                let _ = child.kill();
                timed_out = true;
                let _ = child.wait();
                break None;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let output = std::fs::read(&out_path).unwrap_or_default();
        live(&output);
        ExecResult {
            output,
            exit: status.and_then(|s| s.code()),
            timed_out,
            unsandboxed: false,
        }
    }
}

fn hooks_from(settings: &str, sandbox: Arc<dyn Sandbox>, cwd: PathBuf) -> ExecHooks {
    let value: Value = serde_json::from_str(settings).unwrap();
    let (decls, warnings) = parse_exec_hooks(&value);
    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
    ExecHooks::new(decls, sandbox, cwd)
}

/// stdin payload, stdout reply mapping
#[test]
fn json_in_out_contract() {
    let env = TestEnv::new();
    let work = env.work();
    std::fs::create_dir_all(&work).unwrap();
    let sandbox = RecordingSandbox::new(&env.tmp(), true);
    let seen = env.tmp().join("stdin.json");
    let payload = serde_json::json!({"toolName": "bash", "args": {"command": "ls"}});
    let mut ui = HeadlessUi::default();

    // Stdin contract: the hook records its stdin and prints nothing -> Continue.
    let settings = format!(r#"{{"tool_call": "cat > '{}'"}}"#, seen.display());
    let hooks = hooks_from(&settings, sandbox.clone(), work.clone());
    assert_eq!(
        hooks.fire(HookPoint::ToolCall, &payload, &mut ui),
        HookReply::Continue
    );
    let stdin = std::fs::read(&seen).unwrap();
    assert_eq!(stdin, hook_stdin(HookPoint::ToolCall, &work, &payload));
    let parsed: Value = serde_json::from_slice(&stdin).unwrap();
    assert_eq!(parsed["event"], "tool_call");
    assert_eq!(parsed["cwd"], work.display().to_string());
    assert_eq!(parsed["payload"], payload);
    assert_eq!(stdin.last(), Some(&b'\n'));

    // Stdout contract: a JSON object on stdout is mapped with the JS reply shapes (E2's
    // `reply_from_js`), then passes the W-guard.
    let settings = r#"{"tool_call": "printf '%s' '{\"block\": true, \"reason\": \"policy\"}'"}"#;
    let hooks = hooks_from(settings, sandbox, work);
    assert_eq!(
        hooks.fire(HookPoint::ToolCall, &payload, &mut ui),
        HookReply::Block("policy".to_string())
    );
}

/// argv through Sandbox
#[test]
fn runs_in_bwrap() {
    let env = TestEnv::new();
    let work = env.work();
    std::fs::create_dir_all(&work).unwrap();
    let sandbox = RecordingSandbox::new(&env.tmp(), true);
    let hooks = hooks_from(r#"{"turn_start": "echo hi"}"#, sandbox.clone(), work.clone());
    let mut ui = HeadlessUi::default();
    let reply = hooks.fire(HookPoint::TurnStart, &serde_json::json!({}), &mut ui);
    assert_eq!(reply, HookReply::Continue);

    let specs = sandbox.specs();
    assert_eq!(specs.len(), 1);
    let s = &specs[0];
    assert!(
        s.command.contains("echo hi"),
        "hook command reaches the sandbox: {}",
        s.command
    );
    assert_eq!(s.cwd, work);
    // Wave H (EXT-091): a hook may write in the working directory (the rest stays read-only).
    assert!(s.writable, "hooks may write in the session's working directory");
    assert!(!s.net, "hooks have no network");
    assert_eq!(s.timeout_s, Some(EXEC_HOOK_TIMEOUT_S));
    assert!(s.env.is_empty());
    // The sandbox owns the argv: the spec it was given is what a bwrap argv is built from.
    let argv = sandbox.bwrap_args(s);
    assert_eq!(argv.last().map(String::as_str), Some(s.command.as_str()));
}

/// hook killed, treated as Continue with a notice
#[test]
fn timeout() {
    let env = TestEnv::new();
    let work = env.work();
    std::fs::create_dir_all(&work).unwrap();
    let sandbox = RecordingSandbox::new(&env.tmp(), true);
    let hooks = hooks_from(
        r#"{"turn_start": {"command": "sleep 5", "timeout": 1}}"#,
        sandbox.clone(),
        work,
    );
    let mut ui = HeadlessUi::default();
    let started = Instant::now();
    let reply = hooks.fire(HookPoint::TurnStart, &serde_json::json!({}), &mut ui);
    assert_eq!(reply, HookReply::Continue);
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "the hook was killed, not waited for"
    );
    assert!(
        ui.notes
            .iter()
            .any(|(l, t)| *l == Level::Warning && t.contains("timed out")),
        "notice: {:?}",
        ui.notes
    );
    assert_eq!(sandbox.specs()[0].timeout_s, Some(1));
}

/// tool_call blocks, observers do not
#[test]
fn failing_hook_blocks_only_where_documented() {
    // Pure mapping: exit 2 blocks at ToolCall / UserBash, cancels at SessionBefore*, is inert
    // elsewhere; any other failure continues with a warning.
    let (r, w) = reply_from_exit(HookPoint::ToolCall, Some(2), false, b"nope\n");
    assert_eq!(r, HookReply::Block("nope".to_string()));
    assert!(w.is_none());
    let (r, _) = reply_from_exit(HookPoint::ToolCall, Some(2), false, b"");
    assert_eq!(r, HookReply::Block("blocked by hook".to_string()));
    let (r, _) = reply_from_exit(HookPoint::UserBash, Some(2), false, b"no bash");
    assert_eq!(r, HookReply::Block("no bash".to_string()));
    let (r, _) = reply_from_exit(HookPoint::SessionBeforeSwitch, Some(2), false, b"stay");
    assert_eq!(r, HookReply::Cancel("stay".to_string()));
    let (r, w) = reply_from_exit(HookPoint::MessageEnd, Some(2), false, b"x");
    assert_eq!(r, HookReply::Continue, "observers never block");
    assert!(w.is_none());
    let (r, w) = reply_from_exit(HookPoint::ToolCall, Some(1), false, b"x");
    assert_eq!(r, HookReply::Continue, "a failing hook never blocks by accident");
    assert!(w.is_some());
    let (r, w) = reply_from_exit(HookPoint::ToolCall, Some(0), true, b"{\"block\":true}");
    assert_eq!(r, HookReply::Continue, "a timed out hook never blocks");
    assert!(w.is_some());
    let (r, w) = reply_from_exit(HookPoint::ToolCall, Some(0), false, b"not json");
    assert_eq!(r, HookReply::Continue);
    assert!(w.is_some());
    let (r, w) = reply_from_exit(HookPoint::ToolCall, Some(0), false, b"");
    assert_eq!(r, HookReply::Continue);
    assert!(w.is_none());

    // Through fire: an observer that exits 2 does not stop the run.
    let env = TestEnv::new();
    let work = env.work();
    std::fs::create_dir_all(&work).unwrap();
    let sandbox = RecordingSandbox::new(&env.tmp(), true);
    let hooks = hooks_from(r#"{"message_end": "echo stop; exit 2"}"#, sandbox, work);
    let mut ui = HeadlessUi::default();
    assert_eq!(
        hooks.fire(HookPoint::MessageEnd, &serde_json::json!({}), &mut ui),
        HookReply::Continue
    );
}

/// settings typo
#[test]
fn unknown_event_name_diagnostic() {
    let value: Value =
        serde_json::from_str(r#"{"tool_cal": "a", "tool_call": "b", "turn_start": 5}"#).unwrap();
    let (decls, warnings) = parse_exec_hooks(&value);
    assert_eq!(decls.len(), 1, "only the valid tool_call entry survives");
    assert_eq!(decls[0].point, HookPoint::ToolCall);
    assert_eq!(decls[0].command, "b");
    assert_eq!(
        warnings.len(),
        2,
        "one for the typo, one for the malformed entry: {warnings:?}"
    );
    assert!(warnings[0].contains("tool_cal"));
    assert!(
        warnings[0].contains("tool_call"),
        "suggests the nearest event: {}",
        warnings[0]
    );
    assert!(warnings[1].contains("turn_start"));
}

#[test]
fn declarations_keep_object_order_and_defaults() {
    let value: Value =
        serde_json::from_str(r#"{"turn_end": "a", "tool_call": ["b", {"command": "c", "timeout": 7}]}"#)
            .unwrap();
    let (decls, warnings) = parse_exec_hooks(&value);
    assert!(warnings.is_empty());
    let got: Vec<(HookPoint, &str, u32)> = decls
        .iter()
        .map(|d| (d.point, d.command.as_str(), d.timeout_s))
        .collect();
    assert_eq!(
        got,
        vec![
            (HookPoint::TurnEnd, "a", EXEC_HOOK_TIMEOUT_S),
            (HookPoint::ToolCall, "b", EXEC_HOOK_TIMEOUT_S),
            (HookPoint::ToolCall, "c", 7),
        ]
    );
}

#[test]
fn malformed_entries_are_skipped_with_warnings() {
    let value: Value = serde_json::from_str(
        r#"{"tool_call": {"command": "", "timeout": 1}, "turn_end": {"command": "x", "timeout": 0},
            "agent_end": 3, "input": {"timeout": 2}}"#,
    )
    .unwrap();
    let (decls, warnings) = parse_exec_hooks(&value);
    assert!(decls.is_empty());
    assert_eq!(warnings.len(), 4, "{warnings:?}");
    let (decls, warnings) = parse_exec_hooks(&serde_json::json!("not an object"));
    assert!(decls.is_empty());
    assert_eq!(warnings.len(), 1);
}

#[test]
fn payload_with_quotes_reaches_stdin_byte_exact() {
    let env = TestEnv::new();
    let work = env.work();
    std::fs::create_dir_all(&work).unwrap();
    let sandbox = RecordingSandbox::new(&env.tmp(), true);
    let seen = env.tmp().join("quoted.json");
    let settings = format!(r#"{{"input": "cat > '{}'"}}"#, seen.display());
    let hooks = hooks_from(&settings, sandbox, work.clone());
    let payload = serde_json::json!({"text": "it's a \"test\" with %s and $HOME `id`"});
    let mut ui = HeadlessUi::default();
    assert_eq!(
        hooks.fire(HookPoint::Input, &payload, &mut ui),
        HookReply::Continue
    );
    assert_eq!(
        std::fs::read(&seen).unwrap(),
        hook_stdin(HookPoint::Input, &work, &payload)
    );
}

#[test]
fn missing_sandbox_runs_nothing_and_warns_once() {
    let env = TestEnv::new();
    let work = env.work();
    std::fs::create_dir_all(&work).unwrap();
    let sandbox = RecordingSandbox::new(&env.tmp(), false);
    let marker = env.tmp().join("ran");
    let settings = format!(
        r#"{{"turn_start": ["touch '{0}'", "touch '{0}'"]}}"#,
        marker.display()
    );
    let hooks = hooks_from(&settings, sandbox.clone(), work);
    let mut ui = HeadlessUi::default();
    assert_eq!(
        hooks.fire(HookPoint::TurnStart, &serde_json::json!({}), &mut ui),
        HookReply::Continue
    );
    assert!(!marker.exists(), "no hook ran without a sandbox");
    assert!(sandbox.specs().is_empty());
    assert_eq!(ui.notes.len(), 1, "one warning: {:?}", ui.notes);
}

#[test]
fn hooks_for_other_points_are_not_run() {
    let env = TestEnv::new();
    let work = env.work();
    std::fs::create_dir_all(&work).unwrap();
    let sandbox = RecordingSandbox::new(&env.tmp(), true);
    let hooks = hooks_from(r#"{"turn_end": "true"}"#, sandbox.clone(), work);
    let mut ui = HeadlessUi::default();
    assert_eq!(
        hooks.fire(HookPoint::ToolCall, &serde_json::json!({}), &mut ui),
        HookReply::Continue
    );
    assert!(sandbox.specs().is_empty());
}

#[test]
fn unknown_command_is_not_found() {
    let env = TestEnv::new();
    let sandbox = RecordingSandbox::new(&env.tmp(), true);
    let hooks = hooks_from("{}", sandbox, env.work());
    let mut ui = HeadlessUi::default();
    let err = hooks.run_command("nope", "", &mut ui).unwrap_err();
    assert_eq!(err.code, codes::NOT_FOUND);
    assert_eq!(hooks.name(), "exec-hooks");
}
