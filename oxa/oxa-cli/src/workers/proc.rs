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
//! Stopping a worker: SIGINT to its pid while it runs, a grace period (`oxa.workers.stopGraceSeconds`),
//! SIGTERM to the whole process group, SIGKILL to the group after 2 seconds. Only when the group is
//! confirmed empty is the leader reaped. Every signal goes through the `kill` executable the caller
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
/// The default for `oxa.workers.stopGraceSeconds`.
const DEFAULT_STOP_GRACE: Duration = Duration::from_secs(10);

enum Msg {
    Spawn {
        argv: Vec<String>,
        log: PathBuf,
        reply: Sender<io::Result<u32>>,
    },
    Stop {
        pgid: u32,
        reply: Sender<io::Result<Option<ExitStatus>>>,
    },
    StopAll {
        reply: Sender<()>,
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
    /// appended to the owner-private `log`. Returns the process group id, which is the leader's pid.
    pub fn spawn(&self, argv: &[&str], log: &Path) -> io::Result<u32> {
        let (reply, rx) = mpsc::channel();
        self.send(Msg::Spawn {
            argv: argv.iter().map(|s| s.to_string()).collect(),
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

    /// Stop every live worker, one after another.
    pub fn stop_all(&self) -> io::Result<()> {
        let (reply, rx) = mpsc::channel();
        self.send(Msg::StopAll { reply })?;
        rx.recv().map_err(|_| gone())
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

/// How long a stopped worker gets after SIGINT: `oxa.workers.stopGraceSeconds` from the user
/// settings only (a project file cannot change process-control timing), else 10 seconds.
pub fn stop_grace(settings: &Settings) -> Duration {
    settings
        .user_get("oxa.workers.stopGraceSeconds")
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
    loop {
        match rx.recv_timeout(REAP_EVERY) {
            Ok(Msg::Spawn { argv, log, reply }) => {
                let _ = reply.send(table.spawn(&argv, &log));
            }
            Ok(Msg::Stop { pgid, reply }) => {
                let _ = reply.send(table.stop(pgid));
            }
            Ok(Msg::StopAll { reply }) => {
                table.stop_all();
                let _ = reply.send(());
            }
            Err(RecvTimeoutError::Timeout) => table.reap(),
            Err(RecvTimeoutError::Disconnected) => {
                table.stop_all();
                return;
            }
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
    fn spawn(&mut self, argv: &[String], log: &Path) -> io::Result<u32> {
        let (program, args) = argv
            .split_first()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty argv"))?;
        let out = oxa_work::shadow::open_owner_log(log).map_err(io::Error::other)?;
        let err = out.try_clone()?;
        let mut cmd = Command::new(program);
        cmd.args(args);
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
                if let Ok(Some(status)) = child.try_wait() {
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
        if leader_running(pgid)? {
            signal(&self.kill, &pgid.to_string(), "INT")?;
        }
        if settle(pgid, Instant::now() + self.grace)? {
            return Ok(());
        }
        signal(&self.kill, &format!("-{pgid}"), "TERM")?;
        if settle(pgid, Instant::now() + KILL_AFTER)? {
            return Ok(());
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

    fn stop_all(&mut self) {
        let pgids: Vec<u32> = self.live.keys().copied().collect();
        for pgid in pgids {
            let _ = self.stop(pgid);
        }
        self.done.clear();
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
    let status = Command::new(kill)
        .env_clear()
        .arg(format!("-{sig}"))
        .arg("--")
        .arg(target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("kill -{sig} {target} failed: {status}")))
    }
}

/// Whether the leader `pid` is still running (not a zombie).
fn leader_running(pid: u32) -> io::Result<bool> {
    Ok(matches!(proc_stat(pid)?, Some((state, _)) if state != 'Z' && state != 'X'))
}

/// Whether any process of group `pgid` is running. Zombies do not count. Errors are returned,
/// never read as absence.
fn group_alive(pgid: u32) -> io::Result<bool> {
    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if let Some((state, group)) = proc_stat(pid)? {
            if group == pgid && state != 'Z' && state != 'X' {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// The state letter and process group of `pid` from `/proc/<pid>/stat`, or `None` once the
/// process is gone.
fn proc_stat(pid: u32) -> io::Result<Option<(char, u32)>> {
    let text = match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let malformed = || io::Error::new(io::ErrorKind::InvalidData, "malformed /proc stat line");
    // `pid (comm) state ppid pgrp ...`: comm may contain spaces and parentheses, so split after
    // the last `)`.
    let rest = text.rsplit_once(')').ok_or_else(malformed)?.1;
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
    Ok(Some((state, group)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs::Permissions;
    use std::os::unix::fs::PermissionsExt;

    const GRACE: Duration = Duration::from_millis(200);

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
            .spawn(&["/bin/sh", "-c", "sleep 60 & wait"], &log)
            .expect("spawn a");
        let b = sup
            .spawn(&["/bin/sh", "-c", "sleep 60 & wait"], &log)
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
        assert!(sup.spawn(&["/bin/sh", "-c", "true"], &link).is_err());
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
        let pgid = sup.spawn(&["/bin/sh", "-c", "true"], &log).expect("spawn");
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
        // A project file cannot change it.
        let project =
            Settings::from_values(json!({}), json!({"oxa": {"workers": {"stopGraceSeconds": 300}}}));
        assert_eq!(stop_grace(&project), Duration::from_secs(10));
    }
}
