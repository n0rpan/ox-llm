//! Deterministic check/inspection supervisor; no engine or model is constructed here.
use oxa_config::{Dirs, Env, Settings};
use oxa_proto::{CancelToken, ExecSpec, Sandbox};
use oxa_sandbox::Bwrap;
use oxa_work::{SupervisorOperation, SupervisorRequest};
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

pub(crate) fn run(
    request: &str,
    cwd: &Path,
    env: &Env,
    dirs: &Dirs,
    err: &mut dyn Write,
) -> Result<i32, String> {
    let request: SupervisorRequest = serde_json::from_str(request).map_err(|e| e.to_string())?;
    let source = request.source.canonicalize().map_err(|e| e.to_string())?;
    if !crate::work_cmd::trusted(&source, env, dirs)? {
        return Err("supervisor source project is not trusted".into());
    }
    let root = cwd.canonicalize().map_err(|e| e.to_string())?;
    let mut b = Bwrap::detect(env.get("PATH").unwrap_or(""), &dirs.home).map_err(|e| e.to_string())?;
    b.layout = crate::sandbox_layout::layout(env, dirs, &root, false, &mut vec![]);
    let b = b.with_git_dirs(&root);
    b.check().map_err(|e| e.to_string())?;
    let settings = Settings::load(dirs, &source, true);
    let commands = match request.operation {
        SupervisorOperation::Inspect => return inspect(&b, &root),
        SupervisorOperation::Checkout { branch } => {
            let sandbox = crate::netguard::with_net(Arc::new(b), &settings);
            let spec = ExecSpec {
                command: format!(
                    "git checkout HEAD -- . && git checkout -b '{}' HEAD",
                    branch.replace('\'', "'\\''")
                ),
                cwd: root,
                writable: true,
                net: sandbox.network(),
                timeout_s: Some(600),
                env: vec![("AI_AGENT".into(), "oxa".into())],
            };
            let mut tail = Vec::new();
            let result = sandbox.run_streaming(&spec, &CancelToken::default(), &mut |bytes| {
                keep_tail(&mut tail, bytes, 8192);
            });
            if result.exit != Some(0) || result.timed_out {
                return Err(format!(
                    "checkout exit {:?}, timeout {}: {}",
                    result.exit,
                    result.timed_out,
                    String::from_utf8_lossy(&tail)
                ));
            }
            return Ok(0);
        }
        SupervisorOperation::Checks { commands } => commands,
    };
    let shell = oxa_tools::ShellConfig {
        shell_path: settings.shell_path(&dirs.home),
        command_prefix: settings.shell_command_prefix(),
    };
    let sandbox = crate::netguard::with_net(Arc::new(b), &settings);
    let mut failed = false;
    let mut diagnostics = Vec::new();
    for entry in commands {
        let (directory, text) = match entry.strip_prefix("in ").and_then(|s| s.split_once(':')) {
            Some((dir, command)) => (Some(dir.trim()), command.trim()),
            None => (None, entry.as_str()),
        };
        if text.is_empty() {
            return Err("empty acceptance command".into());
        }
        let command = shell.command(text)?;
        // Bind the original workspace, then select a child inside that view. A
        // symlink swap cannot turn an outside directory into the writable root.
        let command = if let Some(directory) = directory {
            let child = root.join(directory).canonicalize().map_err(|e| e.to_string())?;
            let relative = child
                .strip_prefix(&root)
                .map_err(|_| "done directory escapes worktree")?;
            let dir = if relative.as_os_str().is_empty() {
                Path::new(".")
            } else {
                relative
            };
            format!(
                "cd -- '{}' || exit 111\n{command}",
                dir.to_string_lossy().replace('\'', "'\\''")
            )
        } else {
            command
        };
        let spec = ExecSpec {
            command,
            cwd: root.clone(),
            writable: true,
            net: sandbox.network(),
            timeout_s: Some(600),
            env: vec![("AI_AGENT".into(), "oxa".into())],
        };
        let mut tail = Vec::new();
        let result = sandbox.run_streaming(&spec, &CancelToken::default(), &mut |bytes| {
            keep_tail(&mut tail, bytes, 8192);
        });
        if result.exit != Some(0) || result.timed_out {
            failed = true;
            let header = format!("{entry}: exit {:?}, timeout {}\n", result.exit, result.timed_out);
            keep_tail(&mut diagnostics, header.as_bytes(), 16384);
            keep_tail(&mut diagnostics, &tail, 16384);
        }
    }
    if failed {
        err.write_all(&diagnostics).map_err(|e| e.to_string())?;
    }
    Ok(i32::from(failed))
}

// Exit codes are the only result: repository callbacks cannot forge a clean
// verdict by printing JSON or diagnostics. Git failures remain distinct from dirty.
fn inspect(sandbox: &Bwrap, root: &Path) -> Result<i32, String> {
    let spec = ExecSpec {
        command: r#"git --no-optional-locks -c core.fsmonitor=false status --porcelain=v1 -z | {
    work_dirty=0
    while IFS= read -r -d '' work_entry; do work_dirty=1; done
    exit "$work_dirty"
}
work_probe_status=("${PIPESTATUS[@]}")
if test "${work_probe_status[0]}" -ne 0; then exit 2; fi
exit "${work_probe_status[1]}"
"#
        .into(),
        cwd: root.to_path_buf(),
        writable: false,
        net: false,
        timeout_s: Some(600),
        env: Vec::new(),
    };
    let mut tail = Vec::new();
    let result = sandbox.run_streaming(&spec, &CancelToken::default(), &mut |bytes| {
        keep_tail(&mut tail, bytes, 8192);
    });
    match (result.exit, result.timed_out) {
        (Some(code @ (0 | 1)), false) => Ok(code),
        _ => Err(format!(
            "sandboxed Git inspection failed: {}",
            String::from_utf8_lossy(&tail)
        )),
    }
}

fn keep_tail(tail: &mut Vec<u8>, bytes: &[u8], cap: usize) {
    if bytes.len() >= cap {
        tail.clear();
        tail.extend_from_slice(&bytes[bytes.len() - cap..]);
    } else {
        let discard = (tail.len() + bytes.len()).saturating_sub(cap);
        tail.drain(..discard);
        tail.extend_from_slice(bytes);
    }
}
