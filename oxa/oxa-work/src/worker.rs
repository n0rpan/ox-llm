//! Consume the child's headless contract without retaining its event stream.
use crate::{Result, RunPolicy, Task};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

const LINE_CAP: usize = 64 * 1024;
const DIAGNOSTIC_CAP: usize = 8192;

#[path = "worker_progress.rs"]
mod progress;

pub(crate) enum Observation {
    Session { id: String, pid: u32 },
    Stall(crate::incidents::Stall),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerOutcome {
    Done,
    NothingToDo,
    Blocked,
    Failed,
    Interrupted,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
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
    on_observation: &mut (dyn FnMut(Observation) -> Result<()> + Send),
) -> Result<Worker> {
    run_bounded(
        binary,
        root,
        task,
        prompt,
        policy,
        lock,
        session,
        remaining,
        on_observation,
        Limits {
            grace: Duration::from_secs(60),
            kill_grace: Duration::from_secs(10),
            probe: Duration::from_secs(900),
        },
    )
}

struct Limits {
    grace: Duration,
    kill_grace: Duration,
    probe: Duration,
}

fn run_bounded(
    binary: &Path,
    root: &Path,
    task: &Task,
    prompt: &str,
    policy: &RunPolicy,
    lock: &File,
    session: Option<&str>,
    remaining: Duration,
    on_observation: &mut (dyn FnMut(Observation) -> Result<()> + Send),
    limits: Limits,
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
            "--stall-timeout",
            "900",
        ]);
    if policy.offline {
        command.arg("--offline");
    }
    if let Some(id) = session {
        command.arg(format!("--continue={id}"));
    }
    let (stop_writer, stop_reader) = UnixStream::pair().map_err(|e| e.to_string())?;
    let mut child = command
        .args(["--", prompt])
        .stdin(lock.try_clone().map_err(|e| e.to_string())?)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("start worker: {e}"))?;
    let stdout = child.stdout.take().ok_or("worker stdout missing")?;
    let stderr = child.stderr.take().ok_or("worker stderr missing")?;
    let pid = child.id();
    std::thread::scope(|scope| {
        let (notice_tx, notice_rx) = std::sync::mpsc::channel();
        let header_tx = notice_tx.clone();
        let header = scope.spawn(move || {
            let result = progress::read(
                stdout,
                stop_reader,
                root,
                policy.status_reader.as_ref(),
                pid,
                limits.probe,
                on_observation,
            );
            if let Err(error) = &result {
                let _ = header_tx.send(Some(error.clone()));
            }
            result
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
            limits.grace,
            limits.kill_grace,
            terminate,
        ) {
            Ok(waited) => waited,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                drop(stop_writer);
                let _ = header.join();
                let _ = terminal.join();
                return Err(error);
            }
        };
        drop(stop_writer);
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

#[cfg(test)]
mod progress_deadline_tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    use std::sync::{mpsc, Arc, Mutex};
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    struct Fixture(std::path::PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    struct Release(Option<mpsc::Sender<()>>);
    impl Drop for Release {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }

    #[test]
    fn a_blocked_status_read_cannot_postpone_the_owned_childs_wall_deadline() {
        let fixture = Fixture(std::env::temp_dir().join(format!(
            "oxa-status-deadline-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        )));
        fs::DirBuilder::new().mode(0o700).create(&fixture.0).unwrap();
        let binary = fixture.0.join("worker");
        fs::write(&binary, "#!/bin/sh\ntrap 'exit 42' TERM\nprintf '%s\\n' '{\"type\":\"session\",\"id\":\"saved\"}'\nwhile :; do :; done\n").unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        let lock = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(fixture.0.join("task.lock"))
            .unwrap();
        lock.try_lock().unwrap();
        let (entered, observed) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let release = Release(Some(release));
        let released = Arc::new(Mutex::new(released));
        let policy = RunPolicy {
            git_binary: "/usr/bin/git".into(),
            host_path: "/usr/bin:/bin".into(),
            terminate_binary: Some("/usr/bin/kill".into()),
            offline: true,
            storage: fixture.0.clone(),
            status_reader: Some(Arc::new(move |_, _| {
                entered.send(()).unwrap();
                released.lock().unwrap().recv().unwrap();
                Err("synthetic unavailable status".into())
            })),
        };
        let (started, identity) = mpsc::channel();
        let root = fixture.0.clone();
        let task = Task {
            id: "test".into(),
            branch: "agent/test".into(),
            base: "main".into(),
            model: "fake".into(),
            prompt: "Synthetic task".into(),
            max_time: 2700,
        };
        let running = std::thread::spawn(move || {
            run_bounded(
                &binary,
                &root,
                &task,
                "Synthetic task",
                &policy,
                &lock,
                None,
                Duration::from_secs(1),
                &mut |event| {
                    if let Observation::Session { pid, .. } = event {
                        started.send(pid).unwrap();
                    }
                    Ok(())
                },
                Limits {
                    grace: Duration::from_millis(30),
                    kill_grace: Duration::from_millis(400),
                    probe: Duration::from_millis(1),
                },
            )
        });
        let pid = identity.recv_timeout(Duration::from_secs(5)).unwrap();
        observed.recv_timeout(Duration::from_secs(5)).unwrap();
        let process = std::path::PathBuf::from(format!("/proc/{pid}"));
        let limit = Instant::now() + Duration::from_secs(3);
        while process.exists() && Instant::now() < limit {
            std::thread::sleep(Duration::from_millis(5));
        }
        let reaped_while_status_blocked = !process.exists();
        drop(release);
        let worker = running.join().unwrap().unwrap();
        assert!(reaped_while_status_blocked);
        assert!(matches!(worker.result.outcome, WorkerOutcome::Interrupted));
        assert_eq!(worker.result.interruption, Some(WorkerInterruption::Deadline));
    }
}
