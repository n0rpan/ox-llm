//! A9: process running with timeout, cancel and process-group kill.
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
/// Most captured output kept per command (head and tail halves); the middle is dropped.
const CAPTURE_CAP: usize = 16 * 1024 * 1024;
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
    run_argv_capture(argv, env, timeout_s, cancel, live, true)
}

/// [`run_argv`] with a choice: `capture = false` streams to `live` only and leaves
/// `ExecResult.output` empty, so a long-lived process never grows the heap.
pub fn run_argv_capture(
    argv: &[String],
    env: &[(String, String)],
    timeout_s: Option<u32>,
    cancel: &CancelToken,
    live: &mut dyn FnMut(&[u8]),
    capture: bool,
) -> ExecResult {
    let Some((prog, args)) = argv.split_first() else {
        return spawn_failed("no command to run".to_string());
    };
    let mut cmd = Command::new(prog);
    cmd.args(args)
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    execute(cmd, prog, None, timeout_s, cancel, live, capture)
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
    execute(cmd, prog, Some(stdin), timeout_s, cancel, live, true)
}

/// Run a resolved command with only the environment chosen by the caller.
pub(crate) fn run_argv_cleared(
    argv: &[String],
    env: &[(String, String)],
    stdin: Option<&[u8]>,
    timeout_s: Option<u32>,
    cancel: &CancelToken,
    live: &mut dyn FnMut(&[u8]),
    capture: bool,
) -> ExecResult {
    let Some((prog, args)) = argv.split_first() else {
        return spawn_failed("no command to run".to_string());
    };
    let mut cmd = Command::new(prog);
    cmd.args(args)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    execute(cmd, prog, stdin, timeout_s, cancel, live, capture)
}

/// Run `argv` (optionally feeding `stdin`) so that only this child inherits the listed close-on-exec descriptors.
pub(crate) fn run_argv_fds(
    argv: &[String],
    stdin: Option<&[u8]>,
    fds: Vec<i32>,
    timeout_s: Option<u32>,
    cancel: &CancelToken,
    live: &mut dyn FnMut(&[u8]),
    capture: bool,
) -> ExecResult {
    let Some((prog, args)) = argv.split_first() else {
        return spawn_failed("no command to run".to_string());
    };
    let mut cmd = Command::new(prog);
    cmd.args(args);
    if !fds.is_empty() {
        // SAFETY: runs between fork and exec; fcntl is async-signal-safe and touches only
        // our own descriptors in the child.
        unsafe {
            cmd.pre_exec(move || {
                for &fd in &fds {
                    if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
    }
    execute(cmd, prog, stdin, timeout_s, cancel, live, capture)
}

/// Run `bash -c <command>` with the user's own rights in `s.cwd` (for `!cmd` / `!!cmd`, pi's
/// behaviour). `s.writable` and `s.net` are ignored.
pub fn run_unsandboxed(s: &ExecSpec, cancel: &CancelToken, live: &mut dyn FnMut(&[u8])) -> ExecResult {
    run_unsandboxed_capture(s, cancel, live, true)
}

/// [`run_unsandboxed`] with `capture = false`: chunks go only to `live`, `output` stays empty.
pub fn run_unsandboxed_capture(
    s: &ExecSpec,
    cancel: &CancelToken,
    live: &mut dyn FnMut(&[u8]),
    capture: bool,
) -> ExecResult {
    run_unsandboxed_env(s, None, cancel, live, capture)
}

/// The command for `s`: the process environment plus `s.env`, or, with `base`, exactly `base`
/// (what bwrap gives the model's bash: [`crate::bwrap::base_env`]).
fn unsandboxed_command(s: &ExecSpec, base: Option<&[(String, String)]>) -> Command {
    let mut cmd = Command::new("bash");
    cmd.arg("-c").arg(&s.command).current_dir(&s.cwd).env_clear();
    // Nothing is inherited: exactly `base`, or the spec's own environment.
    match base {
        Some(env) => cmd.envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str()))),
        None => cmd.envs(s.env.iter().map(|(k, v)| (k.as_str(), v.as_str()))),
    };
    cmd
}

/// [`run_unsandboxed_capture`] with an optional exact environment (`None` = the spec's own).
pub(crate) fn run_unsandboxed_env(
    s: &ExecSpec,
    base: Option<&[(String, String)]>,
    cancel: &CancelToken,
    live: &mut dyn FnMut(&[u8]),
    capture: bool,
) -> ExecResult {
    let cmd = unsandboxed_command(s, base);
    let mut r = execute(cmd, "bash", None, s.timeout_s, cancel, live, capture);
    // Linux keeps `false`: only a platform without bwrap reports a command as unsandboxed.
    r.unsandboxed = !cfg!(target_os = "linux");
    r
}

/// [`run_unsandboxed_stdin`] with an exact environment (exec hooks).
pub(crate) fn run_unsandboxed_stdin_env(
    s: &ExecSpec,
    stdin: &[u8],
    cancel: &CancelToken,
    live: &mut dyn FnMut(&[u8]),
    base: &[(String, String)],
) -> ExecResult {
    let cmd = unsandboxed_command(s, Some(base));
    let mut r = execute(cmd, "bash", Some(stdin), s.timeout_s, cancel, live, true);
    r.unsandboxed = true;
    r
}

/// [`run_unsandboxed`] with `stdin` written to the command's standard input and then closed.
/// Used by explicitly host-side callers that must provide standard input. The child starts from
/// an empty environment plus `s.env`, like every other unsandboxed model command.
pub fn run_unsandboxed_stdin(
    s: &ExecSpec,
    stdin: &[u8],
    cancel: &CancelToken,
    live: &mut dyn FnMut(&[u8]),
) -> ExecResult {
    let mut cmd = Command::new("bash");
    cmd.arg("-c")
        .arg(&s.command)
        .current_dir(&s.cwd)
        .env_clear()
        .envs(s.env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    execute(cmd, "bash", Some(stdin), s.timeout_s, cancel, live, true)
}

/// Bounded capture: the first and last `cap / 2` bytes, with a count of what was dropped.
struct Capture {
    half: usize,
    head: Vec<u8>,
    tail: std::collections::VecDeque<u8>,
    dropped: u64,
}

impl Capture {
    fn new(cap: usize) -> Self {
        Capture {
            half: cap / 2,
            head: Vec::new(),
            tail: std::collections::VecDeque::new(),
            dropped: 0,
        }
    }

    fn push(&mut self, mut chunk: &[u8]) {
        if self.head.len() < self.half {
            let n = (self.half - self.head.len()).min(chunk.len());
            self.head.extend_from_slice(&chunk[..n]);
            chunk = &chunk[n..];
        }
        self.tail.extend(chunk);
        while self.tail.len() > self.half {
            let over = self.tail.len() - self.half;
            self.tail.drain(..over);
            self.dropped += over as u64;
        }
    }

    fn finish(self) -> Vec<u8> {
        let mut out = self.head;
        if self.dropped > 0 {
            out.extend_from_slice(
                format!("\n[... {} bytes of output dropped ...]\n", self.dropped).as_bytes(),
            );
        }
        out.extend(self.tail);
        out
    }
}

fn spawn_failed(text: String) -> ExecResult {
    ExecResult {
        output: text.into_bytes(),
        exit: None,
        timed_out: false,
        unsandboxed: false,
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
    capture: bool,
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
    let mut out = Capture::new(CAPTURE_CAP);
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
                if capture {
                    out.push(&chunk);
                }
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
        output: out.finish(),
        exit,
        timed_out,
        unsandboxed: false,
    }
}

/// Make the child of `cmd` die with this process (ENG-019). Between fork and exec the child
/// sets `PR_SET_PDEATHSIG` to SIGTERM and checks that its parent is still the process that
/// spawned it (the parent pid is taken before the fork); if the parent already died, the child
/// exits at once. Linux only: the signal fires when the thread that forked the child exits, so
/// the forking thread must stay alive until the child is waited for.
#[cfg(target_os = "linux")]
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

/// Off Linux there is no parent-death signal: the child is not supervised (a no-op, never a
/// refusal). A child that outlives a crashed parent is the platform's known gap.
#[cfg(not(target_os = "linux"))]
pub fn die_with_parent(_cmd: &mut Command) {}

/// A checked, resolved working directory held open until process spawn.
/// Opening walks each component without following symlinks; applying it uses `fchdir`,
/// never a second pathname lookup in the child.
pub struct PinnedCwd(std::fs::File);

impl PinnedCwd {
    /// Open an absolute, already resolved directory, refusing substituted symlinks.
    pub fn open(path: &std::path::Path) -> std::io::Result<Self> {
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::os::unix::ffi::OsStrExt;
        use std::path::Component;
        if !path.is_absolute() {
            return Err(std::io::Error::other("expected a resolved absolute directory"));
        }
        let mut dir = std::fs::File::open("/")?;
        for component in path.components() {
            let name = match component {
                Component::RootDir => continue,
                Component::Normal(name) => name,
                _ => return Err(std::io::Error::other("expected a resolved absolute directory")),
            };
            let name = std::ffi::CString::new(name.as_bytes()).map_err(std::io::Error::other)?;
            // SAFETY: dir is open and name is a valid single-component C string.
            let fd = unsafe {
                libc::openat(dir.as_raw_fd(), name.as_ptr(), {
                    #[cfg(target_os = "linux")]
                    let access = libc::O_PATH;
                    #[cfg(not(target_os = "linux"))]
                    let access = libc::O_RDONLY;
                    access | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC
                })
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: fd was just opened and has no other owner.
            dir = unsafe { std::fs::File::from_raw_fd(fd) };
        }
        Ok(Self(dir))
    }

    /// Open and validate a socket beneath an already resolved directory.
    #[cfg(target_os = "linux")]
    pub(crate) fn open_socket(path: &std::path::Path) -> std::io::Result<std::fs::File> {
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::os::unix::{ffi::OsStrExt, fs::FileTypeExt};

        let dir = Self::open(
            path.parent()
                .ok_or_else(|| std::io::Error::other("missing socket parent"))?,
        )?;
        let name = path
            .file_name()
            .ok_or_else(|| std::io::Error::other("missing socket name"))?;
        let name = std::ffi::CString::new(name.as_bytes()).map_err(std::io::Error::other)?;
        // SAFETY: the directory FD and single-component name are valid for this call.
        let fd = unsafe {
            libc::openat(
                dir.0.as_raw_fd(),
                name.as_ptr(),
                libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: openat returned an owned descriptor.
        let socket = unsafe { std::fs::File::from_raw_fd(fd) };
        if socket.metadata()?.file_type().is_socket() {
            Ok(socket)
        } else {
            Err(std::io::Error::other("path is no longer a Unix socket"))
        }
    }

    /// Set the child's cwd to this open directory, retaining the descriptor until exec.
    /// Do not also set `Command::current_dir`.
    pub fn apply(&self, cmd: &mut Command) -> std::io::Result<()> {
        use std::os::fd::AsRawFd;
        let dir = self.0.try_clone()?;
        // SAFETY: the closure owns the open directory and calls only async-signal-safe
        // fchdir before exec; it allocates nothing and takes no locks.
        unsafe {
            cmd.pre_exec(move || {
                if libc::fchdir(dir.as_raw_fd()) == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn huge_output_is_capped_keeping_head_and_tail() {
        let argv: Vec<String> = ["sh", "-c", "printf HEAD; head -c 40000000 /dev/zero; printf TAIL"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let r = run_argv(&argv, &[], None, &CancelToken::default(), &mut |_| {});
        assert!(
            r.output.len() < CAPTURE_CAP + 1024,
            "captured {} bytes",
            r.output.len()
        );
        assert!(r.output.starts_with(b"HEAD"));
        assert!(r.output.ends_with(b"TAIL"));
        let text = String::from_utf8_lossy(&r.output);
        assert!(text.contains("bytes of output dropped"), "no drop notice");
    }

    #[test]
    fn small_output_is_untouched() {
        let mut c = Capture::new(10);
        c.push(b"hello");
        assert_eq!(c.finish(), b"hello");
    }

    #[test]
    fn pinned_cwd_refuses_symlinks_and_parent_components() {
        let env = oxa_fake::TestEnv::new();
        let work = std::fs::canonicalize(env.work()).unwrap();
        let target = work.join("real");
        std::fs::create_dir_all(target.join("sub")).unwrap();
        std::os::unix::fs::symlink(&target, work.join("link")).unwrap();
        assert!(PinnedCwd::open(&work.join("link")).is_err());
        assert!(PinnedCwd::open(&work.join("link/sub")).is_err());
        assert!(PinnedCwd::open(&work.join("real/../real")).is_err());
        assert!(PinnedCwd::open(std::path::Path::new("real")).is_err());
        assert!(PinnedCwd::open(&work.join("missing")).is_err());
    }

    #[test]
    fn pinned_cwd_spawn_uses_open_directory_after_path_is_retargeted() {
        let env = oxa_fake::TestEnv::new();
        let work = std::fs::canonicalize(env.work()).unwrap();
        let original = work.join("original");
        let moved = work.join("moved");
        let outside = env.root().join("elsewhere");
        std::fs::create_dir_all(&original).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let pin = PinnedCwd::open(&original).unwrap();
        let mut cmd = Command::new("/bin/pwd");
        pin.apply(&mut cmd).unwrap();
        // Even a swap after applying the command cannot redirect the child.
        std::fs::rename(&original, &moved).unwrap();
        std::os::unix::fs::symlink(&outside, &original).unwrap();
        drop(pin);
        let out = cmd.output().unwrap();
        assert!(out.status.success());
        assert_eq!(
            String::from_utf8(out.stdout).unwrap().trim(),
            moved.to_str().unwrap()
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn die_with_parent_command_runs_normally() {
        let mut cmd = Command::new("true");
        die_with_parent(&mut cmd);
        let status = cmd.status().expect("spawn true");
        assert!(status.success());
    }
}
