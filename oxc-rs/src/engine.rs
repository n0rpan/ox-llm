//! Ownership of the native engine, lease, per-launch credentials and admission.
use crate::{guard, models, paths};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use nix::{
    errno::Errno,
    libc,
    sys::signal::{kill, killpg, Signal},
    unistd::{Pid, Uid},
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::{
        atomic::{AtomicU8, AtomicUsize, Ordering},
        Arc, Mutex, RwLock,
    },
    time::{Duration, Instant},
};
use tokio::{
    io::AsyncReadExt,
    process::{Child, Command},
    sync::{watch, Mutex as AsyncMutex, Notify, OwnedSemaphorePermit, Semaphore},
    task::JoinHandle,
};

pub fn seconds(name: &str, default: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v > 0.0)
        .unwrap_or(default)
}
fn random<const N: usize>() -> Result<[u8; N], String> {
    let mut bytes = [0_u8; N];
    getrandom::fill(&mut bytes).map_err(|_| {
        "Local model cannot start: no private runtime directory for its access token."
    })?;
    Ok(bytes)
}
fn private_directory(path: &Path) -> Option<PathBuf> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return None,
    }
    let info = fs::symlink_metadata(path).ok()?;
    (info.is_dir() && info.uid() == Uid::current().as_raw() && info.mode() & 0o077 == 0)
        .then(|| path.to_owned())
}
struct Token {
    value: String,
    path: PathBuf,
    temporary: bool,
}
impl Token {
    fn new(port: u16) -> Result<Self, String> {
        let mut temporary = false;
        let directory = if let Some(explicit) =
            std::env::var_os("LOCAL_AI_RUNTIME_DIR").filter(|v| !v.is_empty())
        {
            private_directory(Path::new(&explicit))
        } else if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute() && p.is_dir())
        {
            private_directory(&runtime.join("local-ai"))
        } else {
            None
        };
        let directory = if let Some(directory) = directory {
            directory
        } else {
            if std::env::var_os("LOCAL_AI_RUNTIME_DIR").is_some_and(|v| !v.is_empty()) {
                return Err(
                    "Local model cannot start: no private runtime directory for its access token."
                        .into(),
                );
            }
            temporary = true;
            let suffix = URL_SAFE_NO_PAD.encode(random::<12>()?);
            private_directory(&std::env::temp_dir().join(format!("local-ai-{suffix}"))).ok_or(
                "Local model cannot start: no private runtime directory for its access token.",
            )?
        };
        let value = URL_SAFE_NO_PAD.encode(random::<32>()?);
        let suffix = random::<4>()?
            .iter()
            .map(|v| format!("{v:02x}"))
            .collect::<String>();
        let path = directory.join(format!("oxe-backend-{port}-{suffix}.key"));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)
            .map_err(|_| {
                "Local model cannot start: no private runtime directory for its access token."
            })?;
        if file
            .set_permissions(fs::Permissions::from_mode(0o600))
            .and_then(|_| file.write_all(value.as_bytes()))
            .is_err()
        {
            let _ = fs::remove_file(&path);
            return Err(
                "Local model cannot start: no private runtime directory for its access token."
                    .into(),
            );
        }
        Ok(Self {
            value,
            path,
            temporary,
        })
    }
}
impl Drop for Token {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        if self.temporary {
            if let Some(parent) = self.path.parent() {
                let _ = fs::remove_dir(parent);
            }
        }
    }
}
fn verify(profile: &Value) -> Result<(), String> {
    let error = |name: &str| {
        format!("The model could not start (ValueError: {name}). Details: journalctl --user -u local-ai-chat.")
    };
    let binary = profile["server_binary"]
        .as_str()
        .ok_or_else(|| error("oxe_absolute_path_required"))?;
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(binary)
        .map_err(|_| error("oxe_bounded_owned_binary_required"))?;
    let before = file
        .metadata()
        .map_err(|_| error("oxe_bounded_owned_binary_required"))?;
    if !before.is_file()
        || ![0, Uid::current().as_raw()].contains(&before.uid())
        || before.len() == 0
        || before.len() > 256 * 1024 * 1024
    {
        return Err(error("oxe_bounded_owned_binary_required"));
    }
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 65536];
    let mut bytes_read = 0_u64;
    loop {
        let n = file
            .read(&mut buffer)
            .map_err(|_| error("oxe_engine_identity_changed"))?;
        if n == 0 {
            break;
        }
        bytes_read += n as u64;
        if bytes_read > 256 * 1024 * 1024 {
            return Err(error("oxe_engine_identity_changed"));
        }
        hash.update(&buffer[..n]);
    }
    let after = file
        .metadata()
        .map_err(|_| error("oxe_engine_identity_changed"))?;
    let identity = |m: &fs::Metadata| {
        (
            m.dev(),
            m.ino(),
            m.len(),
            m.mtime(),
            m.mtime_nsec(),
            m.ctime(),
            m.ctime_nsec(),
        )
    };
    let actual = format!("{:x}", hash.finalize());
    if identity(&before) != identity(&after) || profile["server_sha256"] != actual {
        return Err(error("oxe_engine_identity_changed"));
    }
    Ok(())
}
struct Process {
    child: Child,
    owned: Arc<Mutex<bool>>,
}
impl Process {
    fn pid(&self) -> u32 {
        self.child.id().unwrap_or(0)
    }
    fn poll(&mut self) -> std::io::Result<Option<ExitStatus>> {
        let mut owned = self.owned.lock().unwrap_or_else(|e| e.into_inner());
        let status = self.child.try_wait()?;
        if status.is_some() {
            *owned = false;
        }
        Ok(status)
    }
    fn signal(&self, signal: Signal) {
        let owned = self.owned.lock().unwrap_or_else(|e| e.into_inner());
        if *owned {
            match killpg(Pid::from_raw(self.pid() as i32), signal) {
                Ok(()) => {}
                Err(Errno::ESRCH) => {
                    let _ = kill(Pid::from_raw(self.pid() as i32), signal);
                }
                Err(_) => {}
            }
        }
    }
    async fn wait(&mut self, seconds: f64) -> bool {
        let deadline = Instant::now() + Duration::from_secs_f64(seconds);
        loop {
            match self.poll() {
                Ok(Some(_)) => return true,
                Err(_) => return false,
                _ => {}
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}
struct Running {
    process: Process,
    _lease: File,
    token: Token,
    guard: guard::Guard,
    stderr: Option<JoinHandle<()>>,
    engine_ledger: Option<crate::engine_ledger::Reader>,
    props: Value,
}
struct State {
    running: Option<Running>,
    stop_pending: Option<String>,
    pressure_streak: u32,
}
#[derive(Clone)]
struct CacheBackend {
    backend: Backend,
    owned: Arc<Mutex<bool>>,
    generation: usize,
    preload: bool,
}
struct PreloadNote {
    generation: usize,
    state: &'static str,
    src: Value,
    ms: Value,
    at: Instant,
}

pub struct Manager {
    pub harness: Mutex<crate::harness::Registry>,
    pub ledger: Arc<crate::request_ledger::Ledger>,
    state: AsyncMutex<State>,
    loaded: RwLock<Option<Value>>,
    phase: AtomicU8,
    active: AtomicUsize,
    finalizing: AtomicUsize,
    finished: Notify,
    queued: AtomicUsize,
    slot: Arc<Semaphore>,
    last_used: Mutex<Instant>,
    last_failure: Mutex<Option<String>>,
    stop: watch::Sender<bool>,
    port: u16,
    preload: Mutex<Option<Value>>,
    cache_backend: RwLock<Option<CacheBackend>>,
    cache_preload: Mutex<Option<PreloadNote>>,
    session_hints: Mutex<std::collections::HashMap<String, String>>,
    launches: AtomicUsize,
    engine_ledger: RwLock<Option<crate::engine_ledger::Metadata>>,
}
struct Waiting(Arc<Manager>);
impl Drop for Waiting {
    fn drop(&mut self) {
        self.0.queued.fetch_sub(1, Ordering::SeqCst);
    }
}
pub struct Admission {
    manager: Arc<Manager>,
    _permit: OwnedSemaphorePermit,
    pub queue_seconds: f64,
    pub begin_seconds: f64,
    finalizing: bool,
}
impl Admission {
    pub fn begin_finalizing(&mut self) {
        if !self.finalizing {
            self.finalizing = true;
            self.manager.finalizing.fetch_add(1, Ordering::SeqCst);
            self.manager.finished.notify_waiters();
        }
    }
}
impl Drop for Admission {
    fn drop(&mut self) {
        *self
            .manager
            .last_used
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Instant::now();
        self.manager.active.fetch_sub(1, Ordering::SeqCst);
        if self.finalizing {
            self.manager.finalizing.fetch_sub(1, Ordering::SeqCst);
        }
        self.manager.finished.notify_waiters();
    }
}
#[derive(Clone)]
pub struct Backend {
    pub url: String,
    pub key: String,
}
impl Manager {
    pub fn new() -> Result<Arc<Self>, String> {
        let port = std::env::var("LOCAL_LLM_BACKEND_PORT")
            .unwrap_or_else(|_| "8093".into())
            .parse::<u16>()
            .map_err(|_| "Invalid LOCAL_LLM_BACKEND_PORT.")?;
        let flash = std::env::var("LOCAL_FLASH_BACKEND_PORT")
            .unwrap_or_else(|_| "8094".into())
            .parse::<u16>()
            .map_err(|_| "Invalid LOCAL_FLASH_BACKEND_PORT.")?;
        if port == flash {
            return Err("Classic and Flash backends require distinct private ports.".into());
        }
        let (stop, _) = watch::channel(false);
        let manager = Arc::new(Self {
            harness: Mutex::new(crate::harness::Registry::new()),
            ledger: Arc::new(crate::request_ledger::Ledger::default()),
            state: AsyncMutex::new(State {
                running: None,
                stop_pending: None,
                pressure_streak: 0,
            }),
            loaded: RwLock::new(None),
            phase: AtomicU8::new(0),
            active: AtomicUsize::new(0),
            finalizing: AtomicUsize::new(0),
            finished: Notify::new(),
            queued: AtomicUsize::new(0),
            slot: Arc::new(Semaphore::new(1)),
            last_used: Mutex::new(Instant::now()),
            last_failure: Mutex::new(None),
            preload: Mutex::new(None),
            cache_backend: RwLock::new(None),
            cache_preload: Mutex::new(None),
            session_hints: Mutex::new(std::collections::HashMap::new()),
            launches: AtomicUsize::new(0),
            engine_ledger: RwLock::new(None),
            stop,
            port,
        });
        let weak = Arc::downgrade(&manager);
        let mut stop = manager.stop.subscribe();
        tokio::spawn(async move {
            loop {
                tokio::select! {_=stop.changed()=>return,_=tokio::time::sleep(Duration::from_secs(1))=>{}}
                let Some(manager) = weak.upgrade() else {
                    return;
                };
                manager.reap().await;
            }
        });
        Ok(manager)
    }
    pub fn phase(&self) -> &'static str {
        match self.phase.load(Ordering::SeqCst) {
            1 => "loading",
            2 => "ready",
            3 => "stopping",
            _ => "unloaded",
        }
    }
    pub fn busy(&self) -> bool {
        self.active.load(Ordering::SeqCst) > 0
    }
    pub fn loaded(&self) -> Option<Value> {
        self.loaded
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    pub fn launches(&self) -> usize {
        self.launches.load(Ordering::SeqCst)
    }
    fn cache_backend(&self) -> Option<CacheBackend> {
        let backend = self
            .cache_backend
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()?;
        if let Ok(mut state) = self.state.try_lock() {
            if let Some(running) = state.running.as_mut() {
                let _ = running.process.poll();
            }
        }
        let live = *backend.owned.lock().unwrap_or_else(|e| e.into_inner());
        live.then_some(backend)
    }
    pub fn remember_session(&self, model: &str, hint: &str) {
        self.session_hints
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(model.into(), hint.into());
    }
    pub async fn cache_usage(&self, config: &Value) -> std::io::Result<Value> {
        let mut value = crate::state_cache::usage(config)?;
        if let Some(backend) = self.cache_backend() {
            value["engine"] = match crate::engine_cache::request(
                &backend.backend,
                reqwest::Method::GET,
                "/state-cache",
                None,
                &[200],
                1 << 20,
                30.0,
            )
            .await
            {
                Ok(engine) => Value::Object(
                    ["budget_bytes", "used_bytes"]
                        .iter()
                        .filter_map(|key| {
                            engine
                                .get(*key)
                                .filter(|v| v.is_i64() || v.is_u64())
                                .map(|v| ((*key).into(), v.clone()))
                        })
                        .collect(),
                ),
                Err(_) => Value::Null,
            };
        }
        Ok(value)
    }
    fn preload_status(&self) -> Value {
        let idle = |state: &str| json!({"state":state,"src":null,"ms":null,"age_s":null});
        if self.phase() == "unloaded" {
            return idle("idle");
        }
        let Some(backend) = self.cache_backend().filter(|b| b.preload) else {
            return idle("unavailable");
        };
        let note = self.cache_preload.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(note) = note.as_ref().filter(|n| n.generation == backend.generation) {
            json!({"state":note.state,"src":note.src,"ms":note.ms,"age_s":note.at.elapsed().as_secs()})
        } else {
            idle("idle")
        }
    }
    fn note_preload(&self, generation: usize, value: &Value) {
        if !self
            .cache_backend()
            .is_some_and(|b| b.generation == generation)
        {
            return;
        }
        let (state, src, ms) = crate::engine_cache::preload_view(value);
        *self.cache_preload.lock().unwrap_or_else(|e| e.into_inner()) = Some(PreloadNote {
            generation,
            state,
            src,
            ms,
            at: Instant::now(),
        });
    }
    pub fn preload_state(self: &Arc<Self>, model: &str, generation: usize) {
        let Some(backend) = self
            .cache_backend()
            .filter(|b| b.preload && b.generation == generation)
        else {
            return;
        };
        if !self.loaded().is_some_and(|p| p["model_alias"] == model) {
            return;
        }
        let hint = self
            .session_hints
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(model)
            .cloned()
            .or_else(|| crate::request_ledger::last_session(model));
        let payload = hint
            .map(|hint| json!({"session":hint}))
            .unwrap_or_else(|| json!({}));
        self.note_preload(generation, &json!({"status":"running"}));
        let manager = self.clone();
        let mut stop = self.stop.subscribe();
        tokio::spawn(async move {
            let reply = crate::engine_cache::request(
                &backend.backend,
                reqwest::Method::POST,
                "/state-cache/preload",
                Some(&payload),
                &[200, 202],
                4096,
                5.0,
            )
            .await
            .unwrap_or(Value::Null);
            manager.note_preload(generation, &reply);
            let deadline = tokio::time::Instant::now() + Duration::from_secs(300);
            loop {
                let running = manager
                    .cache_preload
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                    .is_some_and(|n| n.generation == generation && n.state == "running");
                if !running || tokio::time::Instant::now() >= deadline {
                    break;
                }
                tokio::select! {_=stop.changed()=>return,_=tokio::time::sleep(Duration::from_millis(500))=>{}}
                if !manager
                    .cache_backend()
                    .is_some_and(|b| b.generation == generation)
                {
                    return;
                }
                let usage = crate::engine_cache::request(
                    &backend.backend,
                    reqwest::Method::GET,
                    "/state-cache",
                    None,
                    &[200],
                    1 << 20,
                    30.0,
                )
                .await;
                match usage {
                    Ok(usage) => manager.note_preload(generation, &usage["preload"]),
                    Err(_) => {
                        manager.note_preload(generation, &Value::Null);
                        return;
                    }
                }
            }
        });
    }
    pub fn sampling(&self, alias: &str) -> Option<Vec<String>> {
        if !self.loaded().is_some_and(|p| p["model_alias"] == alias) {
            return None;
        }
        let state = self.state.try_lock().ok()?;
        let props = &state.running.as_ref()?.props;
        if props.is_null() {
            return None;
        }
        let controls = ["presence_penalty", "frequency_penalty", "logit_bias"];
        if let Some(values) = props["oxe_hints"]["sampling"].as_array() {
            return Some(
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|v| controls.contains(v))
                    .map(str::to_owned)
                    .collect(),
            );
        }
        Some(
            if props["default_generation_settings"]
                .get("max_output_tokens")
                .is_some()
            {
                controls.iter().map(|s| s.to_string()).collect()
            } else {
                Vec::new()
            },
        )
    }
    pub fn status(&self, config: &Value) -> Value {
        let profile = self
            .loaded()
            .or_else(|| models::resolve(config, None).ok())
            .unwrap_or_else(|| json!({}));
        let (memory, pid) = self
            .state
            .try_lock()
            .ok()
            .and_then(|s| {
                s.running
                    .as_ref()
                    .map(|r| (Some(r.guard.metadata()), Some(r.process.pid())))
            })
            .unwrap_or((None, None));
        let loaded = self.loaded();
        let idle = setting(
            &profile,
            "idle_unload_seconds",
            "LOCAL_LLM_KEEP_WARM_IDLE_SECONDS",
            1e-9,
            7200.0,
        );
        let remaining = if loaded.is_some() && self.phase() == "ready" && !self.busy() {
            idle.map(|idle| {
                ((idle
                    - self
                        .last_used
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .elapsed()
                        .as_secs_f64())
                .max(0.0)
                    * 10.0)
                    .round_ties_even()
                    / 10.0
            })
        } else {
            None
        };
        let harness = self
            .harness
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .status(&profile, config);
        let limits = models::rows(config)
            .into_iter()
            .filter_map(|row| {
                let alias = row["alias_of"].as_str().or_else(|| row["id"].as_str())?;
                let profile = models::resolve(config, Some(alias)).ok()?;
                Some((
                    row["id"].as_str()?.to_owned(),
                    models::model_limit(&profile, config, self.sampling(alias)),
                ))
            })
            .collect::<serde_json::Map<_, _>>();
        let mut status = json!({"state":self.phase(),"model":profile["model_alias"],"selected_model":models::resolve(config,None).ok().map(|p|p["model_alias"].clone()),
            "validated":models::eligible(&profile),"active_requests":self.active.load(Ordering::SeqCst),"queued_requests":self.queued.load(Ordering::SeqCst),
            "capabilities":["models","storage","harness","harness_learning","ledger","active","openai_aliases","state_cache","context_modes","preload","model_preload"],
            "context_size":profile["context_size"],"context":models::context(&profile,config),"model_limits":limits,"memory_policy":memory,
            "idle_unload_seconds":idle,"loaded_model":loaded.as_ref().map(|p|p["model_alias"].clone()),"resident_bytes":pid.and_then(resident_bytes),
            "idle_unload_remaining_seconds":remaining,"unload":{"method":"POST","path":"/_local/unload","available":loaded.is_some() && !self.busy()},
            "error_at":null,"error_origin":null,
            "last_error":self.last_failure.lock().unwrap_or_else(|e|e.into_inner()).clone(),"model_preload":self.preload.lock().unwrap_or_else(|e|e.into_inner()).clone(),"preload":self.preload_status(),
            "engine_ledger":self.engine_ledger.read().unwrap_or_else(|e|e.into_inner()).as_ref().map(|m|m.value()).unwrap_or_else(||json!({"enabled":false}))});
        status
            .as_object_mut()
            .expect("status object")
            .extend(harness.as_object().expect("harness facts").clone());
        status
    }
    pub async fn set_harness_learning(
        &self,
        enabled: Value,
        min_requests: Value,
    ) -> Result<Value, String> {
        if !enabled.is_boolean()
            || (!min_requests.is_null()
                && !min_requests
                    .as_u64()
                    .is_some_and(|n| (1..=100).contains(&n)))
        {
            return Err(
                "Send {\"enabled\": true or false} and optionally \"min_requests\" (1-100).".into(),
            );
        }
        let _state = tokio::time::timeout(Duration::from_secs(2), self.state.lock())
            .await
            .map_err(|_| "A model is loading; change the setting when it is ready.")?;
        let mut config =
            paths::read_config().ok_or("The harness learning setting could not be saved.")?;
        let mut learning = config["harness_learning"]
            .as_object()
            .cloned()
            .unwrap_or_default();
        learning.insert("enabled".into(), enabled);
        if !min_requests.is_null() {
            learning.insert("min_requests".into(), min_requests);
        }
        config["harness_learning"] = Value::Object(learning);
        paths::write_config(&config)
            .map_err(|_| "The harness learning setting could not be saved.")?;
        Ok(crate::harness::settings(&config))
    }
    pub async fn set_cache_budget(self: &Arc<Self>, budget: Value) -> Result<Value, (u16, Value)> {
        if crate::state_cache::valid_budget(&budget).is_none() {
            return Err((
                400,
                json!({"error":"Send {\"bytes\": N}: 0 (disk tier off) or a whole number of at least 4000000000."}),
            ));
        }
        // A disconnected control caller does not release the manager/lease
        // while a disk mutation is still running on the blocking executor.
        let manager = self.clone();
        tokio::spawn(async move { manager.cache_budget_inner(budget).await })
            .await
            .unwrap_or_else(|_| {
                Err((
                    503,
                    json!({"error":"The cache budget could not be applied."}),
                ))
            })
    }
    pub async fn clear_cache(self: &Arc<Self>, control: Value) -> Result<Value, (u16, Value)> {
        let manager = self.clone();
        tokio::spawn(async move { manager.clear_cache_inner(control).await })
            .await
            .unwrap_or_else(|_| Err((503, json!({"error":"Saved state could not be cleared."}))))
    }
    async fn clear_cache_inner(&self, control: Value) -> Result<Value, (u16, Value)> {
        let scope = control["scope"].as_str().unwrap_or("all");
        let mut state = self.state.lock().await;
        if self.busy() {
            return Err((
                409,
                json!({"error":"A request is running. Try again when it finishes."}),
            ));
        }
        let failure = || (503, json!({"error":"Saved state could not be cleared."}));
        let config = paths::read_config().ok_or_else(failure)?;
        if scope == "all" {
            let profile = models::resolve(&config, None).map_err(|_| failure())?;
            if profile["backend_kind"] == "bigmoe-cpu-stream" {
                return Err((
                    409,
                    json!({"error":"Unload Flash to clear its resident cache."}),
                ));
            }
            if profile["prefix_cache"]["enabled"] == true {
                let prefix = profile["prefix_cache"].clone();
                tokio::task::spawn_blocking(move ||crate::prefix_store::clear(&prefix)).await
                    .map_err(|_|(503,json!({"error":"Prefix cache could not be cleared. Try again in a moment."})))?
                    .map_err(|_|(503,json!({"error":"Prefix cache could not be cleared. Try again in a moment."})))?;
            }
            self.harness
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .cleared();
        }
        let hint = if scope == "hint" {
            let hint = self
                .ledger
                .session_hint(control["hint"].as_str().unwrap_or(""))
                .ok_or_else(failure)?;
            Some(hint)
        } else {
            None
        };
        if let Some(running) = state.running.as_mut() {
            let _ = running.process.poll();
        }
        if let Some(backend) = self.cache_backend() {
            let mut body = json!({"scope":scope});
            if scope == "model" {
                body["model"] = control["model"].clone();
            }
            if scope == "session" {
                body["session"] = control["session"].clone();
            }
            if scope == "hint" {
                body["hint"] = json!(hint);
            }
            let engine = crate::engine_cache::request(
                &backend.backend,
                reqwest::Method::POST,
                "/state-cache/clear",
                Some(&body),
                &[200],
                1 << 20,
                30.0,
            )
            .await
            .map_err(|_| failure())?;
            let mut result =
                json!({"scope":scope,"freed_bytes":engine["freed_bytes"],"applied_by":"engine"});
            if scope == "hint" {
                result["sessions"] = engine["sessions"].clone();
            }
            return Ok(json!({"cleared":true,"state_cache":result}));
        }
        let hint =
            hint.map(|hint| format!("{:x}", Sha256::digest(hint.as_bytes()))[..32].to_owned());
        let lease = if state.running.is_none() {
            let path = std::env::var_os("LOCAL_AI_LEASE")
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    paths::xdg("XDG_STATE_HOME", ".local/state").join("local-ai/inference.lock")
                });
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(|_| failure())?;
            }
            let file=OpenOptions::new().append(true).create(true).mode(0o600).custom_flags(libc::O_NOFOLLOW|libc::O_CLOEXEC).open(&path)
                .map_err(|error|if error.raw_os_error()==Some(libc::ELOOP){(409,json!({"error":format!("The inference lease {} is a symbolic link; refusing to use it. Remove the link and retry.",path.display())}))}else{failure()})?;
            file.try_lock().map_err(|_|(409,json!({"error":"Another image, chat or setup job is running. Try again when it finishes."})))?;
            Some(file)
        } else {
            None
        };
        tokio::task::spawn_blocking(move ||{
            let _lease=lease;
            let scope=control["scope"].as_str().unwrap_or("all");
            let mut freed=crate::state_cache::clear(&config,scope,control["model"].as_str(),control["session"].as_str(),hint.as_deref())?;
            if scope=="all"{freed+=crate::storage::clear_legacy_prefix(&config)?;}
            Ok::<_,std::io::Error>(json!({"cleared":true,"state_cache":{"scope":scope,"freed_bytes":freed,"applied_by":"controller"}}))
        }).await.map_err(|_|failure())?.map_err(|_|failure())
    }
    pub async fn set_mode(
        self: &Arc<Self>,
        alias: Value,
        mode: Value,
    ) -> Result<Value, (u16, Value)> {
        let manager = self.clone();
        tokio::spawn(async move { manager.set_mode_inner(alias, mode).await })
            .await
            .unwrap_or_else(|_| Err((409, json!({"error":"The mode could not be saved."}))))
    }
    async fn set_mode_inner(&self, alias: Value, mode: Value) -> Result<Value, (u16, Value)> {
        let error = |message: &str| (409, json!({"error":message}));
        let alias = alias
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| error("Name the model."))?;
        let mode = mode
            .as_str()
            .filter(|s| ["fast", "deep", "max"].contains(s))
            .ok_or_else(|| error("Mode must be fast, deep or max."))?;
        let mut state = tokio::time::timeout(Duration::from_secs(2), self.state.lock())
            .await
            .map_err(|_| error("A model is loading; change the mode when it is ready."))?;
        let mut config = paths::read_config()
            .ok_or_else(|| error("Local model setup is still being prepared."))?;
        let profile = models::resolve(&config, Some(alias)).map_err(error)?;
        let rows = models::mode_rows(&profile);
        if rows.is_empty() {
            return Err(error("This model has no context modes."));
        }
        let row = rows.iter().find(|r| r["id"] == mode).expect("context mode");
        if row["available"] != true {
            return Err(error(
                row["reason"]
                    .as_str()
                    .unwrap_or("This mode is unavailable."),
            ));
        }
        let alias = profile["model_alias"].as_str().expect("model alias");
        let applied = models::apply_mode(profile.clone(), mode);
        let cap_change = state.running.is_some()
            && self.loaded().is_some_and(|p| {
                p["model_alias"] == alias && p["context_size"] != applied["context_size"]
            });
        if cap_change && self.busy() {
            return Err(error(
                "A request is running, so the mode was not changed. Try again when it finishes.",
            ));
        }
        let mut block = config["context_modes"]
            .as_object()
            .cloned()
            .unwrap_or_default();
        let mut models = block
            .get("models")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        models.insert(alias.into(), json!(mode));
        block.insert("models".into(), Value::Object(models));
        config["context_modes"] = Value::Object(block);
        paths::write_config(&config).map_err(|_| error("The mode could not be saved."))?;
        if cap_change {
            self.cleanup(&mut state)
                .await
                .map_err(|e| (503, json!({"error":e})))?;
        }
        let profile = models::resolve(&config, Some(alias)).map_err(error)?;
        Ok(models::context(&profile, &config))
    }
    async fn cache_budget_inner(&self, budget: Value) -> Result<Value, (u16, Value)> {
        let failure = || {
            (
                503,
                json!({"error":"The cache budget could not be applied."}),
            )
        };
        let mut state = tokio::time::timeout(Duration::from_secs(2), self.state.lock())
            .await
            .map_err(|_| {
                (
                    409,
                    json!({"error":"A model is loading; change the budget when it is ready."}),
                )
            })?;
        let mut config = paths::read_config()
            .ok_or_else(|| (409, json!({"error":"The cache budget could not be saved."})))?;
        let mut cache = config["cache"].as_object().cloned().unwrap_or_default();
        cache.insert("disk_budget_bytes".into(), budget.clone());
        config["cache"] = Value::Object(cache);
        paths::write_config(&config)
            .map_err(|_| (409, json!({"error":"The cache budget could not be saved."})))?;
        if let Some(running) = state.running.as_mut() {
            let _ = running.process.poll();
        }
        if let Some(backend) = self.cache_backend() {
            let engine=crate::engine_cache::request(&backend.backend,reqwest::Method::POST,"/state-cache/budget",Some(&json!({"bytes":budget})),&[200],1<<20,30.0).await.map_err(|_|(409,json!({"error":"The budget was saved; the engine did not apply it yet (it will at the next load)."})))?;
            return Ok(
                json!({"budget_bytes":budget,"used_bytes":engine["used_bytes"],"freed_bytes":engine["freed_bytes"],"applied_by":"engine"}),
            );
        }
        crate::state_cache::root(&config).map_err(|e| {
            if e.kind() == std::io::ErrorKind::InvalidInput {
                (400, json!({"error":e.to_string()}))
            } else {
                failure()
            }
        })?;
        let lease = if state.running.is_none() {
            let path = std::env::var_os("LOCAL_AI_LEASE")
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    paths::xdg("XDG_STATE_HOME", ".local/state").join("local-ai/inference.lock")
                });
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(|_| failure())?;
            }
            let saved_busy = || {
                (
                    409,
                    json!({"error":"The budget was saved; another job is running, so saved state is trimmed later (at the next load or budget change)."}),
                )
            };
            let file = OpenOptions::new()
                .append(true)
                .create(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(path)
                .map_err(|e| {
                    if e.raw_os_error() == Some(libc::ELOOP) {
                        saved_busy()
                    } else {
                        failure()
                    }
                })?;
            file.try_lock().map_err(|_| saved_busy())?;
            Some(file)
        } else {
            None
        };
        tokio::task::spawn_blocking(move ||{
            let _lease=lease;
            let freed=crate::state_cache::gc(&config)?;
            let used=crate::state_cache::usage(&config)?["used_bytes"].clone();
            Ok::<_,std::io::Error>(json!({"budget_bytes":budget,"used_bytes":used,"freed_bytes":freed,"applied_by":"controller"}))
        }).await.map_err(|_|failure())?.map_err(|_|failure())
    }
    pub async fn prepare_harness(
        self: &Arc<Self>,
        harness: &str,
        profile: Value,
    ) -> Result<Value, (u16, String)> {
        use futures_util::StreamExt;
        if profile["backend_kind"] != "oxe" {
            return Err((
                409,
                "Harness prefixes are prepared on the Rust engine models only.".into(),
            ));
        }
        let alias = profile["model_alias"].as_str().unwrap_or("");
        let names = if harness == "*" {
            self.harness
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .names(&profile)
        } else {
            vec![harness.to_owned()]
        };
        let mut done = Vec::new();
        let mut all = Vec::new();
        for name in names {
            let primers = self
                .harness
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .primers(&name, alias);
            if primers.is_empty() {
                if harness == "*" {
                    continue;
                }
                return Err((
                    409,
                    "No recorded prefix for this harness yet; run it once against the local model."
                        .into(),
                ));
            }
            let result=async {
                let mut prepared=Vec::new();
                for (key,mut body) in primers {
                    let (admission,backend)=self.admit(profile.clone()).await.map_err(|e|(409,e))?;
                    let started=Instant::now();crate::proxy::defaults(body.as_object_mut().expect("primer body"),&profile,true);
                    let error=|| (503,"Harness prefix could not be prepared.".to_owned());
                    let response=client(900.0).post(format!("{}/v1/chat/completions",backend.url)).bearer_auth(&backend.key).json(&body).send().await.map_err(|_|error())?;
                    if response.status()!=200 {return Err(error());}
                    let mut stream=response.bytes_stream();let mut raw=Vec::new();
                    while let Some(chunk)=stream.next().await {let chunk=chunk.map_err(|_|error())?;if raw.len()+chunk.len()>4*1024*1024 {return Err(error());}raw.extend_from_slice(&chunk);}
                    let response=serde_json::from_slice::<Value>(&raw).ok().filter(Value::is_object).ok_or_else(error)?;
                    let cache=response["timings"]["cache_n"].clone();let prompt=response["timings"]["prompt_n"].clone();
                    self.harness.lock().unwrap_or_else(|e|e.into_inner()).complete(&key,&profile,cache.clone(),prompt.clone());
                    prepared.push(json!({"key":&key[..12],"cache_n":cache,"prompt_n":prompt,"seconds":(started.elapsed().as_secs_f64()*100.0).round_ties_even()/100.0}));drop(admission);
                }Ok::<_,(u16,String)>(prepared)
            }.await;
            match result {
                Ok(prepared) => {
                    all.extend(prepared.clone());
                    done.push(json!({"harness":name,"model":alias,"prepared":prepared}));
                }
                Err((409, _)) if harness == "*" => {}
                Err(error) => return Err(error),
            }
        }
        if harness == "*" {
            Ok(json!({"harness":"*","model":alias,"harnesses":done,"prepared":all}))
        } else {
            Ok(done.into_iter().next().expect("prepared harness"))
        }
    }
    pub async fn admit(self: &Arc<Self>, profile: Value) -> Result<(Admission, Backend), String> {
        let queued_at = Instant::now();
        self.queued.fetch_add(1, Ordering::SeqCst);
        let waiting = Waiting(self.clone());
        let permit = tokio::time::timeout(
            Duration::from_secs_f64(seconds("LOCAL_LLM_QUEUE_WAIT_SECONDS", 120.0)),
            self.slot.clone().acquire_owned(),
        )
        .await
        .map_err(|_| "The local model is still busy. Please try again in a moment.")?
        .map_err(|_| "The local model service is stopping.")?;
        drop(waiting);
        let queue_seconds = queued_at.elapsed().as_secs_f64();
        let begin_at = Instant::now();
        self.load(profile).await?;
        let state = self.state.lock().await;
        let running = state
            .running
            .as_ref()
            .ok_or("The local model stopped before the request could start.")?;
        let backend = Backend {
            url: format!("http://127.0.0.1:{}", self.port),
            key: running.token.value.clone(),
        };
        self.active.fetch_add(1, Ordering::SeqCst);
        Ok((
            Admission {
                manager: self.clone(),
                _permit: permit,
                queue_seconds,
                begin_seconds: begin_at.elapsed().as_secs_f64(),
                finalizing: false,
            },
            backend,
        ))
    }
    pub async fn load(self: &Arc<Self>, profile: Value) -> Result<(), String> {
        let manager = self.clone();
        tokio::spawn(async move { manager.load_inner(profile, false).await })
            .await
            .map_err(|_| {
                "The model could not start. Details: journalctl --user -u local-ai-chat.".to_owned()
            })??;
        *self.last_used.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
        Ok(())
    }
    pub fn preload(self: &Arc<Self>, profile: Value, explicit: bool) -> (u16, Value) {
        let alias = profile["model_alias"].clone();
        let mut info = self.preload.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(current) = info.as_ref().filter(|v| v["state"] == "loading") {
            return (
                202,
                json!({"state":"loading","started":false,"model":current["model"]}),
            );
        }
        if self.phase() == "loading" {
            return (
                202,
                json!({"state":"loading","started":false,"model":self.loaded().map(|p|p["model_alias"].clone()).unwrap_or(alias)}),
            );
        }
        let live = self.state.try_lock().ok().is_some_and(|mut state| {
            state
                .running
                .as_mut()
                .is_some_and(|running| running.process.poll().ok() == Some(None))
        });
        if self.phase() == "ready" && live {
            let loaded = self
                .loaded()
                .map(|p| p["model_alias"].clone())
                .unwrap_or(Value::Null);
            if !explicit || loaded == alias {
                return (200, json!({"state":"ready","started":false,"model":loaded}));
            }
            return (
                409,
                json!({"error":"Another model is loaded; a preload never switches models.","state":"other_model_loaded","model":loaded}),
            );
        }
        *info = Some(
            json!({"state":"loading","model":alias,"error":null,"t":chrono::Utc::now().timestamp()}),
        );
        let manager = self.clone();
        let started_alias = alias.clone();
        tokio::spawn(async move {
            let before = manager.launches();
            let result = manager.load_inner(profile, true).await;
            if result.is_ok() && manager.launches() != before {
                manager.preload_state(started_alias.as_str().unwrap_or(""), manager.launches());
            }
            *manager.preload.lock().unwrap_or_else(|e| e.into_inner()) = Some(
                json!({"state":if result.is_ok() {"ready"} else {"failed"},"model":started_alias,"error":result.err(),"t":chrono::Utc::now().timestamp()}),
            );
        });
        (202, json!({"state":"loading","started":true,"model":alias}))
    }
    pub async fn select(&self, profile: Value) -> Result<String, String> {
        let mut state = self.state.try_lock().map_err(|_| {
            "A model is loading or a request is running. Try again when it finishes."
        })?;
        if self.busy() {
            return Err(
                "A request is running, so the model was not changed. Try again when it finishes."
                    .into(),
            );
        }
        if !models::eligible(&profile) {
            return Err("The Rust engine is not enabled and ready.".into());
        }
        let alias = profile["model_alias"]
            .as_str()
            .ok_or("Choose a local model.")?
            .to_owned();
        let mut config =
            paths::read_config().ok_or("Local model setup is still being prepared.")?;
        if self.loaded().is_some_and(|p| p["model_alias"] != alias) {
            self.cleanup(&mut state).await?;
        }
        config["selected_model"] = json!(alias);
        paths::write_config(&config).map_err(|_| "The model setting could not be saved.")?;
        Ok(alias)
    }
    async fn load_inner(&self, profile: Value, no_switch: bool) -> Result<(), String> {
        let mut state = self.state.lock().await;
        if *self.stop.borrow() {
            return Err("The local model service is stopping.".into());
        }
        if state.stop_pending.is_some() && self.cleanup(&mut state).await.is_err() {
            return Err("The previous model has not exited yet. Try again in a moment.".into());
        }
        if let Some(running) = state.running.as_mut() {
            if running.process.poll().ok().flatten().is_some() || running.guard.failure().is_some()
            {
                self.cleanup(&mut state).await?;
            } else if self
                .loaded()
                .is_some_and(|p| p["model_alias"] == profile["model_alias"])
            {
                return Ok(());
            } else {
                if no_switch {
                    return Err("Another model is loaded; a preload never switches models.".into());
                }
                if self.busy() {
                    return Err("Another request is running. Try again when it finishes.".into());
                }
                self.cleanup(&mut state).await?;
            }
        }
        if !models::eligible(&profile) {
            return Err("The Rust engine is not enabled and ready.".into());
        }
        let probe=std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST,self.port)).map_err(|_|format!("Local engine port {} is already in use. Close that app or set LOCAL_LLM_BACKEND_PORT to an unused port.",self.port))?;
        drop(probe);
        let path = profile["model_path"]
            .as_str()
            .ok_or("The model file is not in place yet. Run `local-ai status` to check setup.")?;
        let info = fs::metadata(path).map_err(|_| {
            "The model file is not in place yet. Run `local-ai status` to check setup."
        })?;
        if !info.is_file() {
            return Err(
                "The model file is not in place yet. Run `local-ai status` to check setup.".into(),
            );
        }
        if profile["expected_bytes"]
            .as_u64()
            .is_some_and(|v| v != info.len())
        {
            return Err(
                "The model file is incomplete or changed. Run `local-ai status` to check setup."
                    .into(),
            );
        }
        let required = info.len().saturating_add(3 * 1024 * 1024 * 1024);
        if let Some(available) = guard::available_memory().filter(|v| *v < required) {
            return Err(format!("This model needs about {:.1} GiB of available RAM; {:.1} GiB is available. Close other large apps and retry.",required as f64/(1_u64<<30) as f64,available as f64/(1_u64<<30) as f64));
        }
        let lease_path = std::env::var_os("LOCAL_AI_LEASE")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                paths::xdg("XDG_STATE_HOME", ".local/state").join("local-ai/inference.lock")
            });
        if let Some(parent) = lease_path.parent() {
            fs::create_dir_all(parent).map_err(|_| "The inference lease is unavailable.")?;
        }
        let lease=OpenOptions::new().append(true).create(true).mode(0o600).custom_flags(libc::O_NOFOLLOW|libc::O_CLOEXEC).open(&lease_path).map_err(|error|if error.raw_os_error()==Some(libc::ELOOP) {format!("The inference lease {} is a symbolic link; refusing to use it. Remove the link and retry.",lease_path.display())} else {"The inference lease is unavailable.".into()})?;
        lease.try_lock().map_err(|_| {
            "Another image, chat or setup job is running. Try again when it finishes."
        })?;
        verify(&profile)?;
        let mut guard = guard::Guard::prepare(seconds("LOCAL_LLM_START_SECONDS", 180.0))
            .map_err(|reason| guard::refusal(&reason))?;
        let token = Token::new(self.port)?;
        let mut args = vec![
            profile["server_binary"].as_str().unwrap_or("").to_owned(),
            "--host".into(),
            "127.0.0.1".into(),
            "--port".into(),
            self.port.to_string(),
        ];
        for arg in profile["oxe_args"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            args.push(match arg {
                "{model}" => path.to_owned(),
                "{alias}" => profile["model_alias"].as_str().unwrap_or("").to_owned(),
                "{threads}" => profile["threads"].to_string(),
                "{context}" => profile["context_size"].to_string(),
                _ => arg.to_owned(),
            });
        }
        args=crate::state_cache::conversation_args(&profile,args).map_err(|_|"LOCAL_AI_STATE_CACHE_ROOT (or runtime.json cache.root) must be an absolute path inside your home; the model was not started.".to_owned())?;
        let settings = crate::state_cache::Settings::new(&profile);
        let overridden = crate::state_cache::overridden(&profile);
        let state_root = if settings.budget > 0 || overridden {
            Some(crate::state_cache::root(&profile).map_err(|_|"LOCAL_AI_STATE_CACHE_ROOT (or runtime.json cache.root) must be an absolute path inside your home; the model was not started.".to_owned())?)
        } else {
            None
        };
        let state_engine = settings.budget > 0 && has_state_cache(Path::new(&args[0]))?;
        let state_preload = state_engine
            && binary_holds(Path::new(&args[0]), &[b"oxe-capability state-preload-v1"])?;
        if state_engine {
            let root = state_root.as_ref().expect("state root");
            args.extend([
                "--state-cache".into(),
                root.to_string_lossy().into_owned(),
                "--state-budget-bytes".into(),
                settings.budget.to_string(),
                "--state-ttl-secs".into(),
                settings.ttl.to_string(),
            ]);
        }
        if overridden {
            for (index, arg) in args.iter().enumerate() {
                let (name, value) = arg
                    .split_once('=')
                    .map(|(name, value)| (name, Some(value)))
                    .unwrap_or((arg.as_str(), args.get(index + 1).map(String::as_str)));
                if ["--prefix-cache", "--session-cache", "--state-cache"].contains(&name) {
                    if let Some(path) = value.filter(|s| !s.is_empty()) {
                        crate::state_cache::make_private(Path::new(path)).map_err(|_|"LOCAL_AI_STATE_CACHE_ROOT (or runtime.json cache.root) must be an absolute path inside your home; the model was not started.".to_owned())?;
                    }
                }
            }
        }
        let ledger = if crate::engine_ledger::enabled(&profile, &args) {
            match crate::engine_ledger::Prepared::new(&profile) {
                Ok(ledger) => {
                    args.extend([
                        "--ledger".into(),
                        crate::engine_ledger::PIPE_ARGUMENT.into(),
                    ]);
                    Some(ledger)
                }
                Err(_) => {
                    crate::diagnostics::Writer::new(
                        paths::xdg("XDG_STATE_HOME", ".local/state")
                            .join("local-ai/engine-stderr.log"),
                    )
                    .write(b"{\"event\":\"ledger_disabled\",\"reason\":\"log_unavailable\"}\n");
                    None
                }
            }
        } else {
            None
        };
        let mut command = Command::new("/proc/self/exe");
        command
            .arg(if ledger.is_some() {
                "__engine_ledger_exec"
            } else {
                "__engine_exec"
            })
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        command.env_clear();
        for (key, value) in std::env::vars_os() {
            let name = key.to_string_lossy();
            if !name.starts_with("GGML_") && !name.starts_with("LLAMA_") {
                command.env(key, value);
            }
        }
        if let Some(environment) = profile["environment"].as_object() {
            for (key, value) in environment {
                let value = value.as_str().ok_or("The engine environment is invalid.")?;
                command.env(key, value);
            }
        }
        command.env("OXE_API_KEY_FILE", &token.path);
        let (ledger_reader, ledger_metadata) = if let Some(ledger) = ledger {
            let (reader, metadata) = ledger.start(&mut command);
            (Some(reader), Some(metadata))
        } else {
            (None, None)
        };
        self.phase.store(1, Ordering::SeqCst);
        *self.loaded.write().unwrap_or_else(|e| e.into_inner()) = Some(profile);
        let child = command.spawn();
        // Command retains its configured stdin endpoint after spawning.
        // Close the parent's write copy so child exit gives the reader EOF.
        drop(command);
        let child = match child {
            Ok(child) => child,
            Err(_) => {
                self.phase.store(0, Ordering::SeqCst);
                *self.loaded.write().unwrap_or_else(|e| e.into_inner()) = None;
                return Err(
                    "The model could not start. Details: journalctl --user -u local-ai-chat."
                        .into(),
                );
            }
        };
        *self
            .engine_ledger
            .write()
            .unwrap_or_else(|e| e.into_inner()) = ledger_metadata;
        let mut process = Process {
            child,
            owned: Arc::new(Mutex::new(true)),
        };
        guard.start(
            process.pid(),
            process.owned.clone(),
            seconds("LOCAL_LLM_START_SECONDS", 180.0),
        );
        let stderr = process
            .child
            .stderr
            .take()
            .map(|stderr| spawn_stderr(stderr, token.value.clone()));
        let generation = self.launches.fetch_add(1, Ordering::SeqCst) + 1;
        *self.cache_preload.lock().unwrap_or_else(|e| e.into_inner()) = None;
        *self
            .cache_backend
            .write()
            .unwrap_or_else(|e| e.into_inner()) = state_engine.then(|| CacheBackend {
            backend: Backend {
                url: format!("http://127.0.0.1:{}", self.port),
                key: token.value.clone(),
            },
            owned: process.owned.clone(),
            generation,
            preload: state_preload,
        });
        state.running = Some(Running {
            process,
            _lease: lease,
            token,
            guard,
            stderr,
            engine_ledger: ledger_reader,
            props: Value::Null,
        });
        let deadline =
            Instant::now() + Duration::from_secs_f64(seconds("LOCAL_LLM_START_SECONDS", 180.0));
        let client = client(2.0);
        let result = loop {
            if *self.stop.borrow() {
                break Err("The local model service is stopping.".into());
            }
            let running = state.running.as_mut().expect("owned process");
            if let Some(reason) = running.guard.failure() {
                break Err(guard::refusal(&reason));
            }
            if running.process.poll().ok().flatten().is_some() {
                break Err(
                    "The model exited during loading. Details: journalctl --user -u local-ai-chat."
                        .into(),
                );
            }
            if let Ok(response) = client
                .get(format!("http://127.0.0.1:{}/health", self.port))
                .bearer_auth(&running.token.value)
                .send()
                .await
            {
                if response.status().is_success()
                    && response
                        .json::<Value>()
                        .await
                        .ok()
                        .is_some_and(|v| v["status"] == "ok")
                {
                    running.guard.mark_ready();
                    if let Some(reason) = running.guard.failure() {
                        break Err(guard::refusal(&reason));
                    }
                    running.props = match client
                        .get(format!("http://127.0.0.1:{}/props", self.port))
                        .bearer_auth(&running.token.value)
                        .send()
                        .await
                    {
                        Ok(reply) => reply.json().await.unwrap_or(Value::Null),
                        Err(_) => Value::Null,
                    };
                    if let Ok(reply) = client
                        .get(format!("http://127.0.0.1:{}/v1/models", self.port))
                        .bearer_auth(&running.token.value)
                        .send()
                        .await
                    {
                        let _ = reply.bytes().await;
                    }
                    break Ok(());
                }
            }
            if Instant::now() >= deadline {
                break Err("The model took too long to load; nothing was generated. Try again in a moment.".into());
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        };
        if let Err(error) = result {
            let _ = self.cleanup(&mut state).await;
            *self.last_failure.lock().unwrap_or_else(|e| e.into_inner()) = Some(error.clone());
            return Err(error);
        }
        self.phase.store(2, Ordering::SeqCst);
        *self.last_used.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
        *self.last_failure.lock().unwrap_or_else(|e| e.into_inner()) = None;
        Ok(())
    }
    async fn cleanup(&self, state: &mut State) -> Result<(), String> {
        if let Some(running) = state.running.as_mut() {
            self.phase.store(3, Ordering::SeqCst);
            running.process.signal(if state.stop_pending.is_some() {
                Signal::SIGKILL
            } else {
                Signal::SIGTERM
            });
            let exited = running
                .process
                .wait(if state.stop_pending.is_some() {
                    1.0
                } else {
                    10.0
                })
                .await;
            let exited = if exited {
                true
            } else {
                running.process.signal(Signal::SIGKILL);
                running.process.wait(1.0).await
            };
            if !exited {
                let message =
                    "The previous model has not exited yet. Try again in a moment.".to_owned();
                state.stop_pending = Some(message.clone());
                return Err(message);
            }
            running.guard.stop().await;
            if let Some(mut task) = running.stderr.take() {
                if tokio::time::timeout(Duration::from_secs(5), &mut task)
                    .await
                    .is_err()
                {
                    task.abort();
                    let _ = task.await;
                }
            }
            if let Some(reader) = running.engine_ledger.take() {
                reader.stop().await;
            }
        }
        state.running = None;
        *self
            .engine_ledger
            .write()
            .unwrap_or_else(|e| e.into_inner()) = None;
        *self
            .cache_backend
            .write()
            .unwrap_or_else(|e| e.into_inner()) = None;
        state.stop_pending = None;
        state.pressure_streak = 0;
        *self.loaded.write().unwrap_or_else(|e| e.into_inner()) = None;
        self.phase.store(0, Ordering::SeqCst);
        Ok(())
    }
    pub async fn unload(&self, wanted: Option<&str>) -> Result<(), (u16, Value)> {
        // Once every active request has finished forwarding, permit up to the
        // reference's two seconds for advisory bookkeeping and slot release.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let notified = self.finished.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let active = self.active.load(Ordering::SeqCst);
            if active == 0 || active != self.finalizing.load(Ordering::SeqCst) {
                break;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                break;
            }
        }
        if self.busy() {
            return Err((
                409,
                json!({"error":"A request is running. Try again when it finishes.","retryable":true}),
            ));
        }
        let mut state = self.state.lock().await;
        if self.busy() {
            return Err((
                409,
                json!({"error":"A request is running. Try again when it finishes.","retryable":true}),
            ));
        }
        if let Some(wanted) = wanted {
            if !self.loaded().is_some_and(|p| p["model_alias"] == wanted) {
                return Err((409, json!({"error":"That model is not loaded."})));
            }
        }
        self.cleanup(&mut state)
            .await
            .map_err(|error| (503, json!({"error":error})))
    }
    pub async fn shutdown(&self) {
        self.stop.send_replace(true);
        let mut state = self.state.lock().await;
        let _ = self.cleanup(&mut state).await;
    }
    async fn reap(&self) {
        let Ok(mut state) = self.state.try_lock() else {
            return;
        };
        if state.stop_pending.is_some() {
            let _ = self.cleanup(&mut state).await;
            return;
        }
        if let Some(running) = state.running.as_mut() {
            if running.guard.failure().is_some() || running.process.poll().ok().flatten().is_some()
            {
                let _ = self.cleanup(&mut state).await;
                return;
            }
            if !self.busy()
                && self.queued.load(Ordering::SeqCst) == 0
                && self.slot.available_permits() == 1
            {
                let idle = self
                    .last_used
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .elapsed()
                    .as_secs_f64();
                if let Some(profile) = self.loaded() {
                    if idle_due(&profile, idle, &mut state.pressure_streak) {
                        let _ = self.cleanup(&mut state).await;
                    }
                }
            } else {
                state.pressure_streak = 0;
            }
        }
    }
}
fn resident_bytes(pid: u32) -> Option<u64> {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    for line in status.lines().filter(|s| s.starts_with("VmRSS:")) {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() == 3 && fields[2] == "kB" && fields[1].bytes().all(|b| b.is_ascii_digit()) {
            return fields[1].parse::<u64>().ok()?.checked_mul(1024);
        }
    }
    None
}
pub fn client(seconds: f64) -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(Duration::from_secs(2))
        .read_timeout(Duration::from_secs_f64(seconds))
        .build()
        .expect("HTTP client")
}
fn setting(config: &Value, key: &str, env: &str, minimum: f64, default: f64) -> Option<f64> {
    let parse = |value: &Value, fallback: Option<f64>| {
        if value.is_null()
            || value.as_str().is_some_and(|v| {
                ["off", "never", "none", "disabled"]
                    .contains(&v.trim().to_ascii_lowercase().as_str())
            })
        {
            None
        } else {
            value
                .as_f64()
                .or_else(|| value.as_str().and_then(|v| v.parse().ok()))
                .filter(|v| v.is_finite() && *v >= minimum)
                .or(fallback)
        }
    };
    let base = std::env::var(env)
        .ok()
        .map(|v| parse(&json!(v), Some(default)))
        .unwrap_or(Some(default));
    config["keep_warm"]
        .get(key)
        .map(|v| parse(v, base))
        .unwrap_or(base)
}
fn idle_due(profile: &Value, idle: f64, streak: &mut u32) -> bool {
    if setting(
        profile,
        "idle_unload_seconds",
        "LOCAL_LLM_KEEP_WARM_IDLE_SECONDS",
        1e-9,
        7200.0,
    )
    .is_some_and(|v| idle >= v)
    {
        return true;
    }
    let grace = setting(
        profile,
        "pressure_grace_seconds",
        "LOCAL_LLM_PRESSURE_GRACE_SECONDS",
        0.0,
        60.0,
    )
    .unwrap_or(60.0);
    if idle < grace {
        *streak = 0;
        return false;
    }
    let psi = setting(
        profile,
        "psi_some_avg10",
        "LOCAL_LLM_PRESSURE_PSI_SOME_AVG10",
        1e-9,
        10.0,
    );
    let floor = setting(
        profile,
        "min_available_mib",
        "LOCAL_LLM_PRESSURE_MIN_AVAILABLE_MIB",
        1e-9,
        1024.0,
    );
    let current = fs::read_to_string("/proc/pressure/memory")
        .ok()
        .and_then(|text| {
            text.lines()
                .find(|line| line.starts_with("some "))
                .and_then(|line| {
                    line.split_whitespace().find_map(|part| {
                        part.strip_prefix("avg10=")
                            .and_then(|v| v.parse::<f64>().ok())
                    })
                })
        });
    let pressure = psi
        .zip(current)
        .is_some_and(|(threshold, value)| value.is_finite() && value >= threshold)
        || floor
            .zip(guard::available_memory())
            .is_some_and(|(threshold, value)| (value as f64) < threshold * 1024.0 * 1024.0);
    *streak = if pressure { *streak + 1 } else { 0 };
    *streak >= 3
}
fn has_state_cache(path: &Path) -> Result<bool, String> {
    binary_holds(
        path,
        &[
            b"--state-cache ",
            b"--state-budget-bytes ",
            b"--state-ttl-secs ",
        ],
    )
}
fn binary_holds(path: &Path, flags: &[&[u8]]) -> Result<bool, String> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|_| "The engine binary became unreadable.")?
        .take(256 * 1024 * 1024 + 1);
    let mut found = vec![false; flags.len()];
    let mut buffer = vec![0_u8; 1024 * 1024 + 64];
    let mut tail = 0;
    loop {
        let n = file
            .read(&mut buffer[tail..])
            .map_err(|_| "The engine binary became unreadable.")?;
        if n == 0 {
            break;
        }
        let end = tail + n;
        for (i, flag) in flags.iter().enumerate() {
            found[i] |= buffer[..end]
                .windows(flag.len())
                .any(|window| window == *flag);
        }
        if found.iter().all(|v| *v) {
            return Ok(true);
        }
        tail = end.min(64);
        buffer.copy_within(end - tail..end, 0);
    }
    Ok(false)
}
// Keep a potential partial credential until the next read. A pipe read may end
// at any byte, including in the middle of the credential.
fn redact(pending: &mut Vec<u8>, token: &[u8], finish: bool) -> Vec<u8> {
    let mut out = Vec::new();
    let mut index = 0;
    while index < pending.len() {
        if pending[index..].starts_with(token) {
            out.extend_from_slice(b"<redacted>");
            index += token.len();
        } else if !finish && token.starts_with(&pending[index..]) {
            break;
        } else {
            out.push(pending[index]);
            index += 1;
        }
    }
    if finish {
        out.extend_from_slice(&pending[index..]);
        index = pending.len();
    }
    pending.drain(..index);
    out
}
fn spawn_stderr(mut stderr: tokio::process::ChildStderr, token: String) -> JoinHandle<()> {
    tokio::spawn(async move {
        let path = paths::xdg("XDG_STATE_HOME", ".local/state").join("local-ai/engine-stderr.log");
        let mut writer = crate::diagnostics::Writer::new(path);
        let mut buffer = [0_u8; 4096];
        let mut pending = Vec::new();
        loop {
            let n = stderr.read(&mut buffer).await.unwrap_or(0);
            pending.extend_from_slice(&buffer[..n]);
            writer.feed(&redact(&mut pending, token.as_bytes(), n == 0));
            if n == 0 {
                writer.finish();
                return;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn log_redaction_handles_every_pipe_boundary() {
        let token = b"synthetic-credential-for-pipe-boundaries";
        let input = [b"before ".as_slice(), token, b" between ", token, b" after"].concat();
        for chunk_size in 1..=input.len() {
            let mut pending = Vec::new();
            let mut output = Vec::new();
            for chunk in input.chunks(chunk_size) {
                pending.extend_from_slice(chunk);
                output.extend(super::redact(&mut pending, token, false));
            }
            output.extend(super::redact(&mut pending, token, true));
            assert_eq!(output, b"before <redacted> between <redacted> after");
        }
    }
}
