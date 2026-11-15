use std::io::Write;
use std::path::{Path, PathBuf};

pub(crate) fn run(args: &[String], cwd: &Path, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
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
        let rerun = args.last().is_some_and(|s| s == "--rerun");
        let args = if rerun { &args[..args.len() - 1] } else { args };
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
                    "usage: oxa work <taskfile> [--rerun] | --issue N --model MODEL [--rerun] | --status"
                        .into(),
                )
            }
        };
        let binary: PathBuf = std::env::current_exe().map_err(|e| e.to_string())?;
        let mut code = 0;
        for task in tasks {
            let record = oxa_work::run_task_with_options(cwd, &binary, &task, rerun, oxa_proto::offline())
                .unwrap_or_else(|detail| oxa_work::blocked_record(&task, detail));
            if matches!(
                record.outcome,
                oxa_work::Outcome::Red | oxa_work::Outcome::Blocked
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
