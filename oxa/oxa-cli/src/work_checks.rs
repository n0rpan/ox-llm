//! Deterministic check/inspection supervisor; no engine or model is constructed here.
use oxa_config::{Dirs, Env, Settings};
use oxa_proto::{sh_quote, CancelToken, ExecSpec, Sandbox};
use oxa_sandbox::Bwrap;
use oxa_work::{SupervisorOperation, SupervisorRequest};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::Path;
use std::sync::Arc;

pub(crate) fn run(
    request: &str,
    cwd: &Path,
    env: &Env,
    dirs: &Dirs,
    out: &mut dyn Write,
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
    // #1442: check rounds get a private writable output mount, so generated build outputs
    // never dirty the work the round judges. Committed checks keep their export-local
    // directory; working-tree checks get a per-task directory beside the worktree. The
    // other supervisor operations run no project command and mount nothing extra.
    let outputs = match &request.operation {
        SupervisorOperation::Checks { task, .. } => {
            if task.is_empty()
                || task.len() > 128
                || !task
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
            {
                return Err("invalid task identity for check outputs".into());
            }
            let scratch = root
                .parent()
                .ok_or("worktree has no parent for check outputs")?
                .join(".work-output");
            ensure_private_check_directory(&scratch)?;
            let outputs = scratch.join(task);
            ensure_private_check_directory(&outputs)?;
            Some(outputs)
        }
        SupervisorOperation::CommittedChecks { .. } => {
            let outputs = root
                .parent()
                .ok_or("committed checkout has no parent")?
                .join(".committed-output");
            if !std::fs::symlink_metadata(&outputs).is_ok_and(|metadata| metadata.is_dir()) {
                return Err("invalid committed-check output directory".into());
            }
            // No user cache/extra bind may reopen tracked inputs for writes.
            b.layout.write.retain(|path| {
                path.canonicalize()
                    .is_ok_and(|path| !path.starts_with(&root) && !root.starts_with(path))
            });
            Some(outputs)
        }
        _ => None,
    };
    if let Some(outputs) = &outputs {
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
        SupervisorOperation::ChangedPaths { base } => return changed_paths(&b, &root, &base, out),
        SupervisorOperation::Inspect => return inspect(&b, &root),
        SupervisorOperation::InspectCommitted { index } => return inspect_committed(&b, &root, &index),
        SupervisorOperation::Checkout {
            branch,
            create,
            adopted,
            normalize,
        } => {
            // Adopted content is materialized without ambient GitHub authority.
            // The ordinary checkout path retains its existing hook policy.
            let sandbox: Arc<dyn Sandbox> = if adopted {
                Arc::new(b)
            } else {
                crate::netguard::with_net(Arc::new(b), &settings)
            };
            let git = if adopted {
                "git --no-replace-objects -c core.hooksPath=/dev/null -c credential.helper="
            } else {
                "git --no-replace-objects"
            };
            let mut command = if let Some(update) = normalize {
                // Materialize filters first, then advance under the caller's Git
                // worktree hold, with the selected checkout callback policy.
                format!("{git} read-tree --reset -u HEAD && test $({git} symbolic-ref --quiet {}; echo $?) = 1 && {git} update-ref --no-deref {} {} {} && checkout_head=$({git} rev-parse HEAD) && {git} hook run --ignore-missing post-checkout -- \"$checkout_head\" \"$checkout_head\" 0", sh_quote(&update.reference), sh_quote(&update.reference), sh_quote(&update.new), sh_quote(&update.old))
            } else {
                format!("{git} checkout HEAD -- .")
            };
            if create {
                command.push_str(&format!(" && {git} checkout -b {} HEAD", sh_quote(&branch)));
            }
            let spec = ExecSpec {
                command,
                cwd: root,
                writable: true,
                net: !adopted && sandbox.network(),
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
            // Local Git callbacks do not need repository network authority.
            let spec = ExecSpec {
                command: format!("git --no-replace-objects merge --no-edit -- {}", sh_quote(&base)),
                cwd: root,
                writable: true,
                net: false,
                timeout_s: Some(600),
                env: git_environment(),
            };
            let mut tail = Vec::new();
            let result = b.run_streaming(&spec, &CancelToken::default(), &mut |bytes| {
                keep_tail(&mut tail, bytes, 8192);
            });
            err.write_all(&tail).map_err(|e| e.to_string())?;
            if result.timed_out || result.exit.is_none() {
                return Err("sandboxed base merge did not complete; worktree retained".into());
            }
            return Ok(i32::from(result.exit != Some(0)));
        }
        SupervisorOperation::Save { task_id } => {
            // Keep save callbacks local, including content from an adopted head.
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
                net: false,
                timeout_s: Some(600),
                env: git_environment(),
            };
            let mut tail = Vec::new();
            let result = b.run_streaming(&spec, &CancelToken::default(), &mut |bytes| {
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
        SupervisorOperation::Checks { commands, .. }
        | SupervisorOperation::CommittedChecks { commands, .. } => commands,
    };
    let shell = oxa_tools::ShellConfig {
        shell_path: settings.shell_path(&dirs.home),
        command_prefix: settings.shell_command_prefix(),
    };
    let sandbox = crate::netguard::without_github(Arc::new(b), &settings);
    let mut failed = false;
    let mut diagnostics = Vec::new();
    // #1442: one fact per command. The runner pairs this record with what it sent, decides
    // each verdict itself, and keeps the list as the evidence a `done` rests on.
    let mut evidence = Vec::new();
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
        evidence.push(serde_json::json!({
            "command": entry,
            "exit": result.exit,
            "timed_out": result.timed_out,
        }));
        if result.exit != Some(0) || result.timed_out {
            failed = true;
            let header = format!("{entry}: exit {:?}, timeout {}\n", result.exit, result.timed_out);
            keep_tail(&mut diagnostics, header.as_bytes(), 16384);
            keep_tail(&mut diagnostics, &tail, 16384);
        }
    }
    let record = serde_json::json!({"type": "oxa_checks", "version": 1, "checks": evidence});
    serde_json::to_writer(&mut *out, &record).map_err(|e| e.to_string())?;
    writeln!(out).map_err(|e| e.to_string())?;
    if failed {
        let ctx = crate::report_cmd::ScrubCtx::from_env(env, dirs);
        let safe = crate::report_cmd::scrub(&String::from_utf8_lossy(&diagnostics), &ctx);
        err.write_all(safe.as_bytes()).map_err(|e| e.to_string())?;
    }
    Ok(i32::from(failed))
}

// Exit codes remain the gate: repository callbacks cannot forge a clean verdict by printing
// JSON or diagnostics. The evidence record restates what the runner itself sent plus each
// bare exit, and the runner pairs it with its own commands before deciding anything.
fn changed_paths(sandbox: &Bwrap, root: &Path, base: &str, out: &mut dyn Write) -> Result<i32, String> {
    if !oxa_work::board::queue::is_object_id(base) {
        return Err("invalid base for worktree change inventory".into());
    }
    let git = "git --no-replace-objects --no-optional-locks -c core.fsmonitor=false";
    let spec = ExecSpec {
        command: format!(
            "{git} diff --name-only --no-relative --no-renames --no-ext-diff --no-textconv --ignore-submodules=none -z {} -- 2>/dev/null && \n             {git} ls-files --others --exclude-standard -z 2>/dev/null",
            sh_quote(base)
        ),
        cwd: root.into(),
        writable: false,
        net: false,
        timeout_s: Some(600),
        env: git_environment(),
    };
    const MAX_BYTES: usize = 512 * 1024;
    let mut paths = Vec::new();
    let mut overflow = false;
    let result = sandbox.run_streaming(&spec, &CancelToken::default(), &mut |bytes| {
        if !overflow && paths.len() + bytes.len() <= MAX_BYTES {
            paths.extend_from_slice(bytes);
        } else {
            overflow = true;
        }
    });
    if result.exit != Some(0) || result.timed_out || overflow {
        return Err("sandboxed Git change inventory did not complete within its limits".into());
    }
    out.write_all(&paths).map_err(|error| error.to_string())?;
    Ok(0)
}

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

fn ensure_private_check_directory(path: &Path) -> Result<(), String> {
    match std::fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.to_string()),
    }
    let metadata = std::fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    let uid = std::fs::metadata("/proc/self")
        .map_err(|error| error.to_string())?
        .uid();
    if metadata.is_dir() && metadata.uid() == uid && metadata.mode() & 0o077 == 0 {
        Ok(())
    } else {
        Err("invalid private check output directory".into())
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
