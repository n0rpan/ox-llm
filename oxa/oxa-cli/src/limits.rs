//! Cooperative headless termination: signal callbacks only set an atomic flag.
use oxa_core::{Handle, WrapUpReason};
use oxa_proto::{codes, OxaError};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub(crate) struct LimitWatcher {
    stop: Arc<(Mutex<bool>, Condvar)>,
    worker: Option<JoinHandle<()>>,
    signals: Vec<signal_hook::SigId>,
}

impl LimitWatcher {
    pub fn start(
        handle: Handle,
        max_time: Option<Duration>,
        max_tokens: Option<u64>,
        supervised_child: bool,
    ) -> Result<Option<Self>, OxaError> {
        handle.set_token_budget(max_tokens);
        // The parent's abort and kernel parent-death signal both use SIGTERM. A child
        // must exit on that signal, while the root owns cooperative run termination.
        if supervised_child && max_time.is_none() {
            return Ok(None);
        }
        let deadline = max_time
            .map(|duration| {
                Instant::now()
                    .checked_add(duration)
                    .ok_or_else(|| OxaError::new(codes::USAGE, "--max-time is too large"))
            })
            .transpose()?;
        let terminated = Arc::new(AtomicBool::new(false));
        // Register escalation first: the first SIGTERM sets the flag below; any
        // subsequent signal takes its default action even if the watcher is blocked.
        // A deadline also arms escalation, so SIGTERM can stop a stuck final turn.
        let mut signals = Vec::new();
        if !supervised_child {
            let escalate = signal_hook::flag::register_conditional_default(
                signal_hook::consts::SIGTERM,
                terminated.clone(),
            )
            .map_err(|error| OxaError::io("cannot install SIGTERM escalation", &error))?;
            let signal = match signal_hook::flag::register(signal_hook::consts::SIGTERM, terminated.clone()) {
                Ok(signal) => signal,
                Err(error) => {
                    signal_hook::low_level::unregister(escalate);
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
                while !*stopped {
                    let reason = if terminated.load(Ordering::SeqCst) {
                        Some(WrapUpReason::Signal)
                    } else if deadline.is_some_and(|at| Instant::now() >= at) {
                        Some(WrapUpReason::TimeLimit)
                    } else {
                        handle.wrap_up_reason()
                    };
                    if let Some(reason) = reason {
                        terminated.store(true, Ordering::SeqCst);
                        handle.request_wrap_up(reason);
                        return;
                    }
                    let delay = deadline
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
        Ok(Some(Self {
            stop,
            worker: Some(worker),
            signals,
        }))
    }
}

impl Drop for LimitWatcher {
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
    }
}
