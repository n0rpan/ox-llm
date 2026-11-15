use std::io::Write;
use std::path::{Path, PathBuf};

pub(crate) fn run(
    args: &[String],
    cwd: &Path,
    dirs: &oxa_config::Dirs,
    env: &oxa_config::Env,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let result = (|| -> Result<i32, String> {
        if args == ["--status"] {
            for record in oxa_work::status(cwd)? {
                let state = serde_json::to_value(record.outcome).map_err(|e| e.to_string())?;
                let step = serde_json::to_value(record.step).map_err(|e| e.to_string())?;
                writeln!(
                    out,
                    "{} {} {} {}",
                    record.id,
                    state.as_str().unwrap_or("unknown"),
                    record.progress_at,
                    step.as_str().unwrap_or("unknown")
                )
                .map_err(|e| e.to_string())?;
            }
            return Ok(0);
        }
        let mut end = args.len();
        let (mut rerun, mut publish, mut reports) = (false, false, false);
        while let Some(flag) = args.get(end.wrapping_sub(1)) {
            match flag.as_str() {
                "--rerun" => rerun = true,
                "--publish" => publish = true,
                "--report-problems" => reports = true,
                _ => break,
            }
            end -= 1;
        }
        let args = &args[..end];
        if (publish || reports) && env.get("OXA_CODING_AGENT").is_some() {
            return Err("publishing and problem reporting must be explicitly launched by the user outside an agent tool".into());
        }
        let tasks = match args {
            [file] if !file.starts_with('-') => oxa_work::read_tasks(&cwd.join(file))?,
            [flag, number, model_flag, model] if flag == "--issue" && model_flag == "--model" => {
                if oxa_proto::offline() {
                    return Err("GitHub issue lookup is unavailable in offline mode".into());
                }
                vec![oxa_work::issue_task(
                    cwd,
                    number.parse().map_err(|_| "invalid issue number")?,
                    model.clone(),
                )?]
            }
            _ => {
                return Err(
                    "usage: oxa work <taskfile> | --issue N --model MODEL | --status; task runs accept --rerun, --publish, --report-problems"
                        .into(),
                )
            }
        };
        let binary: PathBuf = std::env::current_exe().map_err(|e| e.to_string())?;
        let mut code = 0;
        for task in tasks {
            let mut record =
                oxa_work::run_task_with_options(cwd, &binary, &task, rerun, oxa_proto::offline())
                    .unwrap_or_else(|detail| oxa_work::blocked_record(&task, detail));
            if publish && !record.worktree.as_os_str().is_empty() {
                record = oxa_work::publish(
                    cwd,
                    &task,
                    oxa_proto::offline(),
                    &mut crate::report_cmd::tty_confirm(env),
                )
                .unwrap_or_else(|detail| oxa_work::blocked_record(&task, detail));
            }
            if reports && !record.worktree.as_os_str().is_empty() {
                let settings = oxa_config::Settings::load(dirs, cwd, false);
                let ctx = crate::report_cmd::ScrubCtx::from_env(env, dirs);
                record =
                    oxa_work::report_notes(cwd, &task, oxa_proto::offline(), &mut |note, lock, offline| {
                        let mut runner = |program: &str, args: &[String]| {
                            crate::work_reports::process(program, args, cwd, lock)
                        };
                        crate::work_reports::report(
                            note,
                            dirs,
                            &settings,
                            &ctx,
                            offline || oxa_proto::offline(),
                            &mut crate::report_cmd::tty_confirm(env),
                            &mut runner,
                        )
                    })
                    .unwrap_or_else(|detail| oxa_work::blocked_record(&task, detail));
            }
            if record.failed() {
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
