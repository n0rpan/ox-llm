//! Issue #127 (b): background jobs for the `bash` tool.
//!
//! A [`JobRegistry`] owns the detached commands a session starts with
//! `bash {command, background: true}`. Each job runs the same sandboxed command as a
//! normal `bash` call, but on its own thread with a per-job [`CancelToken`]; the call
//! returns at once with a job id. A later `bash {job: "job-N"}` reads the job's status
//! and the last 8 KiB of its combined output. [`JobRegistry::kill_all`] cancels every
//! running job, which makes the sandbox's run loop kill the whole process group.
//!
//! Limits: at most [`MAX_JOBS`] jobs run at once (a further start is refused); a
//! finished job keeps at most [`BUFFER_CAP`] bytes of output (a ring buffer).

use oxa_proto::{CancelToken, ExecResult, ExecSpec, Sandbox};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// How many background jobs may run at once.
pub const MAX_JOBS: usize = 4;
/// The ring-buffer cap of a job's combined output, in bytes.
pub const BUFFER_CAP: usize = 64 * 1024;
/// How much of a job's output a read returns, in bytes.
pub const READ_CAP: usize = 8 * 1024;

/// The status of one job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    Running,
    /// The command exited with this code.
    Exited(i32),
    /// Killed by a signal, a cancel or a timeout (no exit code).
    Killed,
}

impl JobStatus {
    /// The one-line status text a read returns.
    pub fn text(self) -> String {
        match self {
            JobStatus::Running => "running".to_string(),
            JobStatus::Exited(code) => format!("exited {code}"),
            JobStatus::Killed => "killed".to_string(),
        }
    }
}

/// The shared state of one job.
struct Job {
    cancel: CancelToken,
    output: Arc<Mutex<Vec<u8>>>,
    status: Arc<Mutex<JobStatus>>,
}

/// A snapshot of a job for a read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRead {
    pub status: JobStatus,
    /// The last [`READ_CAP`] bytes of the combined output so far.
    pub output: String,
}

#[derive(Default)]
struct State {
    jobs: HashMap<String, Job>,
    next: u64,
}

/// The background jobs of one session. `Arc`-shared between the `bash` tool and the
/// session that owns it; `Send + Sync` so it can live in `Arc<dyn Tool>`.
pub struct JobRegistry {
    state: Mutex<State>,
    /// The sandbox every job runs under, set once by the session that owns the registry.
    sandbox: Mutex<Option<Arc<dyn Sandbox>>>,
}

impl Default for JobRegistry {
    fn default() -> Self {
        JobRegistry {
            state: Mutex::new(State::default()),
            sandbox: Mutex::new(None),
        }
    }
}

impl std::fmt::Debug for JobRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("JobRegistry { .. }")
    }
}

impl JobRegistry {
    pub fn new() -> JobRegistry {
        JobRegistry::default()
    }

    /// The sandbox every job runs under. Set once by the session that owns the registry
    /// (the same `Arc` the session's tools run with).
    pub fn set_sandbox(&self, sandbox: Arc<dyn Sandbox>) {
        *self.sandbox.lock().unwrap() = Some(sandbox);
    }

    /// How many jobs are currently running.
    pub fn running(&self) -> usize {
        self.state
            .lock()
            .unwrap()
            .jobs
            .values()
            .filter(|j| *j.status.lock().unwrap() == JobStatus::Running)
            .count()
    }

    /// Start `spec` detached and return its id. `Err` when [`MAX_JOBS`] jobs are already
    /// running, or when no sandbox has been set.
    pub fn spawn(&self, spec: ExecSpec) -> Result<String, String> {
        let sandbox = self
            .sandbox
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| "background jobs are unavailable: no sandbox".to_string())?;
        let mut state = self.state.lock().unwrap();
        if state
            .jobs
            .values()
            .filter(|j| *j.status.lock().unwrap() == JobStatus::Running)
            .count()
            >= MAX_JOBS
        {
            return Err(format!(
                "At most {MAX_JOBS} background jobs can run at once; wait for one to finish"
            ));
        }
        state.next += 1;
        let id = format!("job-{}", state.next);
        let cancel = CancelToken::new();
        let output = Arc::new(Mutex::new(Vec::new()));
        let status = Arc::new(Mutex::new(JobStatus::Running));
        state.jobs.insert(
            id.clone(),
            Job {
                cancel: cancel.clone(),
                output: output.clone(),
                status: status.clone(),
            },
        );
        drop(state);

        std::thread::spawn(move || {
            let mut sink = |chunk: &[u8]| {
                let mut buf = output.lock().unwrap();
                buf.extend_from_slice(chunk);
                if buf.len() > BUFFER_CAP {
                    let keep = buf.len() - BUFFER_CAP;
                    buf.drain(..keep);
                }
            };
            let res: ExecResult = sandbox.run_streaming(&spec, &cancel, &mut sink);
            let final_status = match res.exit {
                Some(code) => JobStatus::Exited(code),
                None => JobStatus::Killed,
            };
            *status.lock().unwrap() = final_status;
        });
        Ok(id)
    }

    /// Read a job's status and the last [`READ_CAP`] bytes of its output. `Err` for an
    /// unknown id.
    pub fn read(&self, id: &str) -> Result<JobRead, String> {
        let state = self.state.lock().unwrap();
        let job = state.jobs.get(id).ok_or_else(|| format!("Unknown job: {id}"))?;
        let status = *job.status.lock().unwrap();
        let output = job.output.lock().unwrap();
        let start = output.len().saturating_sub(READ_CAP);
        let text = String::from_utf8_lossy(&output[start..]).into_owned();
        Ok(JobRead { status, output: text })
    }

    /// The bytes a job's ring buffer holds right now (for tests of the cap). `None` for an
    /// unknown id.
    #[doc(hidden)]
    pub fn buffered_len(&self, id: &str) -> Option<usize> {
        let state = self.state.lock().unwrap();
        state.jobs.get(id).map(|j| j.output.lock().unwrap().len())
    }

    /// Cancel every running job AND forget them all: ids stop resolving and the slots free
    /// at once. The job threads keep their own `Arc`s and settle on their own.
    pub fn reset(&self) {
        let mut state = self.state.lock().unwrap();
        for job in state.jobs.values() {
            job.cancel.cancel();
        }
        state.jobs.clear();
    }

    /// Cancel every running job. The sandbox's run loop then kills each process group;
    /// the jobs' statuses become [`JobStatus::Killed`] as their threads settle.
    pub fn kill_all(&self) {
        let state = self.state.lock().unwrap();
        for job in state.jobs.values() {
            job.cancel.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxa_proto::OxaError;
    use std::path::Path;

    /// A sandbox that blocks until its cancel token is set, then reports "killed".
    struct Blocking {
        seen: Mutex<Option<ExecSpec>>,
    }

    impl Sandbox for Blocking {
        fn check(&self) -> Result<(), OxaError> {
            Ok(())
        }
        fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
            Vec::new()
        }
        fn run(&self, s: &ExecSpec, c: &CancelToken, live: &mut dyn FnMut(&[u8])) -> ExecResult {
            *self.seen.lock().unwrap() = Some(s.clone());
            live(b"started\n");
            while !c.is_cancelled() {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            ExecResult {
                output: b"started\n".to_vec(),
                exit: None,
                timed_out: false,
            }
        }
    }

    fn spec() -> ExecSpec {
        ExecSpec {
            command: "sleep 100".to_string(),
            cwd: Path::new("/work").to_path_buf(),
            writable: true,
            net: false,
            timeout_s: None,
            env: Vec::new(),
        }
    }

    #[test]
    fn spawn_read_and_kill() {
        let reg = JobRegistry::new();
        let sb = Arc::new(Blocking {
            seen: Mutex::new(None),
        });
        reg.set_sandbox(sb.clone());
        let id = reg.spawn(spec()).unwrap();
        assert_eq!(id, "job-1");
        // Give the thread a moment to stream its first chunk.
        std::thread::sleep(std::time::Duration::from_millis(50));
        let read = reg.read(&id).unwrap();
        assert_eq!(read.status, JobStatus::Running);
        assert_eq!(read.output, "started\n");
        assert_eq!(reg.running(), 1);

        reg.kill_all();
        // The thread notices the cancel and records "killed".
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let read = reg.read(&id).unwrap();
            if read.status == JobStatus::Killed {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "job did not settle as killed"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(reg.running(), 0);
    }

    #[test]
    fn unknown_job_is_an_error() {
        let reg = JobRegistry::new();
        assert_eq!(reg.read("job-9").unwrap_err(), "Unknown job: job-9");
    }

    #[test]
    fn spawn_without_a_sandbox_is_an_error() {
        let reg = JobRegistry::new();
        assert_eq!(
            reg.spawn(spec()).unwrap_err(),
            "background jobs are unavailable: no sandbox"
        );
    }

    #[test]
    fn fifth_job_is_refused() {
        let reg = JobRegistry::new();
        let sb = Arc::new(Blocking {
            seen: Mutex::new(None),
        });
        reg.set_sandbox(sb.clone());
        for _ in 0..MAX_JOBS {
            reg.spawn(spec()).unwrap();
        }
        assert_eq!(reg.running(), MAX_JOBS);
        let err = reg.spawn(spec()).unwrap_err();
        assert!(err.contains("At most 4 background jobs"), "{err}");
        reg.kill_all();
    }

    #[test]
    fn exited_status_records_the_code() {
        struct Exits(i32);
        impl Sandbox for Exits {
            fn check(&self) -> Result<(), OxaError> {
                Ok(())
            }
            fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
                Vec::new()
            }
            fn run(&self, _s: &ExecSpec, _c: &CancelToken, live: &mut dyn FnMut(&[u8])) -> ExecResult {
                live(b"done\n");
                ExecResult {
                    output: b"done\n".to_vec(),
                    exit: Some(self.0),
                    timed_out: false,
                }
            }
        }
        let reg = JobRegistry::new();
        reg.set_sandbox(Arc::new(Exits(3)));
        let id = reg.spawn(spec()).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let read = reg.read(&id).unwrap();
            if read.status == JobStatus::Exited(3) {
                assert_eq!(read.output, "done\n");
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "job did not record its exit code"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn output_is_a_ring_buffer() {
        struct Flood;
        impl Sandbox for Flood {
            fn check(&self) -> Result<(), OxaError> {
                Ok(())
            }
            fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
                Vec::new()
            }
            fn run(&self, _s: &ExecSpec, _c: &CancelToken, live: &mut dyn FnMut(&[u8])) -> ExecResult {
                let chunk = vec![b'x'; 8192];
                for _ in 0..16 {
                    live(&chunk);
                }
                ExecResult {
                    output: Vec::new(),
                    exit: Some(0),
                    timed_out: false,
                }
            }
        }
        let reg = JobRegistry::new();
        reg.set_sandbox(Arc::new(Flood));
        let id = reg.spawn(spec()).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let read = reg.read(&id).unwrap();
            if read.status == JobStatus::Exited(0) {
                // 16 * 8192 = 128 KiB written; the buffer keeps only the last 64 KiB.
                assert_eq!(read.output.len(), READ_CAP);
                assert!(read.output.bytes().all(|b| b == b'x'));
                break;
            }
            assert!(std::time::Instant::now() < deadline, "job did not finish");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}
