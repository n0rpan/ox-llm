//! Deterministic acceptance supervisor; no engine or model is constructed here.
use oxa_config::{Dirs, Env, Settings};
use oxa_proto::{CancelToken, ExecSpec};
use oxa_sandbox::Bwrap;
use oxa_work::AcceptanceRequest;
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
    let request: AcceptanceRequest = serde_json::from_str(request).map_err(|e| e.to_string())?;
    let source = request.source.canonicalize().map_err(|e| e.to_string())?;
    if !crate::work_cmd::trusted(&source, env, dirs)? {
        return Err("acceptance source project is not trusted".into());
    }
    let root = cwd.canonicalize().map_err(|e| e.to_string())?;
    let settings = Settings::load(dirs, &source, true);
    let shell = oxa_tools::ShellConfig {
        shell_path: settings.shell_path(&dirs.home),
        command_prefix: settings.shell_command_prefix(),
    };
    let mut b = Bwrap::detect(env.get("PATH").unwrap_or(""), &dirs.home).map_err(|e| e.to_string())?;
    b.layout = crate::sandbox_layout::layout(env, dirs, &root, false, &mut vec![]);
    let sandbox = crate::netguard::with_net(Arc::new(b.with_git_dirs(&root)), &settings);
    sandbox.check().map_err(|e| e.to_string())?;
    let mut failed = false;
    let mut diagnostics = Vec::new();
    for entry in request.commands {
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
