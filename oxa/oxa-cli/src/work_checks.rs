//! Deterministic check/inspection supervisor; no engine or model is constructed here.
use oxa_config::{Dirs, Env, Settings};
use oxa_proto::{sh_quote, CancelToken, ExecSpec, Sandbox};
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
    let committed = matches!(&request.operation, SupervisorOperation::CommittedChecks { .. });
    let mut b = Bwrap::detect(env.get("PATH").unwrap_or(""), &dirs.home).map_err(|e| e.to_string())?;
    b.layout = crate::sandbox_layout::layout(env, dirs, &root, false, &mut vec![]);
    let outputs = if committed {
        Some(
            root.parent()
                .ok_or("committed checkout has no parent")?
                .join(".committed-output"),
        )
    } else {
        None
    };
    if let Some(outputs) = &outputs {
        if !std::fs::symlink_metadata(outputs).is_ok_and(|metadata| metadata.is_dir()) {
            return Err("invalid committed-check output directory".into());
        }
        // No user cache/extra bind may reopen tracked inputs for writes.
        b.layout.write.retain(|path| {
            path.canonicalize()
                .is_ok_and(|path| !path.starts_with(&root) && !root.starts_with(path))
        });
        b.layout.write.push(outputs.clone());
    }
    if let SupervisorOperation::CommittedChecks { index, .. }
    | SupervisorOperation::InspectCommitted { index } = &request.operation
    {
        if index.parent() != root.parent()
            || index.file_name().is_none_or(|name| name != ".committed-index")
            || !std::fs::symlink_metadata(index).is_ok_and(|metadata| metadata.is_file())
        {
            return Err("invalid committed inspection index".into());
        }
        // The private index remains readable even when its parent is masked as
        // part of HOME. This adds only the one immutable generated file.
        b.layout.read.push(index.clone());
    }
    let b = b.with_git_dirs(&root);
    b.check().map_err(|e| e.to_string())?;
    let settings = Settings::load(dirs, &source, true);
    let commands = match request.operation {
        SupervisorOperation::Inspect => return inspect(&b, &root),
        SupervisorOperation::InspectCommitted { index } => return inspect_committed(&b, &root, &index),
        SupervisorOperation::Checkout { branch, create } => {
            let sandbox = crate::netguard::with_net(Arc::new(b), &settings);
            let spec = ExecSpec {
                command: if create {
                    format!(
                        "git --no-replace-objects checkout HEAD -- . && git --no-replace-objects checkout -b {} HEAD",
                        sh_quote(&branch)
                    )
                } else {
                    "git --no-replace-objects checkout HEAD -- .".into()
                },
                cwd: root,
                writable: true,
                net: sandbox.network(),
                timeout_s: Some(600),
                env: git_environment(),
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
        SupervisorOperation::Merge { base } => {
            let sandbox = crate::netguard::with_net(Arc::new(b), &settings);
            let spec = ExecSpec {
                command: format!("git --no-replace-objects merge --no-edit -- {}", sh_quote(&base)),
                cwd: root,
                writable: true,
                net: sandbox.network(),
                timeout_s: Some(600),
                env: git_environment(),
            };
            let mut tail = Vec::new();
            let result = sandbox.run_streaming(&spec, &CancelToken::default(), &mut |bytes| {
                keep_tail(&mut tail, bytes, 8192);
            });
            err.write_all(&tail).map_err(|e| e.to_string())?;
            if result.timed_out || result.exit.is_none() {
                return Err("sandboxed base merge did not complete; worktree retained".into());
            }
            return Ok(i32::from(result.exit != Some(0)));
        }
        SupervisorOperation::Save { task_id } => {
            let sandbox = crate::netguard::with_net(Arc::new(b), &settings);
            let message = sh_quote(&format!("oxa work: save task {task_id}"));
            let spec = ExecSpec {
                command: format!(
                    r#"git --no-replace-objects diff --cached --quiet --no-ext-diff --no-textconv --diff-filter=U
work_index_status=$?
if test "$work_index_status" -ne 0; then exit 2; fi
git --no-replace-objects add --all || exit 2
git --no-replace-objects rev-parse --verify --quiet MERGE_HEAD >/dev/null
work_merge_status=$?
if test "$work_merge_status" -ne 1; then exit 2; fi
git --no-replace-objects diff --cached --quiet --no-ext-diff --no-textconv
work_changes=$?
if test "$work_changes" -eq 0; then exit 0; fi
if test "$work_changes" -ne 1; then exit 2; fi
git --no-replace-objects commit -m {message}
"#
                ),
                cwd: root,
                writable: true,
                net: sandbox.network(),
                timeout_s: Some(600),
                env: git_environment(),
            };
            let mut tail = Vec::new();
            let result = sandbox.run_streaming(&spec, &CancelToken::default(), &mut |bytes| {
                keep_tail(&mut tail, bytes, 8192);
            });
            if result.exit != Some(0) || result.timed_out {
                return Err(format!(
                    "save exit {:?}, timeout {}: {}",
                    result.exit,
                    result.timed_out,
                    String::from_utf8_lossy(&tail)
                ));
            }
            return Ok(0);
        }
        SupervisorOperation::Checks { commands } | SupervisorOperation::CommittedChecks { commands, .. } => {
            commands
        }
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
                "cd -- {} || exit 111\n{command}",
                sh_quote(&dir.to_string_lossy())
            )
        } else {
            command
        };
        let mut check_env = git_environment();
        if let Some(outputs) = &outputs {
            check_env.extend([
                (
                    "OXA_WORK_OUTPUT_DIR".into(),
                    outputs.to_string_lossy().into_owned(),
                ),
                (
                    "CARGO_TARGET_DIR".into(),
                    outputs.join("target").to_string_lossy().into_owned(),
                ),
                ("TMPDIR".into(), outputs.to_string_lossy().into_owned()),
            ]);
        }
        let spec = ExecSpec {
            command,
            cwd: root.clone(),
            writable: !committed,
            net: sandbox.network(),
            timeout_s: Some(600),
            env: check_env,
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
        let ctx = crate::report_cmd::ScrubCtx::from_env(env, dirs);
        let safe = crate::report_cmd::scrub(&String::from_utf8_lossy(&diagnostics), &ctx);
        err.write_all(safe.as_bytes()).map_err(|e| e.to_string())?;
    }
    Ok(i32::from(failed))
}

// Exit codes are the only result: repository callbacks cannot forge a clean
// verdict by printing JSON or diagnostics. Git failures remain distinct from dirty.
fn inspect(sandbox: &Bwrap, root: &Path) -> Result<i32, String> {
    let spec = ExecSpec {
        command: r#"git --no-replace-objects --no-optional-locks -c core.fsmonitor=false status --porcelain=v1 -z | {
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
        env: git_environment(),
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

// Compare actual tracked contents using the runner's fresh, protected index.
// The check-writable index and user Git filters supply no authority here.
fn inspect_committed(sandbox: &Bwrap, root: &Path, index: &Path) -> Result<i32, String> {
    if index.parent() != root.parent() || index.file_name().is_none_or(|name| name != ".committed-index") {
        return Err("invalid committed inspection index".into());
    }
    let store = root
        .parent()
        .ok_or("committed checkout has no parent")?
        .join("objects.git");
    let mut env = git_environment();
    env.extend([
        ("GIT_INDEX_FILE".into(), index.to_string_lossy().into_owned()),
        ("GIT_DIR".into(), store.to_string_lossy().into_owned()),
        ("GIT_COMMON_DIR".into(), store.to_string_lossy().into_owned()),
        ("GIT_WORK_TREE".into(), root.to_string_lossy().into_owned()),
        ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
        ("GIT_CONFIG_SYSTEM".into(), "/dev/null".into()),
    ]);
    let result = sandbox.run_streaming(
        &ExecSpec {
            command: "test -r \"$GIT_INDEX_FILE\" || exit 2\ngit --no-replace-objects --no-optional-locks -c core.fsmonitor=false -c core.ignoreStat=false -c core.trustctime=true -c core.checkStat=default -c core.filemode=true -c core.symlinks=true -c core.sparseCheckout=false -c index.sparse=false diff --quiet --no-ext-diff --no-textconv --ignore-submodules=none --".into(),
            cwd: root.into(),
            writable: false,
            net: false,
            timeout_s: Some(600),
            env,
        },
        &CancelToken::default(),
        &mut |_| {},
    );
    match (result.exit, result.timed_out) {
        (Some(code @ (0 | 1)), false) => Ok(code),
        _ => Err("committed source inspection failed".into()),
    }
}

fn git_environment() -> Vec<(String, String)> {
    vec![
        ("AI_AGENT".into(), "oxa".into()),
        ("GIT_NO_REPLACE_OBJECTS".into(), "1".into()),
    ]
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
