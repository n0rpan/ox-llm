use oxa_config::{Dirs, Env};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};

pub(crate) fn trusted(cwd: &Path, env: &Env, dirs: &Dirs) -> Result<bool, String> {
    crate::dispatch::project_trusted(
        &crate::Args::default(),
        env,
        dirs,
        cwd,
        &oxa_perm::gated_resources(cwd),
        false,
        &mut vec![],
    )
    .map_err(|e| e.to_string())
}

// Keep owner state outside every worker Git bind.
pub(crate) fn protected_storage(storage: &Path, repo: &Path, common: &Path) -> Result<(), String> {
    if !storage.is_absolute()
        || storage
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err("runner storage must have an absolute canonical path".into());
    }
    let existing = storage
        .ancestors()
        .find(|path| path.exists())
        .ok_or("runner storage has no existing parent")?;
    let resolved = existing
        .canonicalize()
        .map_err(|_| "cannot resolve runner storage")?
        .join(
            storage
                .strip_prefix(existing)
                .map_err(|_| "cannot resolve runner storage")?,
        );
    let git_repo = if common.file_name().is_some_and(|name| name == ".git") {
        common.parent().ok_or("Git directory has no parent")?
    } else {
        common
    };
    let trees = git_repo
        .parent()
        .ok_or("repository has no worktree parent")?
        .join(".oxa-worktrees")
        .join(git_repo.file_name().ok_or("repository has no name")?);
    if resolved.starts_with(repo) || resolved.starts_with(common) || resolved.starts_with(trees) {
        return Err("runner storage must be outside repositories and task worktrees".into());
    }
    for parent in resolved.ancestors() {
        if parent.join(".git").exists()
            || (parent.join("HEAD").is_file()
                && parent.join("config").is_file()
                && parent.join("objects").is_dir()
                && parent.join("refs").is_dir())
        {
            return Err("runner storage must be outside Git metadata and worktrees".into());
        }
    }
    Ok(())
}

pub(crate) fn run(
    args: &[String],
    cwd: &Path,
    env: &Env,
    dirs: &Dirs,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    if let Some(help) = crate::help::requested("work", args) {
        return if write!(out, "{help}").is_ok() { 0 } else { 2 };
    }
    let result = (|| -> Result<i32, String> {
        if let [flag, request] = args {
            if flag == "--supervise" {
                return crate::work_checks::run(request, cwd, env, dirs, err);
            }
        }
        let binary: PathBuf = std::env::current_exe().map_err(|e| e.to_string())?;
        // Find the source boundary from files before executing any host tool.
        let cwd_c = cwd.canonicalize().map_err(|e| e.to_string())?;
        let project = cwd_c
            .ancestors()
            .find(|p| p.join(".git").exists())
            .ok_or("oxa work requires a Git repository")?;
        // Nested repository metadata cannot bless code from an outer project.
        let host_project = project
            .ancestors()
            .filter(|p| p.join(".git").exists())
            .last()
            .unwrap_or(project);
        let host_path = crate::netguard::trusted_work_path(
            std::ffi::OsStr::new(env.get("PATH").unwrap_or("")),
            host_project,
            &dirs.home,
        )
        .ok_or("cannot resolve trusted worker PATH")?;
        let git_binary = crate::netguard::trusted_executable_for(
            "git",
            Some(host_path.as_os_str()),
            Some(project),
            Some(&dirs.home),
        )
        .ok_or("no trusted Git executable outside the source project")?;
        let output = oxa_work::metadata_git(&git_binary, &host_path, cwd, &["rev-parse", "--show-toplevel"])
            .output()
            .map_err(|e| e.to_string())?;
        if !output.status.success() {
            return Err("oxa work requires a Git repository".into());
        }
        let root = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim())
            .canonicalize()
            .map_err(|e| e.to_string())?;
        if root != project {
            return Err("Git root differs from the source project boundary".into());
        }
        let common = oxa_work::metadata_git(
            &git_binary,
            &host_path,
            &root,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .output()
        .map_err(|e| e.to_string())?;
        if !common.status.success() {
            return Err("cannot resolve runner repository".into());
        }
        let common = PathBuf::from(String::from_utf8_lossy(&common.stdout).trim())
            .canonicalize()
            .map_err(|e| e.to_string())?;
        let key = Sha256::digest(common.as_os_str().as_encoded_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let terminate_binary = crate::netguard::trusted_executable_for(
            "kill",
            Some(host_path.as_os_str()),
            Some(&root),
            Some(&dirs.home),
        );
        let policy = oxa_work::RunPolicy {
            git_binary,
            host_path,
            terminate_binary,
            offline: oxa_proto::offline(),
            storage: dirs.agent_dir.join("work").join(key),
        };
        protected_storage(&policy.storage, &root, &common)?;
        if args.first().is_some_and(|arg| arg == "board") {
            return crate::work_board::run(&args[1..], &root, &common, env, dirs, &policy);
        }
        if matches!(args.first().map(String::as_str), Some("status" | "--status")) {
            let json = match args {
                [_] => false,
                [_, flag] if flag == "--json" => true,
                _ => return Err("usage: oxa work status [--json]".into()),
            };
            let rows = oxa_work::status(&policy.storage, &binary)?;
            if json {
                serde_json::to_writer(&mut *out, &rows).map_err(|e| e.to_string())?;
                writeln!(out).map_err(|e| e.to_string())?;
            } else {
                for row in rows {
                    let outcome = serde_json::to_value(row.record.outcome).map_err(|e| e.to_string())?;
                    let step = serde_json::to_value(row.record.step).map_err(|e| e.to_string())?;
                    writeln!(
                        out,
                        "{} {} {} {} {:?}",
                        row.record.id,
                        outcome.as_str().unwrap_or("unknown"),
                        row.record.checkpoint_at_ms,
                        step.as_str().unwrap_or("unknown"),
                        row.liveness
                    )
                    .map_err(|e| e.to_string())?;
                }
            }
            return Ok(0);
        }
        if let [command, id] = args {
            if command == "verify" {
                if !trusted(&root, env, dirs)? {
                    return Err(
                        "source project is not trusted; approve it before running saved checks".into(),
                    );
                }
                let verified = oxa_work::verify_saved_task(&root, &binary, &policy, id)?;
                let record = verified.record();
                serde_json::to_writer(
                    &mut *out,
                    &serde_json::json!({
                        "type":"oxa_committed_acceptance", "version":1, "task":record.id,
                        "head_sha":record.head_sha, "tree_sha":record.tree_sha, "base_sha":record.base_sha,
                        "checked_at_ms":verified.checked_at_ms(),
                    }),
                )
                .map_err(|error| error.to_string())?;
                writeln!(out).map_err(|error| error.to_string())?;
                return Ok(0);
            }
        }
        let rerun = args.last().is_some_and(|arg| arg == "--rerun");
        let task_args = if rerun { &args[..args.len() - 1] } else { args };
        let is_trusted = trusted(&root, env, dirs)?;
        let tasks = match task_args {
            [file] if !file.starts_with('-') => oxa_work::read_tasks(&cwd.join(file))?,
            [flag, number, model_flag, model] if flag == "--issue" && model_flag == "--model" => {
                if oxa_proto::offline() {
                    return Err("GitHub issue lookup is unavailable in offline mode".into());
                }
                if !is_trusted {
                    return Err("source project is not trusted; approve it before issue lookup".into());
                }
                let number = number.parse().map_err(|_| "invalid issue number")?;
                if let Some(task) = oxa_work::saved_issue_task(&policy, number, model)? {
                    vec![task]
                } else {
                    let gh = crate::netguard::trusted_executable_for(
                        "gh",
                        Some(policy.host_path.as_os_str()),
                        Some(&root),
                        Some(&dirs.home),
                    )
                    .ok_or("no trusted gh executable outside the source project")?;
                    let mut task =
                        oxa_work::issue_task(&root, &gh, &policy.host_path, number, model.clone())?;
                    task.prompt.insert_str(0, &oxa_work::github_runner_guidance("issue"));
                    vec![task]
                }
            }
            [flag, number, model_flag, model] if flag == "--pr" && model_flag == "--model" => {
                if oxa_proto::offline() {
                    return Err("GitHub pull request lookup is unavailable in offline mode".into());
                }
                if !is_trusted {
                    return Err("source project is not trusted; approve it before pull request lookup".into());
                }
                let gh = crate::netguard::trusted_executable_for(
                    "gh",
                    Some(policy.host_path.as_os_str()),
                    Some(&root),
                    Some(&dirs.home),
                )
                .ok_or("no trusted gh executable outside the source project")?;
                let number = number.parse().map_err(|_| "invalid pull request number")?;
                vec![oxa_work::pull_request_task(&root, &gh, &policy, number, model.clone())?]
            }
            _ => return Err(
                "usage: oxa work <taskfile> [--rerun] | --issue N --model MODEL [--rerun] | --pr N --model MODEL [--rerun] | status [--json]"
                    .into(),
            ),
        };
        // Freeze every task before checkout hooks or a first worker can change
        // shared refs. A bad plan remains a per-task error, not a batch abort.
        let planned: Vec<_> = tasks
            .into_iter()
            .map(|task| {
                if is_trusted {
                    oxa_work::plan_task_for_run(&root, task.clone(), &policy, rerun)
                        .map_err(|detail| Box::new(oxa_work::blocked_record(&task, detail)))
                } else {
                    Err(Box::new(oxa_work::blocked_record(
                        &task,
                        "source project is not trusted; approve it before unattended work".into(),
                    )))
                }
            })
            .collect();
        let mut code = 0;
        for task in planned {
            let record = match task {
                Ok(plan) => {
                    match oxa_work::run_task_with_observer(
                        &root,
                        &binary,
                        &plan,
                        &policy,
                        rerun,
                        &|_: &oxa_work::Record| Ok(()),
                    ) {
                        Ok(record) => record,
                        Err(oxa_work::RunError::Blocked(detail)) => {
                            let mut record = oxa_work::blocked_record(plan.task(), detail);
                            record.base_sha = Some(plan.base_sha().to_owned());
                            record
                        }
                        Err(oxa_work::RunError::Checkpoint(error)) => return Err(error),
                    }
                }
                Err(record) => *record,
            };
            if matches!(
                record.outcome,
                oxa_work::Outcome::Red | oxa_work::Outcome::Blocked | oxa_work::Outcome::Interrupted
            ) {
                code = 1;
            }
            serde_json::to_writer(&mut *out, &record).map_err(|e| e.to_string())?;
            writeln!(out).map_err(|e| e.to_string())?;
        }
        Ok(code)
    })();
    match result {
        Ok(code) => code,
        Err(e) => {
            let _ = writeln!(err, "oxa work: {e}");
            2
        }
    }
}
