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

pub(crate) fn run(
    args: &[String],
    cwd: &Path,
    env: &Env,
    dirs: &Dirs,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let result = (|| -> Result<i32, String> {
        if let [flag, request] = args {
            if flag == "--acceptance" {
                return crate::work_checks::run(request, cwd, env, dirs, err);
            }
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
            _ => return Err("usage: oxa work <taskfile> | --issue N --model MODEL".into()),
        };
        let binary: PathBuf = std::env::current_exe().map_err(|e| e.to_string())?;
        let output = std::process::Command::new("git")
            .current_dir(cwd)
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .map_err(|e| e.to_string())?;
        if !output.status.success() {
            return Err("oxa work requires a Git repository".into());
        }
        let root = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim())
            .canonicalize()
            .map_err(|e| e.to_string())?;
        let is_trusted = trusted(&root, env, dirs)?;
        let common = std::process::Command::new("git")
            .current_dir(&root)
            .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
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
        let policy = oxa_work::RunPolicy {
            offline: oxa_proto::offline(),
            storage: dirs.agent_dir.join("work").join(key),
        };
        let mut code = 0;
        for task in tasks {
            let record = if is_trusted {
                oxa_work::run_task_with_policy(&root, &binary, &task, &policy)
                    .unwrap_or_else(|detail| oxa_work::blocked_record(&task, detail))
            } else {
                oxa_work::blocked_record(
                    &task,
                    "source project is not trusted; approve it before unattended work".into(),
                )
            };
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
