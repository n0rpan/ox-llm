//! No pipe reader threads: every descriptor is nonblocking and serviced by the
//! supervisor, so a descendant retaining a pipe cannot hang a reader join.
use serde_json::Value;
use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

const LINE_LIMIT: usize = 256 * 1024;
const INPUT_LIMIT: usize = 256 * 1024;

pub(crate) struct Process {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: ChildStdout,
    stderr: ChildStderr,
    pending: VecDeque<u8>,
    line: Vec<u8>,
    pub(crate) bytes: usize,
    byte_limit: usize,
    stdout_eof: bool,
    reaped: bool,
}

pub(crate) fn invalid(message: &'static str) -> io::Error {
    io::Error::other(message)
}

impl Process {
    pub(crate) fn spawn(
        binary: &Path,
        bwrap: &Path,
        endpoint: &str,
        model: &str,
        role: &str,
        mode: &str,
        byte_limit: usize,
        workspace: Option<&crate::workspace::WorkingCopy>,
        local_only: bool,
    ) -> io::Result<Self> {
        #[cfg(feature = "interactive-control")]
        if !local_only && oxa_proto::offline() {
            return Err(invalid("offline mode refuses cloud worker inference"));
        }
        // Build a new filesystem instead of exposing the parent's root/home.
        // Only the trusted runtime libraries and the explicitly chosen binary
        // are mounted. HOME is private; /work is empty or an approved private copy.
        let mut command = Command::new(bwrap);
        command.args([
            "--unshare-all",
            "--hostname",
            "oxa-worker",
            "--share-net",
            "--die-with-parent",
            "--cap-drop",
            "ALL",
            "--ro-bind",
            "/usr",
            "/usr",
            "--ro-bind-try",
            "/lib",
            "/lib",
            "--ro-bind-try",
            "/lib64",
            "/lib64",
            "--symlink",
            "usr/bin",
            "/bin",
            "--symlink",
            "usr/sbin",
            "/sbin",
            "--new-session",
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--tmpfs",
            "/tmp",
            "--dir",
            "/home/worker",
        ]);
        if let Some(workspace) = workspace {
            command
                .arg(if workspace.editing() {
                    "--bind"
                } else {
                    "--ro-bind"
                })
                .arg(workspace.path())
                .arg("/work");
            if let Some(mask) = workspace.git_mask() {
                command.arg("--ro-bind").arg(mask).arg("/work/.git");
            }
        } else {
            command.args(["--tmpfs", "/work"]);
        }
        command
            .args(["--chdir", "/work", "--ro-bind"])
            .arg(binary)
            .arg("/agent")
            .args([
                "--clearenv",
                "--setenv",
                "HOME",
                "/home/worker",
                "--setenv",
                "PATH",
                "/usr/bin:/bin",
                "--setenv",
                "XDG_CONFIG_HOME",
                "/home/worker/config",
                "--setenv",
                "XDG_STATE_HOME",
                "/home/worker/state",
                "--setenv",
                "XDG_CACHE_HOME",
                "/home/worker/cache",
                "--setenv",
                "XDG_RUNTIME_DIR",
                "/home/worker/runtime",
                "--setenv",
                "OXA_AGENT_DIR",
                "/home/worker/.oxa/agent",
                "--setenv",
                "OXA_OFFLINE",
                "1",
                "--setenv",
                "OXA_SYSTEM_SKILLS_DIR",
                "",
                "--setenv",
                "OXA_PROFILE",
                "pi",
                "--setenv",
                "OXA_ENGINE_URL",
                endpoint,
                "--setenv",
                "LANG",
                "C.UTF-8",
                "--setenv",
                "TERM",
                "dumb",
                "--",
                "/agent",
                "--mode",
                "rpc",
                "--permission-mode",
                mode,
                "--model",
                model,
                "--no-session",
                "--no-mcp",
                "--no-web-search",
                "--no-extensions",
                "--no-skills",
                "--no-prompt-templates",
                "--no-themes",
                "--no-context-files",
                "--no-approve",
            ])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("LANG", "C.UTF-8")
            .current_dir("/")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        if local_only {
            command.arg("--offline");
        }
        if let Some(workspace) = workspace {
            command.arg("--tools").arg(workspace.tools().join(","));
        } else {
            command.arg("--no-tools");
        }
        command.arg("--append-system-prompt").arg(format!(
            "Orchestrator-assigned worker role:\n{role}{}",
            workspace.map(|w| w.role_context()).unwrap_or_default()
        ));
        let mut child = command.spawn()?;
        let descriptors = (child.stdin.take(), child.stdout.take(), child.stderr.take());
        let (Some(stdin), Some(stdout), Some(stderr)) = descriptors else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(invalid("missing worker pipes"));
        };
        let mut process = Self {
            child,
            stdin: Some(stdin),
            stdout,
            stderr,
            pending: VecDeque::new(),
            line: Vec::new(),
            bytes: 0,
            byte_limit,
            stdout_eof: false,
            reaped: false,
        };
        for fd in [
            process.stdin.as_ref().unwrap().as_raw_fd(),
            process.stdout.as_raw_fd(),
            process.stderr.as_raw_fd(),
        ] {
            if let Err(error) = nonblocking(fd) {
                process.kill();
                return Err(error);
            }
        }
        Ok(process)
    }

    pub(crate) fn queue(&mut self, value: &Value) -> io::Result<()> {
        let mut bytes = serde_json::to_vec(value)?;
        bytes.push(b'\n');
        if bytes.len() > INPUT_LIMIT.saturating_sub(self.pending.len()) {
            return Err(invalid("worker input limit exceeded"));
        }
        self.pending.extend(bytes);
        Ok(())
    }

    /// Bound each poll's work too: an infinite emitter cannot starve other
    /// workers, controls, cancellation or the deadline checks.
    pub(crate) fn poll(&mut self) -> io::Result<Vec<Value>> {
        if let Some(stdin) = self.stdin.as_mut() {
            for _ in 0..8 {
                if self.pending.is_empty() {
                    break;
                }
                let buffer = self.pending.make_contiguous();
                match stdin.write(buffer) {
                    Ok(0) => return Err(invalid("worker stdin closed")),
                    Ok(n) => {
                        self.pending.drain(..n);
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => return Err(invalid("worker input failed")),
                }
            }
        }
        let mut records = Vec::new();
        let mut buffer = [0u8; 4096];
        for _ in 0..8 {
            match self.stdout.read(&mut buffer) {
                Ok(0) => {
                    self.stdout_eof = true;
                    break;
                }
                Ok(n) => {
                    self.account(n)?;
                    for byte in &buffer[..n] {
                        if *byte == b'\n' {
                            if !self.line.is_empty() {
                                let record: Value = serde_json::from_slice(&self.line)
                                    .map_err(|_| invalid("malformed worker record"))?;
                                if !record.is_object() {
                                    return Err(invalid("worker record must be an object"));
                                }
                                records.push(record);
                                self.line.clear();
                            }
                        } else {
                            if self.line.len() == LINE_LIMIT {
                                return Err(invalid("worker record limit exceeded"));
                            }
                            self.line.push(*byte);
                        }
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(invalid("worker output failed")),
            }
        }
        for _ in 0..8 {
            match self.stderr.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => self.account(n)?,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(invalid("worker diagnostic read failed")),
            }
        }
        Ok(records)
    }

    fn account(&mut self, n: usize) -> io::Result<()> {
        if n > self.byte_limit.saturating_sub(self.bytes) {
            return Err(invalid("worker output budget exceeded"));
        }
        self.bytes += n;
        Ok(())
    }

    pub(crate) fn ended(&mut self) -> io::Result<bool> {
        if self.child.try_wait()?.is_some() {
            self.reaped = true;
            return Ok(true);
        }
        Ok(self.stdout_eof)
    }

    pub(crate) fn pid(&self) -> u32 {
        self.child.id()
    }

    pub(crate) fn kill(&mut self) {
        // bwrap --unshare-pid ensures terminating its namespace init also kills
        // grandchildren even if a child changed process groups or held a pipe.
        // SAFETY: child PID is the process-group leader we created at spawn.
        if !self.reaped {
            unsafe {
                libc::kill(-(self.child.id() as i32), libc::SIGKILL);
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
            self.reaped = true;
        }
        self.stdin.take();
    }

    pub(crate) fn stop(&mut self, grace: Duration) {
        let _ = self.queue(&serde_json::json!({"id":"shutdown","type":"abort"}));
        let _ = self.poll();
        self.stdin.take();
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            if self.child.try_wait().ok().flatten().is_some() {
                self.reaped = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        self.kill();
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.kill();
    }
}

pub(crate) fn nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: live owned pipe descriptor; fcntl only changes its status flags.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: same owned descriptor and valid flag bits.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(crate) fn checked_program(path: &Path) -> io::Result<std::path::PathBuf> {
    let path = path.canonicalize()?;
    if !File::open(&path)?.metadata()?.is_file() {
        return Err(invalid("worker program must be a file"));
    }
    Ok(path)
}
