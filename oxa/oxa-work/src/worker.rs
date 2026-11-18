//! Consume the child's headless contract without retaining its event stream.
use crate::{Result, RunPolicy, Task};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

pub mod owner_cli;

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

/// Invocation-local token counts reported by the worker at settlement.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct WorkerTokens {
    pub prompt: Option<u64>,
    pub completion: Option<u64>,
    /// Cached input is included in the prompt count.
    pub cached: Option<u64>,
    pub complete: bool,
    pub unreported_requests: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkerResult {
    pub outcome: WorkerOutcome,
    pub exit_code: i32,
    pub error_class: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interruption: Option<WorkerInterruption>,
    /// Measured process usage. Older workers and interrupted processes may omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<WorkerTokens>,
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
    pub notes: Vec<crate::ProblemNote>,
    /// Crates the worker declared as acceptance tests in its last message (#1442).
    pub declared: crate::derived::Declaration,
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
    #[cfg(target_os = "linux")] managed_steering: Option<&crate::managed_steering::Launch>,
    on_session: &mut (dyn FnMut(&str) -> Result<()> + Send),
) -> Result<Worker> {
    let terminate = policy
        .terminate_binary
        .as_deref()
        .ok_or("no trusted kill executable outside the source project")?;
    let mut command = Command::new(binary);
    crate::isolate_git_environment(&mut command);
    #[cfg(target_os = "linux")]
    {
        command.env_remove(crate::managed_steering::FD_ENV);
        if let Some(launch) = managed_steering {
            launch.attach(&mut command)?;
        }
    }
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
    if policy.runner_owns_github {
        // The provider connection is host-side. The worker's tool sandbox and
        // independent tool adapters have no GitHub access.
        crate::restrict_child_github(&mut command);
        command.args(["--no-mcp", "--no-extensions", "--no-web-search", "--no-subagent"]);
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
            let tokens = terminal
                .ok()
                .filter(|(result, _)| status.code() == Some(result.exit_code))
                .and_then(|(result, _)| result.tokens);
            return Ok(Worker {
                result: WorkerResult {
                    outcome: WorkerOutcome::Interrupted,
                    exit_code: status
                        .code()
                        .unwrap_or_else(|| 128 + status.signal().unwrap_or(9)),
                    error_class: Some("worker_deadline".into()),
                    interruption: Some(WorkerInterruption::Deadline),
                    tokens,
                },
                session: session.ok().and_then(|(id, _, _)| id),
                diagnostic: "worker exceeded max_time and its wrap-up grace; worktree retained".into(),
                notes: vec![],
                declared: crate::derived::Declaration::Absent,
            });
        }
        let (session, notes, declared) = session?;
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
            notes,
            declared,
        })
    })
}

fn read_session(
    pipe: impl Read,
    on_session: &mut dyn FnMut(&str) -> Result<()>,
) -> Result<(
    Option<String>,
    Vec<crate::ProblemNote>,
    crate::derived::Declaration,
)> {
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
    let mut notes = vec![];
    // The worker's last message carries its declarations; a later message replaces them.
    let mut declared = crate::derived::Declaration::Absent;
    while let Some(bytes) = line(&mut reader)? {
        if bytes.is_empty() {
            // An oversized event could have contained the last acceptance block.
            // A later complete assistant message can still supply its own declaration.
            declared = crate::derived::Declaration::Unreadable;
            notes.clear();
            continue;
        }
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            continue;
        };
        if value["type"] == "message_end" && value["message"]["role"] == "assistant" {
            let mut text = String::new();
            if let Some(parts) = value["message"]["content"].as_array() {
                for part in parts {
                    if part["type"] == "text" {
                        if let Some(value) = part["text"].as_str() {
                            text.push_str(value);
                        }
                    }
                }
            }
            notes = crate::notes::parse(&text);
            declared = crate::derived::parse_declaration(&text);
        }
    }
    recorded?;
    Ok((session, notes, declared))
}

fn read_result(pipe: impl Read) -> Result<(WorkerResult, String)> {
    let mut reader = BufReader::new(pipe);
    let mut result = None;
    let mut duplicate = false;
    let mut diagnostic = Vec::new();
    while let Some(bytes) = line(&mut reader)? {
        let value = serde_json::from_slice::<serde_json::Value>(&bytes).ok();
        if let Some(mut value) = value.filter(|v| v["type"] == "oxa_result") {
            let measured_process = value["usage_scope"] == "process";
            // Usage is optional evidence. Strip it before parsing the outcome so a
            // missing or malformed measurement cannot change the worker's disposition.
            let tokens = value.get_mut("tokens").map(std::mem::take);
            let mut parsed: WorkerResult =
                serde_json::from_value(value).map_err(|e| format!("worker result: {e}"))?;
            if measured_process {
                parsed.tokens = tokens.and_then(|tokens| serde_json::from_value(tokens).ok());
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_discarded_assistant_event_leaves_declaration_evidence_unreadable() {
        let message = json!({
            "type": "message_end",
            "message": {
                "role": "assistant",
                "content": [{"type": "text", "text": "plain text ".repeat(LINE_CAP / 8)}],
            }
        });
        let events = format!("{{\"type\":\"session\",\"id\":\"fixture\"}}\n{message}\n");
        let (_, _, declaration) = read_session(events.as_bytes(), &mut |_| Ok(())).unwrap();
        assert_eq!(declaration, crate::derived::Declaration::Unreadable);
    }

    #[test]
    fn terminal_process_usage_survives_serializing_the_worker_result() {
        let receipt = json!({
            "type": "oxa_result", "outcome": "done", "exit_code": 0, "usage_scope": "process",
            "tokens": {
                "prompt": 480, "completion": 21, "cached": null, "complete": false,
                "unreported_requests": 1,
            }
        });
        let (result, diagnostic) = read_result(receipt.to_string().as_bytes()).unwrap();
        assert!(result.accepted());
        assert!(diagnostic.is_empty());
        let saved = serde_json::to_value(result).unwrap();
        assert_eq!(saved["tokens"], receipt["tokens"]);
        let restored: WorkerResult = serde_json::from_value(saved).unwrap();
        assert_eq!(
            restored.tokens,
            Some(WorkerTokens {
                prompt: Some(480),
                completion: Some(21),
                cached: None,
                complete: false,
                unreported_requests: Some(1),
            })
        );
    }

    #[test]
    fn legacy_or_unusable_usage_remains_unknown_without_changing_outcomes() {
        let mut receipt = json!({"type": "oxa_result", "outcome": "done", "exit_code": 0});
        let (legacy, _) = read_result(receipt.to_string().as_bytes()).unwrap();
        assert!(legacy.accepted());
        assert!(legacy.tokens.is_none());
        assert!(serde_json::to_value(legacy).unwrap().get("tokens").is_none());

        receipt["usage_scope"] = json!("merged_task");
        receipt["tokens"] = json!({
            "prompt": 0, "completion": 0, "cached": 0, "complete": true, "unreported_requests": 0
        });
        let (other_scope, _) = read_result(receipt.to_string().as_bytes()).unwrap();
        assert!(other_scope.accepted());
        assert!(other_scope.tokens.is_none());

        receipt["usage_scope"] = json!("process");
        receipt["tokens"]["prompt"] = json!("unknown");
        let (malformed, _) = read_result(receipt.to_string().as_bytes()).unwrap();
        assert!(malformed.accepted());
        assert!(malformed.tokens.is_none());
    }
}
