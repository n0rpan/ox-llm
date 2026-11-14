//! A9: process running with timeout, cancel and process-group kill. Owner: task A9. Signatures below are frozen; bodies are yours.
use oxa_proto::{CancelToken, ExecResult, ExecSpec};
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

/// How often the supervising loop looks at the child, the cancel flag and the deadline.
const POLL: Duration = Duration::from_millis(20);
/// After the child exited, how long a still-open pipe (a background process) may hold the call.
const DRAIN: Duration = Duration::from_millis(200);
/// SIGTERM to the group, then SIGKILL after this long.
const KILL_GRACE: Duration = Duration::from_secs(2);

/// Run `argv` in a new process group with stdout and stderr merged into one pipe, streaming
/// chunks to `live` and collecting them. On timeout (`timed_out = true`) or cancel: SIGTERM
/// to the group, then SIGKILL after 2 s; `exit` is `None`. Returns as soon as the child has
/// exited and the pipe is drained or 200 ms have passed since exit (a background process that
/// inherited the pipe must not hang the call). Spawn failure = output holds the error text,
/// `exit: None`.
pub fn run_argv(
    argv: &[String],
    env: &[(String, String)],
    timeout_s: Option<u32>,
    cancel: &CancelToken,
    live: &mut dyn FnMut(&[u8]),
) -> ExecResult {
    let Some((prog, args)) = argv.split_first() else {
        return spawn_failed("no command to run".to_string());
    };
    let mut cmd = Command::new(prog);
    cmd.args(args)
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    execute(cmd, prog, None, timeout_s, cancel, live)
}

/// [`run_argv`] with `stdin` written to the child's standard input and then closed. A child
/// that does not read it all is not waited for: the writer stops when the pipe closes.
pub fn run_argv_stdin(
    argv: &[String],
    env: &[(String, String)],
    stdin: &[u8],
    timeout_s: Option<u32>,
    cancel: &CancelToken,
    live: &mut dyn FnMut(&[u8]),
) -> ExecResult {
    let Some((prog, args)) = argv.split_first() else {
        return spawn_failed("no command to run".to_string());
    };
    let mut cmd = Command::new(prog);
    cmd.args(args)
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    execute(cmd, prog, Some(stdin), timeout_s, cancel, live)
}

/// Run `bash -c <command>` with the user's own rights in `s.cwd` (for `!cmd` / `!!cmd`, pi's
/// behaviour). `s.writable` and `s.net` are ignored.
pub fn run_unsandboxed(s: &ExecSpec, cancel: &CancelToken, live: &mut dyn FnMut(&[u8])) -> ExecResult {
    let mut cmd = Command::new("bash");
    cmd.arg("-c")
        .arg(&s.command)
        .current_dir(&s.cwd)
        .envs(s.env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    execute(cmd, "bash", None, s.timeout_s, cancel, live)
}

fn spawn_failed(text: String) -> ExecResult {
    ExecResult {
        output: text.into_bytes(),
        exit: None,
        timed_out: false,
    }
}

pub(crate) fn signal_group(pgid: libc::pid_t, sig: libc::c_int) {
    // SAFETY: plain kill(2) on a process group we created; a stale group id yields ESRCH.
    unsafe {
        libc::kill(-pgid, sig);
    }
}

fn group_alive(pgid: libc::pid_t) -> bool {
    // SAFETY: signal 0 only probes for the group's existence.
    unsafe { libc::kill(-pgid, 0) == 0 }
}

fn execute(
    mut cmd: Command,
    prog: &str,
    stdin: Option<&[u8]>,
    timeout_s: Option<u32>,
    cancel: &CancelToken,
    live: &mut dyn FnMut(&[u8]),
) -> ExecResult {
    let (reader, writer) = match std::io::pipe() {
        Ok(p) => p,
        Err(e) => return spawn_failed(format!("{prog}: cannot create pipe: {e}")),
    };
    let writer_err = match writer.try_clone() {
        Ok(w) => w,
        Err(e) => return spawn_failed(format!("{prog}: cannot create pipe: {e}")),
    };
    cmd.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::from(writer))
    .stderr(Stdio::from(writer_err))
    .process_group(0);
    let spawned = cmd.spawn();
    // The Command still holds the write ends; they must close here or the reader never sees EOF.
    drop(cmd);
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => return spawn_failed(format!("{prog}: {e}")),
    };
    let pgid = child.id() as libc::pid_t;
    if let (Some(bytes), Some(mut pipe)) = (stdin, child.stdin.take()) {
        // Fed from its own thread so a child that writes before it reads cannot deadlock us;
        // a closed pipe (the child exited or ignores its input) just ends the writer.
        let bytes = bytes.to_vec();
        std::thread::spawn(move || {
            use std::io::Write as _;
            let _ = pipe.write_all(&bytes);
        });
    }

    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut reader = reader;
        let mut buf = vec![0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    });

    let start = Instant::now();
    let mut out: Vec<u8> = Vec::new();
    let mut status: Option<ExitStatus> = None;
    let mut exited_at: Option<Instant> = None;
    let mut eof = false;
    let mut term_at: Option<Instant> = None;
    let mut hard_killed = false;
    let mut timed_out = false;

    loop {
        match rx.recv_timeout(POLL) {
            Ok(chunk) => {
                live(&chunk);
                out.extend_from_slice(&chunk);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => eof = true,
        }
        if status.is_none() {
            if let Ok(Some(st)) = child.try_wait() {
                status = Some(st);
                exited_at = Some(Instant::now());
            }
        }
        if term_at.is_none() {
            let timeout_hit = timeout_s
                .filter(|s| *s > 0)
                .is_some_and(|s| start.elapsed() >= Duration::from_secs(u64::from(s)));
            let cancelled = cancel.is_cancelled();
            if timeout_hit || cancelled {
                timed_out = timeout_hit && !cancelled;
                signal_group(pgid, libc::SIGTERM);
                term_at = Some(Instant::now());
            }
        }
        if let Some(t) = term_at {
            if !hard_killed && (t.elapsed() >= KILL_GRACE || (status.is_some() && !group_alive(pgid))) {
                signal_group(pgid, libc::SIGKILL);
                hard_killed = true;
            }
        }
        if let Some(at) = exited_at {
            let settled = term_at.is_none() || hard_killed;
            if settled && (eof || at.elapsed() >= DRAIN) {
                break;
            }
        }
        if eof && status.is_none() {
            // The pipe is closed but the child still runs: poll without spinning.
            std::thread::sleep(POLL);
        }
    }

    let killed = term_at.is_some();
    let exit = if killed {
        None
    } else {
        status.and_then(|s| s.code())
    };
    ExecResult {
        output: out,
        exit,
        timed_out,
    }
}

/// Make the child of `cmd` die with this process (ENG-019). Between fork and exec the child
/// sets `PR_SET_PDEATHSIG` to SIGTERM and checks that its parent is still the process that
/// spawned it (the parent pid is taken before the fork); if the parent already died, the child
/// exits at once. Linux only: the signal fires when the thread that forked the child exits, so
/// the forking thread must stay alive until the child is waited for.
pub fn die_with_parent(cmd: &mut Command) {
    let parent = std::process::id() as libc::pid_t;
    // SAFETY: the closure runs in the forked child before exec and calls only async-signal-safe
    // functions (prctl, getppid, _exit).
    unsafe {
        cmd.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 || libc::getppid() != parent {
                libc::_exit(1);
            }
            Ok(())
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn die_with_parent_command_runs_normally() {
        let mut cmd = Command::new("true");
        die_with_parent(&mut cmd);
        let status = cmd.status().expect("spawn true");
        assert!(status.success());
    }
}
