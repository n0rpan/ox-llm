//! C6: `!cmd` and `!!cmd`.
// ported from pi packages/coding-agent/src/core/bash-executor.ts and agent-session.ts (executeBash)
use crate::{append_entry, SessionState};
use oxa_proto::{
    codes, AgentEvent, CancelToken, EntryBody, EventSink, ExecSpec, HookPoint, HookReply, Level, OxaError,
    Ui, Value,
};
use oxa_sandbox::{run_argv, run_unsandboxed};
use oxa_tools::strip_ansi;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{ErrorKind, Write as _};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// pi's `DEFAULT_MAX_LINES` for `truncateTail`.
const MAX_LINES: usize = 2000;
/// pi's `DEFAULT_MAX_BYTES` (50 KB) for `truncateTail`.
const MAX_BYTES: usize = 50 * 1024;

/// Run a user command with the user's own rights (`oxa_sandbox::run_unsandboxed`; pi's
/// behaviour [P]). Hook `UserBash` first (`Block`, `ReplaceResult`). Output streams as
/// `BashExecutionUpdate`. The result is appended as one `BashExecution` entry: `!` (not
/// `excluded`) enters the context at the next prompt as an append (the fold places it before
/// the next user message); `!!` never does. Output over pi's limit is truncated for the
/// entry with pi's notice and the full output is saved under `tmp_dir`. Allowed while idle
/// only (`busy` otherwise).
///
/// Notes on this implementation:
/// * A hook `Block(reason)` runs nothing; `reason` becomes the entry's output with no exit code.
/// * A hook `ReplaceResult` supplies the output; its `ui["exit"]` (an integer) becomes the exit code.
/// * The pi notice is the last line of `output`, so it lands inside the fence that
///   `bash_text` writes (the entry has no separate "truncated" field).
pub fn user_bash(
    st: &mut SessionState,
    cmd: &str,
    excluded: bool,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) -> Result<(), OxaError> {
    let shared = st.shared.clone();
    let _claim = Claim::take(&shared.busy)?;
    // We own the run now: clear a cancel left over from an idle `abort`.
    shared.cancel.reset();
    // A headless SIGINT is sticky, including when it arrived before this claim.
    if shared.gate.interrupted.load(Ordering::SeqCst) {
        shared.cancel.cancel();
    }

    let (raw, exit) = if shared.cancel.is_cancelled() {
        ("Command aborted".to_string(), None)
    } else {
        match consult_hooks(st, cmd, excluded, sink, ui) {
            Some(replaced) => (replaced.text, replaced.exit),
            None => run_command(st, cmd, sink, &shared.cancel),
        }
    };
    let output = finish_output(&st.spill_dir, &raw);
    let entry = append_entry(
        st,
        EntryBody::BashExecution {
            command: cmd.to_string(),
            output,
            exit,
            excluded,
        },
    )?;
    sink.emit(&AgentEvent::EntryAppended { entry });
    // E-INT 1: settled host requests are applied before the claim is released.
    crate::host::serve_host(st, true, sink, ui);
    Ok(())
}

/// Holds the `busy` flag for the duration of one `!cmd`; releases it on every path.
struct Claim<'a>(&'a AtomicBool);

impl<'a> Claim<'a> {
    fn take(flag: &'a AtomicBool) -> Result<Claim<'a>, OxaError> {
        match flag.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => Ok(Claim(flag)),
            Err(_) => Err(OxaError::new(codes::BUSY, "cannot run !cmd during a run")
                .with_hint("wait until the agent has settled")),
        }
    }
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// The outcome a hook chose instead of running the command.
struct Replaced {
    text: String,
    exit: Option<i32>,
}

/// Fire `UserBash` through the hook sets in order. `Continue` moves on; `Block` and
/// `ReplaceResult` end the chain; any other reply is ignored with a notice.
fn consult_hooks(
    st: &mut SessionState,
    cmd: &str,
    excluded: bool,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) -> Option<Replaced> {
    let payload = json!({
        "command": cmd,
        "excludeFromContext": excluded,
        "cwd": st.cwd.display().to_string(),
    });
    for hooks in st.hooks.clone() {
        match crate::host::fire_hook(st, &hooks, HookPoint::UserBash, &payload, sink, ui) {
            HookReply::Continue => {}
            HookReply::Block(reason) => {
                return Some(Replaced {
                    text: reason,
                    exit: None,
                })
            }
            HookReply::ReplaceResult(out) => {
                let exit = out
                    .ui
                    .get("exit")
                    .and_then(Value::as_i64)
                    .and_then(|c| i32::try_from(c).ok());
                return Some(Replaced { text: out.text, exit });
            }
            _ => sink.emit(&AgentEvent::Notice {
                level: Level::Warning,
                text: "user_bash: a hook reply was ignored".to_string(),
            }),
        }
    }
    None
}

/// Run `cmd` with `bash -c` in the session cwd, streaming `BashExecutionUpdate` deltas.
/// Returns the raw merged output and the exit code (`None` when killed or cancelled).
fn run_command(
    st: &SessionState,
    cmd: &str,
    sink: &mut dyn EventSink,
    cancel: &CancelToken,
) -> (String, Option<i32>) {
    // pi (agent-session.ts executeBash): `shellCommandPrefix` and `shellPath` apply to `!cmd`
    // too. The session variables (OXA_SESSION_*, OXA_MODEL, ...) do not: pi's
    // environment-variables.md injects them into the LLM-callable shell tools only.
    let shell = oxa_tools::ShellConfig {
        shell_path: st.settings.shell_path(&st.dirs.home),
        command_prefix: st.settings.shell_command_prefix(),
    };
    let command = match shell.command(cmd) {
        Ok(c) => c,
        Err(e) => return (e, None),
    };
    let env: Vec<(String, String)> = st
        .tool_env
        .iter()
        .filter(|(k, _)| k == "AI_AGENT" || k == "OXA_CODING_AGENT")
        .cloned()
        .collect();
    let spec = ExecSpec {
        command,
        cwd: st.cwd.clone(),
        writable: true,
        net: true,
        timeout_s: None,
        env,
    };
    let mut pending: Vec<u8> = Vec::new();
    let res = {
        let mut live = |chunk: &[u8]| {
            pending.extend_from_slice(chunk);
            let text = take_valid_utf8(&mut pending);
            if !text.is_empty() {
                sink.emit(&AgentEvent::BashExecutionUpdate {
                    id: None,
                    delta: strip_ansi(&text),
                });
            }
        };
        match offline_plan(oxa_proto::offline(), &path_var(), &st.dirs.home, &spec) {
            Ok(Some(argv)) => run_argv(&argv, &[], None, cancel, &mut live),
            Ok(None) => run_unsandboxed(&spec, cancel, &mut live),
            Err(e) => return (e.to_string(), None),
        }
    };
    if !pending.is_empty() {
        let rest = String::from_utf8_lossy(&pending).into_owned();
        sink.emit(&AgentEvent::BashExecutionUpdate {
            id: None,
            delta: strip_ansi(&rest),
        });
    }
    (String::from_utf8_lossy(&res.output).into_owned(), res.exit)
}

/// The process `PATH`, where bwrap is looked up for an offline launch.
fn path_var() -> String {
    std::env::var("PATH").unwrap_or_default()
}

/// Item 1 (sec-prod): offline, `!cmd` runs through bwrap without network (`oxa_sandbox::offline_argv`,
/// the user's own file system). `Ok(None)` when not offline: the run is unchanged. A missing or
/// unusable bwrap is refused with the offline error; the command never runs with network.
fn offline_plan(
    offline: bool,
    path_var: &str,
    home: &Path,
    spec: &ExecSpec,
) -> Result<Option<Vec<String>>, OxaError> {
    if !offline {
        return Ok(None);
    }
    let argv = ["bash", "-c", spec.command.as_str()].map(str::to_string);
    oxa_sandbox::offline_argv(path_var, home, &spec.cwd, &spec.env, &argv)
        .map(Some)
        .map_err(|_| oxa_proto::offline_error("`!cmd`"))
}

/// Split off the longest valid UTF-8 prefix of `pending` (an incomplete trailing sequence waits
/// for the next chunk); invalid bytes become U+FFFD.
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

/// The entry's output: ANSI removed; over pi's limit, the tail that fits plus pi's notice, with
/// the full (sanitized) output saved under `spill_dir`.
fn finish_output(spill_dir: &Path, raw: &str) -> String {
    let text = strip_ansi(raw);
    match truncate_tail(&text) {
        None => text,
        Some(kept) => {
            let notice = match save_full(spill_dir, text.as_bytes()) {
                Some(path) => format!("[Output truncated. Full output: {}]", path.display()),
                None => "[Output truncated. Full output not saved]".to_string(),
            };
            format!("{kept}\n\n{notice}")
        }
    }
}

/// pi's `truncateTail`: the last lines that fit in both limits, whole lines only. A single last
/// line over the byte limit keeps its end. `None` when nothing needs cutting.
fn truncate_tail(text: &str) -> Option<String> {
    let lines: Vec<&str> = text.split('\n').collect();
    if lines.len() <= MAX_LINES && text.len() <= MAX_BYTES {
        return None;
    }
    let mut keep = 0usize;
    let mut bytes = 0usize;
    for line in lines.iter().rev() {
        if keep == MAX_LINES {
            break;
        }
        let add = line.len() + usize::from(keep > 0);
        if bytes + add > MAX_BYTES {
            break;
        }
        bytes += add;
        keep += 1;
    }
    if keep == 0 {
        let last = lines[lines.len() - 1];
        let mut cut = last.len().saturating_sub(MAX_BYTES);
        while !last.is_char_boundary(cut) {
            cut += 1;
        }
        return Some(last[cut..].to_string());
    }
    Some(lines[lines.len() - keep..].join("\n"))
}

/// Write the full output to `<dir>/bash-<sha256 hex>.log` (0600 in a 0700 directory). The name
/// is content-addressed, so an existing file of that name is kept. `None` when it cannot be saved.
fn save_full(dir: &Path, bytes: &[u8]) -> Option<PathBuf> {
    DirBuilder::new().recursive(true).mode(0o700).create(dir).ok()?;
    let mut name = String::from("bash-");
    for b in Sha256::digest(bytes) {
        let _ = write!(name, "{b:02x}");
    }
    name.push_str(".log");
    let path = dir.join(name);
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
    {
        Ok(mut file) => {
            if file.write_all(bytes).is_err() {
                let _ = fs::remove_file(&path);
                return None;
            }
            Some(path)
        }
        Err(e) if e.kind() == ErrorKind::AlreadyExists => Some(path),
        Err(_) => None,
    }
}

#[cfg(test)]
mod offline_tests {
    use super::*;
    use oxa_fake::TestEnv;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    /// A script written and then run races with a fork in another test thread (ETXTBSY).
    static SCRIPT_LOCK: Mutex<()> = Mutex::new(());

    fn spec(env: &TestEnv) -> ExecSpec {
        ExecSpec {
            command: "echo hi".to_string(),
            cwd: env.work(),
            writable: true,
            net: true,
            timeout_s: None,
            env: vec![("AI_AGENT".to_string(), "oxa".to_string())],
        }
    }

    /// A fake `bwrap` on a PATH directory: the probe (`--unshare-all`) passes.
    fn fake_bwrap(env: &TestEnv) -> String {
        let _guard = SCRIPT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let script = env.write("bin/bwrap", "#!/bin/sh\nexit 0\n");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        script.parent().expect("dir").to_string_lossy().into_owned()
    }

    #[test]
    fn not_offline_keeps_the_unchanged_run() {
        let env = TestEnv::new();
        let dir = fake_bwrap(&env);
        assert!(offline_plan(false, &dir, &env.home(), &spec(&env))
            .expect("ok")
            .is_none());
    }

    #[test]
    fn offline_golden_is_bwrap_without_network_and_the_users_file_system() {
        let env = TestEnv::new();
        let dir = fake_bwrap(&env);
        let argv = offline_plan(true, &dir, &env.home(), &spec(&env))
            .expect("ok")
            .expect("wrapped");
        let bwrap = format!("{dir}/bwrap");
        let cwd = env.work().display().to_string();
        let want = [
            bwrap.as_str(),
            "--unshare-net",
            "--die-with-parent",
            "--bind",
            "/",
            "/",
            "--dev-bind",
            "/dev",
            "/dev",
            "--proc",
            "/proc",
            "--chdir",
            cwd.as_str(),
            "--setenv",
            "AI_AGENT",
            "oxa",
            "--",
            "bash",
            "-c",
            "echo hi",
        ];
        assert_eq!(argv, want);
    }

    #[test]
    fn offline_without_bwrap_is_refused_with_the_offline_shape() {
        let env = TestEnv::new();
        let err = offline_plan(true, "", &env.home(), &spec(&env)).expect_err("no bwrap, no run");
        assert_eq!(err.code, oxa_proto::codes::OFFLINE);
        assert!(
            err.to_string()
                .starts_with("Refused: `!cmd` needs the network: offline mode is on"),
            "{err}"
        );
    }
}
