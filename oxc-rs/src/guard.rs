//! GUARD-2b: stop-only host/cgroup monitoring for the non-Flash engine.
use nix::{
    sys::signal::{kill, Signal},
    unistd::Pid,
};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{sync::watch, task::JoinHandle};

#[derive(Clone)]
struct Sample {
    available: u64,
    swap_free: u64,
    full: f64,
    swap_current: u64,
    swap_max: String,
    events: HashMap<String, u64>,
    pids_max: u64,
    host_oom: u64,
}
fn counter(text: &str, key: &str) -> Result<u64, String> {
    text.lines()
        .find_map(|line| {
            let mut fields = line.split_whitespace();
            (fields.next() == Some(key))
                .then(|| fields.next()?.parse().ok())
                .flatten()
        })
        .ok_or_else(|| "memory_metadata_unavailable".into())
}
fn counters(text: &str) -> Result<HashMap<String, u64>, String> {
    text.lines()
        .map(|line| {
            let mut row = line.split_whitespace();
            let name = row.next().ok_or("memory_metadata_unavailable")?;
            let value = row
                .next()
                .and_then(|v| v.parse().ok())
                .ok_or("memory_metadata_unavailable")?;
            Ok((name.to_owned(), value))
        })
        .collect()
}
pub fn available_memory() -> Option<u64> {
    counter(&fs::read_to_string("/proc/meminfo").ok()?, "MemAvailable:")
        .ok()?
        .checked_mul(1024)
}
fn cgroup() -> Result<PathBuf, String> {
    let text =
        fs::read_to_string("/proc/self/cgroup").map_err(|_| "memory_metadata_unavailable")?;
    let paths = text
        .lines()
        .filter_map(|line| line.strip_prefix("0::"))
        .collect::<Vec<_>>();
    if paths.len() != 1
        || !paths[0].starts_with('/')
        || paths[0].split('/').any(|part| part == "..")
    {
        return Err("memory_metadata_unavailable".into());
    }
    Ok(PathBuf::from("/sys/fs/cgroup").join(paths[0].trim_start_matches('/')))
}
fn sample() -> Result<Sample, String> {
    let group = cgroup()?;
    let read = |name: &str| {
        fs::read_to_string(group.join(name)).map_err(|_| "memory_metadata_unavailable".to_owned())
    };
    let mem = fs::read_to_string("/proc/meminfo").map_err(|_| "memory_metadata_unavailable")?;
    let psi =
        fs::read_to_string("/proc/pressure/memory").map_err(|_| "memory_metadata_unavailable")?;
    let field = |kind: &str, key: &str| -> Result<String, String> {
        psi.lines()
            .find_map(|line| {
                let mut fields = line.split_whitespace();
                if fields.next() != Some(kind) {
                    return None;
                }
                fields.find_map(|part| part.strip_prefix(&format!("{key}=")).map(str::to_owned))
            })
            .ok_or_else(|| "memory_metadata_unavailable".into())
    };
    let full: f64 = field("full", "avg10")?
        .parse()
        .map_err(|_| "memory_metadata_unavailable")?;
    let some: f64 = field("some", "avg10")?
        .parse()
        .map_err(|_| "memory_metadata_unavailable")?;
    if !full.is_finite()
        || !(0.0..=100.0).contains(&full)
        || !some.is_finite()
        || !(0.0..=100.0).contains(&some)
    {
        return Err("memory_metadata_unavailable".into());
    }
    for kind in ["full", "some"] {
        field(kind, "total")?
            .parse::<u64>()
            .map_err(|_| "memory_metadata_unavailable")?;
    }
    read("memory.high")?;
    read("memory.max")?;
    Ok(Sample {
        available: counter(&mem, "MemAvailable:")?
            .checked_mul(1024)
            .ok_or("memory_metadata_unavailable")?,
        swap_free: counter(&mem, "SwapFree:")?
            .checked_mul(1024)
            .ok_or("memory_metadata_unavailable")?,
        full,
        swap_current: read("memory.swap.current")?
            .trim()
            .parse()
            .map_err(|_| "memory_metadata_unavailable")?,
        swap_max: read("memory.swap.max")?.trim().to_owned(),
        events: counters(&read("memory.events")?)?,
        pids_max: counter(&read("pids.events")?, "max")?,
        host_oom: counter(
            &fs::read_to_string("/proc/vmstat").map_err(|_| "memory_metadata_unavailable")?,
            "oom_kill",
        )?,
    })
}
pub fn refusal(reason: &str) -> String {
    const UNIT: &str = "oxc-chat.service";
    if matches!(
        reason,
        "own_swap_limit_not_zero" | "memory_metadata_unavailable"
    ) {
        let swap = cgroup()
            .ok()
            .and_then(|p| fs::read_to_string(p.join("memory.swap.max")).ok());
        if let Some(value) = swap {
            if value.trim() != "0" {
                return format!("The model was not started: the controller service has no swap limit (memory.swap.max = {}); a model must never be swapped out, so no model is started. Fix: add MemorySwapMax=0 under [Service] of {UNIT} (systemctl --user edit oxc-chat), then systemctl --user restart oxc-chat; the installer writes it.",value.trim());
            }
        } else {
            return format!("The model was not started: the controller cannot read its own cgroup memory files. Fix: run the controller as the systemd user service {UNIT} (cgroup v2 required).");
        }
        if reason == "own_swap_limit_not_zero" {
            return format!("The model was not started: the controller service has no swap limit. Fix: add MemorySwapMax=0 under [Service] of {UNIT} and restart it.");
        }
        return format!("The model was not started: the controller cannot read its memory counters. Run it as {UNIT}.");
    }
    if reason == "own_swap_nonzero" {
        return format!(
            "The model was not started: part of the controller is already in swap. Restart {UNIT}."
        );
    }
    "Not enough free memory to load the model. Close some large apps and try again.".into()
}
struct State {
    baseline: Sample,
    last: Sample,
    loading: bool,
    deadline: Instant,
    pressure: Option<Instant>,
    loading_pressure: Option<Instant>,
    failure: Option<String>,
}
impl State {
    fn observe(&mut self, value: Sample) {
        let now = Instant::now();
        let mut conditions = Vec::new();
        if value.available.saturating_add(value.swap_free) < 2 * 1024 * 1024 * 1024 {
            conditions.push("host_memory_and_swap_exhausted");
        }
        if !self.loading && value.full > 20.0 {
            conditions.push("host_memory_pressure_full_sustained");
        }
        if value.available < 256 * 1024 * 1024 {
            conditions.push("host_available_emergency");
        }
        self.pressure = if conditions.is_empty() {
            None
        } else {
            Some(self.pressure.unwrap_or(now))
        };
        self.loading_pressure = if self.loading && value.full > 60.0 {
            Some(self.loading_pressure.unwrap_or(now))
        } else {
            None
        };
        let reason = if value.swap_max != "0" {
            Some("own_swap_limit_not_zero")
        } else if value.swap_current != 0 {
            Some("own_swap_nonzero")
        } else if ["oom", "oom_kill", "oom_group_kill"].iter().any(|key| {
            value.events.get(*key).unwrap_or(&0) > self.baseline.events.get(*key).unwrap_or(&0)
        }) {
            Some("cgroup_oom")
        } else if value.host_oom > self.baseline.host_oom {
            Some("host_oom_kill")
        } else if value.pids_max > self.baseline.pids_max {
            Some("cgroup_pids_limit")
        } else if self
            .pressure
            .is_some_and(|at| now.duration_since(at) >= Duration::from_secs(3))
        {
            conditions.first().copied()
        } else if self
            .loading_pressure
            .is_some_and(|at| now.duration_since(at) >= Duration::from_secs(5))
        {
            Some("host_memory_pressure_full_loading_sustained")
        } else if self.loading && now >= self.deadline {
            Some("model_loading_timeout")
        } else {
            None
        };
        if self.failure.is_none() {
            self.failure = reason.map(str::to_owned);
        }
        self.last = value;
    }
}
pub struct Guard {
    state: Arc<Mutex<State>>,
    stop: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
}
impl Guard {
    pub fn prepare(seconds: f64) -> Result<Self, String> {
        let initial = sample()?;
        let mut state = State {
            baseline: initial.clone(),
            last: initial.clone(),
            loading: true,
            deadline: Instant::now() + Duration::from_secs_f64(seconds.min(300.0)),
            pressure: None,
            loading_pressure: None,
            failure: None,
        };
        state.observe(initial);
        if let Some(reason) = &state.failure {
            return Err(reason.clone());
        }
        let (stop, _) = watch::channel(false);
        Ok(Self {
            state: Arc::new(Mutex::new(state)),
            stop,
            task: None,
        })
    }
    pub fn start(&mut self, pid: u32, owned: Arc<Mutex<bool>>, seconds: f64) {
        let state = self.state.clone();
        state.lock().unwrap_or_else(|e| e.into_inner()).deadline =
            Instant::now() + Duration::from_secs_f64(seconds.min(300.0));
        let mut stop = self.stop.subscribe();
        self.task = Some(tokio::spawn(async move {
            let signal = |signal| {
                let owned = owned.lock().unwrap_or_else(|e| e.into_inner());
                if *owned {
                    let _ = kill(Pid::from_raw(pid as i32), signal);
                }
            };
            loop {
                if *stop.borrow() {
                    return;
                }
                let failure = {
                    let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                    match sample() {
                        Ok(value) => state.observe(value),
                        Err(reason) => {
                            if state.failure.is_none() {
                                state.failure = Some(reason);
                            }
                        }
                    }
                    state.failure.is_some()
                };
                if failure {
                    signal(Signal::SIGTERM);
                    tokio::select! {_=stop.changed()=>{},_=tokio::time::sleep(Duration::from_secs(3))=>signal(Signal::SIGKILL)}
                    return;
                }
                tokio::select! {_=stop.changed()=>return,_=tokio::time::sleep(Duration::from_millis(250))=>{}}
            }
        }));
    }
    pub fn mark_ready(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if Instant::now() >= state.deadline && state.failure.is_none() {
            state.failure = Some("model_loading_timeout".into());
        }
        state.loading = false;
        state.loading_pressure = None;
    }
    pub fn failure(&self) -> Option<String> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .failure
            .clone()
    }
    pub fn metadata(&self) -> Value {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        json!({"guard_version":"GUARD-2","guard_revision":"GUARD-2b","reason":state.failure,"available_bytes":state.last.available,"psi_full_avg10":state.last.full,"loading":state.loading})
    }
    pub async fn stop(&mut self) {
        self.stop.send_replace(true);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        self.stop.send_replace(true);
    }
}
