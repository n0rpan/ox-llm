//! E7: the stdio transport (child process, newline-delimited JSON). Owner: task E7. Signatures below are frozen; bodies are yours.
// ported from pi packages/mcp/src/transports/{stdio,lines,pipe}.ts
use crate::{StdioConfig, Transport};
use oxa_proto::{codes, OxaError, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Stderr lines kept for `stderr_tail`.
const STDERR_KEEP: usize = 20;
/// Each stop stage waits this long before the next signal.
const STOP_STEP: Duration = Duration::from_secs(2);

/// A server process speaking newline-delimited JSON-RPC on stdin/stdout.
pub struct StdioTransport {
    pub(crate) child: Option<std::process::Child>,
    pub(crate) stdin: Option<std::process::ChildStdin>,
    pub(crate) incoming: std::sync::mpsc::Receiver<Result<Value, OxaError>>,
    pub(crate) stderr: Arc<Mutex<Vec<String>>>,
    pub(crate) closed: bool,
}

/// Item 1 (sec-prod): the argv (program first) and the working directory of a stdio server.
/// Offline, the argv is wrapped in bwrap without network (`crate::offline::launch`); a missing
/// bwrap refuses the start with the offline error.
pub(crate) fn stdio_plan(
    cfg: &StdioConfig,
    session_cwd: &Path,
    home: &Path,
    offline: bool,
    path_var: &str,
) -> Result<(Vec<String>, PathBuf), OxaError> {
    let program = expand_home(&cfg.command, home).to_string_lossy().into_owned();
    let cwd = match cfg.cwd.as_deref() {
        None => session_cwd.to_path_buf(),
        Some(c) => {
            let p = expand_home(c, home);
            if p.is_absolute() {
                p
            } else {
                session_cwd.join(p)
            }
        }
    };
    let mut argv = vec![program.clone()];
    argv.extend(
        cfg.args
            .iter()
            .map(|a| expand_home(a, home).to_string_lossy().into_owned()),
    );
    let argv = crate::offline::launch(
        &format!("the MCP server `{program}`"),
        offline,
        path_var,
        home,
        &cwd,
        argv,
    )?;
    Ok((argv, cwd))
}

/// `~/` at the start of a value is the home directory.
fn expand_home(value: &str, home: &Path) -> PathBuf {
    match value.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => PathBuf::from(value),
    }
}

/// `true` when the leader of process group `pid` has exited. Does not reap it, so the group
/// id stays valid for the signals that follow.
fn leader_exited(pid: libc::pid_t) -> bool {
    // SAFETY: `info` is a plain C struct, zeroed before the call; waitid only writes to it.
    unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        let r = libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        );
        r == 0 && info.si_pid() != 0
    }
}

fn wait_leader_exit(pid: libc::pid_t, window: Duration) -> bool {
    let deadline = Instant::now() + window;
    loop {
        if leader_exited(pid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Signal every process of the group `pid` (the server and what it started).
fn signal_group(pid: libc::pid_t, sig: libc::c_int) {
    // SAFETY: kill has no memory effects; a group that is already gone yields ESRCH, ignored.
    unsafe {
        libc::kill(-pid, sig);
    }
}

impl StdioTransport {
    /// Start the server. The caller has already asked the permission gate.
    ///
    /// * `command` is one executable (no shell); a leading `~/` in the command, an argument or
    ///   `cwd` is the home directory `home`; a relative `cwd` resolves against `session_cwd`.
    /// * `env` = the parent environment + `expanded_env` (values already expanded by
    ///   `crate::expand_value`).
    /// * The child is the leader of a new process group (`setsid`) and dies with the parent
    ///   (`PR_SET_PDEATHSIG`).
    /// * A reader thread parses stdout lines (non-JSON lines are dropped), another keeps the
    ///   last 20 stderr lines.
    ///
    /// Errors: `mcp` with the OS error when the executable cannot be started.
    pub fn spawn(
        cfg: &StdioConfig,
        expanded_env: &[(String, String)],
        session_cwd: &Path,
        home: &Path,
    ) -> Result<StdioTransport, OxaError> {
        let (argv, cwd) = stdio_plan(
            cfg,
            session_cwd,
            home,
            oxa_proto::offline(),
            &std::env::var("PATH").unwrap_or_default(),
        )?;
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..]);
        cmd.envs(expanded_env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        cmd.current_dir(&cwd);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // SAFETY: the closure runs in the forked child before exec and makes only the
        // async-signal-safe calls setsid and prctl.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| OxaError::new(codes::MCP, format!("cannot start MCP server `{}`: {e}", argv[0])))?;

        let stdout = child.stdout.take();
        let stderr_pipe = child.stderr.take();
        let stdin = child.stdin.take();

        let (tx, rx) = mpsc::channel();
        if let Some(stdout) = stdout {
            thread::spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    let line = match line {
                        Ok(l) => l,
                        Err(e) => {
                            let _ = tx.send(Err(OxaError::new(
                                codes::MCP,
                                format!("reading from MCP server failed: {e}"),
                            )));
                            break;
                        }
                    };
                    let text = line.trim();
                    if text.is_empty() {
                        continue;
                    }
                    // Non-JSON lines (a server's stray prints) are dropped.
                    if let Ok(value) = serde_json::from_str::<Value>(text) {
                        if tx.send(Ok(value)).is_err() {
                            break;
                        }
                    }
                }
            });
        }

        let stderr: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        if let Some(pipe) = stderr_pipe {
            let buf = stderr.clone();
            thread::spawn(move || {
                for line in BufReader::new(pipe).lines() {
                    let Ok(line) = line else { break };
                    let mut lines = buf.lock().unwrap_or_else(|p| p.into_inner());
                    lines.push(line);
                    if lines.len() > STDERR_KEEP {
                        let excess = lines.len() - STDERR_KEEP;
                        lines.drain(..excess);
                    }
                }
            });
        }

        Ok(StdioTransport {
            child: Some(child),
            stdin,
            incoming: rx,
            stderr,
            closed: false,
        })
    }

    /// Process id of the server, while it runs.
    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(|c| c.id())
    }
}

impl Transport for StdioTransport {
    fn send(&mut self, msg: &Value) -> Result<(), OxaError> {
        if self.closed {
            return Err(OxaError::new(codes::MCP, "MCP server transport is closed"));
        }
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(OxaError::new(codes::MCP, "MCP server stdin is closed"));
        };
        let mut line = serde_json::to_string(msg)
            .map_err(|e| OxaError::new(codes::MCP, format!("cannot encode message: {e}")))?;
        line.push('\n');
        let written = stdin.write_all(line.as_bytes()).and_then(|_| stdin.flush());
        written.map_err(|e| OxaError::new(codes::MCP, format!("write to MCP server failed: {e}")))
    }

    fn recv(&mut self, timeout: Duration) -> Result<Option<Value>, OxaError> {
        if self.closed {
            return Err(OxaError::new(codes::MCP, "MCP server transport is closed"));
        }
        match self.incoming.recv_timeout(timeout) {
            Ok(Ok(v)) => Ok(Some(v)),
            Ok(Err(e)) => Err(e),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(OxaError::new(
                codes::MCP,
                "MCP server closed its output (it may have exited)",
            )),
        }
    }

    /// Stop = stdin close, then SIGTERM, then SIGKILL of the process group (2 s apart).
    fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        // Stage 0: EOF on stdin is the polite stop.
        self.stdin = None;
        let Some(mut child) = self.child.take() else {
            return;
        };
        let pid = child.id() as libc::pid_t;
        if wait_leader_exit(pid, STOP_STEP) {
            // The server left; end anything it started and still runs.
            signal_group(pid, libc::SIGTERM);
        } else {
            // Stage 1: SIGTERM to the group, then wait.
            signal_group(pid, libc::SIGTERM);
            if !wait_leader_exit(pid, STOP_STEP) {
                // Stage 2: SIGKILL to the group.
                signal_group(pid, libc::SIGKILL);
            }
        }
        let _ = child.wait();
    }

    fn stderr_tail(&self) -> String {
        self.stderr.lock().unwrap_or_else(|p| p.into_inner()).join("\n")
    }

    fn kind(&self) -> &'static str {
        "stdio"
    }
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        if !self.closed {
            self.close();
        }
    }
}

#[cfg(test)]
mod offline_tests {
    use super::*;
    use oxa_fake::TestEnv;
    use std::os::unix::fs::PermissionsExt;

    fn cfg() -> StdioConfig {
        StdioConfig {
            command: "~/bin/server".to_string(),
            args: vec!["--flag".to_string(), "~/data".to_string()],
            env: Vec::new(),
            cwd: None,
        }
    }

    fn fake_bwrap(env: &TestEnv) -> String {
        let script = env.write("bin/bwrap", "#!/bin/sh\nexit 0\n");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        script.parent().expect("dir").to_string_lossy().into_owned()
    }

    #[test]
    fn not_offline_starts_the_server_directly() {
        let env = TestEnv::new();
        let home = env.home();
        let work = env.work();
        let (argv, cwd) = stdio_plan(&cfg(), &work, &home, false, "").expect("plan");
        assert_eq!(
            argv,
            vec![
                home.join("bin/server").display().to_string(),
                "--flag".to_string(),
                home.join("data").display().to_string()
            ]
        );
        assert_eq!(cwd, work);
    }

    #[test]
    fn offline_wraps_the_server_in_bwrap_without_network() {
        let env = TestEnv::new();
        let home = env.home();
        let work = env.work();
        let dir = fake_bwrap(&env);
        let (argv, _) = stdio_plan(&cfg(), &work, &home, true, &dir).expect("plan");
        let want = [
            format!("{dir}/bwrap"),
            "--unshare-net".to_string(),
            "--die-with-parent".to_string(),
            "--bind".to_string(),
            "/".to_string(),
            "/".to_string(),
            "--dev-bind".to_string(),
            "/dev".to_string(),
            "/dev".to_string(),
            "--proc".to_string(),
            "/proc".to_string(),
            "--chdir".to_string(),
            work.display().to_string(),
            "--".to_string(),
            home.join("bin/server").display().to_string(),
            "--flag".to_string(),
            home.join("data").display().to_string(),
        ];
        assert_eq!(argv, want);
    }

    #[test]
    fn offline_without_bwrap_refuses_the_start() {
        let env = TestEnv::new();
        let err = stdio_plan(&cfg(), &env.work(), &env.home(), true, "").expect_err("refused");
        assert_eq!(err.code, oxa_proto::codes::OFFLINE);
        assert!(err.to_string().starts_with("Refused: the MCP server `"), "{err}");
    }
}
