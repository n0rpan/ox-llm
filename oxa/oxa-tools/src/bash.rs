//! A9: the `bash` tool. Owner: task A9. Signatures below are frozen; bodies are yours.
// ported from pi packages/coding-agent/src/core/tools/bash.ts (execution replaced by the sandbox)
use crate::descriptions::{self, errors};
use oxa_proto::{Access, ExecSpec, Mode, Tool, ToolCtx, ToolOutput, ToolSpec, Value};
use serde_json::json;
use std::path::{Path, PathBuf};

/// `bash {command, timeout?}`: runs through `ToolCtx::sandbox` (never directly). `writable` = mode is not Plan; `net` = [`net_for`] (O-167(b): the sandbox network by mode and `oxa.sandbox.net`; `access` keeps `net: false`). Output streams to `live`; the result text is the merged output plus pi's exit-code / timeout lines; no timing text (W4). If `sandbox.check()` fails the result is that error (`is_error`). `ToolOutput::ui` = `{"exit": n|null, "timedOut": bool}`. Environment: `ToolCtx::env`.
#[derive(Debug, Clone)]
pub struct BashTool {
    spec: ToolSpec,
    shell: ShellConfig,
}

impl BashTool {
    pub fn new() -> BashTool {
        BashTool {
            spec: crate::spec_of(&descriptions::BASH),
            shell: ShellConfig::default(),
        }
    }
    /// H: the tool with pi's `shellPath` / `shellCommandPrefix` settings.
    pub fn with_shell(shell: ShellConfig) -> BashTool {
        BashTool {
            spec: crate::spec_of(&descriptions::BASH),
            shell,
        }
    }
}

/// H: pi's `shellPath` and `shellCommandPrefix` settings (settings.md "Shell"), shared by the
/// `bash` tool and the user's `!cmd`. The sandbox always runs `bash -c <text>`; this builds
/// that text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShellConfig {
    /// The custom shell executable (`shellPath`, `~` already expanded). It must be visible
    /// inside the sandbox: `/` is read-only there, home and `/tmp` are hidden.
    pub shell_path: Option<PathBuf>,
    /// `shellCommandPrefix`, prepended to every command on its own line (pi bash.ts).
    pub command_prefix: Option<String>,
}

impl ShellConfig {
    /// The text the sandbox runs for `command`: pi's `${prefix}\n${command}`, and with a
    /// `shell_path` `exec '<shell>' -c '<that>'`. `Err` is pi's text for a missing shell
    /// (`Custom shell path not found: <path>`, utils/shell.ts).
    pub fn command(&self, command: &str) -> Result<String, String> {
        let resolved = match self.command_prefix.as_deref().filter(|p| !p.is_empty()) {
            Some(prefix) => format!("{prefix}\n{command}"),
            None => command.to_string(),
        };
        match &self.shell_path {
            None => Ok(resolved),
            Some(shell) => {
                if !shell.exists() {
                    return Err(format!("Custom shell path not found: {}", shell.display()));
                }
                Ok(format!(
                    "exec {} -c {}",
                    single_quote(&shell.to_string_lossy()),
                    single_quote(&resolved)
                ))
            }
        }
    }
}

/// `s` as one POSIX shell word.
fn single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

impl Default for BashTool {
    fn default() -> Self {
        Self::new()
    }
}

/// The validated arguments of one call.
#[derive(Debug, Clone, PartialEq)]
struct BashArgs {
    command: String,
    /// Whole seconds, rounded up; `None` = no timeout.
    timeout_s: Option<u32>,
}

/// pi's validation text for a missing required property (TypeBox's message, verbatim shape).
const MISSING_COMMAND: &str =
    "Validation failed for tool \"bash\":\n  - must have required property 'command'";

fn parse(args: &Value) -> Result<BashArgs, String> {
    let command = args
        .get("command")
        .and_then(Value::as_str)
        .ok_or_else(|| MISSING_COMMAND.to_string())?
        .to_string();
    let timeout_s = match args.get("timeout") {
        None | Some(Value::Null) => None,
        Some(v) => {
            // pi: a finite number of seconds greater than zero, else refused.
            let t = v
                .as_f64()
                .filter(|t| t.is_finite() && *t > 0.0)
                .ok_or_else(|| errors::BASH_BAD_TIMEOUT.to_string())?;
            Some(t.ceil().min(f64::from(u32::MAX)) as u32)
        }
    };
    Ok(BashArgs { command, timeout_s })
}

/// Split off the longest prefix of `pending` that is valid UTF-8 (keeping an incomplete trailing
/// sequence for the next chunk); invalid bytes become U+FFFD.
fn take_valid_utf8(pending: &mut Vec<u8>) -> String {
    let mut out = String::new();
    loop {
        match std::str::from_utf8(pending) {
            Ok(s) => {
                out.push_str(s);
                pending.clear();
                break;
            }
            Err(e) => {
                let valid = e.valid_up_to();
                out.push_str(&String::from_utf8_lossy(&pending[..valid]));
                match e.error_len() {
                    Some(n) => {
                        out.push('\u{FFFD}');
                        pending.drain(..valid + n);
                    }
                    None => {
                        pending.drain(..valid);
                        break;
                    }
                }
            }
        }
    }
    out
}

/// TOOL-DIET for the `bash` result text (`ToolTune::diet`): each run of three or more blank
/// lines (empty or whitespace-only) is cut to its first two. Nothing else changes: every other
/// line keeps its bytes, trailing blanks included (a diff's context lines and a file shown with
/// `cat` stay copyable), and a run of one or two blank lines stays as it is. Deterministic.
pub(crate) fn diet_output(text: &str) -> String {
    let (body, tail) = match text.strip_suffix('\n') {
        Some(body) => (body, "\n"),
        None => (text, ""),
    };
    let mut out: Vec<&str> = Vec::new();
    let mut blanks = 0usize;
    for line in body.split('\n') {
        if line.trim().is_empty() {
            blanks += 1;
            if blanks > 2 {
                continue;
            }
        } else {
            blanks = 0;
        }
        out.push(line);
    }
    format!("{}{tail}", out.join("\n"))
}

/// A hedged note for `No such file or directory` on a path under the real home that is outside
/// the workspace. The sandbox may hide unbound home paths, but a missing file under a mounted
/// directory (a read-only toolchain bind, a configured bind) is simply absent, so the note never
/// claims the path is unmounted; it tells the reader to check the parent directory first.
/// Only the workspace (`cwd`) counts as visible: neither `ToolCtx` nor `Sandbox` exposes the
/// bound/extra paths, so no mount introspection is done here.
pub(crate) fn unmounted_home_note(output: &str, home: &str, cwd: &Path) -> Option<String> {
    let home = home.trim_end_matches('/');
    if home.is_empty() {
        return None;
    }
    let prefix = format!("{home}/");
    for line in output.lines().filter(|l| l.contains("No such file or directory")) {
        for tok in line.split(|c: char| c.is_whitespace() || matches!(c, '\'' | '"' | '`' | ':')) {
            if tok.starts_with(&prefix) && !Path::new(tok).starts_with(cwd) {
                return Some(format!(
                    "{tok}: not found. It may be hidden because the path is outside the sandbox mounts (only the workspace and listed binds are visible), or it may simply not exist; check whether its parent directory exists before concluding it is unmounted"
                ));
            }
        }
    }
    None
}

/// O-167(b): the network of a bash command. `sandbox_net` is the sandbox's answer
/// (`oxa.sandbox.net`, default on, and never on while offline). Per mode: plan = never (a
/// read-only mode does not gain the network silently); workspace = `sandbox_net`; ask =
/// `sandbox_net` (the user approves every command). The model cannot change it: it is not an
/// argument and not an `Access` grant (`access` keeps `net: false`, so the policy table and
/// its prompts are unchanged).
pub fn net_for(mode: Mode, sandbox_net: bool) -> bool {
    sandbox_net && !matches!(mode, Mode::Plan)
}

impl Tool for BashTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    /// `[Access::Exec { command, net: false }]`.
    fn access(&self, args: &Value, _cx: &ToolCtx) -> Result<Vec<Access>, String> {
        let a = parse(args)?;
        Ok(vec![Access::Exec {
            command: a.command,
            net: false,
        }])
    }
    fn run(&self, args: &Value, cx: &ToolCtx, live: &mut dyn FnMut(&str)) -> ToolOutput {
        let a = match parse(args) {
            Ok(a) => a,
            Err(e) => return ToolOutput::error(e),
        };
        if let Err(e) = cx.sandbox.check() {
            return ToolOutput::error(e.to_string());
        }
        let command = match self.shell.command(&a.command) {
            Ok(c) => c,
            Err(e) => return ToolOutput::error(e),
        };
        let spec = ExecSpec {
            command,
            cwd: cx.cwd.to_path_buf(),
            writable: !matches!(cx.mode, Mode::Plan),
            net: net_for(cx.mode, cx.sandbox.network()),
            // F5: the call's own timeout wins; else the epoch's default (0 = none, pi).
            timeout_s: a
                .timeout_s
                .or_else(|| (cx.tune.bash_timeout_s > 0).then_some(cx.tune.bash_timeout_s)),
            env: cx.env.to_vec(),
        };
        let mut pending: Vec<u8> = Vec::new();
        let mut sink = |chunk: &[u8]| {
            pending.extend_from_slice(chunk);
            let text = take_valid_utf8(&mut pending);
            if !text.is_empty() {
                live(&text);
            }
        };
        let res = cx.sandbox.run(&spec, cx.cancel, &mut sink);
        if !pending.is_empty() {
            live(&String::from_utf8_lossy(&pending));
        }

        let output = String::from_utf8_lossy(&res.output).into_owned();
        // TOOL-DIET: the result text only; the raw bytes went to `live` above.
        let output = if cx.tune.diet {
            diet_output(&output)
        } else {
            output
        };
        let ui = json!({ "exit": res.exit, "timedOut": res.timed_out });
        let note = std::env::var("HOME")
            .ok()
            .and_then(|h| unmounted_home_note(&output, &h, cx.cwd));
        let shown = if output.is_empty() {
            errors::BASH_NO_OUTPUT.to_string()
        } else if let Some(n) = note {
            format!("{}\n[{n}]", output.trim_end())
        } else {
            output.clone()
        };

        if res.timed_out {
            let secs = spec.timeout_s.unwrap_or(0);
            return ToolOutput {
                text: errors::bash_with_status(&output, &errors::bash_timed_out(secs)),
                is_error: true,
                ui,
            };
        }
        if cx.cancel.is_cancelled() {
            return ToolOutput {
                text: errors::bash_with_status(&output, errors::BASH_ABORTED),
                is_error: true,
                ui,
            };
        }
        match res.exit {
            None => ToolOutput {
                text: errors::bash_with_status(&shown, errors::BASH_NO_EXIT_CODE),
                is_error: true,
                ui,
            },
            Some(0) => ToolOutput {
                text: shown,
                is_error: false,
                ui,
            },
            Some(code) => ToolOutput {
                text: errors::bash_with_status(&shown, &errors::bash_exit(code)),
                is_error: true,
                ui,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// H (SET-055, SET-056): pi's `${prefix}\n${command}`, the custom shell through `exec`, and
    /// pi's error for a missing shell.
    #[test]
    fn shell_config_builds_the_sandboxed_command() {
        let plain = ShellConfig::default();
        assert_eq!(plain.command("echo hi").unwrap(), "echo hi");
        let prefixed = ShellConfig {
            shell_path: None,
            command_prefix: Some("shopt -s expand_aliases".to_string()),
        };
        assert_eq!(prefixed.command("ll").unwrap(), "shopt -s expand_aliases\nll");
        let shell = ShellConfig {
            shell_path: Some(PathBuf::from("/bin/sh")),
            command_prefix: Some("X=1".to_string()),
        };
        assert_eq!(
            shell.command("echo 'a'").unwrap(),
            r#"exec '/bin/sh' -c 'X=1
echo '\''a'\'''"#
        );
        let missing = ShellConfig {
            shell_path: Some(PathBuf::from("/nonexistent/shell-h")),
            command_prefix: None,
        };
        assert_eq!(
            missing.command("x").unwrap_err(),
            "Custom shell path not found: /nonexistent/shell-h"
        );
    }
    use oxa_proto::{CancelToken, ExecResult, Mode, OxaError, Sandbox};
    use std::path::Path;
    use std::sync::Mutex;

    /// A canned sandbox: `check` result, a fixed `run` result, streamed chunks, and the spec seen.
    struct Fake {
        check: Option<OxaError>,
        result: ExecResult,
        chunks: Vec<Vec<u8>>,
        seen: Mutex<Option<ExecSpec>>,
    }

    impl Fake {
        fn ok(output: &[u8], exit: Option<i32>) -> Fake {
            Fake {
                check: None,
                result: ExecResult {
                    output: output.to_vec(),
                    exit,
                    timed_out: false,
                },
                chunks: vec![output.to_vec()],
                seen: Mutex::new(None),
            }
        }
    }

    impl Sandbox for Fake {
        fn check(&self) -> Result<(), OxaError> {
            match &self.check {
                Some(e) => Err(e.clone()),
                None => Ok(()),
            }
        }
        fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
            Vec::new()
        }
        fn run(&self, s: &ExecSpec, _c: &CancelToken, live: &mut dyn FnMut(&[u8])) -> ExecResult {
            *self.seen.lock().unwrap() = Some(s.clone());
            for c in &self.chunks {
                live(c);
            }
            self.result.clone()
        }
    }

    fn run_with(sb: &Fake, args: Value, mode: Mode, cancel: &CancelToken) -> (ToolOutput, String) {
        let cwd = Path::new("/work");
        let env = vec![("OXA_SESSION_ID".to_string(), "s".to_string())];
        let cx = ToolCtx {
            cwd,
            spill_dir: cwd,
            sandbox: sb,
            mode,
            cancel,
            env: &env,
            tune: Default::default(),
        };
        let mut streamed = String::new();
        let out = BashTool::new().run(&args, &cx, &mut |t| streamed.push_str(t));
        (out, streamed)
    }

    #[test]
    fn note_for_unmounted_home_path_only() {
        let cwd = Path::new("/home/u/proj");
        let msg = "touch: cannot touch '/home/u/lab/probe': No such file or directory\n";
        let note = unmounted_home_note(msg, "/home/u", cwd).expect("note");
        assert!(note.contains("/home/u/lab/probe: not found."));
        assert!(note.contains("may"));
        assert!(note.contains("parent directory"));
        assert!(!note.contains("path outside the sandbox (not mounted)"));
        let mounted_parent = "ls: cannot access '/home/u/.cargo/bin/nothing': No such file or directory\n";
        let hedged = unmounted_home_note(mounted_parent, "/home/u", cwd).expect("note");
        assert!(hedged.contains("may"));
        assert!(hedged.contains("parent directory"));
        assert!(!hedged.contains("path outside the sandbox (not mounted)"));
        let inside = "cat: /home/u/proj/x: No such file or directory\n";
        assert!(unmounted_home_note(inside, "/home/u", cwd).is_none());
        let other = "cat: /etc/x: No such file or directory\n";
        assert!(unmounted_home_note(other, "/home/u", cwd).is_none());
        assert!(unmounted_home_note("/home/u/lab/p: Permission denied", "/home/u", cwd).is_none());
    }

    #[test]
    fn refuses_without_sandbox_with_the_error_text() {
        let err = OxaError::new(
            oxa_proto::codes::NO_SANDBOX,
            "bwrap is not available; bash is disabled",
        )
        .with_hint("install bubblewrap");
        let sb = Fake {
            check: Some(err.clone()),
            ..Fake::ok(b"", Some(0))
        };
        let (out, _) = run_with(
            &sb,
            json!({"command": "ls"}),
            Mode::Workspace,
            &CancelToken::new(),
        );
        assert!(out.is_error);
        assert_eq!(out.text, err.to_string());
        assert!(
            sb.seen.lock().unwrap().is_none(),
            "nothing runs without a sandbox"
        );
    }

    #[test]
    fn exit_code_and_empty_output_lines() {
        let sb = Fake::ok(b"hi", Some(3));
        let (out, _) = run_with(&sb, json!({"command": "x"}), Mode::Workspace, &CancelToken::new());
        assert!(out.is_error);
        assert_eq!(out.text, "hi\n\nCommand exited with code 3");
        assert_eq!(out.ui, json!({"exit": 3, "timedOut": false}));

        let sb = Fake::ok(b"", Some(0));
        let (out, _) = run_with(
            &sb,
            json!({"command": "true"}),
            Mode::Workspace,
            &CancelToken::new(),
        );
        assert!(!out.is_error);
        assert_eq!(out.text, "(no output)");

        let sb = Fake::ok(b"", Some(1));
        let (out, _) = run_with(
            &sb,
            json!({"command": "false"}),
            Mode::Workspace,
            &CancelToken::new(),
        );
        assert_eq!(out.text, "(no output)\n\nCommand exited with code 1");
    }

    #[test]
    fn timeout_line_and_rounding() {
        let mut sb = Fake::ok(b"partial", None);
        sb.result.timed_out = true;
        let (out, _) = run_with(
            &sb,
            json!({"command": "sleep 9", "timeout": 1.5}),
            Mode::Workspace,
            &CancelToken::new(),
        );
        assert!(out.is_error);
        assert_eq!(out.text, "partial\n\nCommand timed out after 2 seconds");
        assert_eq!(out.ui, json!({"exit": null, "timedOut": true}));
        assert_eq!(sb.seen.lock().unwrap().as_ref().unwrap().timeout_s, Some(2));
    }

    #[test]
    fn cancel_and_missing_exit_code_lines() {
        let sb = Fake::ok(b"", None);
        let cancel = CancelToken::new();
        cancel.cancel();
        let (out, _) = run_with(&sb, json!({"command": "sleep 9"}), Mode::Workspace, &cancel);
        assert!(out.is_error);
        assert_eq!(out.text, "Command aborted");

        let sb = Fake::ok(b"", None);
        let (out, _) = run_with(
            &sb,
            json!({"command": "kill -9 $$"}),
            Mode::Workspace,
            &CancelToken::new(),
        );
        assert!(out.is_error);
        assert_eq!(out.text, "(no output)\n\nCommand terminated without an exit code");
    }

    #[test]
    fn plan_mode_is_read_only_and_net_is_off() {
        let sb = Fake::ok(b"", Some(0));
        let (_, _) = run_with(&sb, json!({"command": "ls"}), Mode::Plan, &CancelToken::new());
        let seen = sb.seen.lock().unwrap().clone().expect("spec");
        assert!(!seen.writable);
        assert!(!seen.net);
        assert_eq!(seen.command, "ls");
        assert_eq!(seen.env, vec![("OXA_SESSION_ID".to_string(), "s".to_string())]);

        let sb = Fake::ok(b"", Some(0));
        let _ = run_with(
            &sb,
            json!({"command": "ls"}),
            Mode::Workspace,
            &CancelToken::new(),
        );
        assert!(sb.seen.lock().unwrap().as_ref().expect("spec").writable);
    }

    #[test]
    fn invalid_arguments_are_refused() {
        let sb = Fake::ok(b"", Some(0));
        let (out, _) = run_with(&sb, json!({"timeout": 3}), Mode::Workspace, &CancelToken::new());
        assert!(out.is_error);
        assert_eq!(out.text, MISSING_COMMAND);
        for bad in [json!(0), json!(-1), json!("soon")] {
            let (out, _) = run_with(
                &sb,
                json!({"command": "ls", "timeout": bad}),
                Mode::Workspace,
                &CancelToken::new(),
            );
            assert!(out.is_error);
            assert_eq!(out.text, errors::BASH_BAD_TIMEOUT);
        }
        assert!(sb.seen.lock().unwrap().is_none());
        let cwd = Path::new("/work");
        let cancel = CancelToken::new();
        let cx = ToolCtx {
            cwd,
            spill_dir: cwd,
            sandbox: &sb,
            mode: Mode::Workspace,
            cancel: &cancel,
            env: &[],
            tune: Default::default(),
        };
        let got = BashTool::new().access(&json!({"command": "ls", "timeout": 0}), &cx);
        assert_eq!(got, Err(errors::BASH_BAD_TIMEOUT.to_string()));
    }

    #[test]
    fn access_is_exec_without_net() {
        let sb = Fake::ok(b"", Some(0));
        let cwd = Path::new("/work");
        let cancel = CancelToken::new();
        let cx = ToolCtx {
            cwd,
            spill_dir: cwd,
            sandbox: &sb,
            mode: Mode::Ask,
            cancel: &cancel,
            env: &[],
            tune: Default::default(),
        };
        let got = BashTool::new()
            .access(&json!({"command": "cargo test"}), &cx)
            .expect("access");
        assert_eq!(
            got,
            vec![Access::Exec {
                command: "cargo test".to_string(),
                net: false
            }]
        );
    }

    #[test]
    fn streamed_chunks_split_utf8_are_joined() {
        let mut sb = Fake::ok(b"", Some(0));
        sb.chunks = vec![vec![0xE2, 0x82], vec![0xAC, b'!', 0xFF]];
        sb.result.output = "€!\u{FFFD}".as_bytes().to_vec();
        let (out, streamed) = run_with(&sb, json!({"command": "x"}), Mode::Workspace, &CancelToken::new());
        assert_eq!(streamed, "€!\u{FFFD}");
        assert_eq!(out.text, "€!\u{FFFD}");
    }

    #[test]
    fn take_valid_utf8_keeps_an_incomplete_tail() {
        let mut p = vec![b'a', 0xE2, 0x82];
        assert_eq!(take_valid_utf8(&mut p), "a");
        assert_eq!(p, vec![0xE2, 0x82]);
        p.push(0xAC);
        assert_eq!(take_valid_utf8(&mut p), "€");
        assert!(p.is_empty());
    }

    fn run_tuned(sb: &Fake, args: Value, tune: oxa_proto::ToolTune) -> ToolOutput {
        let cwd = Path::new("/work");
        let env: Vec<(String, String)> = Vec::new();
        let cancel = CancelToken::new();
        let cx = ToolCtx {
            cwd,
            spill_dir: cwd,
            sandbox: sb,
            mode: Mode::Workspace,
            cancel: &cancel,
            env: &env,
            tune,
        };
        BashTool::new().run(&args, &cx, &mut |_| {})
    }

    /// F5: `bash_timeout_s` applies only when the call names no timeout; the timeout text is
    /// the existing one.
    #[test]
    fn tune_timeout_is_the_default_and_an_explicit_timeout_wins() {
        let tuned = oxa_proto::ToolTune {
            bash_timeout_s: 7,
            ..Default::default()
        };
        let sb = Fake::ok(b"", Some(0));
        let _ = run_tuned(&sb, json!({"command": "sleep 9"}), tuned);
        assert_eq!(sb.seen.lock().unwrap().as_ref().unwrap().timeout_s, Some(7));

        let sb = Fake::ok(b"", Some(0));
        let _ = run_tuned(&sb, json!({"command": "sleep 9", "timeout": 2}), tuned);
        assert_eq!(sb.seen.lock().unwrap().as_ref().unwrap().timeout_s, Some(2));

        let sb = Fake::ok(b"", Some(0));
        let _ = run_tuned(&sb, json!({"command": "sleep 9"}), Default::default());
        assert_eq!(sb.seen.lock().unwrap().as_ref().unwrap().timeout_s, None);

        let mut sb = Fake::ok(b"partial", None);
        sb.result.timed_out = true;
        let out = run_tuned(&sb, json!({"command": "sleep 9"}), tuned);
        assert!(out.is_error);
        assert_eq!(out.text, "partial\n\nCommand timed out after 7 seconds");
    }

    /// Diet: the default keeps the bytes; the diet cuts runs of three or more blank lines to two
    /// and changes nothing else.
    #[test]
    fn diet_cuts_long_blank_runs_only() {
        let raw = "Compiling a v1\n   \n\t\n\nerror: boom  \n\n\n\n\nnext\t\t\n";
        let expected = "Compiling a v1\n   \n\t\nerror: boom  \n\n\nnext\t\t\n";
        let sb = Fake::ok(raw.as_bytes(), Some(0));
        let plain = run_tuned(&sb, json!({"command": "x"}), Default::default());
        assert_eq!(plain.text, raw);

        let sb = Fake::ok(raw.as_bytes(), Some(0));
        let diet = oxa_proto::ToolTune {
            diet: true,
            ..Default::default()
        };
        let out = run_tuned(&sb, json!({"command": "x"}), diet);
        assert_eq!(out.text, expected);
        assert!(!out.is_error);

        // Deterministic and idempotent; one or two blank lines and trailing blanks are kept.
        assert_eq!(diet_output(expected), expected);
        assert_eq!(diet_output(""), "");
        assert_eq!(diet_output("\n"), "\n");
        assert_eq!(diet_output("a\n\n\nb"), "a\n\n\nb");
        assert_eq!(diet_output("a \n\n\n\n\n b\t"), "a \n\n\n b\t");
    }

    /// The exit-status line follows the shaped output unchanged.
    #[test]
    fn diet_keeps_the_status_lines() {
        let diet = oxa_proto::ToolTune {
            diet: true,
            ..Default::default()
        };
        let sb = Fake::ok(b"x  \n\n\n\n\n", Some(3));
        let out = run_tuned(&sb, json!({"command": "x"}), diet);
        assert!(out.is_error);
        assert!(out.text.starts_with("x  \n"), "{:?}", out.text);
        assert!(out.text.ends_with("Command exited with code 3"), "{:?}", out.text);
        assert_eq!(out.ui, json!({"exit": 3, "timedOut": false}));
    }
}
