//! Worker process supervisor: one long-lived thread owns every spawn of an oxa worker.
//!
//! Each worker runs in its own process group (`process_group(0)`) and dies with the thread
//! that spawned it (`oxa_sandbox::die_with_parent`, PR_SET_PDEATHSIG). That signal fires when
//! the forking thread exits, so every spawn happens on the supervisor thread, which lives as
//! long as the [`Supervisor`] handle does.
//!
//! Identity: the leader stays unreaped (a zombie is enough) until its group is confirmed empty.
//! While the leader is unreaped its pid, and therefore its process group number, cannot be
//! reused, so every signal the supervisor sends reaches the group it started. Group membership
//! is read from `/proc`, which skips zombies, so an exited leader does not look like a live group.
//!
//! Environment: a worker gets exactly the environment its caller passes (`env_clear` first), so the
//! launch policy in `launch::child_env` is applied by the caller and never bypassed by inheritance.
//!
//! Stopping a worker: SIGINT to its pid while it runs, a grace period (`oxa.workers.stopGraceSeconds`),
//! SIGTERM to the whole process group, SIGKILL to the group after 2 seconds. Only when the group is
//! confirmed empty a last SIGKILL sweeps threads that outlived their leader thread (shown as `Z`),
//! then the leader is reaped. Every signal goes through the `kill` executable the caller
//! resolved with [`kill_helper`]; a failed signal or probe is an error and keeps the worker tracked.

use oxa_config::Settings;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

/// How long the group gets after SIGTERM, and after SIGKILL, to disappear.
const KILL_AFTER: Duration = Duration::from_secs(2);
/// How often a stop polls the group.
const POLL: Duration = Duration::from_millis(50);
/// How often the idle thread reaps workers that exited on their own.
const REAP_EVERY: Duration = Duration::from_millis(200);
/// The most a single `kill` helper run may take before it is killed and counted as a failed signal.
const SIGNAL_DEADLINE: Duration = Duration::from_secs(5);
/// Generous cap on what the `kill` helper may print; it prints nothing in normal use.
const SIGNAL_OUTPUT_BYTES: usize = 64 * 1024;
/// The default for `oxa.workers.stopGraceSeconds`.
const DEFAULT_STOP_GRACE: Duration = Duration::from_secs(10);

enum Msg {
    Spawn {
        argv: Vec<String>,
        env: Vec<(String, String)>,
        log: PathBuf,
        reply: Sender<io::Result<u32>>,
    },
    Stop {
        pgid: u32,
        reply: Sender<io::Result<Option<ExitStatus>>>,
    },
    StopAll {
        reply: Sender<io::Result<()>>,
    },
}

/// Handle to the supervisor thread. Dropping it stops every live worker.
pub struct Supervisor {
    tx: Sender<Msg>,
}

impl Supervisor {
    /// Start the supervisor thread. `grace` is the time a stopped worker gets after SIGINT, and
    /// `kill` is the absolute path of the `kill` executable used for every signal (see
    /// [`kill_helper`]). The bare name is never looked up on `PATH`.
    pub fn new(grace: Duration, kill: PathBuf) -> io::Result<Supervisor> {
        if !kill.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the kill helper must be an absolute path",
            ));
        }
        let (tx, rx) = mpsc::channel();
        thread::Builder::new()
            .name("oxa-workers".into())
            .spawn(move || run(rx, grace, kill))?;
        Ok(Supervisor { tx })
    }

    /// Start `argv[0]` with `argv[1..]` in a new process group, stdin null, stdout and stderr
    /// appended to the owner-private `log`. The child gets exactly `env` (the environment is
    /// cleared first): callers pass `launch::child_env` output so the worker launch policy applies.
    /// Returns the process group id, which is the leader's pid.
    pub fn spawn(&self, argv: &[&str], env: &[(String, String)], log: &Path) -> io::Result<u32> {
        let (reply, rx) = mpsc::channel();
        self.send(Msg::Spawn {
            argv: argv.iter().map(|s| s.to_string()).collect(),
            env: env.to_vec(),
            log: log.to_path_buf(),
            reply,
        })?;
        rx.recv().map_err(|_| gone())?
    }

    /// Stop the worker whose process group is `pgid` (see the module docs for the sequence).
    /// Returns the leader's exit status, or `None` when no such worker is live. Blocks until the
    /// group is gone. On error the worker stays tracked, so a later stop can finish the job.
    pub fn stop(&self, pgid: u32) -> io::Result<Option<ExitStatus>> {
        let (reply, rx) = mpsc::channel();
        self.send(Msg::Stop { pgid, reply })?;
        rx.recv().map_err(|_| gone())?
    }

    /// Stop every live worker, one after another. Every worker is attempted; when any teardown
    /// fails the first error is returned and those workers stay tracked for a later retry.
    pub fn stop_all(&self) -> io::Result<()> {
        let (reply, rx) = mpsc::channel();
        self.send(Msg::StopAll { reply })?;
        rx.recv().map_err(|_| gone())?
    }

    fn send(&self, msg: Msg) -> io::Result<()> {
        self.tx.send(msg).map_err(|_| gone())
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        let _ = self.stop_all();
    }
}

/// The `kill` executable a supervisor signals through: the first `kill` in an absolute `PATH`
/// entry that is not inside the project and not group- or world-writable, as for every host tool.
pub fn kill_helper(host_path: &std::ffi::OsStr, project: &Path, home: &Path) -> Option<PathBuf> {
    crate::netguard::trusted_executable_for("kill", Some(host_path), Some(project), Some(home))
}

/// How long a stopped worker gets after SIGINT: `oxa.workers.stopGraceSeconds`, else 10 seconds.
pub fn stop_grace(settings: &Settings) -> Duration {
    settings
        .get("oxa.workers.stopGraceSeconds")
        .and_then(|value| value.as_f64())
        .and_then(|seconds| Duration::try_from_secs_f64(seconds).ok())
        .unwrap_or(DEFAULT_STOP_GRACE)
}

/// A supervisor for the host environment: the `kill` helper resolved from `host_path` (never a
/// project shim) and the grace from `oxa.workers.stopGraceSeconds`. Fails when no trusted `kill`
/// exists, so no worker is ever started without a way to stop it.
pub fn start(
    settings: &Settings,
    host_path: &std::ffi::OsStr,
    project: &Path,
    home: &Path,
) -> io::Result<Supervisor> {
    let kill = kill_helper(host_path, project, home)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no trusted kill executable on PATH"))?;
    Supervisor::new(stop_grace(settings), kill)
}

fn gone() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "worker supervisor is gone")
}

/// The supervisor thread: serves requests until every handle is dropped.
fn run(rx: Receiver<Msg>, grace: Duration, kill: PathBuf) {
    let mut table = Table {
        grace,
        kill,
        live: HashMap::new(),
        done: HashMap::new(),
    };
    let mut last_reap = Instant::now();
    loop {
        // Reaping runs on its own deadline, so a steady stream of requests cannot starve it.
        let wait = REAP_EVERY.saturating_sub(last_reap.elapsed());
        match rx.recv_timeout(wait) {
            Ok(Msg::Spawn {
                argv,
                env,
                log,
                reply,
            }) => {
                let _ = reply.send(table.spawn(&argv, &env, &log));
            }
            Ok(Msg::Stop { pgid, reply }) => {
                let _ = reply.send(table.stop(pgid));
            }
            Ok(Msg::StopAll { reply }) => {
                let _ = reply.send(table.stop_all());
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                let _ = table.stop_all();
                return;
            }
        }
        if last_reap.elapsed() >= REAP_EVERY {
            table.reap();
            last_reap = Instant::now();
        }
    }
}

/// The workers this thread started, by process group id. `live` holds a worker whose group may
/// still have members, with its leader unreaped. `done` holds the status of a worker whose group
/// is empty: no signal is ever sent to such a pgid again.
struct Table {
    grace: Duration,
    kill: PathBuf,
    live: HashMap<u32, Child>,
    done: HashMap<u32, ExitStatus>,
}

impl Table {
    fn spawn(&mut self, argv: &[String], env: &[(String, String)], log: &Path) -> io::Result<u32> {
        let (program, args) = argv
            .split_first()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty argv"))?;
        let out = oxa_work::shadow::open_owner_log(log).map_err(io::Error::other)?;
        let err = out.try_clone()?;
        let mut cmd = Command::new(program);
        cmd.args(args);
        cmd.env_clear().envs(env.iter().map(|(k, v)| (k, v)));
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::from(out));
        cmd.stderr(Stdio::from(err));
        cmd.process_group(0);
        oxa_sandbox::die_with_parent(&mut cmd);
        let child = cmd.spawn()?;
        let pgid = child.id();
        self.live.insert(pgid, child);
        Ok(pgid)
    }

    /// Reap the leaders whose group is empty. A leader whose group still has members stays
    /// unreaped, which keeps its group number reserved. A probe error keeps the worker tracked.
    fn reap(&mut self) {
        let mut finished = Vec::new();
        for (pgid, child) in self.live.iter_mut() {
            if let Ok(false) = group_alive(*pgid) {
                // The probe skips zombies, so a member whose thread-group leader exited while other
                // threads still run reads as gone. The leader is unreaped, so `pgid` is still ours:
                // sweep the group before the worker is retired, as `sweep` does on stop. "No such
                // process" is the usual outcome here, so the sweep's result is not read; the probe
                // after it decides.
                let _ = signal(&self.kill, &format!("-{pgid}"), "KILL");
                if let (Ok(false), Ok(Some(status))) = (group_alive(*pgid), child.try_wait()) {
                    finished.push((*pgid, status));
                }
            }
        }
        for (pgid, status) in finished {
            self.live.remove(&pgid);
            self.done.insert(pgid, status);
        }
    }

    fn stop(&mut self, pgid: u32) -> io::Result<Option<ExitStatus>> {
        let Some(mut child) = self.live.remove(&pgid) else {
            return Ok(self.done.remove(&pgid));
        };
        if let Err(error) = self.teardown(pgid) {
            // Ownership stays here until the group is confirmed gone.
            self.live.insert(pgid, child);
            return Err(error);
        }
        // The group is empty, so the leader has exited or left the group. The kill cannot reach
        // a reused pid (the leader is unreaped until now), and makes the wait below safe.
        let _ = child.kill();
        child.wait().map(Some)
    }

    /// The signal sequence for one group. Returns once the group has no live member.
    fn teardown(&self, pgid: u32) -> io::Result<()> {
        // A grace the monotonic clock cannot reach is a configuration error (`oxa.workers.
        // stopGraceSeconds` takes any whole number): report it instead of panicking in the one
        // supervisor thread, which would drop every later request and the tracked children.
        let grace = Instant::now()
            .checked_add(self.grace)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "worker stop grace is too large"))?;
        if leader_running(pgid)? {
            signal(&self.kill, &pgid.to_string(), "INT")?;
        }
        if settle(pgid, grace)? {
            return self.sweep(pgid);
        }
        signal(&self.kill, &format!("-{pgid}"), "TERM")?;
        if settle(pgid, Instant::now() + KILL_AFTER)? {
            return self.sweep(pgid);
        }
        signal(&self.kill, &format!("-{pgid}"), "KILL")?;
        if settle(pgid, Instant::now() + KILL_AFTER)? {
            return Ok(());
        }
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("worker group {pgid} survived SIGKILL"),
        ))
    }

    /// A group the probe reads as empty can still hold threads: when a member's thread-group
    /// leader exited while other threads run, `/proc/<pid>/stat` shows `Z`. The leader of our
    /// group is unreaped, so the group number is ours: one last SIGKILL to the whole group is safe.
    /// "No such process" (the usual case, the group really is empty) is not an error; the group is
    /// then probed again.
    fn sweep(&self, pgid: u32) -> io::Result<()> {
        let _ = signal(&self.kill, &format!("-{pgid}"), "KILL");
        if settle(pgid, Instant::now() + KILL_AFTER)? {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("worker group {pgid} survived SIGKILL"),
            ))
        }
    }

    /// Attempt every live worker. Returns the first teardown error; failed workers stay in `live`.
    fn stop_all(&mut self) -> io::Result<()> {
        let pgids: Vec<u32> = self.live.keys().copied().collect();
        let mut first = None;
        for pgid in pgids {
            if let Err(error) = self.stop(pgid) {
                first.get_or_insert(error);
            }
        }
        self.done.clear();
        first.map_or(Ok(()), Err)
    }
}

/// Poll until the group has no live member (true) or `until` passes (false).
fn settle(pgid: u32, until: Instant) -> io::Result<bool> {
    loop {
        if !group_alive(pgid)? {
            return Ok(true);
        }
        if Instant::now() >= until {
            return Ok(false);
        }
        thread::sleep(POLL);
    }
}

/// `kill -<sig> -- <target>` through the trusted executable, with a cleared environment.
/// `target` is a pid, or `-<pgid>` for a whole group. A signal that does not land is an error,
/// never read as the worker being gone.
fn signal(kill: &Path, target: &str, sig: &str) -> io::Result<()> {
    let mut command = Command::new(kill);
    command.env_clear().arg(format!("-{sig}")).arg("--").arg(target);
    // Bounded: a stalled helper is killed at the deadline and reported as a failed signal, so it
    // can never hold the supervisor thread back from escalating.
    match oxa_work::command_runner(command, Some(SIGNAL_OUTPUT_BYTES), SIGNAL_DEADLINE) {
        Some((0, _)) => Ok(()),
        Some((code, _)) => Err(io::Error::other(format!(
            "kill -{sig} {target} failed: exit {code}"
        ))),
        None => Err(io::Error::other(format!(
            "kill -{sig} {target} did not finish within {SIGNAL_DEADLINE:?}"
        ))),
    }
}

/// Whether the leader `pid` is still running (not a zombie).
fn leader_running(pid: u32) -> io::Result<bool> {
    Ok(matches!(proc_stat(pid)?, Some((state, _)) if state != 'Z' && state != 'X'))
}

/// Whether any process of group `pgid` is running. Zombies do not count. Errors are returned,
/// never read as absence, except that a pid whose stat cannot be read for permission reasons is
/// not ours (see [`group_alive_with`]).
fn group_alive(pgid: u32) -> io::Result<bool> {
    let mut pids = Vec::new();
    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        if let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        {
            pids.push(pid);
        }
    }
    group_alive_with(pgid, pids, proc_stat)
}

/// [`group_alive`] over the given pids with an injectable stat reader. A pid that answers EPERM or
/// EACCES (another user's process under `hidepid`) is not in our group: workers run as our own
/// uid, and a same-uid process is always readable.
fn group_alive_with(
    pgid: u32,
    pids: impl IntoIterator<Item = u32>,
    stat: impl Fn(u32) -> io::Result<Option<(char, u32)>>,
) -> io::Result<bool> {
    for pid in pids {
        match stat(pid) {
            Ok(Some((state, group))) if group == pgid && state != 'Z' && state != 'X' => return Ok(true),
            Ok(_) => {}
            Err(error) if permission_denied(&error) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}

/// True for EPERM (1) and EACCES (13), however the error was classified.
fn permission_denied(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::PermissionDenied || matches!(error.raw_os_error(), Some(1 | 13))
}

/// `ESRCH`: the pid exited between the open and the read. Linux returns this from a
/// `/proc/<pid>` read rather than `NotFound`, and `group_alive` scans unrelated processes, so
/// ordinary host churn hits it.
const ESRCH: i32 = 3;

/// The state letter and process group of `pid` from `/proc/<pid>/stat`, or `None` once the
/// process is gone.
fn proc_stat(pid: u32) -> io::Result<Option<(char, u32)>> {
    match fs::read(format!("/proc/{pid}/stat")) {
        Ok(bytes) => parse_stat(&bytes).map(Some),
        Err(error) if process_gone(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

/// True for the two ways a `/proc/<pid>` read reports that the process is gone.
fn process_gone(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(ESRCH)
}

/// Parse a `/proc/<pid>/stat` record as bytes: `comm` may hold any byte (not only UTF-8), so only
/// the ASCII fields after the last `)` are decoded.
fn parse_stat(bytes: &[u8]) -> io::Result<(char, u32)> {
    let malformed = || io::Error::new(io::ErrorKind::InvalidData, "malformed /proc stat line");
    // `pid (comm) state ppid pgrp ...`: comm may contain spaces and parentheses, so split after
    // the last `)`.
    let close = bytes
        .iter()
        .rposition(|byte| *byte == b')')
        .ok_or_else(malformed)?;
    let rest = std::str::from_utf8(&bytes[close + 1..]).map_err(|_| malformed())?;
    let mut fields = rest.split_whitespace();
    let state = fields
        .next()
        .and_then(|field| field.chars().next())
        .ok_or_else(malformed)?;
    let _ppid = fields.next().ok_or_else(malformed)?;
    let group = fields
        .next()
        .and_then(|field| field.parse().ok())
        .ok_or_else(malformed)?;
    Ok((state, group))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs::Permissions;
    use std::os::unix::fs::PermissionsExt;

    const GRACE: Duration = Duration::from_millis(200);

    /// The test process's own environment, as the caller would pass it.
    fn host_env() -> Vec<(String, String)> {
        std::env::vars().collect()
    }

    /// A fresh scratch directory for one test.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("oxa-k4-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    /// The real `kill` executable, resolved the way the caller resolves it.
    fn real_kill() -> PathBuf {
        let path = std::env::var_os("PATH").unwrap_or_default();
        crate::netguard::trusted_executable_for("kill", Some(path.as_os_str()), None, None)
            .unwrap_or_else(|| PathBuf::from("/bin/kill"))
    }

    /// An executable shell script at `path`.
    fn script(path: &Path, body: &str) {
        fs::write(path, format!("#!/bin/sh\n{body}\n")).expect("script");
        fs::set_permissions(path, Permissions::from_mode(0o755)).expect("chmod");
    }

    /// The pid a worker wrote to `pidfile` once its background sleep started.
    fn background_pid(pidfile: &Path) -> u32 {
        for _ in 0..100 {
            if let Some(pid) = fs::read_to_string(pidfile)
                .ok()
                .and_then(|text| text.trim().parse().ok())
            {
                return pid;
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!("the worker never wrote its background pid");
    }

    /// `sleep 60 &` in the worker's group, its pid written to `pidfile`, then wait.
    fn group_with_background(pidfile: &Path) -> String {
        format!("sleep 60 & echo $! > {}; wait", pidfile.display())
    }

    #[test]
    fn stop_kills_whole_process_group() {
        let dir = scratch("group");
        let sup = Supervisor::new(GRACE, real_kill()).expect("supervisor");
        let pidfile = dir.join("bg.pid");
        let pgid = sup
            .spawn(
                &["/bin/sh", "-c", &group_with_background(&pidfile)],
                &host_env(),
                &dir.join("worker.log"),
            )
            .expect("spawn");
        let background = background_pid(&pidfile);
        assert!(group_alive(pgid).expect("probe"));
        let status = sup.stop(pgid).expect("stop");
        assert!(status.is_some(), "the leader was live");
        assert!(!group_alive(pgid).expect("probe"), "the group still has members");
        // Independent of the group probe: the background sleep itself is dead (or a zombie).
        assert!(
            matches!(proc_stat(background).expect("stat"), None | Some(('Z', _))),
            "the background sleep survived stop"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn leader_stays_unreaped_while_group_has_members() {
        let dir = scratch("unreaped");
        let kill = real_kill();
        let sup = Supervisor::new(GRACE, kill.clone()).expect("supervisor");
        let pidfile = dir.join("bg.pid");
        let pgid = sup
            .spawn(
                &["/bin/sh", "-c", &group_with_background(&pidfile)],
                &host_env(),
                &dir.join("worker.log"),
            )
            .expect("spawn");
        background_pid(&pidfile);
        // SIGINT to the leader alone: it exits, the background sleep remains in the group.
        signal(&kill, &pgid.to_string(), "INT").expect("INT");
        // The idle thread runs several times in this window.
        thread::sleep(Duration::from_millis(500));
        assert!(
            matches!(proc_stat(pgid).expect("stat"), Some(('Z', _))),
            "the leader was reaped while its group still has members, so its group number could be reused"
        );
        assert!(
            group_alive(pgid).expect("probe"),
            "the background sleep should remain"
        );
        let status = sup.stop(pgid).expect("stop").expect("status");
        assert!(!status.success(), "the leader died of SIGINT");
        assert!(
            !group_alive(pgid).expect("probe"),
            "the background sleep survived stop"
        );
        assert!(sup.stop(pgid).expect("second stop").is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stop_all_leaves_no_child() {
        let dir = scratch("all");
        let sup = Supervisor::new(GRACE, real_kill()).expect("supervisor");
        let log = dir.join("worker.log");
        let a = sup
            .spawn(&["/bin/sh", "-c", "sleep 60 & wait"], &host_env(), &log)
            .expect("spawn a");
        let b = sup
            .spawn(&["/bin/sh", "-c", "sleep 60 & wait"], &host_env(), &log)
            .expect("spawn b");
        thread::sleep(Duration::from_millis(300));
        sup.stop_all().expect("stop_all");
        assert!(!group_alive(a).expect("probe"), "group a survived stop_all");
        assert!(!group_alive(b).expect("probe"), "group b survived stop_all");
        assert!(sup.stop(a).expect("stop a").is_none());
        assert!(sup.stop(b).expect("stop b").is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stop_signals_through_the_resolved_helper() {
        let dir = scratch("helper");
        let record = dir.join("signals.txt");
        let helper = dir.join("kill");
        script(
            &helper,
            &format!(
                "echo \"$@\" >> '{}'\nexec '{}' \"$@\"",
                record.display(),
                real_kill().display()
            ),
        );
        let sup = Supervisor::new(GRACE, helper).expect("supervisor");
        let pidfile = dir.join("bg.pid");
        let pgid = sup
            .spawn(
                &["/bin/sh", "-c", &group_with_background(&pidfile)],
                &host_env(),
                &dir.join("worker.log"),
            )
            .expect("spawn");
        background_pid(&pidfile);
        assert!(sup.stop(pgid).expect("stop").is_some());
        let signals = fs::read_to_string(&record).expect("the helper recorded no signal");
        assert!(
            signals.contains("-INT -- "),
            "no SIGINT went through the helper: {signals}"
        );
        assert!(
            signals.contains("-TERM -- -"),
            "no group SIGTERM went through the helper: {signals}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stop_keeps_the_worker_when_a_signal_fails() {
        let dir = scratch("failed-signal");
        let allow = dir.join("allow");
        let helper = dir.join("kill");
        // Refuses every signal until `allow` exists.
        script(
            &helper,
            &format!(
                "[ -e '{}' ] || exit 2\nexec '{}' \"$@\"",
                allow.display(),
                real_kill().display()
            ),
        );
        let sup = Supervisor::new(GRACE, helper).expect("supervisor");
        let pidfile = dir.join("bg.pid");
        let pgid = sup
            .spawn(
                &["/bin/sh", "-c", &group_with_background(&pidfile)],
                &host_env(),
                &dir.join("worker.log"),
            )
            .expect("spawn");
        let background = background_pid(&pidfile);
        assert!(
            sup.stop(pgid).is_err(),
            "a failed signal must not look like success"
        );
        assert!(group_alive(pgid).expect("probe"), "the group was treated as gone");
        assert!(
            matches!(proc_stat(background).expect("stat"), Some((state, _)) if state != 'Z'),
            "the background sleep was stopped although no signal landed"
        );
        fs::write(&allow, "").expect("allow");
        let status = sup.stop(pgid).expect("second stop");
        assert!(status.is_some(), "the retained worker was not stopped");
        assert!(
            !group_alive(pgid).expect("probe"),
            "the group survived the second stop"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stop_all_reports_a_worker_that_could_not_be_stopped() {
        let dir = scratch("stop-all-error");
        let allow = dir.join("allow");
        let helper = dir.join("kill");
        script(
            &helper,
            &format!(
                "[ -e '{}' ] || exit 2\nexec '{}' \"$@\"",
                allow.display(),
                real_kill().display()
            ),
        );
        let sup = Supervisor::new(GRACE, helper).expect("supervisor");
        let pidfile = dir.join("bg.pid");
        let pgid = sup
            .spawn(
                &["/bin/sh", "-c", &group_with_background(&pidfile)],
                &host_env(),
                &dir.join("worker.log"),
            )
            .expect("spawn");
        background_pid(&pidfile);
        assert!(
            sup.stop_all().is_err(),
            "stop_all returned Ok although the worker is still running"
        );
        assert!(group_alive(pgid).expect("probe"), "the worker was lost");
        fs::write(&allow, "").expect("allow");
        sup.stop_all().expect("retry stop_all");
        assert!(!group_alive(pgid).expect("probe"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_stat_accepts_a_non_utf8_process_name() {
        let mut record = b"4242 (a".to_vec();
        record.push(0xff);
        record.extend_from_slice(b") b) S 1 777 777 0 -1\n");
        assert_eq!(parse_stat(&record).expect("parse"), ('S', 777));
        assert!(parse_stat(b"no close paren").is_err());
    }

    #[test]
    fn a_non_utf8_process_name_does_not_block_stop() {
        let dir = scratch("non-utf8-comm");
        let sup = Supervisor::new(GRACE, real_kill()).expect("supervisor");
        let pidfile = dir.join("bg.pid");
        let body = format!(
            "printf '\\377' > /proc/$$/comm; {}",
            group_with_background(&pidfile)
        );
        let pgid = sup
            .spawn(&["/bin/sh", "-c", &body], &host_env(), &dir.join("worker.log"))
            .expect("spawn");
        background_pid(&pidfile);
        assert!(group_alive(pgid).expect("probe"));
        assert!(sup.stop(pgid).expect("stop").is_some());
        assert!(!group_alive(pgid).expect("probe"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stuck_signal_helper_cannot_hold_the_supervisor() {
        let dir = scratch("stuck-helper");
        let helper = dir.join("kill");
        // Stalls far past the deadline before doing anything.
        script(&helper, "sleep 60");
        let sup = Supervisor::new(GRACE, helper).expect("supervisor");
        let pgid = sup
            .spawn(
                &["/bin/sh", "-c", "sleep 60 & wait"],
                &host_env(),
                &dir.join("worker.log"),
            )
            .expect("spawn");
        let started = Instant::now();
        assert!(
            sup.stop(pgid).is_err(),
            "a stalled helper must read as a failed signal"
        );
        assert!(
            started.elapsed() < SIGNAL_DEADLINE + Duration::from_secs(4),
            "the stalled helper blocked the supervisor"
        );
        // The supervisor still answers.
        assert!(sup
            .spawn(&["/bin/sh", "-c", "true"], &host_env(), &dir.join("b.log"))
            .is_ok());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn exited_workers_are_reaped_while_requests_keep_arriving() {
        let dir = scratch("reap-busy");
        let sup = Supervisor::new(GRACE, real_kill()).expect("supervisor");
        let log = dir.join("worker.log");
        let first = sup
            .spawn(&["/bin/sh", "-c", "true"], &host_env(), &log)
            .expect("spawn");
        // Requests arrive well inside REAP_EVERY, so the receive never times out.
        let mut reaped = false;
        let until = Instant::now() + REAP_EVERY * 4;
        while Instant::now() < until {
            sup.spawn(&["/bin/sh", "-c", "true"], &host_env(), &log)
                .expect("spawn");
            thread::sleep(Duration::from_millis(20));
            if proc_stat(first).expect("stat").is_none() {
                reaped = true;
                break;
            }
        }
        assert!(
            reaped,
            "an exited worker stayed unreaped under a steady request stream"
        );
        let _ = sup.stop_all();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn kill_helper_must_be_an_absolute_path() {
        assert!(Supervisor::new(GRACE, PathBuf::from("kill")).is_err());
    }

    #[test]
    fn kill_helper_refuses_a_project_shim() {
        let dir = scratch("shim");
        let project = dir.join("project");
        let bin = project.join("bin");
        fs::create_dir_all(&bin).expect("bin");
        script(&bin.join("kill"), "exit 0");
        let home = dir.join("home");
        fs::create_dir_all(&home).expect("home");
        assert!(kill_helper(bin.as_os_str(), &project, &home).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn spawn_refuses_a_symlinked_log() {
        let dir = scratch("symlink-log");
        let target = dir.join("target.log");
        fs::write(&target, "").expect("target");
        let link = dir.join("worker.log");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        let sup = Supervisor::new(GRACE, real_kill()).expect("supervisor");
        assert!(sup.spawn(&["/bin/sh", "-c", "true"], &host_env(), &link).is_err());
        assert!(
            fs::read(&target).expect("target").is_empty(),
            "the symlink target was written"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn spawn_creates_an_owner_only_log() {
        let dir = scratch("log-mode");
        let log = dir.join("worker.log");
        let sup = Supervisor::new(GRACE, real_kill()).expect("supervisor");
        let pgid = sup
            .spawn(&["/bin/sh", "-c", "true"], &host_env(), &log)
            .expect("spawn");
        let mode = fs::metadata(&log).expect("log").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the worker log is not owner-only");
        let _ = sup.stop(pgid);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn start_refuses_when_no_trusted_kill_exists() {
        let dir = scratch("start");
        let settings = Settings::from_values(json!({}), json!({}));
        let refused = start(&settings, std::ffi::OsStr::new(""), &dir, &dir);
        assert!(refused.is_err(), "a supervisor started without a kill helper");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stop_grace_is_read_from_stop_grace_seconds() {
        let set = Settings::from_values(json!({"oxa": {"workers": {"stopGraceSeconds": 3}}}), json!({}));
        assert_eq!(stop_grace(&set), Duration::from_secs(3));
        let unset = Settings::from_values(json!({}), json!({}));
        assert_eq!(stop_grace(&unset), Duration::from_secs(10));
    }

    /// A `/proc/<pid>` read reports a process that exited between the open and the read as
    /// `ESRCH`, not `NotFound`; scanning the host hits that, and it must not abort a stop.
    #[test]
    fn a_proc_read_that_reports_esrch_is_a_gone_process() {
        assert!(process_gone(&io::Error::from_raw_os_error(ESRCH)));
        assert!(process_gone(&io::Error::from(io::ErrorKind::NotFound)));
        assert!(
            !process_gone(&io::Error::from_raw_os_error(13)),
            "EPERM stays an error"
        );
        assert!(proc_stat(u32::MAX)
            .expect("a missing pid is not an error")
            .is_none());
    }

    /// `oxa.workers.stopGraceSeconds` takes any whole number, including one the clock cannot
    /// reach: stopping must report that instead of panicking in the supervisor thread.
    #[test]
    fn an_unreachable_stop_grace_is_an_error_not_a_panic() {
        let dir = scratch("grace-overflow");
        let kill = dir.join("kill");
        script(&kill, "exit 0");
        let sup = Supervisor::new(Duration::from_secs(u64::MAX / 2), kill).expect("supervisor");
        let pgid = sup
            .spawn(&["/bin/sh", "-c", "true"], &host_env(), &dir.join("worker.log"))
            .expect("spawn");
        let refused = sup.stop(pgid).expect_err("an unreachable grace is refused");
        assert!(
            refused.to_string().contains("stop grace is too large"),
            "{refused}"
        );
        let _ = fs::remove_dir_all(&dir);
        // The supervisor thread survives to serve later stops.
        let _ = sup.stop(pgid);
    }

    #[test]
    fn spawn_applies_exact_child_env() {
        let dir = scratch("env");
        let sup = Supervisor::new(GRACE, real_kill()).expect("supervisor");
        let out = dir.join("env.out");
        let body = format!("/usr/bin/env > {}", out.display());
        // The parent carries the vars the launch policy strips; the child env omits them.
        std::env::set_var("OXA_TRUST_PROJECT", "1");
        std::env::set_var("OXA_SESSION_HINT", "abc.s1");
        let env = vec![("OXA_WORKER_DEPTH".to_string(), "1".to_string())];
        let pgid = sup
            .spawn(&["/bin/sh", "-c", &body], &env, &dir.join("worker.log"))
            .expect("spawn");
        for _ in 0..100 {
            if matches!(group_alive(pgid), Ok(false)) {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let text = fs::read_to_string(&out).expect("env output");
        std::env::remove_var("OXA_TRUST_PROJECT");
        std::env::remove_var("OXA_SESSION_HINT");
        let mut lines: Vec<&str> = text
            .lines()
            .filter(|line| {
                !line.starts_with("PWD=") && !line.starts_with("_=") && !line.starts_with("SHLVL=")
            })
            .collect();
        lines.sort();
        assert_eq!(
            lines,
            vec!["OXA_WORKER_DEPTH=1"],
            "the child saw more than the given env"
        );
        sup.stop(pgid).expect("stop");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn group_alive_skips_unreadable_unrelated_pids() {
        let stat = |pid: u32| match pid {
            1 => Err(io::Error::from_raw_os_error(13)),
            2 => Err(io::Error::from_raw_os_error(1)),
            3 => Ok(Some(('S', 7))),
            _ => Ok(Some(('S', 99))),
        };
        assert!(
            group_alive_with(99, [1, 2, 4], stat).expect("probe"),
            "pid 4 is in the group"
        );
        assert!(
            !group_alive_with(55, [1, 2, 3], stat).expect("probe"),
            "unreadable pids are not ours"
        );
        // Any other error is still an error, not absence.
        let broken = |_: u32| Err(io::Error::from_raw_os_error(5));
        assert!(group_alive_with(99, [1], broken).is_err());
    }

    #[test]
    fn stop_kills_threads_left_after_the_leader_thread_exits() {
        let python = Path::new("/usr/bin/python3");
        if !python.exists() {
            return;
        }
        let dir = scratch("threads");
        let sup = Supervisor::new(GRACE, real_kill()).expect("supervisor");
        // The main thread exits through pthread_exit while a second thread keeps running: the
        // process shows state Z in /proc, though it still executes.
        let code = "import ctypes,threading,time\nthreading.Thread(target=lambda: time.sleep(60)).start()\nctypes.CDLL(None).pthread_exit(None)";
        let pgid = sup
            .spawn(
                &["/usr/bin/python3", "-c", code],
                &host_env(),
                &dir.join("worker.log"),
            )
            .expect("spawn");
        thread::sleep(Duration::from_millis(500));
        let started = Instant::now();
        sup.stop(pgid).expect("stop");
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "stop waited for the surviving thread instead of killing the group"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// Whether any thread of `pid` still runs (a task whose state is neither `Z` nor `X`).
    fn thread_survives(pid: u32) -> bool {
        let Ok(tasks) = fs::read_dir(format!("/proc/{pid}/task")) else {
            return false;
        };
        tasks.flatten().any(|task| {
            matches!(
                fs::read(task.path().join("stat")).map(|bytes| parse_stat(&bytes)),
                Ok(Ok((state, _))) if state != 'Z' && state != 'X'
            )
        })
    }

    #[test]
    fn a_descendant_thread_outliving_its_exited_leader_is_swept() {
        let python = Path::new("/usr/bin/python3");
        if !python.exists() {
            return;
        }
        let dir = scratch("descendant");
        let sup = Supervisor::new(GRACE, real_kill()).expect("supervisor");
        let pidfile = dir.join("bg.pid");
        // The leader shell exits at once. Its background python keeps one thread running after
        // its main thread called pthread_exit, so the group probe reads as empty while a thread
        // still executes. Without the reap sweep that thread would outlive the supervisor's
        // ownership of the worker.
        let code = "import ctypes,threading,time\nthreading.Thread(target=lambda: time.sleep(60)).start()\nctypes.CDLL(None).pthread_exit(None)";
        let script = format!("/usr/bin/python3 -c '{code}' & echo $! > {}", pidfile.display());
        let _pgid = sup
            .spawn(&["/bin/sh", "-c", &script], &host_env(), &dir.join("worker.log"))
            .expect("spawn");
        let descendant = background_pid(&pidfile);
        let until = Instant::now() + Duration::from_secs(10);
        while thread_survives(descendant) && Instant::now() < until {
            thread::sleep(POLL);
        }
        assert!(
            !thread_survives(descendant),
            "a thread of a retired worker kept running: the reaper did not sweep its group"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
