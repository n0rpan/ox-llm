//! Consume the child's headless contract without retaining its event stream.
use crate::{Result, RunPolicy, Task};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

const LINE_CAP: usize = 64 * 1024;
const DIAGNOSTIC_CAP: usize = 8192;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerOutcome {
    Done,
    NothingToDo,
    Blocked,
    Failed,
    Interrupted,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerInterruption {
    Deadline,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkerResult {
    pub outcome: WorkerOutcome,
    pub exit_code: i32,
    pub error_class: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interruption: Option<WorkerInterruption>,
}

impl WorkerResult {
    pub(crate) fn accepted(&self) -> bool {
        matches!(
            (&self.outcome, self.exit_code),
            (WorkerOutcome::Done, 0) | (WorkerOutcome::NothingToDo, 4)
        )
    }
}

pub(crate) struct Worker {
    pub result: WorkerResult,
    pub session: Option<String>,
    pub diagnostic: String,
}

impl Worker {
    pub fn failure(&self) -> Option<String> {
        (!self.result.accepted()).then(|| {
            format!(
                "worker {:?} (exit {}, {}): {}",
                self.result.outcome,
                self.result.exit_code,
                self.result.error_class.as_deref().unwrap_or("no error class"),
                self.diagnostic.trim()
            )
        })
    }
}

pub(crate) fn run(
    binary: &Path,
    root: &Path,
    task: &Task,
    prompt: &str,
    policy: &RunPolicy,
    lock: &File,
    session: Option<&str>,
    remaining: Duration,
    on_session: &mut (dyn FnMut(&str) -> Result<()> + Send),
) -> Result<Worker> {
    let terminate = policy
        .terminate_binary
        .as_deref()
        .ok_or("no trusted kill executable outside the source project")?;
    let mut command = Command::new(binary);
    crate::isolate_git_environment(&mut command);
    command
        .env("PATH", &policy.host_path)
        .env_remove("OXA_TRUST_PROJECT")
        .current_dir(root)
        .args([
            "-na",
            "--mode=json",
            "--model",
            &task.model,
            "--max-time",
            &remaining.as_secs_f64().to_string(),
        ]);
    if policy.offline {
        command.arg("--offline");
    }
    if task.runner_owns_github() {
        // The provider connection is host-side. The worker's tool sandbox and
        // independent tool adapters have no GitHub access.
        command
            .env(crate::RUNNER_GITHUB_ENV, "1")
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .env_remove("GH_ENTERPRISE_TOKEN")
            .env_remove("GITHUB_ENTERPRISE_TOKEN")
            .env_remove("GH_CONFIG_DIR")
            .args(["--no-mcp", "--no-extensions", "--no-web-search", "--no-subagent"]);
    }
    if let Some(id) = session {
        command.arg(format!("--continue={id}"));
    }
    let mut child = command
        .args(["--", prompt])
        .stdin(lock.try_clone().map_err(|e| e.to_string())?)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("start worker: {e}"))?;
    let stdout = child.stdout.take().ok_or("worker stdout missing")?;
    let stderr = child.stderr.take().ok_or("worker stderr missing")?;
    std::thread::scope(|scope| {
        let (notice_tx, notice_rx) = std::sync::mpsc::channel();
        let header_tx = notice_tx.clone();
        let header = scope.spawn(move || {
            read_session(stdout, &mut |id| {
                let result = on_session(id);
                if let Err(error) = &result {
                    let _ = header_tx.send(Some(error.clone()));
                }
                result
            })
        });
        let terminal = scope.spawn(move || {
            let result = read_result(stderr);
            let _ = notice_tx.send(None);
            result
        });
        let waited = match crate::worker_wait::wait(
            &mut child,
            &notice_rx,
            remaining,
            Duration::from_secs(60),
            Duration::from_secs(10),
            terminate,
        ) {
            Ok(waited) => waited,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = header.join();
                let _ = terminal.join();
                return Err(error);
            }
        };
        let session = header.join().map_err(|_| "worker header reader failed")?;
        let terminal = terminal.join().map_err(|_| "worker result reader failed")?;
        if let Some(error) = waited.failure {
            return Err(error);
        }
        let status = waited.status;
        if waited.deadline {
            return Ok(Worker {
                result: WorkerResult {
                    outcome: WorkerOutcome::Interrupted,
                    exit_code: status
                        .code()
                        .unwrap_or_else(|| 128 + status.signal().unwrap_or(9)),
                    error_class: Some("worker_deadline".into()),
                    interruption: Some(WorkerInterruption::Deadline),
                },
                session: session.ok().flatten(),
                diagnostic: "worker exceeded max_time and its wrap-up grace; worktree retained".into(),
            });
        }
        let session = session?;
        let (result, diagnostic) = terminal?;
        if status.code() != Some(result.exit_code) {
            return Err("worker exit code disagrees with its oxa_result".into());
        }
        if result.accepted() && session.is_none() {
            return Err("successful worker did not report a session id".into());
        }
        Ok(Worker {
            result,
            session,
            diagnostic,
        })
    })
}

fn read_session(pipe: impl Read, on_session: &mut dyn FnMut(&str) -> Result<()>) -> Result<Option<String>> {
    let mut reader = BufReader::new(pipe);
    let session = line(&mut reader)?.and_then(|line| {
        let value: serde_json::Value = serde_json::from_slice(&line).ok()?;
        (value["type"] == "session")
            .then(|| value["id"].as_str().map(str::to_owned))
            .flatten()
            .filter(|id| {
                !id.is_empty()
                    && id.len() <= 128
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
            })
    });
    let recorded = session.as_deref().map(on_session).transpose();
    std::io::copy(&mut reader, &mut std::io::sink()).map_err(|e| e.to_string())?;
    recorded?;
    Ok(session)
}

fn read_result(pipe: impl Read) -> Result<(WorkerResult, String)> {
    let mut reader = BufReader::new(pipe);
    let mut result = None;
    let mut duplicate = false;
    let mut diagnostic = Vec::new();
    while let Some(bytes) = line(&mut reader)? {
        let value = serde_json::from_slice::<serde_json::Value>(&bytes).ok();
        if let Some(value) = value.filter(|v| v["type"] == "oxa_result") {
            let parsed = serde_json::from_value(value).map_err(|e| format!("worker result: {e}"))?;
            duplicate |= result.replace(parsed).is_some();
        } else {
            keep_tail(&mut diagnostic, &bytes, DIAGNOSTIC_CAP);
        }
    }
    if duplicate {
        return Err("worker reported more than one oxa_result".into());
    }
    let diagnostic = String::from_utf8_lossy(&diagnostic).into_owned();
    Ok((
        result.ok_or_else(|| format!("worker did not report oxa_result: {diagnostic}"))?,
        diagnostic,
    ))
}

// Discard an oversized record while still draining the pipe. Event/progress size
// cannot increase the runner's memory use or block a child writing to either pipe.
fn line(reader: &mut impl BufRead) -> Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    let mut oversized = false;
    let mut read = false;
    loop {
        let bytes = reader.fill_buf().map_err(|e| e.to_string())?;
        if bytes.is_empty() {
            return Ok(read.then_some(line));
        }
        read = true;
        let end = bytes.iter().position(|b| *b == b'\n');
        let count = end.map_or(bytes.len(), |i| i + 1);
        if !oversized && line.len() + count <= LINE_CAP {
            line.extend_from_slice(&bytes[..count]);
        } else {
            line.clear();
            oversized = true;
        }
        reader.consume(count);
        if end.is_some() {
            return Ok(Some(line));
        }
    }
}

pub(crate) fn keep_tail(tail: &mut Vec<u8>, bytes: &[u8], cap: usize) {
    if bytes.len() >= cap {
        tail.clear();
        tail.extend_from_slice(&bytes[bytes.len() - cap..]);
    } else {
        let discard = (tail.len() + bytes.len()).saturating_sub(cap);
        tail.drain(..discard);
        tail.extend_from_slice(bytes);
    }
}
