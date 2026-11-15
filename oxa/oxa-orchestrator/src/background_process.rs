//! Bounded owner of an already sandbox-prepared command. No shell/argv policy here.
use oxa_proto::CancelToken;
use std::{
    io::{self, Read},
    os::fd::AsRawFd,
    os::unix::process::CommandExt,
    process::{Child, Stdio},
    thread,
    time::Duration,
};

pub(crate) struct Result {
    pub exit: Option<i32>,
    pub reason: &'static str,
    pub output: Vec<u8>,
    pub truncated: bool,
}
struct Owned(Option<Child>);
impl Owned {
    fn signal(&self, signal: i32) {
        if let Some(child) = &self.0 {
            // The unreaped child pins its PID/PGID until all group cleanup is done.
            unsafe {
                libc::kill(-(child.id() as i32), signal);
            }
        }
    }
    fn exited(&self) -> io::Result<bool> {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let child = self.0.as_ref().unwrap();
        // WNOWAIT observes exit without freeing the ownership identity for reuse.
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                child.id(),
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { info.si_pid() } != 0)
    }
    fn reap(&mut self) -> io::Result<Option<i32>> {
        let status = self.0.as_mut().unwrap().wait()?;
        self.0.take();
        Ok(status.code())
    }
}
impl Drop for Owned {
    fn drop(&mut self) {
        self.signal(libc::SIGKILL);
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
fn nonblocking(fd: i32) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
fn clock() -> io::Result<u64> {
    crate::resume::boot_time_ns()
}
fn read(
    pipe: &mut impl Read,
    prepared: &mut oxa_proto::PreparedBackground,
    stderr: bool,
    output: &mut Vec<u8>,
    cap: usize,
    truncated: &mut bool,
) -> io::Result<bool> {
    let mut bytes = [0; 8192];
    for _ in 0..4 {
        match pipe.read(&mut bytes) {
            Ok(0) => {
                let tail = prepared.filter(stderr, &[], true);
                let keep = tail.len().min(cap.saturating_sub(output.len()));
                output.extend_from_slice(&tail[..keep]);
                *truncated |= keep != tail.len();
                return Ok(true);
            }
            Ok(n) => {
                let safe = prepared.filter(stderr, &bytes[..n], false);
                let keep = safe.len().min(cap.saturating_sub(output.len()));
                output.extend_from_slice(&safe[..keep]);
                *truncated |= keep != safe.len();
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(false)
}
pub(crate) fn run(
    mut prepared: oxa_proto::PreparedBackground,
    timeout_s: u32,
    cap: usize,
    cancel: &CancelToken,
    mut spawned: impl FnMut(u32),
) -> io::Result<Result> {
    if cancel.is_cancelled() {
        return Ok(Result {
            exit: None,
            reason: "canceled",
            output: vec![],
            truncated: false,
        });
    }
    let started = clock()?;
    let deadline = started
        .checked_add(u64::from(timeout_s) * 1_000_000_000)
        .ok_or_else(|| io::Error::other("task deadline overflow"))?;
    let mut command = prepared.command();
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LANG", "C.UTF-8")
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let parent = std::process::id() as i32;
    // Only async-signal-safe syscalls between fork and exec. The owner thread
    // remains alive through reap; unexpected parent/thread death kills bwrap.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 || libc::getppid() != parent {
                libc::_exit(1);
            }
            Ok(())
        });
    }
    let mut owned = Owned(Some(command.spawn()?));
    let mut stdout = owned
        .0
        .as_mut()
        .unwrap()
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("missing task stdout"))?;
    let mut stderr = owned
        .0
        .as_mut()
        .unwrap()
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("missing task stderr"))?;
    nonblocking(stdout.as_raw_fd())?;
    nonblocking(stderr.as_raw_fd())?;
    spawned(owned.0.as_ref().unwrap().id());
    let mut output = Vec::new();
    let mut truncated = false;
    let mut reason = "completed";
    let mut term_at = None;
    let mut exited_at = None;
    let mut killed = false;
    loop {
        let out_eof = read(
            &mut stdout,
            &mut prepared,
            false,
            &mut output,
            cap,
            &mut truncated,
        )?;
        let err_eof = read(&mut stderr, &mut prepared, true, &mut output, cap, &mut truncated)?;
        let now = clock()?;
        if reason == "completed" {
            reason = if truncated {
                "output_limit"
            } else if cancel.is_cancelled() {
                "canceled"
            } else if now >= deadline {
                "timeout"
            } else {
                "completed"
            };
        }
        if reason != "completed" && term_at.is_none() {
            owned.signal(libc::SIGTERM);
            term_at = Some(now);
        }
        if term_at.is_some_and(|at| now.saturating_sub(at) >= 100_000_000) && !killed {
            owned.signal(libc::SIGKILL);
            killed = true;
        }
        if exited_at.is_none() && owned.exited()? {
            // Never leave descendants behind after the command leader exits.
            owned.signal(libc::SIGKILL);
            exited_at = Some(now);
        }
        if exited_at.is_some_and(|at| (out_eof && err_eof) || now.saturating_sub(at) >= 100_000_000) {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    let exit = owned.reap()?;
    Ok(Result {
        exit,
        reason,
        output,
        truncated,
    })
}
