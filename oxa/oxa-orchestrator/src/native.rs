//! Explicit native RPC process ownership with optional approved private copies.
use crate::pipes::JsonPipes;
use serde_json::Value;
use std::fs::File;
use std::io;
use std::net::SocketAddr;
use std::os::unix::{fs::MetadataExt, process::CommandExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Operator assertion that this literal loopback route cannot forward to paid
/// or remote inference. Loopback and model metadata alone do not prove that.
#[derive(Clone, Debug)]
pub struct VerifiedLocal {
    address: SocketAddr,
    model: String,
}
impl VerifiedLocal {
    pub fn model(&self) -> &str {
        &self.model
    }
    pub fn register(address: SocketAddr, model: String) -> io::Result<Self> {
        if !address.ip().is_loopback()
            || address.port() == 0
            || model.is_empty()
            || model.len() > 256
            || model.starts_with('-')
            || model.chars().any(char::is_control)
        {
            return Err(invalid("invalid verified local inference route"));
        }
        Ok(Self { address, model })
    }
}

/// Explicit trusted launch configuration. Worker code and the bwrap executable
/// are operator-selected; task messages and agent cards never select them.
/// Role text must already be approved for delegation and is not audit metadata.
pub struct Launch {
    pub worker_binary: PathBuf,
    pub bwrap_binary: PathBuf,
    pub inference: VerifiedLocal,
    pub role: String,
    pub wall_time: Duration,
    pub output_bytes: usize,
}

/// Owns one child in a new process group and bwrap PID namespace. EOF is never
/// used as process-stopped evidence. Only the retained Child can be signalled;
/// this API neither adopts recorded PIDs nor restarts failed workers.
pub struct NativeWorker {
    child: Child,
    pipes: JsonPipes,
    exit: Option<ExitStatus>,
    deadline: Instant,
}
impl NativeWorker {
    pub fn spawn(config: Launch) -> io::Result<Self> {
        Self::spawn_profile(config, None)
    }
    /// The caller must retain this exact copy for post-stop review collection.
    /// Source checkouts are never mounted; the grant fixes the tool/mode profile.
    pub fn spawn_with_workspace(
        config: Launch,
        copy: Arc<crate::workspace::WorkingCopy>,
    ) -> io::Result<Self> {
        Self::spawn_profile(config, Some(copy))
    }
    fn spawn_profile(
        config: Launch,
        workspace: Option<Arc<crate::workspace::WorkingCopy>>,
    ) -> io::Result<Self> {
        let deadline = Instant::now()
            .checked_add(config.wall_time)
            .filter(|_| !config.wall_time.is_zero())
            .ok_or_else(|| invalid("invalid worker deadline"))?;
        if config.output_bytes == 0
            || config.role.trim().is_empty()
            || config.role.len() > 16 * 1024
            || config.role.contains('\0')
        {
            return Err(invalid("invalid worker role or output allowance"));
        }
        let binary = checked_program(&config.worker_binary)?;
        let bwrap = checked_program(&config.bwrap_binary)?;
        let mut command = Command::new(bwrap);
        let mode = workspace.as_ref().map_or("plan", |w| w.mode());
        if let Some(copy) = &workspace {
            let directory = crate::namespace::protected_directory(copy.path())?;
            let metadata = directory.metadata()?;
            // SAFETY: geteuid only returns the effective identity.
            if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
                return Err(invalid("worker copy must be private and operator-owned"));
            }
        }
        if let Some(copy) = &workspace {
            command
                .arg(if copy.mode() == "workspace" {
                    "--bind"
                } else {
                    "--ro-bind"
                })
                .arg(copy.path())
                .arg("/work");
            if let Some(mask) = copy.git_mask() {
                command.arg("--ro-bind").arg(mask).arg("/work/.git");
            }
        } else {
            command.args(["--tmpfs", "/work"]);
        }
        command
            .args([
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
                "--chdir",
                "/work",
                "--ro-bind",
            ])
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
                "XDG_DATA_HOME",
                "/home/worker/data",
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
                "OXA_ENGINE_URL",
            ])
            .arg(format!("http://{}", config.inference.address))
            .args([
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
                "--offline",
                "--permission-mode",
            ])
            .arg(mode)
            .args(["--model"])
            .arg(&config.inference.model)
            .args([
                "--no-session",
                "--no-mcp",
                "--no-web-search",
                "--no-extensions",
                "--no-skills",
                "--no-prompt-templates",
                "--no-themes",
                "--no-context-files",
                "--no-approve",
                "--append-system-prompt",
            ])
            .arg(format!(
                "Orchestrator-assigned worker role:\n{}{}",
                config.role,
                workspace.as_ref().map(|w| w.role_context()).unwrap_or_default()
            ))
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("LANG", "C.UTF-8")
            .current_dir("/")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        if let Some(copy) = &workspace {
            command.arg("--tools").arg(copy.tools().join(","));
        } else {
            command.arg("--no-tools");
        }
        let mut child = command.spawn()?;
        let pipes = JsonPipes::new(
            child.stdin.take().expect("piped worker stdin"),
            child.stdout.take().expect("piped worker stdout"),
            child.stderr.take().expect("piped worker stderr"),
            config.output_bytes,
        );
        let pipes = match pipes {
            Ok(pipes) => pipes,
            Err(error) => return Err(startup_error(&mut child, error)),
        };
        Ok(Self {
            child,
            pipes,
            exit: None,
            deadline,
        })
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }
    /// Pipe state only; callers must separately confirm process stop.
    pub fn output_eof(&self) -> bool {
        self.pipes.output_eof()
    }
    pub fn output_bytes(&self) -> usize {
        self.pipes.output_bytes()
    }
    /// Query/reap only this owned child. Pipe EOF cannot establish this state.
    pub fn process_stopped(&mut self) -> io::Result<bool> {
        if self.exit.is_none() {
            self.exit = self.child.try_wait()?;
        }
        Ok(self.exit.is_some())
    }
    pub fn exit_status(&self) -> Option<ExitStatus> {
        self.exit
    }

    pub fn queue(&mut self, request: &Value) -> io::Result<()> {
        self.check_deadline()?;
        if self.process_stopped()? {
            return Err(invalid("worker already stopped"));
        }
        match self.pipes.queue(request) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.terminate()?;
                Err(error)
            }
        }
    }
    pub fn poll(&mut self) -> io::Result<Vec<Value>> {
        self.check_deadline()?;
        match self.pipes.poll() {
            Ok(records) => {
                self.process_stopped()?;
                Ok(records)
            }
            Err(error) => {
                self.terminate()?;
                Err(error)
            }
        }
    }
    fn check_deadline(&mut self) -> io::Result<()> {
        if self.exit.is_none() && Instant::now() >= self.deadline {
            self.terminate()?;
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "worker deadline exceeded",
            ));
        }
        Ok(())
    }
    /// Request abort, close input, then force cleanup if the child has not exited.
    /// Grace is capped at one second; force-cleanup observation is also bounded.
    pub fn stop(&mut self, grace: Duration) -> io::Result<()> {
        if self.process_stopped()? {
            return Ok(());
        }
        let _ = self
            .pipes
            .queue(&serde_json::json!({"id":"shutdown","type":"abort"}));
        let _ = self.pipes.poll();
        self.pipes.close_input();
        let end = Instant::now() + grace.min(Duration::from_secs(1));
        while Instant::now() < end {
            if self.process_stopped()? {
                return Ok(());
            }
            let _ = self.pipes.poll();
            std::thread::sleep(Duration::from_millis(2));
        }
        self.terminate()
    }
    fn terminate(&mut self) -> io::Result<()> {
        self.pipes.close_input();
        if self.exit.is_none() {
            self.exit = Some(kill_owned_group(&mut self.child)?);
        }
        Ok(())
    }
}
impl Drop for NativeWorker {
    fn drop(&mut self) {
        let _ = self.terminate();
    }
}
// Typed evidence for the narrow post-spawn pipe-setup error path. These PIDs
// are history only; callers must not adopt/signal them after this call.
#[derive(Debug)]
struct StartupFailure {
    pid: u32,
    stopped: bool,
}
impl std::fmt::Display for StartupFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("worker pipe setup failed; consult owned startup cleanup evidence")
    }
}
impl std::error::Error for StartupFailure {}
fn startup_error(child: &mut Child, error: io::Error) -> io::Error {
    let pid = child.id();
    let stopped = kill_owned_group(child).is_ok();
    io::Error::new(error.kind(), StartupFailure { pid, stopped })
}
pub(crate) fn startup_evidence(error: &io::Error) -> Option<(u32, bool)> {
    let evidence = error.get_ref()?.downcast_ref::<StartupFailure>()?;
    Some((evidence.pid, evidence.stopped))
}
fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
pub(crate) fn checked_program(path: &Path) -> io::Result<PathBuf> {
    let path = path.canonicalize()?;
    let metadata = File::open(&path)?.metadata()?;
    if !metadata.is_file() || metadata.mode() & 0o111 == 0 {
        return Err(invalid("worker program must be an executable file"));
    }
    Ok(path)
}
fn kill_owned_group(child: &mut Child) -> io::Result<ExitStatus> {
    // A successful try_wait would release PID identity. Never signal its group
    // after that point; the PID namespace owns descendant death on normal exit.
    if let Some(status) = child.try_wait()? {
        return Ok(status);
    }
    // SAFETY: this is our unreaped Child and its group was established at spawn.
    // No recorded/ambient PID is ever adopted. The retained child keeps its PID
    // reserved even if it exits between this query and the signal.
    let group_result = unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
    let group_error = (group_result != 0).then(io::Error::last_os_error);
    let direct = child.kill();
    if let Err(error) = direct {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        return Err(group_error.unwrap_or(error));
    }
    let end = Instant::now() + Duration::from_secs(1);
    while Instant::now() < end {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "owned worker cleanup unconfirmed",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxa_fake::TestEnv;
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;

    fn fixture(script: &str, wall_time: Duration) -> (TestEnv, Launch) {
        let env = TestEnv::new();
        let binary = env.write("runtime.py", &format!("#!/usr/bin/python3 -I\n{script}\n"));
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let launch = Launch {
            worker_binary: binary,
            bwrap_binary: PathBuf::from("/usr/bin/bwrap"),
            inference: VerifiedLocal::register("127.0.0.1:1".parse().unwrap(), "mock-model".into()).unwrap(),
            role: "synthetic tool-free worker".into(),
            wall_time,
            output_bytes: 128 * 1024,
        };
        (env, launch)
    }
    fn records(worker: &mut NativeWorker, count: usize) -> Vec<Value> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut values = Vec::new();
        while values.len() < count {
            values.extend(worker.poll().unwrap());
            assert!(
                Instant::now() < deadline,
                "sandbox fixture did not produce records"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        values
    }

    #[test]
    fn isolated_profile_clears_ambient_state_and_preserves_current_rpc_flags() {
        let (env, mut config) = fixture("", Duration::from_secs(5));
        let sentinel = env.write("source-only", "synthetic not delegated");
        let script = format!(
            r#"import os,sys,json
print(json.dumps({{'argv':sys.argv[1:],'home':os.environ.get('HOME'),'envKeys':sorted(os.environ),'work':os.listdir('/work'),'sourceVisible':os.path.exists({sentinel:?}),'usrReadonly':bool(os.statvfs('/usr').f_flag & os.ST_RDONLY),'pidns':os.readlink('/proc/self/ns/pid')}}),flush=True)
for line in sys.stdin:
 value=json.loads(line)
 if value.get('type')=='abort': break
 print(json.dumps({{'reply':value}}),flush=True)
"#
        );
        std::fs::write(&config.worker_binary, format!("#!/usr/bin/python3 -I\n{script}")).unwrap();
        config.role = "first line\nsecond line".into();
        let mut worker = NativeWorker::spawn(config).unwrap();
        let observed = records(&mut worker, 1).remove(0);
        assert_eq!(observed["home"], "/home/worker");
        assert_eq!(observed["work"], json!([]));
        assert_eq!(observed["sourceVisible"], false);
        assert_eq!(observed["usrReadonly"], true);
        let parent_ns = std::fs::read_link("/proc/self/ns/pid").unwrap();
        assert_ne!(observed["pidns"].as_str().unwrap(), parent_ns.to_str().unwrap());
        let keys = observed["envKeys"].as_array().unwrap();
        assert!(!keys
            .iter()
            .any(|v| v == "OXA_PROFILE" || v == "HTTP_PROXY" || v == "OXA_AUDIT_PARTIAL_IO_CHILD"));
        let argv: Vec<_> = observed["argv"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        for flag in [
            "--offline",
            "--no-tools",
            "--no-session",
            "--no-mcp",
            "--no-extensions",
            "--no-web-search",
            "--no-skills",
            "--no-context-files",
            "--no-approve",
        ] {
            assert!(argv.contains(&flag), "{flag}");
        }
        assert!(argv.windows(2).any(|a| a == ["--mode", "rpc"]));
        assert!(argv.windows(2).any(|a| a == ["--permission-mode", "plan"]));
        assert!(argv.windows(2).any(|a| a == ["--model", "mock-model"]));
        worker.queue(&json!({"id":"request","type":"get_state"})).unwrap();
        assert_eq!(
            records(&mut worker, 1),
            [json!({"reply":{"id":"request","type":"get_state"}})]
        );
        worker.stop(Duration::from_millis(100)).unwrap();
        assert!(worker.process_stopped().unwrap());
        assert!(worker.exit_status().unwrap().success());
        assert_eq!(
            std::fs::read_to_string(sentinel).unwrap(),
            "synthetic not delegated"
        );
    }

    #[test]
    fn deadline_protocol_and_admission_failure_confirm_cleanup() {
        let (_env, config) = fixture("import time; time.sleep(30)", Duration::from_millis(30));
        let mut worker = NativeWorker::spawn(config).unwrap();
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(worker.poll().unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert!(worker.process_stopped().unwrap());
        let (_env, config) = fixture(
            "print('malformed',flush=True)\nimport time; time.sleep(30)",
            Duration::from_secs(5),
        );
        let mut worker = NativeWorker::spawn(config).unwrap();
        let end = Instant::now() + Duration::from_secs(5);
        while worker.poll().is_ok() {
            assert!(Instant::now() < end);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(worker.process_stopped().unwrap());
        let (_env, config) = fixture("import time; time.sleep(30)", Duration::from_secs(5));
        let mut worker = NativeWorker::spawn(config).unwrap();
        assert!(worker.queue(&json!([])).is_err());
        assert!(worker.process_stopped().unwrap());
    }

    #[test]
    fn drop_kills_a_namespace_descendant_even_after_it_changes_process_group() {
        use std::io::Read;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let script = format!(
            r#"import os,socket,time,json
lease=socket.create_connection(('127.0.0.1',{port}))
r,w=os.pipe()
pid=os.fork()
if pid==0:
 os.setsid()
 os.write(w,b'x')
 time.sleep(30)
 os._exit(0)
os.close(w)
os.read(r,1)
print(json.dumps({{'descendantStarted':True}}),flush=True)
time.sleep(30)
"#
        );
        let (_env, config) = fixture(&script, Duration::from_secs(5));
        let mut worker = NativeWorker::spawn(config).unwrap();
        assert_eq!(records(&mut worker, 1), [json!({"descendantStarted":true})]);
        let (mut lease, _) = listener.accept().unwrap();
        lease.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        drop(worker);
        // This fixture never closes the inherited socket while alive. EOF
        // proves both namespace processes released it, including the setsid child.
        assert_eq!(lease.read(&mut [0u8; 1]).unwrap(), 0);
    }

    #[test]
    fn invalid_configuration_is_refused_before_launch() {
        for address in ["192.0.2.1:1", "127.0.0.1:0"] {
            assert!(VerifiedLocal::register(address.parse().unwrap(), "mock".into()).is_err());
        }
        for model in ["", "bad\nmodel", "--option"] {
            assert!(VerifiedLocal::register("127.0.0.1:1".parse().unwrap(), model.into()).is_err());
        }
        for kind in 0..5 {
            let (_env, mut config) = fixture("raise RuntimeError('must not launch')", Duration::from_secs(5));
            match kind {
                0 => config.wall_time = Duration::ZERO,
                1 => config.output_bytes = 0,
                2 => config.role.clear(),
                3 => config.role = "x".repeat(16 * 1024 + 1),
                _ => config.worker_binary = PathBuf::from("/nonexistent-a2a-worker"),
            }
            assert!(NativeWorker::spawn(config).is_err());
        }
    }

    #[test]
    fn closed_runtime_stdout_does_not_claim_the_wrapper_is_stopped() {
        use std::io::Read;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let script = format!("import os,socket,time; os.close(1); lease=socket.create_connection(('127.0.0.1',{port})); lease.sendall(b'c'); time.sleep(30)");
        let (_env, config) = fixture(&script, Duration::from_secs(5));
        let mut worker = NativeWorker::spawn(config).unwrap();
        let end = Instant::now() + Duration::from_secs(5);
        let mut lease = loop {
            match listener.accept() {
                Ok((lease, _)) => break lease,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < end);
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("owned fixture accept: {error}"),
            }
        };
        lease.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut ack = [0];
        lease.read_exact(&mut ack).unwrap();
        assert_eq!(ack, *b"c");
        assert!(worker.poll().unwrap().is_empty());
        assert!(!worker.process_stopped().unwrap());
        worker.stop(Duration::ZERO).unwrap();
        assert!(worker.process_stopped().unwrap());
        assert_eq!(lease.read(&mut ack).unwrap(), 0);
    }
    #[test]
    fn failed_pipe_setup_retains_distinct_spawn_and_reap_evidence() {
        let mut child = Command::new("/usr/bin/python3")
            .args(["-I", "-S", "-c", "import time; time.sleep(30)"])
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id();
        let failure = startup_error(&mut child, io::Error::other("synthetic pipe setup failure"));
        assert_eq!(startup_evidence(&failure), Some((pid, true)));
        assert!(child.try_wait().unwrap().is_some());
        assert_eq!(startup_evidence(&io::Error::other("pre-spawn refusal")), None);
    }
    #[test]
    fn workspace_mount_and_tool_mode_follow_the_immutable_grant() {
        use crate::workspace::{Grant, RelativePath, Snapshot};
        use std::fs;
        use std::os::unix::fs::DirBuilderExt;
        for editing in [false, true] {
            let (env, config) = fixture("", Duration::from_secs(5));
            let source = env.write("input.txt", "before\n");
            let snapshot = Arc::new(
                Snapshot::approve(env.root(), vec![RelativePath::new("input.txt".into()).unwrap()]).unwrap(),
            );
            let grant = Arc::new(if editing {
                Grant::editing(snapshot.clone(), vec![]).unwrap()
            } else {
                Grant::read_only(snapshot.clone())
            });
            let parent = env.root().join("private-copy");
            fs::DirBuilder::new().mode(0o700).create(&parent).unwrap();
            let copy = Arc::new(grant.materialize(&parent.join("worker")).unwrap());
            let script = format!(
                r#"#!/usr/bin/python3 -I
import os,sys,json
before=open('/work/input.txt').read()
try:
 open('/work/input.txt','w').write('after\n');changed=True
except OSError:
 changed=False
print(json.dumps({{'before':before,'changed':changed,'argv':sys.argv[1:],'sourceVisible':os.path.exists({source:?})}}),flush=True)
for line in sys.stdin:
 if json.loads(line).get('type')=='abort':break
"#
            );
            fs::write(&config.worker_binary, script).unwrap();
            let mut worker = NativeWorker::spawn_with_workspace(config, copy.clone()).unwrap();
            let observed = records(&mut worker, 1).remove(0);
            assert_eq!(observed["before"], "before\n");
            assert_eq!(observed["changed"], editing);
            assert_eq!(observed["sourceVisible"], false);
            let argv: Vec<_> = observed["argv"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
            assert!(argv
                .windows(2)
                .any(|a| a == ["--permission-mode", if editing { "workspace" } else { "plan" }]));
            assert!(argv
                .windows(2)
                .any(|a| a == ["--tools", if editing { "read,edit,write" } else { "read" }]));
            assert!(!argv.contains(&"--no-tools"));
            worker.stop(Duration::from_millis(100)).unwrap();
            assert!(worker.process_stopped().unwrap());
            assert_eq!(fs::read_to_string(source).unwrap(), "before\n");
            let review = copy.collect("worker").unwrap();
            review.verify(&snapshot, &[]).unwrap();
            assert_eq!(review.changes.len(), usize::from(editing));
        }
    }
}
