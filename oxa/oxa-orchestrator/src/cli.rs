//! Opt-in command implementation; the existing binary has only a gated shim.
//! Parsing/help never starts a listener, probes an engine or reads agent state.
use crate::{
    a2a_run::*,
    rpc_agent::{RpcAgent, RpcAgentConfig},
    workspace::{Grant, RelativePath, Snapshot},
    LocalInference, PermissionMode, WorkerBudget,
};
use oxa_a2a::{
    loopback::Limits,
    server::{Server, ServerLimits},
};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::DirBuilder;
use std::io::{self, Write};
use std::net::SocketAddr;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering},
    mpsc, Arc, Mutex, MutexGuard,
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub const HELP: &str = r#"Usage: oxa orchestrate [options] <goal>

Run local offline workers over A2A; tools are off unless files are approved. Print worker status to stderr
and a final JSON report to stdout. Ctrl-C cancels and cleans up owned workers.

Required:
  --engine <address:port>       Explicit literal loopback inference endpoint
  --model <id>                  Exact model ID
  --local-only                  Assert this endpoint cannot forward to cloud
  --directory <path>            New private audit directory; parent must exist

Options:
  --workers <n>                 Number of workers, 1..16 (default: 3)
  --parallel <n>                Concurrent workers (default: min(workers, 4))
  --role <text>                 Repeat per worker, or repeat one role for all
  --permission-mode <mode>      plan (default), ask or workspace
  --worker-timeout <seconds>    Per-worker cap, 1..600 (default: 30)
  --timeout <seconds>           Run cap, 1..86400 (default: enough for all batches)
  --response-bytes <n>          Per-worker incoming-wire cap (default: 1048576)
  --reported-tokens <n>         Observed usage cap (default: 200000)
  --source <path>               Root of explicitly approved source files
  --file <relative-path>        Approve this file's contents; repeat up to 32
  --edit                       Allow private-copy edits; requires workspace mode
  --allow-new <relative-path>   Allow a new review path; repeat, requires --edit
  --offline                    Explicit offline mode (always enabled here)
  --                           Treat remaining words as goal text
  --help                       Show this help without starting workers

Only explicitly verified local inference is supported. Results are proposals;
source files are never changed or merged. Private edits return review data. Token caps act on reported usage, not money.
"#;

#[derive(Debug)]
pub struct Options {
    engine: SocketAddr,
    model: String,
    directory: PathBuf,
    roles: Vec<String>,
    goal: String,
    parallel: usize,
    mode: PermissionMode,
    worker_timeout: Duration,
    run_timeout: Duration,
    response_bytes: usize,
    reported_tokens: u64,
    source: Option<PathBuf>,
    files: Vec<RelativePath>,
    new_files: Vec<RelativePath>,
    editing: bool,
}

pub fn parse(args: &[String]) -> Result<Option<Options>, &'static str> {
    let mut source = None;
    let mut files = Vec::new();
    let mut new_files = Vec::new();
    let mut editing = false;
    let mut engine = None;
    let mut model = None;
    let mut directory = None;
    let mut roles = Vec::new();
    let mut workers = None;
    let mut parallel = None;
    let mut mode = PermissionMode::Plan;
    let mut worker_secs = 30u64;
    let mut run_secs = None;
    let mut bytes = 1024 * 1024usize;
    let mut tokens = 200_000u64;
    let mut local_only = false;
    let mut goal = Vec::new();
    let mut options = true;
    let mut seen = BTreeSet::new();
    let mut i = 0;
    while i < args.len() {
        let flag = args[i].as_str();
        i += 1;
        if options && flag == "--" {
            options = false;
            continue;
        }
        if !options || !flag.starts_with('-') {
            goal.push(flag.to_owned());
            continue;
        }
        if flag == "--help" || flag == "-h" {
            return Ok(None);
        }
        if flag == "--offline" {
            continue;
        }
        if flag == "--local-only" {
            local_only = true;
            continue;
        }
        if !matches!(flag, "--role" | "--file" | "--allow-new") && !seen.insert(flag) {
            return Err("an option was specified more than once");
        }
        if flag == "--edit" {
            editing = true;
            continue;
        }
        if !matches!(
            flag,
            "--source"
                | "--file"
                | "--allow-new"
                | "--engine"
                | "--model"
                | "--directory"
                | "--workers"
                | "--parallel"
                | "--role"
                | "--permission-mode"
                | "--worker-timeout"
                | "--timeout"
                | "--response-bytes"
                | "--reported-tokens"
        ) {
            return Err("unknown orchestrate option; use --help");
        }
        let value = args.get(i).ok_or("an option needs a value")?;
        i += 1;
        if value.is_empty() {
            return Err("option values must not be empty");
        }
        match flag {
            "--source" => source = Some(PathBuf::from(value)),
            "--file" | "--allow-new" => {
                let path =
                    RelativePath::new(value.clone()).map_err(|_| "invalid or protected source path")?;
                let paths = if flag == "--file" {
                    &mut files
                } else {
                    &mut new_files
                };
                if paths.contains(&path) || paths.len() >= 32 {
                    return Err("duplicate or excessive source paths");
                }
                paths.push(path);
            }
            "--engine" => {
                engine = Some(
                    value
                        .parse::<SocketAddr>()
                        .map_err(|_| "engine must be a literal loopback address:port")?,
                )
            }
            "--model" => model = Some(value.clone()),
            "--directory" => directory = Some(PathBuf::from(value)),
            "--role" => roles.push(value.clone()),
            "--workers" => workers = Some(number(value, 1, 16)? as usize),
            "--parallel" => parallel = Some(number(value, 1, 16)? as usize),
            "--permission-mode" => {
                mode = match value.as_str() {
                    "plan" => PermissionMode::Plan,
                    "ask" => PermissionMode::Ask,
                    "workspace" => PermissionMode::Workspace,
                    _ => return Err("permission mode must be plan, ask or workspace"),
                }
            }
            "--worker-timeout" => worker_secs = number(value, 1, 600)?,
            "--timeout" => run_secs = Some(number(value, 1, 86400)?),
            "--response-bytes" => bytes = number(value, 1024, 16 * 1024 * 1024)? as usize,
            "--reported-tokens" => tokens = number(value, 1, 100_000_000)?,
            _ => unreachable!(),
        }
    }
    if !local_only {
        return Err("--local-only is required: loopback alone does not exclude cloud routing");
    }
    let engine = engine.ok_or("--engine is required")?;
    let model = model.ok_or("--model is required")?;
    LocalInference::register_verified_local(engine, model.clone())
        .map_err(|_| "invalid local engine address or model ID")?;
    let directory = directory.ok_or("--directory is required")?;
    let goal = goal.join(" ");
    if goal.trim().is_empty() || goal.len() > 16 * 1024 {
        return Err("provide a nonempty goal of at most 16384 bytes");
    }
    if roles.len() > 16 || roles.iter().any(|r| r.trim().is_empty() || r.len() > 4096) {
        return Err("provide at most 16 nonempty roles, each at most 4096 bytes");
    }
    if source.is_some() == files.is_empty() || files.len() + new_files.len() > 32 {
        return Err("--source requires 1..32 explicit --file paths; total eligible paths must not exceed 32");
    }
    if editing && (source.is_none() || mode != PermissionMode::Workspace) {
        return Err("--edit requires approved files and --permission-mode workspace");
    }
    if !new_files.is_empty() && !editing {
        return Err("--allow-new requires --edit");
    }
    let workers = workers.unwrap_or(if roles.is_empty() { 3 } else { roles.len() });
    if roles.is_empty() {
        roles = (0..workers)
            .map(|n| ["planner", "analyst", "reviewer"][n % 3].to_owned())
            .collect();
    } else if roles.len() == 1 {
        roles.resize(workers, roles[0].clone());
    } else if roles.len() != workers {
        return Err("provide one role or exactly one role per worker");
    }
    // The native bridge limits serialized task parts, not just visible bytes.
    for role in &roles {
        let text = format!("Assigned role:\n{role}\nDelegated goal:\n{goal}");
        if serde_json::to_vec(&serde_json::json!([{"kind":"text","text":text}]))
            .map_err(|_| "invalid task data")?
            .len()
            > 32 * 1024 - 1024
        {
            return Err("serialized role and goal exceed the task limit");
        }
    }
    let parallel = parallel.unwrap_or(workers.min(4));
    if parallel > workers {
        return Err("parallelism cannot exceed the number of workers");
    }
    let run_secs = run_secs.unwrap_or(worker_secs * workers.div_ceil(parallel) as u64 + 5);
    Ok(Some(Options {
        engine,
        model,
        directory,
        roles,
        goal,
        parallel,
        mode,
        worker_timeout: Duration::from_secs(worker_secs),
        run_timeout: Duration::from_secs(run_secs),
        response_bytes: bytes,
        reported_tokens: tokens,
        source,
        files,
        new_files,
        editing,
    }))
}

fn number(text: &str, min: u64, max: u64) -> Result<u64, &'static str> {
    let value = text
        .parse::<u64>()
        .map_err(|_| "expected an integer option value")?;
    if !(min..=max).contains(&value) {
        return Err("numeric option value is outside its supported range");
    }
    Ok(value)
}

/// Intended for the gated binary shim. Returns None for every other command.
/// It does not discover user/project configuration or inherit model credentials.
pub fn maybe_run(args: &[String]) -> Option<i32> {
    if args.first().map(String::as_str) != Some("orchestrate") {
        return None;
    }
    let options = match parse(&args[1..]) {
        Ok(None) => {
            let _ = io::stdout().write_all(HELP.as_bytes());
            return Some(0);
        }
        Ok(Some(options)) => options,
        Err(error) => {
            let _ = writeln!(io::stderr(), "oxa orchestrate: {error}");
            return Some(2);
        }
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let signals = match Signals::install(cancel.clone()) {
        Ok(guard) => guard,
        Err(_) => {
            let _ = writeln!(
                io::stderr(),
                "oxa orchestrate: cannot install cancellation handlers"
            );
            return Some(1);
        }
    };
    let mut stderr = Interruptible {
        inner: Descriptor(libc::STDERR_FILENO),
        epoch: 0,
    };
    let mut previous = BTreeMap::new();
    let result = execute(options, cancel.clone(), |status| {
        if !changed_status(&mut previous, &status) {
            return;
        }
        let line = format!(
            "{}: {} ({} response bytes)\n",
            status.worker, status.state, status.response_bytes
        );
        if write_all(&mut stderr, line.as_bytes()).is_err() {
            cancel.store(true, Ordering::Release);
        }
    });
    let code = match result {
        Ok(report) => {
            let mut stdout = Interruptible {
                inner: Descriptor(libc::STDOUT_FILENO),
                epoch: SIGNAL_COUNT.load(Ordering::Acquire),
            };
            let serialized = {
                let mut serializer = serde_json::Serializer::with_formatter(&mut stdout, TerminalJson);
                report.serialize(&mut serializer)
            };
            let wrote = serialized.and_then(|_| stdout.write_all(b"\n").map_err(serde_json::Error::io));
            if wrote.is_err() {
                1
            } else if report.succeeded() {
                0
            } else {
                1
            }
        }
        Err(error) => {
            let _ = writeln!(io::stderr(), "oxa orchestrate: {error}");
            1
        }
    };
    let signal = SIGNAL.load(Ordering::Acquire);
    drop(signals);
    Some(if signal != 0 { 128 + signal } else { code })
}

/// Explicit CLI plan execution, also exercised by model-free hosted tests.
/// The observer must be trusted and nonblocking, as for run_a2a.
pub fn execute(
    options: Options,
    cancel: Arc<AtomicBool>,
    observe: impl FnMut(A2AStatus),
) -> io::Result<A2ARunReport> {
    let parent = options
        .directory
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."))
        .canonicalize()?;
    let directory = parent.join(
        options
            .directory
            .file_name()
            .ok_or_else(|| io::Error::other("missing output directory name"))?,
    );
    let binary = std::env::current_exe()?;
    // No runtime work before full argument validation, including route checks.
    let inference = LocalInference::register_verified_local(options.engine, options.model.clone())?;
    let workspace = options
        .source
        .as_ref()
        .map(|root| {
            let snapshot = Arc::new(Snapshot::approve(root, options.files)?);
            let grant = if options.editing {
                Grant::editing(snapshot, options.new_files)?
            } else {
                Grant::read_only(snapshot)
            };
            Ok::<_, io::Error>(Arc::new(grant))
        })
        .transpose()?;
    DirBuilder::new()
        .mode(0o700)
        .create(&directory)
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                if error.kind() == io::ErrorKind::AlreadyExists {
                    "output directory already exists; choose a new directory"
                } else {
                    "cannot create the private output directory"
                },
            )
        })?;
    let agent=Arc::new(RpcAgent::new(RpcAgentConfig {workspace,worker_binary:binary,bwrap_binary:"/usr/bin/bwrap".into(),directory:directory.join("agents"),
        role:"Complete the delegated task under its assigned role. Return proposals and evidence; do not change permissions.".into(),inference,mode:options.mode,
        budget:WorkerBudget {wall_time:options.worker_timeout,output_bytes:1024*1024,reported_tokens:options.reported_tokens},max_live:options.parallel,max_tasks:options.roles.len(),journal_bytes:1024*1024})?);
    let server = Server::start(
        agent,
        ServerLimits {
            connections: (options.parallel * 2 + 2).min(32),
            ..ServerLimits::default()
        },
    )?;
    let mut plan = Vec::new();
    for (index, role) in options.roles.into_iter().enumerate() {
        plan.push(A2AWorkerSpec {
            id: format!("worker-{}", index + 1),
            role,
            task: options.goal.clone(),
            agent: LocalAgent::register_verified_local(server.client(Limits::default())?),
            wall_time: options.worker_timeout,
            response_bytes: options.response_bytes,
            reported_tokens: Some(options.reported_tokens),
        });
    }
    let (_control, controls) = mpsc::sync_channel(1);
    let report = run_a2a(
        A2ARunSettings {
            id: "orchestrate".into(),
            directory: directory.join("run"),
            parallelism: options.parallel,
            wall_time: options.run_timeout,
            response_reservation: options.response_bytes * plan.len(),
            journal_bytes: 1024 * 1024,
            request_timeout: Duration::from_millis(250),
            poll_interval: Duration::from_millis(20),
            cancel_grace: Duration::from_secs(1),
        },
        plan,
        controls,
        cancel,
        observe,
    );
    // Owned servers/supervisors are stopped before writing a potentially large
    // stdout report, including when a downstream consumer closes its pipe.
    drop(server);
    report
}

static SIGNAL: AtomicI32 = AtomicI32::new(0);
static SIGNAL_COUNT: AtomicUsize = AtomicUsize::new(0);
static SIGNAL_OWNER: Mutex<()> = Mutex::new(());
extern "C" fn on_signal(signal: libc::c_int) {
    SIGNAL.store(signal, Ordering::Relaxed);
    SIGNAL_COUNT.fetch_add(1, Ordering::Relaxed);
}

struct Signals {
    previous: [(libc::c_int, libc::sigaction); 2],
    stop: Arc<AtomicBool>,
    watcher: Option<JoinHandle<()>>,
    _owner: MutexGuard<'static, ()>,
}
impl Signals {
    fn install(cancel: Arc<AtomicBool>) -> io::Result<Self> {
        let owner = SIGNAL_OWNER
            .try_lock()
            .map_err(|_| io::Error::other("orchestration signal handlers already owned"))?;
        SIGNAL.store(0, Ordering::Release);
        SIGNAL_COUNT.store(0, Ordering::Release);
        // SAFETY: sigaction is a C POD; the handler only writes lock-free
        // atomics and uses no allocation, locks or I/O. No SA_RESTART, so a
        // second interrupt can also interrupt final-report output.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = on_signal as *const () as usize;
        unsafe {
            libc::sigemptyset(&mut action.sa_mask);
        }
        let mut old_int = std::mem::MaybeUninit::uninit();
        let mut old_term = std::mem::MaybeUninit::uninit();
        if unsafe { libc::sigaction(libc::SIGINT, &action, old_int.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let old_int = unsafe { old_int.assume_init() };
        if unsafe { libc::sigaction(libc::SIGTERM, &action, old_term.as_mut_ptr()) } != 0 {
            unsafe {
                libc::sigaction(libc::SIGINT, &old_int, std::ptr::null_mut());
            }
            return Err(io::Error::last_os_error());
        }
        let previous = [
            (libc::SIGINT, old_int),
            (libc::SIGTERM, unsafe { old_term.assume_init() }),
        ];
        let stop = Arc::new(AtomicBool::new(false));
        let watched = stop.clone();
        let watcher = thread::Builder::new()
            .name("oxa-orchestrate-cancel".into())
            .spawn(move || {
                while !watched.load(Ordering::Acquire) {
                    if SIGNAL.load(Ordering::Acquire) != 0 {
                        cancel.store(true, Ordering::Release);
                    }
                    thread::sleep(Duration::from_millis(5));
                }
            });
        match watcher {
            Ok(watcher) => Ok(Self {
                previous,
                stop,
                watcher: Some(watcher),
                _owner: owner,
            }),
            Err(error) => {
                for (signal, action) in &previous {
                    unsafe {
                        libc::sigaction(*signal, action, std::ptr::null_mut());
                    }
                }
                Err(error)
            }
        }
    }
}
impl Drop for Signals {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.watcher.take() {
            let _ = thread.join();
        }
        for (signal, action) in &self.previous {
            unsafe {
                libc::sigaction(*signal, action, std::ptr::null_mut());
            }
        }
    }
}

struct Interruptible<W> {
    inner: W,
    epoch: usize,
}

// Raw writes let the outer cancellation-aware writer handle EINTR itself;
// stdio buffering may otherwise restart an interrupted flush internally.
// Descriptor flags and the caller's terminal settings remain unchanged.
struct Descriptor(libc::c_int);
impl Write for Descriptor {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        // SAFETY: borrowed stdout/stderr descriptor and a live byte slice.
        let written = unsafe { libc::write(self.0, bytes.as_ptr().cast(), bytes.len()) };
        if written < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(written as usize)
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct TerminalJson;
impl serde_json::ser::Formatter for TerminalJson {
    fn write_string_fragment<W: Write + ?Sized>(&mut self, writer: &mut W, fragment: &str) -> io::Result<()> {
        let mut start = 0;
        for (index, ch) in fragment.char_indices() {
            if ch.is_control() {
                writer.write_all(&fragment.as_bytes()[start..index])?;
                write!(writer, "\\u{:04x}", ch as u32)?;
                start = index + ch.len_utf8();
            }
        }
        writer.write_all(&fragment.as_bytes()[start..])
    }
}
impl<W: Write> Write for Interruptible<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if SIGNAL_COUNT.load(Ordering::Acquire) > self.epoch {
            return Err(io::Error::other("orchestration interrupted"));
        }
        self.inner.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}
fn write_all(writer: &mut impl Write, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        match writer.write(bytes) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "output closed")),
            Ok(n) => bytes = &bytes[n..],
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Coalesce repeated status values for embedders that render a worker view.
pub fn changed_status(previous: &mut BTreeMap<String, String>, status: &A2AStatus) -> bool {
    if previous.get(&status.worker) == Some(&status.state) {
        false
    } else {
        previous.insert(status.worker.clone(), status.state.clone());
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn terminal_safe_json_preserves_control_characters_as_data() {
        let value = serde_json::json!({"text":"synthetic \u{001b}\u{007f}\u{009d} 😀"});
        let mut bytes = Vec::new();
        value
            .serialize(&mut serde_json::Serializer::with_formatter(
                &mut bytes,
                TerminalJson,
            ))
            .unwrap();
        assert!(!String::from_utf8_lossy(&bytes).chars().any(char::is_control));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
            value
        );
    }
    fn handler(signal: libc::c_int) -> usize {
        let mut action = std::mem::MaybeUninit::<libc::sigaction>::uninit();
        // SAFETY: query-only sigaction, with a valid output pointer.
        assert_eq!(
            unsafe { libc::sigaction(signal, std::ptr::null(), action.as_mut_ptr()) },
            0
        );
        unsafe { action.assume_init() }.sa_sigaction
    }
    #[test]
    fn cancellation_handlers_are_exclusive_and_restore_the_previous_handlers() {
        let before_int = handler(libc::SIGINT);
        let before_term = handler(libc::SIGTERM);
        let canceled = Arc::new(AtomicBool::new(false));
        let guard = Signals::install(canceled.clone()).unwrap();
        assert!(Signals::install(Arc::new(AtomicBool::new(false))).is_err());
        on_signal(libc::SIGTERM);
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !canceled.load(Ordering::Acquire) {
            assert!(std::time::Instant::now() < deadline);
            thread::sleep(Duration::from_millis(2));
        }
        drop(guard);
        assert_eq!(handler(libc::SIGINT), before_int);
        assert_eq!(handler(libc::SIGTERM), before_term);
        drop(Signals::install(Arc::new(AtomicBool::new(false))).unwrap());
    }
}
