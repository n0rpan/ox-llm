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
    sync::{watch, Mutex as AsyncMutex, OwnedSemaphorePermit, Semaphore},
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
    props: Value,
}
struct State {
    running: Option<Running>,
    stop_pending: Option<String>,
    pressure_streak: u32,
}

pub struct Manager {
    pub harness: Mutex<crate::harness::Registry>,
    state: AsyncMutex<State>,
    loaded: RwLock<Option<Value>>,
    phase: AtomicU8,
    active: AtomicUsize,
    queued: AtomicUsize,
    slot: Arc<Semaphore>,
    last_used: Mutex<Instant>,
    last_failure: Mutex<Option<String>>,
    stop: watch::Sender<bool>,
    port: u16,
    preload: Mutex<Option<Value>>,
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
}
impl Drop for Admission {
    fn drop(&mut self) {
        self.manager.active.fetch_sub(1, Ordering::SeqCst);
        *self
            .manager
            .last_used
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Instant::now();
    }
}
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
            state: AsyncMutex::new(State {
                running: None,
                stop_pending: None,
                pressure_streak: 0,
            }),
            loaded: RwLock::new(None),
            phase: AtomicU8::new(0),
            active: AtomicUsize::new(0),
            queued: AtomicUsize::new(0),
            slot: Arc::new(Semaphore::new(1)),
            last_used: Mutex::new(Instant::now()),
            last_failure: Mutex::new(None),
            preload: Mutex::new(None),
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
        let memory = self
            .state
            .try_lock()
            .ok()
            .and_then(|s| s.running.as_ref().map(|r| r.guard.metadata()));
        json!({"state":self.phase(),"model":profile["model_alias"],"selected_model":models::resolve(config,None).ok().map(|p|p["model_alias"].clone()),
            "validated":models::eligible(&profile),"active_requests":self.active.load(Ordering::SeqCst),"queued_requests":self.queued.load(Ordering::SeqCst),
            "capabilities":["models","active"],"context_size":profile["context_size"],"memory_policy":memory,
            "last_error":self.last_failure.lock().unwrap_or_else(|e|e.into_inner()).clone(),"model_preload":self.preload.lock().unwrap_or_else(|e|e.into_inner()).clone()})
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
            let result = manager.load_inner(profile, true).await;
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
        // Never silently discard legacy stores or state-cache settings. Their
        // complete qualified launch adapters are ported separately.
        if args.iter().any(|arg| {
            ["--prefix-cache", "--session-cache"].contains(&arg.split('=').next().unwrap_or(arg))
        }) {
            return Err("The legacy conversation-store launch adapter is not implemented by the Rust controller yet.".into());
        }
        if has_state_cache(Path::new(&args[0]))? {
            return Err(
                "The state-cache launch adapter is not implemented by the Rust controller yet."
                    .into(),
            );
        }
        if profile["environment"]["LOCAL_AI_ENGINE_LEDGER"] == "1"
            && !args
                .iter()
                .any(|arg| arg == "--ledger" || arg.starts_with("--ledger="))
        {
            return Err(
                "The native engine-ledger bridge is not implemented by the Rust controller yet."
                    .into(),
            );
        }
        let mut command = Command::new("/proc/self/exe");
        command
            .arg("__engine_exec")
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
        self.phase.store(1, Ordering::SeqCst);
        *self.loaded.write().unwrap_or_else(|e| e.into_inner()) = Some(profile);
        let child = match command.spawn() {
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
        state.running = Some(Running {
            process,
            _lease: lease,
            token,
            guard,
            stderr,
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
        }
        state.running = None;
        state.stop_pending = None;
        state.pressure_streak = 0;
        *self.loaded.write().unwrap_or_else(|e| e.into_inner()) = None;
        self.phase.store(0, Ordering::SeqCst);
        Ok(())
    }
    pub async fn unload(&self, wanted: Option<&str>) -> Result<(), (u16, Value)> {
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
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|_| "The engine binary became unreadable.")?
        .take(256 * 1024 * 1024 + 1);
    let flags = [
        b"--state-cache ".as_slice(),
        b"--state-budget-bytes ".as_slice(),
        b"--state-ttl-secs ".as_slice(),
    ];
    let mut found = [false; 3];
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
