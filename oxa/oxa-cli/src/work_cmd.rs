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
        let (mut rerun, mut publish, mut reports) = (false, false, false);
        let args: Vec<String> = args
            .iter()
            .filter(|word| match word.as_str() {
                "--rerun" => {
                    rerun = true;
                    false
                }
                "--publish" => {
                    publish = true;
                    false
                }
                "--report-problems" => {
                    reports = true;
                    false
                }
                _ => true,
            })
            .cloned()
            .collect();
        let args = args.as_slice();
        if (publish || reports) && env.get("OXA_CODING_AGENT").is_some() {
            return Err("publishing and problem reporting must be explicitly launched by the user outside an agent tool".into());
        }
        let context = std::sync::Arc::new(Context {
            cwd: cwd.to_path_buf(),
            dirs: dirs.clone(),
            env: env.clone(),
            binary: std::env::current_exe().map_err(|e| e.to_string())?,
            offline: oxa_proto::offline(),
            rerun,
            publish,
            reports,
        });
        if args.first().is_some_and(|s| s == "--queue") {
            let (directory, options) = queue_options(args, cwd)?;
            let worker = {
                let context = context.clone();
                std::sync::Arc::new(move |task: &oxa_work::Task| context.task(task))
            };
            let summary = oxa_work::run_queue(cwd, &directory, &options, worker, &mut |event| {
                serde_json::to_writer(&mut *out, &event).map_err(|e| e.to_string())?;
                writeln!(out).and_then(|_| out.flush()).map_err(|e| e.to_string())
            })?;
            return Ok(i32::from(summary.failed > 0 || summary.pending > 0));
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
                    "usage: oxa work <taskfile> | --issue N --model MODEL | --queue DIR --max N [--once] [--min-free-mib N] | --status; task runs accept --rerun, --publish, --report-problems"
                        .into(),
                )
            }
        };
        let mut code = 0;
        for task in tasks {
            let record = context.task(&task);
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

struct Context {
    cwd: PathBuf,
    dirs: oxa_config::Dirs,
    env: oxa_config::Env,
    binary: PathBuf,
    offline: bool,
    rerun: bool,
    publish: bool,
    reports: bool,
}
impl Context {
    fn task(&self, task: &oxa_work::Task) -> oxa_work::Record {
        let mut record =
            oxa_work::run_task_with_options(&self.cwd, &self.binary, task, self.rerun, self.offline)
                .unwrap_or_else(|detail| oxa_work::blocked_record(task, detail));
        if self.publish && !record.worktree.as_os_str().is_empty() {
            record = oxa_work::publish(
                &self.cwd,
                task,
                self.offline,
                &mut crate::report_cmd::tty_confirm(&self.env),
            )
            .unwrap_or_else(|detail| oxa_work::blocked_record(task, detail));
        }
        if self.reports && !record.worktree.as_os_str().is_empty() {
            let settings = oxa_config::Settings::load(&self.dirs, &self.cwd, false);
            let ctx = crate::report_cmd::ScrubCtx::from_env(&self.env, &self.dirs);
            record = oxa_work::report_notes(&self.cwd, task, self.offline, &mut |note, lock, offline| {
                let mut runner = |program: &str, args: &[String]| {
                    crate::work_reports::process(program, args, &self.cwd, lock)
                };
                crate::work_reports::report(
                    note,
                    &self.dirs,
                    &settings,
                    &ctx,
                    offline || self.offline,
                    &mut crate::report_cmd::tty_confirm(&self.env),
                    &mut runner,
                )
            })
            .unwrap_or_else(|detail| oxa_work::blocked_record(task, detail));
        }
        record
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
