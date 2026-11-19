//! Issue #127 (b): background jobs for the `bash` tool.
//!
//! A [`JobRegistry`] owns the detached commands a session starts with
//! `bash {command, background: true}`. Each job runs the same sandboxed command as a
//! normal `bash` call, but on its own thread with a per-job [`CancelToken`]; the call
//! returns at once with a job id. A later `bash {job: "job-N"}` reads the job's status
//! and the last 8 KiB of its combined output. [`JobRegistry::kill`] stops one job (`bash {job,
//! kill: true}`) and [`JobRegistry::kill_all`] cancels every
//! running job, which makes the sandbox's run loop kill the whole process group.
//!
//! Limits: at most [`MAX_JOBS`] jobs run at once by default (`oxa.bash.maxJobs` sets it per
//! registry, see [`JobRegistry::with_max_jobs`]; a further start is refused); a
//! finished job keeps at most [`BUFFER_CAP`] bytes of output (a ring buffer), and only the
//! last [`MAX_FINISHED`] finished jobs stay readable.

use oxa_proto::{CancelToken, ExecResult, ExecSpec, Sandbox};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How often a [`JobRegistry::wait`] checks the job and the cancel token.
const WAIT_TICK: Duration = Duration::from_millis(10);

/// How long [`JobRegistry::kill`] waits for a cancelled job to settle.
const KILL_SETTLE: Duration = Duration::from_secs(5);

/// The default for how many background jobs may run at once (`oxa.bash.maxJobs`).
pub const MAX_JOBS: usize = 16;
/// How many finished jobs stay readable; spawning a job forgets the oldest ones beyond this.
pub const MAX_FINISHED: usize = 16;
/// The ring-buffer cap of a job's combined output, in bytes.
pub const BUFFER_CAP: usize = 64 * 1024;
/// How much of a job's output a read returns, in bytes.
pub const READ_CAP: usize = 8 * 1024;
/// How much of a job's output a listing looks at for its last line, in bytes.
const LAST_LINE_SCAN: usize = 4 * 1024;
/// The longest last line a listing shows, in characters.
const LAST_LINE_CAP: usize = 100;

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

/// The output of one job: the ring buffer plus counters for progress.
#[derive(Default)]
struct Out {
    /// The last [`BUFFER_CAP`] bytes.
    buf: Vec<u8>,
    /// Every byte the job has written, ever (not capped).
    total: u64,
    /// `total` as of the last read: what the model has seen.
    seen: u64,
    /// Whether new output and the current buffer are limited to [`BUFFER_CAP`].
    bounded: bool,
}

/// The shared state of one job.
struct Job {
    cancel: CancelToken,
    output: Arc<Mutex<Out>>,
    status: Arc<Mutex<JobStatus>>,
    /// When the job was started, for the age a listing shows.
    started: Instant,
    /// The sandbox ran the command with the user's own rights (no OS sandbox).
    unsandboxed: Arc<AtomicBool>,
}

/// A finished job taken out of the registry by [`JobRegistry::take_finished`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobDone {
    pub status: JobStatus,
    /// All the output the job wrote (a job started with `keep_all`), else its last
    /// [`BUFFER_CAP`] bytes.
    pub output: Vec<u8>,
    pub unsandboxed: bool,
}

/// A snapshot of a job for a read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRead {
    pub status: JobStatus,
    /// The last [`READ_CAP`] bytes of the combined output so far.
    pub output: String,
    /// Every byte of output the job has written so far (not only the ones kept).
    pub total: u64,
}

/// The outcome of [`JobRegistry::kill`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobKill {
    /// The job's status after the kill (still `Running` if it did not settle in time).
    pub status: JobStatus,
    /// False when the job had already finished before the call.
    pub was_running: bool,
}

/// A snapshot of one job for a listing (issue #865). Reading it never consumes output: unlike
/// [`JobRegistry::read`] it does not mark the job's output as seen, so a later
/// [`JobRegistry::wait`] still reports the news.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobInfo {
    pub id: String,
    pub status: JobStatus,
    /// How long ago the job started.
    pub age: Duration,
    /// The last non-empty line of the job's output so far, trimmed and capped. Empty when the
    /// job has not written a line yet.
    pub last_line: String,
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
    /// How many jobs may run at once ([`MAX_JOBS`] unless the owner set `oxa.bash.maxJobs`).
    max_jobs: usize,
}

impl Default for JobRegistry {
    fn default() -> Self {
        JobRegistry {
            state: Mutex::new(State::default()),
            sandbox: Mutex::new(None),
            max_jobs: MAX_JOBS,
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

    /// A registry that runs at most `max` jobs at once (`oxa.bash.maxJobs`; at least 1).
    pub fn with_max_jobs(max: usize) -> JobRegistry {
        JobRegistry {
            max_jobs: max.max(1),
            ..JobRegistry::default()
        }
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

    /// Start `spec` detached and return its id. `Err` when the registry's limit of jobs is
    /// already running (the refusal names the oldest one, so the model can stop it), or when no
    /// sandbox has been set.
    pub fn spawn(&self, spec: ExecSpec) -> Result<String, String> {
        let sandbox = self
            .sandbox
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| "background jobs are unavailable: no sandbox".to_string())?;
        self.spawn_on(sandbox, spec, false)
    }

    /// Like [`JobRegistry::spawn`], on `sandbox`. With `keep_all` the job keeps every byte of
    /// its output (not only the last [`BUFFER_CAP`]): a foreground command that may move to the
    /// background keeps the whole result it had before, and [`JobRegistry::take_finished`]
    /// hands it over and forgets the job.
    pub fn spawn_on(
        &self,
        sandbox: Arc<dyn Sandbox>,
        spec: ExecSpec,
        keep_all: bool,
    ) -> Result<String, String> {
        let mut state = self.state.lock().unwrap();
        let running: Vec<String> = state
            .jobs
            .iter()
            .filter(|(_, j)| *j.status.lock().unwrap() == JobStatus::Running)
            .map(|(id, _)| id.clone())
            .collect();
        if running.len() >= self.max_jobs {
            let oldest = running
                .iter()
                .min_by_key(|id| job_number(id))
                .cloned()
                .unwrap_or_default();
            return Err(format!(
                "At most {} background jobs can run at once. Stop one with bash {{\"job\": \"{oldest}\", \"kill\": true}} or wait for it to finish. The limit is oxa.bash.maxJobs.",
                self.max_jobs
            ));
        }
        // Forget the oldest finished jobs (ids grow with start order), so a long session does
        // not keep every job's output tail. Running jobs are never dropped.
        let mut finished: Vec<(u64, String)> = state
            .jobs
            .iter()
            .filter(|(_, j)| *j.status.lock().unwrap() != JobStatus::Running)
            .map(|(id, _)| (job_number(id), id.clone()))
            .collect();
        if finished.len() >= MAX_FINISHED {
            finished.sort();
            let drop_n = finished.len() + 1 - MAX_FINISHED;
            for (_, id) in finished.into_iter().take(drop_n) {
                state.jobs.remove(&id);
            }
        }
        state.next += 1;
        let id = format!("job-{}", state.next);
        let cancel = CancelToken::new();
        let output = Arc::new(Mutex::new(Out {
            bounded: !keep_all,
            ..Out::default()
        }));
        let status = Arc::new(Mutex::new(JobStatus::Running));
        let unsandboxed = Arc::new(AtomicBool::new(false));
        let unsandboxed_job = unsandboxed.clone();
        state.jobs.insert(
            id.clone(),
            Job {
                cancel: cancel.clone(),
                output: output.clone(),
                status: status.clone(),
                started: Instant::now(),
                unsandboxed,
            },
        );
        drop(state);

        std::thread::spawn(move || {
            let mut sink = |chunk: &[u8]| {
                let mut out = output.lock().unwrap();
                out.total += chunk.len() as u64;
                out.buf.extend_from_slice(chunk);
                if out.bounded && out.buf.len() > BUFFER_CAP {
                    let keep = out.buf.len() - BUFFER_CAP;
                    out.buf.drain(..keep);
                }
            };
            let res: ExecResult = sandbox.run_streaming(&spec, &cancel, &mut sink);
            // Streaming executors normally return an empty `output`, but launch failures can
            // happen before there is a process whose pipes feed `sink`. Preserve any such
            // diagnostic in the job buffer so foreground callers following the job receive it.
            if !res.output.is_empty() {
                sink(&res.output);
            }
            unsandboxed_job.store(res.unsandboxed, Ordering::SeqCst);
            let final_status = match res.exit {
                Some(code) => JobStatus::Exited(code),
                None => JobStatus::Killed,
            };
            *status.lock().unwrap() = final_status;
        });
        Ok(id)
    }

    /// Switch a foreground job that detached to bounded background retention.
    pub fn bound_output(&self, id: &str) -> Result<(), String> {
        let state = self.state.lock().unwrap();
        let job = state.jobs.get(id).ok_or_else(|| format!("Unknown job: {id}"))?;
        let mut out = job.output.lock().unwrap();
        out.bounded = true;
        if out.buf.len() > BUFFER_CAP {
            let drop = out.buf.len() - BUFFER_CAP;
            out.buf.drain(..drop);
        }
        Ok(())
    }

    /// Read a job's status and the last [`READ_CAP`] bytes of its output. `Err` for an
    /// unknown id.
    pub fn read(&self, id: &str) -> Result<JobRead, String> {
        let state = self.state.lock().unwrap();
        let job = state.jobs.get(id).ok_or_else(|| format!("Unknown job: {id}"))?;
        let status = *job.status.lock().unwrap();
        let mut out = job.output.lock().unwrap();
        out.seen = out.total;
        let start = out.buf.len().saturating_sub(READ_CAP);
        let text = String::from_utf8_lossy(&out.buf[start..]).into_owned();
        Ok(JobRead {
            status,
            output: text,
            total: out.total,
        })
    }

    /// Take a finished job out of the registry: its status, all buffered output and whether it
    /// ran unsandboxed. `Ok(None)` while the job still runs (it stays); `Err` for an unknown id.
    pub fn take_finished(&self, id: &str) -> Result<Option<JobDone>, String> {
        let mut state = self.state.lock().unwrap();
        let job = state.jobs.get(id).ok_or_else(|| format!("Unknown job: {id}"))?;
        let status = *job.status.lock().unwrap();
        if status == JobStatus::Running {
            return Ok(None);
        }
        let output = std::mem::take(&mut job.output.lock().unwrap().buf);
        let unsandboxed = job.unsandboxed.load(Ordering::SeqCst);
        state.jobs.remove(id);
        Ok(Some(JobDone {
            status,
            output,
            unsandboxed,
        }))
    }

    /// #150 (2): block for up to `max` until the job exits, prints output it has not yet shown
    /// to a read, or `cancel` fires. Returns at once when there is already such news. `Err` for
    /// an unknown id.
    pub fn wait(&self, id: &str, max: Duration, cancel: &CancelToken) -> Result<(), String> {
        let (status, output) = {
            let state = self.state.lock().unwrap();
            let job = state.jobs.get(id).ok_or_else(|| format!("Unknown job: {id}"))?;
            (job.status.clone(), job.output.clone())
        };
        let deadline = Instant::now() + max;
        loop {
            if *status.lock().unwrap() != JobStatus::Running {
                return Ok(());
            }
            {
                let out = output.lock().unwrap();
                if out.total != out.seen {
                    return Ok(());
                }
            }
            let now = Instant::now();
            if now >= deadline || cancel.is_cancelled() {
                return Ok(());
            }
            std::thread::sleep((deadline - now).min(WAIT_TICK));
        }
    }

    /// The bytes a job's ring buffer holds right now (for tests of the cap). `None` for an
    /// unknown id.
    #[doc(hidden)]
    pub fn buffered_len(&self, id: &str) -> Option<usize> {
        let state = self.state.lock().unwrap();
        state.jobs.get(id).map(|j| j.output.lock().unwrap().buf.len())
    }

    /// The bytes a job has written that no read has shown yet (for tests of the listing's
    /// non-consuming contract). `None` for an unknown id.
    #[doc(hidden)]
    pub fn unseen_len(&self, id: &str) -> Option<u64> {
        let state = self.state.lock().unwrap();
        state.jobs.get(id).map(|j| {
            let out = j.output.lock().unwrap();
            out.total - out.seen
        })
    }

    /// A snapshot of every job, oldest first (issue #865). Unlike [`JobRegistry::read`] it does
    /// not mark output as seen, so a later [`JobRegistry::wait`] still reports the news.
    pub fn list(&self) -> Vec<JobInfo> {
        let state = self.state.lock().unwrap();
        let mut ids: Vec<&String> = state.jobs.keys().collect();
        ids.sort_by_key(|id| job_number(id));
        ids.into_iter()
            .map(|id| {
                let job = &state.jobs[id];
                JobInfo {
                    id: id.clone(),
                    status: *job.status.lock().unwrap(),
                    age: job.started.elapsed(),
                    last_line: last_line(&job.output.lock().unwrap().buf),
                }
            })
            .collect()
    }

    /// #519: cancel one job and wait (briefly) for it to settle, so its slot is free when this
    /// returns. A job that already finished is left as it is (`was_running: false`). `Err` for an
    /// unknown id.
    pub fn kill(&self, id: &str) -> Result<JobKill, String> {
        let (cancel, status) = {
            let state = self.state.lock().unwrap();
            let job = state.jobs.get(id).ok_or_else(|| format!("Unknown job: {id}"))?;
            (job.cancel.clone(), job.status.clone())
        };
        let was_running = *status.lock().unwrap() == JobStatus::Running;
        if was_running {
            cancel.cancel();
            let deadline = Instant::now() + KILL_SETTLE;
            while *status.lock().unwrap() == JobStatus::Running && Instant::now() < deadline {
                std::thread::sleep(WAIT_TICK);
            }
        }
        let status = *status.lock().unwrap();
        Ok(JobKill { status, was_running })
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

/// The number of a `job-N` id, for ordering a listing and forgetting the oldest finished jobs.
fn job_number(id: &str) -> u64 {
    id.trim_start_matches("job-").parse().unwrap_or(0)
}

/// The last non-empty line of a job's output, trimmed and capped for a listing. Only the tail
/// of the ring buffer is looked at, so a flooding job costs a bounded scan.
fn last_line(buf: &[u8]) -> String {
    let tail = &buf[buf.len().saturating_sub(LAST_LINE_SCAN)..];
    let text = String::from_utf8_lossy(tail);
    let line = text
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    if line.chars().count() > LAST_LINE_CAP {
        let cut: String = line.chars().take(LAST_LINE_CAP).collect();
        format!("{cut}...")
    } else {
        line.to_string()
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
                unsandboxed: false,
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
    fn job_past_the_limit_is_refused() {
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
        assert!(
            err.contains(&format!("At most {MAX_JOBS} background jobs")),
            "{err}"
        );
        assert!(err.contains("\"job\": \"job-1\""), "{err}");
        reg.kill_all();
    }

    /// `oxa.bash.maxJobs` is the registry's limit: a registry built with 2 refuses the third
    /// start and names the oldest running job; the default registry keeps [`MAX_JOBS`].
    #[test]
    fn jobs_limit_is_per_registry() {
        let reg = JobRegistry::with_max_jobs(2);
        let sb = Arc::new(Blocking {
            seen: Mutex::new(None),
        });
        reg.set_sandbox(sb.clone());
        reg.spawn(spec()).unwrap();
        reg.spawn(spec()).unwrap();
        let err = reg.spawn(spec()).unwrap_err();
        assert!(err.contains("At most 2 background jobs"), "{err}");
        assert!(err.contains("\"job\": \"job-1\""), "{err}");
        reg.kill_all();
        assert_eq!(JobRegistry::with_max_jobs(0).max_jobs, 1);
        assert_eq!(JobRegistry::new().max_jobs, MAX_JOBS);
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
                    unsandboxed: false,
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
    fn finished_jobs_are_bounded_oldest_first() {
        struct Quick;
        impl Sandbox for Quick {
            fn check(&self) -> Result<(), OxaError> {
                Ok(())
            }
            fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
                Vec::new()
            }
            fn run(&self, _s: &ExecSpec, _c: &CancelToken, live: &mut dyn FnMut(&[u8])) -> ExecResult {
                live(b"ok\n");
                ExecResult {
                    output: b"ok\n".to_vec(),
                    exit: Some(0),
                    timed_out: false,
                    unsandboxed: false,
                }
            }
        }
        let reg = JobRegistry::new();
        reg.set_sandbox(Arc::new(Quick));
        let total = MAX_FINISHED + 5;
        let mut ids = Vec::new();
        for _ in 0..total {
            let id = reg.spawn(spec()).unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while reg.read(&id).unwrap().status == JobStatus::Running {
                assert!(std::time::Instant::now() < deadline, "job did not finish");
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            ids.push(id);
        }
        // The registry stays bounded: the oldest finished jobs are forgotten, the newest
        // MAX_FINISHED stay readable (a poll of a just-finished job must still work).
        assert_eq!(reg.read(&ids[0]).unwrap_err(), format!("Unknown job: {}", ids[0]));
        for id in &ids[total - MAX_FINISHED..total - 1] {
            assert_eq!(reg.read(id).unwrap().status, JobStatus::Exited(0), "{id}");
        }
        assert!(reg.read(&ids[total - 1]).is_ok());
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
                    unsandboxed: false,
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

    #[test]
    fn list_snapshots_without_consuming_output() {
        let reg = JobRegistry::new();
        reg.set_sandbox(Arc::new(Blocking {
            seen: Mutex::new(None),
        }));
        let id = reg.spawn(spec()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        let jobs = reg.list();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, id);
        assert_eq!(jobs[0].status, JobStatus::Running);
        assert_eq!(jobs[0].last_line, "started");
        // The listing did not mark the output as seen: a read still has news to show.
        assert_eq!(reg.unseen_len(&id), Some(8));
        assert_eq!(reg.read(&id).unwrap().output, "started\n");
        assert_eq!(reg.unseen_len(&id), Some(0));
        // The buffered output is still there after the listing and the read.
        assert_eq!(reg.list()[0].last_line, "started");
        reg.kill_all();
    }

    #[test]
    fn list_is_oldest_first_and_shows_finished_jobs() {
        let reg = JobRegistry::new();
        reg.set_sandbox(Arc::new(Blocking {
            seen: Mutex::new(None),
        }));
        let first = reg.spawn(spec()).unwrap();
        let second = reg.spawn(spec()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        let ids: Vec<String> = reg.list().into_iter().map(|j| j.id).collect();
        assert_eq!(ids, vec![first.clone(), second.clone()]);
        assert!(reg.list().iter().all(|j| j.status == JobStatus::Running));
        assert_eq!(reg.kill(&first).unwrap().status, JobStatus::Killed);
        let jobs = reg.list();
        assert_eq!(jobs[0].status, JobStatus::Killed);
        assert_eq!(jobs[1].status, JobStatus::Running);
        assert_eq!(reg.unseen_len("job-9"), None);
        reg.kill_all();
    }

    #[test]
    fn list_last_line_skips_blanks_and_is_capped() {
        struct Lines;
        impl Sandbox for Lines {
            fn check(&self) -> Result<(), OxaError> {
                Ok(())
            }
            fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
                Vec::new()
            }
            fn run(&self, _s: &ExecSpec, _c: &CancelToken, live: &mut dyn FnMut(&[u8])) -> ExecResult {
                live(b"first\n");
                live(&[b'x'; LAST_LINE_CAP + 50]);
                live(b"\n\n");
                ExecResult {
                    output: Vec::new(),
                    exit: Some(0),
                    timed_out: false,
                    unsandboxed: false,
                }
            }
        }
        let reg = JobRegistry::new();
        reg.set_sandbox(Arc::new(Lines));
        let id = reg.spawn(spec()).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while reg.read(&id).unwrap().status == JobStatus::Running {
            assert!(std::time::Instant::now() < deadline, "job did not finish");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let line = reg.list()[0].last_line.clone();
        assert_eq!(line, format!("{}...", "x".repeat(LAST_LINE_CAP)));
    }
}
