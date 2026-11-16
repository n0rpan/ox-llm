//! Consume the child's headless contract without retaining its event stream.
use crate::{Result, RunPolicy, Task};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Command, Stdio};

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

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkerResult {
    pub outcome: WorkerOutcome,
    pub exit_code: i32,
    pub error_class: Option<String>,
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
) -> Result<Worker> {
    let mut command = Command::new(binary);
    command
        .env("PATH", &policy.host_path)
        .current_dir(root)
        .args(["--mode=json", "--model", &task.model]);
    if policy.offline {
        command.arg("--offline");
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
        let header = scope.spawn(|| read_session(stdout));
        let terminal = scope.spawn(|| read_result(stderr));
        let status = child.wait().map_err(|e| format!("wait for worker: {e}"))?;
        let session = header.join().map_err(|_| "worker header reader failed")??;
        let (result, diagnostic) = terminal.join().map_err(|_| "worker result reader failed")??;
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

fn read_session(pipe: impl Read) -> Result<Option<String>> {
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
    std::io::copy(&mut reader, &mut std::io::sink()).map_err(|e| e.to_string())?;
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
