use std::io::Write;
use std::path::{Path, PathBuf};

pub(crate) fn run(args: &[String], cwd: &Path, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let result = (|| -> Result<i32, String> {
        let tasks = match args {
            [file] if !file.starts_with('-') => oxa_work::read_tasks(&cwd.join(file))?,
            [flag, number, model_flag, model] if flag == "--issue" && model_flag == "--model" => {
                vec![oxa_work::issue_task(
                    cwd,
                    number.parse().map_err(|_| "invalid issue number")?,
                    model.clone(),
                )?]
            }
            _ => return Err("usage: oxa work <taskfile> | --issue N --model MODEL".into()),
        };
        let binary: PathBuf = std::env::current_exe().map_err(|e| e.to_string())?;
        let mut code = 0;
        for task in tasks {
            let record = oxa_work::run_task(cwd, &binary, &task)?;
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
