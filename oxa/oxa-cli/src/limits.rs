//! Cooperative headless termination: signal callbacks only set an atomic flag.
use oxa_core::{Handle, SubagentHooks, WrapUpReason};
use oxa_proto::{codes, OxaError};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Who receives SIGTERM for this run.
#[derive(Clone, Copy)]
pub(crate) enum SigtermOwner {
    /// The run registers the handler itself (headless, RPC).
    Signal,
    /// Interactive: the terminal's handler sets this flag on the first SIGTERM and the run's
    /// watcher polls it, so the second SIGTERM still restores the terminal and kills.
    Terminal(&'static AtomicBool),
}

/// The invocation's limits, SIGINT/SIGTERM handling and watcher. It belongs to the process,
/// not to one session: `attach` hands it to every session the run opens (resume picker,
/// `/new`, `/resume`, `/fork`, `/clone`, extension swaps), and the new session inherits the
/// remaining token budget and any wrap-up already requested. The deadline lives in the
/// watcher, and the subagent hooks (shared by all sessions) carry it to children.
pub(crate) struct Run {
    stop: Arc<(Mutex<bool>, Condvar)>,
    worker: Option<JoinHandle<()>>,
    signals: Vec<signal_hook::SigId>,
    current: Arc<Mutex<Handle>>,
    subagent: Option<Arc<SubagentHooks>>,
}

impl Run {
    /// Watch `handle` from now on. It takes over the previous session's token tally, budget
    /// and wrap-up request, so a limit survives a session swap.
    pub fn attach(&self, handle: Handle) {
        let mut current = self.current.lock().unwrap_or_else(|p| p.into_inner());
        handle.inherit_run(&current);
        *current = handle;
    }

    pub fn start(
        handle: Handle,
        max_time: Option<Duration>,
        max_tokens: Option<u64>,
        supervised_child: bool,
        subagent: Option<Arc<SubagentHooks>>,
        sigterm: SigtermOwner,
    ) -> Result<Self, OxaError> {
        handle.set_token_budget(max_tokens);
        let current = Arc::new(Mutex::new(handle));
        // The parent's abort and kernel parent-death signal both use SIGTERM. A child
        // must exit on that signal, while the root owns cooperative run termination.
        let started = Instant::now();
        let deadline = max_time
            .map(|duration| {
                started
                    .checked_add(duration)
                    .ok_or_else(|| OxaError::new(codes::USAGE, "--max-time is too large"))
            })
            .transpose()?;
        if let Some(subagent) = &subagent {
            subagent.set_deadline(deadline);
        }
        let watched = current.clone();
        let terminated = Arc::new(AtomicBool::new(false));
        // Register escalation first: the first SIGTERM sets the flag below; any
        // subsequent signal takes its default action even if the watcher is blocked.
        // A deadline also arms escalation, so SIGTERM can stop a stuck final turn.
        let interrupted = Arc::new(AtomicBool::new(false));
        // As for SIGTERM, the first SIGINT arms its own default-action escalation.
        let interrupt_escalate =
            signal_hook::flag::register_conditional_default(signal_hook::consts::SIGINT, interrupted.clone())
                .map_err(|error| OxaError::io("cannot install SIGINT escalation", &error))?;
        let interrupt_signal = signal_hook::flag::register(signal_hook::consts::SIGINT, interrupted.clone())
            .map_err(|error| {
                signal_hook::low_level::unregister(interrupt_escalate);
                OxaError::io("cannot install the SIGINT handler", &error)
            })?;
        let mut signals = vec![interrupt_signal, interrupt_escalate];
        let terminal_flag = match sigterm {
            SigtermOwner::Terminal(flag) => Some(flag),
            SigtermOwner::Signal => None,
        };
        if !supervised_child && terminal_flag.is_none() {
            let escalate = signal_hook::flag::register_conditional_default(
                signal_hook::consts::SIGTERM,
                terminated.clone(),
            )
            .map_err(|error| {
                for &signal in &signals {
                    signal_hook::low_level::unregister(signal);
                }
                OxaError::io("cannot install SIGTERM escalation", &error)
            })?;
            let signal = match signal_hook::flag::register(signal_hook::consts::SIGTERM, terminated.clone()) {
                Ok(signal) => signal,
                Err(error) => {
                    signal_hook::low_level::unregister(escalate);
                    for &signal in &signals {
                        signal_hook::low_level::unregister(signal);
                    }
                    return Err(OxaError::io("cannot install the SIGTERM handler", &error));
                }
            };
            signals.extend([signal, escalate]);
        }
        let stop = Arc::new((Mutex::new(false), Condvar::new()));
        let watch_stop = stop.clone();
        let worker = match thread::Builder::new()
            .name("oxa-wrap-up".to_string())
            .spawn(move || {
                let (lock, wake) = &*watch_stop;
                let mut stopped = lock.lock().unwrap_or_else(|p| p.into_inner());
                let mut requested = false;
                while !*stopped {
                    let handle = watched.lock().unwrap_or_else(|p| p.into_inner()).clone();
                    if interrupted.load(Ordering::SeqCst) {
                        handle.interrupt();
                        return;
                    }
                    if terminal_flag.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
                        terminated.store(true, Ordering::SeqCst);
                    }
                    let reason = if requested {
                        None
                    } else if terminated.load(Ordering::SeqCst) {
                        Some(WrapUpReason::Signal)
                    } else if deadline.is_some_and(|at| Instant::now() >= at) {
                        Some(WrapUpReason::TimeLimit)
                    } else {
                        handle.wrap_up_reason()
                    };
                    if let Some(reason) = reason {
                        terminated.store(true, Ordering::SeqCst);
                        handle.request_wrap_up(reason);
                        requested = true;
                    }
                    // Keep watching SIGINT during a SIGTERM/deadline final turn too.
                    let delay = deadline
                        .filter(|_| !requested)
                        .map(|at| at.saturating_duration_since(Instant::now()))
                        .unwrap_or(Duration::from_millis(20))
                        .min(Duration::from_millis(20));
                    stopped = wake
                        .wait_timeout(stopped, delay)
                        .unwrap_or_else(|p| p.into_inner())
                        .0;
                }
            }) {
            Ok(worker) => worker,
            Err(error) => {
                for signal in signals {
                    signal_hook::low_level::unregister(signal);
                }
                return Err(OxaError::io("cannot start the wrap-up watcher", &error));
            }
        };
        Ok(Self {
            stop,
            worker: Some(worker),
            signals,
            current,
            subagent,
        })
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        let (lock, wake) = &*self.stop;
        *lock.lock().unwrap_or_else(|p| p.into_inner()) = true;
        wake.notify_all();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        for signal in self.signals.drain(..) {
            signal_hook::low_level::unregister(signal);
        }
        if let Some(subagent) = &self.subagent {
            subagent.set_deadline(None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, Write};
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Command, Stdio};

    #[test]
    fn second_sigint_terminates_a_stuck_cooperative_watcher() {
        const CHILD: &str = "OXA_SYNTHETIC_SIGINT_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let env = oxa_fake::TestEnv::new();
            let session = oxa_core::Session::open(
                oxa_core::Deps {
                    engine: Arc::new(oxa_fake::ScriptedEngine::new(Vec::new())),
                    store: Arc::new(oxa_session::MemStore::default()),
                    tools: Vec::new(),
                    hooks: Vec::new(),
                    policy: Box::new(oxa_perm::ModePolicy::new()),
                    sandbox: Arc::new(oxa_sandbox::NoSandbox),
                    settings: oxa_config::Settings::default(),
                    dirs: oxa_config::Dirs::under(env.root()),
                    cwd: env.work(),
                    selection: oxa_core::ToolSelection::default(),
                    resources: oxa_resources::Resources::default(),
                    model: None,
                    thinking: None,
                    mode: None,
                    session_id: None,
                    jobs: oxa_core::fresh_jobs(),
                },
                oxa_core::Open::New,
            )
            .unwrap();
            let run = Run::start(session.handle(), None, None, false, None, SigtermOwner::Signal).unwrap();
            // Deliberately prevent the cooperative watcher from making progress.
            let _stuck = run.stop.0.lock().unwrap();
            let delivered = Arc::new(AtomicBool::new(false));
            let _probe = signal_hook::flag::register(signal_hook::consts::SIGINT, delivered.clone()).unwrap();
            println!("synthetic-ready");
            std::io::stdout().flush().unwrap();
            while !delivered.load(Ordering::SeqCst) {
                thread::yield_now();
            }
            println!("synthetic-first-delivered");
            std::io::stdout().flush().unwrap();
            loop {
                thread::park();
            }
        }
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "limits::tests::second_sigint_terminates_a_stuck_cooperative_watcher",
                "--nocapture",
            ])
            .env_clear()
            .env(CHILD, "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let (send, receive) = std::sync::mpsc::channel();
        let stdout = child.stdout.take().unwrap();
        thread::spawn(move || {
            for line in std::io::BufReader::new(stdout).lines().map_while(Result::ok) {
                if (line.contains("synthetic-ready") || line.contains("synthetic-first-delivered"))
                    && send.send(line).is_err()
                {
                    break;
                }
            }
        });
        let result = (|| -> Result<(), &'static str> {
            let ready = receive
                .recv_timeout(Duration::from_secs(10))
                .map_err(|_| "child not ready")?;
            if !ready.contains("synthetic-ready") {
                return Err("wrong readiness phase");
            }
            let signal = || {
                Command::new("kill")
                    .args(["-INT", &child.id().to_string()])
                    .status()
                    .unwrap()
                    .success()
            };
            if !signal() {
                return Err("first signal failed");
            }
            let first = receive
                .recv_timeout(Duration::from_secs(5))
                .map_err(|_| "first signal not delivered")?;
            if !first.contains("synthetic-first-delivered") {
                return Err("wrong first signal phase");
            }
            if !signal() {
                return Err("second signal failed");
            }
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                if let Some(status) = child.try_wait().unwrap() {
                    return if status.signal() == Some(2) {
                        Ok(())
                    } else {
                        Err("second SIGINT did not use default action")
                    };
                }
                if Instant::now() >= deadline {
                    return Err("second SIGINT swallowed while cooperative watcher stuck");
                }
                thread::sleep(Duration::from_millis(5));
            }
        })();
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(result, Ok(()));
    }
}
