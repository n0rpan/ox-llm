//! Worker process supervisor: one long-lived thread owns every spawn of an oxa worker.
//!
//! Each worker runs in its own process group (`process_group(0)`) and dies with the thread
//! that spawned it (`oxa_sandbox::die_with_parent`, PR_SET_PDEATHSIG). That signal fires when
//! the forking thread exits, so every spawn happens on the supervisor thread, which lives as
//! long as the [`Supervisor`] handle does.
//!
//! Stopping a worker: SIGINT to its pid, a grace period (`oxa.workers.stopGraceSeconds`),
//! SIGTERM to the whole process group, SIGKILL to the group after 2 seconds, and the leader is
//! reaped. Signals go through `kill(1)`, so this crate needs no `unsafe`.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

/// How long the group gets after SIGTERM, and after SIGKILL, to disappear.
const KILL_AFTER: Duration = Duration::from_secs(2);
/// How often a stop polls the leader and the group.
const POLL: Duration = Duration::from_millis(50);
/// How often the idle thread reaps workers that exited on their own.
const REAP_EVERY: Duration = Duration::from_millis(200);

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
    /// Start the supervisor thread. `grace` is the time a stopped worker gets after SIGINT.
    pub fn new(grace: Duration) -> io::Result<Supervisor> {
        let (tx, rx) = mpsc::channel();
        thread::Builder::new()
            .name("oxa-workers".into())
            .spawn(move || run(rx, grace))?;
        Ok(Supervisor { tx })
    }

    /// Start `argv[0]` with `argv[1..]` in a new process group, stdin null, stdout and stderr
    /// appended to `log`. Returns the process group id, which is the leader's pid.
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
    /// group is gone or the sequence has run out.
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

fn gone() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "worker supervisor is gone")
}

/// The supervisor thread: serves requests until every handle is dropped.
fn run(rx: Receiver<Msg>, grace: Duration) {
    let mut table = Table {
        grace,
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

/// The workers this thread started, by process group id. `live` holds a worker whose leader
/// has exited but whose group still has members, so stop can clean that group up. `done` holds
/// the status of a worker whose group is empty: no signal is ever sent to such a pgid again,
/// because the kernel may reuse the number once the group is gone.
struct Table {
    grace: Duration,
    live: HashMap<u32, Child>,
    done: HashMap<u32, ExitStatus>,
}

impl Table {
    fn spawn(&mut self, argv: &[String], log: &Path) -> io::Result<u32> {
        let (program, args) = argv
            .split_first()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty argv"))?;
        let out = OpenOptions::new().create(true).append(true).open(log)?;
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

    /// Reap the leaders that exited on their own. A leader whose group is now empty moves to
    /// `done`; otherwise it stays in `live` until stop cleans up the group.
    fn reap(&mut self) {
        let mut finished = Vec::new();
        for (pgid, child) in self.live.iter_mut() {
            if let Ok(Some(status)) = child.try_wait() {
                if !group_alive(*pgid) {
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
        let mut status = child.try_wait()?;
        if status.is_none() {
            // Not reaped yet, so the pid cannot have been reused: safe to signal.
            signal(&pgid.to_string(), "INT");
        }
        let grace_end = Instant::now() + self.grace;
        let mut gone = settle(&mut child, pgid, grace_end, &mut status)?;
        if !gone {
            signal(&format!("-{pgid}"), "TERM");
            gone = settle(&mut child, pgid, Instant::now() + KILL_AFTER, &mut status)?;
            if !gone {
                signal(&format!("-{pgid}"), "KILL");
                settle(&mut child, pgid, Instant::now() + KILL_AFTER, &mut status)?;
            }
        }
        match status {
            Some(s) => Ok(Some(s)),
            None => {
                // The leader survived SIGKILL of its group (it cannot, short of a stuck kernel
                // state); make sure it dies so the wait below cannot block forever.
                let _ = child.kill();
                child.wait().map(Some)
            }
        }
    }

    fn stop_all(&mut self) {
        let pgids: Vec<u32> = self.live.keys().copied().collect();
        for pgid in pgids {
            let _ = self.stop(pgid);
        }
        self.done.clear();
    }
}

/// Poll until the group is gone (true) or `until` passes (false). The leader is reaped as soon
/// as it exits, so its zombie does not keep the group looking alive.
fn settle(child: &mut Child, pgid: u32, until: Instant, status: &mut Option<ExitStatus>) -> io::Result<bool> {
    loop {
        if status.is_none() {
            *status = child.try_wait()?;
        }
        if !group_alive(pgid) {
            return Ok(true);
        }
        if Instant::now() >= until {
            return Ok(false);
        }
        thread::sleep(POLL);
    }
}

/// `kill -<sig> -- <target>`; `target` is a pid, or `-<pgid>` for a whole group.
fn signal(target: &str, sig: &str) -> bool {
    Command::new("kill")
        .arg(format!("-{sig}"))
        .arg("--")
        .arg(target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn group_alive(pgid: u32) -> bool {
    signal(&format!("-{pgid}"), "0")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const GRACE: Duration = Duration::from_millis(200);

    fn log_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("oxa-k4-{}-{name}.log", std::process::id()))
    }

    #[test]
    fn stop_kills_whole_process_group() {
        let sup = Supervisor::new(GRACE).expect("supervisor");
        let log = log_path("group");
        let pgid = sup
            .spawn(&["/bin/sh", "-c", "sleep 60 & wait"], &log)
            .expect("spawn");
        // Let sh start its background sleep, which shares the group.
        thread::sleep(Duration::from_millis(300));
        assert!(group_alive(pgid));
        let status = sup.stop(pgid).expect("stop");
        assert!(status.is_some(), "the leader was live");
        assert!(!group_alive(pgid), "the background sleep survived stop");
        let _ = fs::remove_file(&log);
    }

    #[test]
    fn exited_child_is_reaped_with_status() {
        let sup = Supervisor::new(GRACE).expect("supervisor");
        let log = log_path("exited");
        let pgid = sup
            .spawn(&["/bin/sh", "-c", "sleep 60 & wait"], &log)
            .expect("spawn");
        thread::sleep(Duration::from_millis(300));
        // The leader exits on its own (SIGINT from outside); the background sleep stays behind.
        assert!(signal(&pgid.to_string(), "INT"));
        // The idle thread reaps the leader within about 200 ms; its status stays cached.
        thread::sleep(Duration::from_millis(500));
        assert!(!signal(&pgid.to_string(), "0"), "the leader was not reaped");
        assert!(group_alive(pgid), "the background sleep should remain");
        let status = sup.stop(pgid).expect("stop").expect("status");
        assert!(!status.success(), "the leader died of SIGINT");
        assert!(!group_alive(pgid), "the background sleep survived stop");
        assert!(sup.stop(pgid).expect("second stop").is_none());
        let _ = fs::remove_file(&log);
    }

    #[test]
    fn stop_all_leaves_no_child() {
        let sup = Supervisor::new(GRACE).expect("supervisor");
        let log = log_path("all");
        let a = sup
            .spawn(&["/bin/sh", "-c", "sleep 60 & wait"], &log)
            .expect("spawn a");
        let b = sup
            .spawn(&["/bin/sh", "-c", "sleep 60 & wait"], &log)
            .expect("spawn b");
        thread::sleep(Duration::from_millis(300));
        sup.stop_all().expect("stop_all");
        assert!(!group_alive(a), "group a survived stop_all");
        assert!(!group_alive(b), "group b survived stop_all");
        assert!(sup.stop(a).expect("stop a").is_none());
        assert!(sup.stop(b).expect("stop b").is_none());
        let _ = fs::remove_file(&log);
    }
}
