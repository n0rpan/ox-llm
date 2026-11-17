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
fn protected_storage(storage: &Path, repo: &Path, common: &Path) -> Result<(), String> {
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
            return crate::work_board::run(&args[1..], &root, env, dirs, &policy);
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
        let (mut rerun, mut publish, mut reports) = (false, false, false);
        let mut end = args.len();
        while let Some(flag) = args.get(end.wrapping_sub(1)) {
            match flag.as_str() {
                "--rerun" => rerun = true,
                "--publish" => publish = true,
                "--report-problems" => reports = true,
                _ => break,
            }
            end -= 1;
        }
        if (publish || reports)
            && env.pairs().iter().any(|(name, _)| {
                matches!(
                    name.as_str(),
                    "OXA_CODING_AGENT" | "AI_AGENT" | "OXA_SUBAGENT_DEPTH"
                )
            })
        {
            return Err("publishing and problem reporting require the owner outside an agent tool".into());
        }
        let task_args = &args[..end];
        let is_trusted = trusted(&root, env, dirs)?;
        let context = std::sync::Arc::new(Context {
            cwd: root.clone(),
            binary: binary.clone(),
            policy: policy.clone(),
            dirs: dirs.clone(),
            env: env.clone(),
            rerun,
            publish,
            reports,
        });
        if task_args.first().is_some_and(|word| word == "--queue") {
            if !is_trusted {
                return Err("source project is not trusted; approve it before unattended work".into());
            }
            let (directory, options) = queue_options(task_args, cwd)?;
            let df = crate::netguard::trusted_executable_for(
                "df",
                Some(policy.host_path.as_os_str()),
                Some(&root),
                Some(&dirs.home),
            )
            .ok_or("no trusted disk-space executable outside the source project")?;
            let worker = std::sync::Arc::new(move |task: &oxa_work::Task| {
                let plan = if context.publish || context.reports {
                    oxa_work::plan_task_for_actions(
                        &context.cwd,
                        task.clone(),
                        &context.policy,
                        context.rerun,
                    )
                } else {
                    oxa_work::plan_task_for_run(&context.cwd, task.clone(), &context.policy, context.rerun)
                };
                match plan {
                    Ok(plan) => context.planned(&plan),
                    Err(error) => Ok(oxa_work::blocked_record(task, error)),
                }
            });
            let summary =
                oxa_work::run_queue(&root, &directory, &options, &policy, &df, worker, &mut |event| {
                    serde_json::to_writer(&mut *out, &event).map_err(|e| e.to_string())?;
                    writeln!(out).and_then(|_| out.flush()).map_err(|e| e.to_string())
                })?;
            return Ok(i32::from(summary.failed > 0 || summary.pending > 0));
        }
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
                    task.prompt.insert_str(0, oxa_work::ISSUE_RUNNER_GUIDANCE);
                    vec![task]
                }
            }
            _ => return Err(
                "usage: oxa work <taskfile> [--rerun] | --issue N --model MODEL [--rerun] | status [--json]"
                    .into(),
            ),
        };
        // Freeze every task before checkout hooks or a first worker can change
        // shared refs. A bad plan remains a per-task error, not a batch abort.
        let planned: Vec<_> = tasks
            .into_iter()
            .map(|task| {
                if is_trusted {
                    let planner = if publish || reports {
                        oxa_work::plan_task_for_actions
                    } else {
                        oxa_work::plan_task_for_run
                    };
                    planner(&root, task.clone(), &policy, rerun)
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
                Ok(plan) => context.planned(&plan)?,
                Err(record) => *record,
            };
            if matches!(
                record.outcome,
                oxa_work::Outcome::Red | oxa_work::Outcome::Blocked | oxa_work::Outcome::Interrupted
            ) || record.reports.iter().any(|report| report.disposition == "failed")
            {
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

struct Context {
    cwd: PathBuf,
    binary: PathBuf,
    policy: oxa_work::RunPolicy,
    dirs: Dirs,
    env: Env,
    rerun: bool,
    publish: bool,
    reports: bool,
}
impl Context {
    fn planned(&self, plan: &oxa_work::PlannedTask) -> Result<oxa_work::Record, String> {
        let root = &self.cwd;
        let binary = &self.binary;
        let policy = &self.policy;
        let dirs = &self.dirs;
        let env = &self.env;
        let (rerun, publish, reports) = (self.rerun, self.publish, self.reports);
        let blocked = |detail| {
            let mut record = oxa_work::blocked_record(plan.task(), detail);
            record.base_sha = Some(plan.base_sha().to_owned());
            record
        };
        let mut locked = match oxa_work::LockedTask::acquire(plan.task(), policy) {
            Ok(locked) => locked,
            Err(error) => return Ok(blocked(error)),
        };
        let cached = if (publish || reports) && !rerun {
            oxa_work::action_record(policy, plan.task())?
        } else {
            None
        };
        let mut record = if let Some(record) = cached {
            record
        } else {
            let ctx = crate::report_cmd::ScrubCtx::from_env(env, dirs);
            match locked.run_with_notes(
                root,
                binary,
                plan,
                rerun,
                &|_: &oxa_work::Record| Ok(()),
                &|note| crate::work_reports::scrub_note(note, &ctx),
            ) {
                Ok(record) => record,
                Err(oxa_work::RunError::Blocked(detail)) => blocked(detail),
                Err(oxa_work::RunError::Checkpoint(error)) => return Err(error),
            }
        };
        if publish && !record.worktree.as_os_str().is_empty() {
            let gh = crate::netguard::trusted_executable_for(
                "gh",
                Some(policy.host_path.as_os_str()),
                Some(root),
                Some(&dirs.home),
            )
            .unwrap_or_else(|| PathBuf::from("/unavailable/gh"));
            record = locked.publish(
                root,
                binary,
                &gh,
                &|_: &oxa_work::Record| Ok(()),
                &mut crate::report_cmd::tty_confirm(env),
            )?;
        }
        if reports && !record.worktree.as_os_str().is_empty() {
            let settings = oxa_config::Settings::load(dirs, root, false);
            let ctx = crate::report_cmd::ScrubCtx::from_env(env, dirs);
            record = locked.report_notes(&|_: &oxa_work::Record| Ok(()), &mut |note, lock, offline| {
                let runner = |program: &str, args: &[String], gate: &std::fs::File| {
                    let executable = crate::netguard::trusted_executable_for(
                        program,
                        Some(policy.host_path.as_os_str()),
                        Some(root),
                        Some(&dirs.home),
                    )?;
                    crate::work_reports::process(&executable.to_string_lossy(), args, root, lock, gate)
                };
                crate::work_reports::report(
                    note,
                    dirs,
                    &settings,
                    &ctx,
                    offline,
                    &mut crate::report_cmd::tty_confirm(env),
                    runner,
                )
            })?;
        }
        Ok(record)
    }
}
fn queue_options(args: &[String], cwd: &Path) -> Result<(PathBuf, oxa_work::QueueOptions), String> {
    let directory = cwd.join(args.get(1).ok_or("--queue needs a directory")?);
    let mut options = oxa_work::QueueOptions::default();
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--max" => {
                i += 1;
                options.max = args
                    .get(i)
                    .ok_or("--max needs a value")?
                    .parse()
                    .map_err(|_| "invalid --max")?;
            }
            "--min-free-mib" => {
                i += 1;
                let mib: u64 = args
                    .get(i)
                    .ok_or("--min-free-mib needs a value")?
                    .parse()
                    .map_err(|_| "invalid free-space minimum")?;
                options.min_free_bytes = mib
                    .checked_mul(1024 * 1024)
                    .ok_or("free-space minimum is too large")?;
            }
            "--once" => options.once = true,
            word => return Err(format!("unknown queue option: {word}")),
        }
        i += 1;
    }
    if options.max == 0 {
        return Err("--max must be positive".into());
    }
    Ok((directory, options))
}
