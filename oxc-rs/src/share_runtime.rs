//! Shared listener ownership, authentication and per-device gates.
use crate::{daemon::Context, paths, request_ledger, sharing};
use axum::{extract::Request, http::Method, response::Response, Router};
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    os::unix::fs::MetadataExt,
    process::Stdio,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;
use tokio::{
    net::TcpSocket,
    sync::{watch, Mutex as AsyncMutex, OwnedSemaphorePermit, Semaphore},
};

pub struct Preempt(watch::Sender<bool>);
impl Preempt {
    pub fn new() -> Arc<Self> {
        Arc::new(Self(watch::channel(false).0))
    }
    pub fn cancel(&self) {
        self.0.send_replace(true);
    }
    pub fn cancelled(&self) -> bool {
        *self.0.borrow()
    }
    pub async fn wait(&self) {
        let mut receiver = self.0.subscribe();
        while !*receiver.borrow_and_update() {
            if receiver.changed().await.is_err() {
                return;
            }
        }
    }
}
pub struct Remote {
    _permit: OwnedSemaphorePermit,
    pub preempt: Arc<Preempt>,
    pub client: String,
    pub max_tokens: u64,
}
struct Listener {
    address: IpAddr,
    closed: AtomicBool,
    counts: Mutex<HashMap<IpAddr, usize>>,
}
struct ConnectionCount {
    listener: Arc<Listener>,
    peer: IpAddr,
}
impl Drop for ConnectionCount {
    fn drop(&mut self) {
        let mut counts = self
            .listener
            .counts
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(count) = counts.get_mut(&self.peer) {
            *count -= 1;
            if *count == 0 {
                counts.remove(&self.peer);
            }
        }
    }
}
#[derive(Clone)]
pub struct Connection {
    listener: Arc<Listener>,
    peer: IpAddr,
    _count: Arc<ConnectionCount>,
}
struct Listening {
    listener: Arc<Listener>,
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}
impl Listening {
    async fn close(self) {
        self.listener.closed.store(true, Ordering::SeqCst);
        self.stop.send_replace(true);
        let _ = self.task.await;
    }
}
type Factory = Arc<dyn Fn(Connection) -> Option<Router> + Send + Sync>;
type Signature = Option<(u64, u64, i64, i64)>;
fn signature() -> Signature {
    std::fs::metadata(paths::config())
        .ok()
        .map(|m| (m.ino(), m.len(), m.mtime(), m.mtime_nsec()))
}
struct State {
    config: Value,
    mode: String,
    error: Option<String>,
    listeners: Vec<Listening>,
    buckets: Vec<(IpAddr, f64, Instant)>,
    signature: Signature,
}
pub struct Manager {
    port: u16,
    ledger: Arc<request_ledger::Ledger>,
    salt: [u8; 32],
    state: AsyncMutex<State>,
    serial: AsyncMutex<()>,
    factory: Mutex<Option<Factory>>,
    slot: Arc<Semaphore>,
    requests: AtomicU64,
    refused: AtomicU64,
    stop: AtomicBool,
}
impl Manager {
    pub fn new(port: u16, ledger: Arc<request_ledger::Ledger>) -> Result<Arc<Self>, String> {
        let mut salt = [0; 32];
        getrandom::fill(&mut salt).map_err(|_| "Sharing privacy salt is unavailable.")?;
        Ok(Arc::new(Self {
            port,
            ledger,
            salt,
            state: AsyncMutex::new(State {
                config: json!({}),
                mode: "off".into(),
                error: None,
                listeners: Vec::new(),
                buckets: Vec::new(),
                signature: None,
            }),
            serial: AsyncMutex::new(()),
            factory: Mutex::new(None),
            slot: Arc::new(Semaphore::new(1)),
            requests: AtomicU64::new(0),
            refused: AtomicU64::new(0),
            stop: AtomicBool::new(false),
        }))
    }
    pub async fn start(self: &Arc<Self>, factory: Factory) {
        *self.factory.lock().unwrap_or_else(|e| e.into_inner()) = Some(factory);
        self.apply().await;
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(10)).await;
                let Some(manager) = weak.upgrade() else {
                    return;
                };
                if manager.stop.load(Ordering::SeqCst) {
                    return;
                }
                let update = {
                    let state = manager.state.lock().await;
                    state.mode != "off" || state.signature != signature()
                };
                if update {
                    manager.apply().await;
                }
            }
        });
    }
    async fn apply(self: &Arc<Self>) {
        let _serial = self.serial.lock().await;
        self.apply_inner().await;
    }
    async fn apply_inner(self: &Arc<Self>) {
        let sig = signature();
        let config = paths::read_config().unwrap_or_else(|| json!({}));
        let mode = config["lan_share"]
            .as_str()
            .filter(|m| ["off", "tailscale", "lan"].contains(m))
            .unwrap_or("off")
            .to_owned();
        let (mut addresses, mut error) = if mode == "tailscale" {
            match discover().await {
                Ok(v) => (v, None),
                Err(e) => (Vec::new(), Some(e)),
            }
        } else if mode == "lan" {
            (
                Vec::new(),
                Some("LAN network pinning is not implemented by the Rust controller yet.".into()),
            )
        } else {
            (Vec::new(), None)
        };
        if mode != "off" {
            match tokio::task::spawn_blocking(sharing::ensure_key).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    addresses.clear();
                    error = Some(format!("Share key unavailable: {e}"));
                }
                Err(_) => {
                    addresses.clear();
                    error = Some("Share key unavailable".into());
                }
            }
        }
        if config["lan_share_options"]["tls_cert"]
            .as_str()
            .is_some_and(|s| std::path::Path::new(s).is_absolute())
            && config["lan_share_options"]["tls_key"]
                .as_str()
                .is_some_and(|s| std::path::Path::new(s).is_absolute())
            && !addresses.is_empty()
        {
            addresses.clear();
            error =
                Some("Shared TLS listeners are not implemented by the Rust controller yet.".into());
        }
        let mut state = self.state.lock().await;
        if sharing::option(&state.config, "rate_per_min", 120, 6000)
            != sharing::option(&config, "rate_per_min", 120, 6000)
            || sharing::option(&state.config, "burst", 30, 1000)
                != sharing::option(&config, "burst", 30, 1000)
        {
            state.buckets.clear();
        }
        let mut index = 0;
        while index < state.listeners.len() {
            if !addresses.contains(&state.listeners[index].listener.address) {
                state.listeners.remove(index).close().await;
            } else {
                index += 1;
            }
        }
        for address in addresses {
            if state
                .listeners
                .iter()
                .any(|l| l.listener.address == address)
            {
                continue;
            }
            let bound = (|| {
                let socket = if address.is_ipv4() {
                    TcpSocket::new_v4()?
                } else {
                    TcpSocket::new_v6()?
                };
                socket.set_reuseaddr(true)?;
                socket.bind(SocketAddr::new(address, self.port))?;
                socket.listen(5)
            })();
            let listener = match bound {
                Ok(listener) => listener,
                Err(e) => {
                    error = Some(format!("Could not share on {address}: {e}"));
                    continue;
                }
            };
            let meta = Arc::new(Listener {
                address,
                closed: AtomicBool::new(false),
                counts: Mutex::new(HashMap::new()),
            });
            let (stop, mut stopped) = watch::channel(false);
            let factory = self
                .factory
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let saved_meta = meta.clone();
            let task = tokio::spawn(async move {
                loop {
                    if *stopped.borrow() {
                        return;
                    }
                    let accepted = tokio::select! {_=stopped.changed()=>return,accepted=listener.accept()=>accepted};
                    let Ok((stream, peer)) = accepted else {
                        return;
                    };
                    let accepted = {
                        let mut counts = meta.counts.lock().unwrap_or_else(|e| e.into_inner());
                        if counts.values().sum::<usize>() >= 32
                            || counts.get(&peer.ip()).copied().unwrap_or(0) >= 8
                        {
                            false
                        } else {
                            *counts.entry(peer.ip()).or_default() += 1;
                            true
                        }
                    };
                    if !accepted {
                        drop(stream);
                        continue;
                    }
                    let count = Arc::new(ConnectionCount {
                        listener: meta.clone(),
                        peer: peer.ip(),
                    });
                    let connection = Connection {
                        listener: meta.clone(),
                        peer: peer.ip(),
                        _count: count,
                    };
                    if let Some(router) = factory.as_ref().and_then(|f| f(connection.clone())) {
                        let mut closed = stopped.clone();
                        tokio::spawn(async move {
                            let _connection = connection;
                            if !*closed.borrow() {
                                tokio::select! {
                                    _=crate::transport::serve(stream, router)=>{},
                                    _=closed.changed()=>{},
                                }
                            }
                        });
                    }
                }
            });
            state.listeners.push(Listening {
                listener: saved_meta,
                stop,
                task,
            });
        }
        state.signature = sig;
        state.mode = mode;
        state.error = error;
        state.config = config;
    }
    pub async fn set_mode(self: &Arc<Self>, mode: String) -> Result<Value, (u16, Value)> {
        let _serial = self.serial.lock().await;
        if mode == "lan" {
            return Err((
                503,
                json!({"error":"LAN network pinning is not implemented by the Rust controller yet."}),
            ));
        }
        let saved = tokio::task::spawn_blocking(move || {
            let mut config = match std::fs::read(paths::config()) {
                Ok(v) => serde_json::from_slice::<Value>(&v)?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
                Err(e) => return Err(e),
            };
            let object = config
                .as_object_mut()
                .ok_or(std::io::ErrorKind::InvalidData)?;
            object.insert("lan_share".into(), json!(mode));
            object.remove("lan_share_paused");
            object.remove("lan_share_pin");
            paths::write_config(&config)
        })
        .await;
        if !matches!(saved, Ok(Ok(()))) {
            return Err((
                503,
                json!({"error":"The LAN share setting could not be saved."}),
            ));
        }
        self.apply_inner().await;
        Ok(self.status().await)
    }
    pub async fn close(&self) {
        self.stop.store(true, Ordering::SeqCst);
        let mut state = self.state.lock().await;
        for listener in state.listeners.drain(..) {
            listener.close().await;
        }
    }
    pub async fn status(&self) -> Value {
        let state = self.state.lock().await;
        let mut out = sharing::status(&state.config, self.port);
        let addresses: Vec<_> = state
            .listeners
            .iter()
            .map(|l| l.listener.address.to_string())
            .collect();
        out["enabled"] = json!(state.mode != "off");
        out["mode"] = json!(state.mode);
        out["active"] = json!(!addresses.is_empty());
        out["urls"] = json!(addresses
            .iter()
            .map(|a| format!(
                "http://{}:{}/v1",
                if a.contains(':') {
                    format!("[{a}]")
                } else {
                    a.clone()
                },
                self.port
            ))
            .collect::<Vec<_>>());
        out["addresses"] = json!(addresses);
        out["error"] = json!(state.error);
        out["remote_active"] = json!(u8::from(self.slot.available_permits() == 0));
        out["remote_requests"] = json!(self.requests.load(Ordering::SeqCst));
        out["remote_refused"] = json!(self.refused.load(Ordering::SeqCst));
        out
    }
    fn refuse(
        &self,
        context: &Context,
        code: u16,
        message: &str,
        kind: Option<&str>,
    ) -> Box<Response> {
        self.refused.fetch_add(1, Ordering::SeqCst);
        let mut error = json!({"message":message});
        if let Some(kind) = kind {
            error["type"] = json!(kind);
        }
        let mut response = context.reply(code, json!({"error":error}));
        response
            .extensions_mut()
            .insert(crate::transport::CloseConnection);
        Box::new(response)
    }
    pub async fn gate(
        &self,
        connection: &Connection,
        request: &mut Request,
        context: &Context,
    ) -> Result<Arc<Remote>, Box<Response>> {
        let (max_body, max_tokens) = {
            let mut state = self.state.lock().await;
            if connection.listener.closed.load(Ordering::SeqCst)
                || state.mode == "off"
                || !state
                    .listeners
                    .iter()
                    .any(|l| Arc::ptr_eq(&l.listener, &connection.listener))
            {
                return Err(self.refuse(context, 503, "LAN share is off.", None));
            }
            let rate = sharing::option(&state.config, "rate_per_min", 120, 6000) as f64 / 60.0;
            let burst = sharing::option(&state.config, "burst", 30, 1000) as f64;
            let now = Instant::now();
            let previous = state
                .buckets
                .iter()
                .position(|b| b.0 == connection.peer)
                .map(|i| state.buckets.remove(i));
            let tokens = previous
                .map(|(_, tokens, at)| {
                    (tokens + now.duration_since(at).as_secs_f64() * rate).min(burst)
                })
                .unwrap_or(burst);
            let allowed = tokens >= 1.0;
            state.buckets.push((
                connection.peer,
                if allowed { tokens - 1.0 } else { tokens },
                now,
            ));
            if state.buckets.len() > 1024 {
                state.buckets.remove(0);
            }
            if !allowed {
                return Err(self.refuse(
                    context,
                    429,
                    "Too many requests from this device; slow down.",
                    Some("rate_limit_error"),
                ));
            }
            (
                sharing::option(&state.config, "max_body_mb", 8, 16) * 1024 * 1024,
                sharing::option(&state.config, "max_tokens", 16384, 1_000_000),
            )
        };
        let key = sharing::auth_key();
        let given = crate::daemon::presented_key(request.headers());
        let valid = given.zip(key.as_deref()).is_some_and(|(given, key)| {
            let left: [u8; 32] = Sha256::digest(given.as_bytes()).into();
            let right: [u8; 32] = Sha256::digest(key.as_bytes()).into();
            bool::from(left.ct_eq(&right))
        });
        if !valid {
            return Err(self.refuse(context,401,"Missing or wrong share key. Send \"Authorization: Bearer <key>\" (local-ai share key on the host).",Some("authentication_error")));
        }
        let raw = request.uri().to_string();
        let path = request.uri().path().trim_end_matches('/');
        let path = if path.is_empty() { "/" } else { path };
        let model = path.strip_prefix("/v1/models/");
        let allowed = raw.starts_with('/')
            && !raw.starts_with("//")
            && ((request.method() == Method::POST
                && ["/v1/chat/completions", "/v1/completions"].contains(&path))
                || ([Method::GET, Method::HEAD].contains(request.method())
                    && (path == "/v1/models"
                        || model.is_some_and(|m| {
                            !m.is_empty()
                                && m.len() <= 128
                                && m.bytes()
                                    .all(|c| c.is_ascii_alphanumeric() || b"._:-".contains(&c))
                                && path.trim_matches(['.', '/']) != "v1/models"
                        }))));
        if !allowed {
            return Err(self.refuse(context,403,"Only the OpenAI inference routes are shared (POST /v1/chat/completions, POST /v1/completions, GET /v1/models).",Some("permission_error")));
        }
        if request
            .headers()
            .get("transfer-encoding")
            .is_some_and(|v| !v.as_bytes().is_empty())
        {
            return Err(self.refuse(
                context,
                411,
                "Use a content length for request bodies.",
                None,
            ));
        }
        let length = request
            .headers()
            .get("content-length")
            .map(|v| v.to_str().unwrap_or(""))
            .unwrap_or("0")
            .trim()
            .parse::<i128>();
        let length = match length {
            Ok(n) => n,
            Err(_) => return Err(self.refuse(context, 400, "Invalid content length.", None)),
        };
        if length < 0 || length > max_body as i128 {
            return Err(self.refuse(
                context,
                413,
                &format!(
                    "Request is too large for a shared request (limit {} MiB).",
                    max_body / (1024 * 1024)
                ),
                None,
            ));
        }
        let permit =
            match tokio::time::timeout(Duration::from_secs(2), self.slot.clone().acquire_owned())
                .await
            {
                Ok(Ok(permit)) => permit,
                _ => {
                    return Err(self.refuse(
                        context,
                        429,
                        "Another shared request is running; retry when it finishes.",
                        Some("rate_limit_error"),
                    ))
                }
            };
        self.requests.fetch_add(1, Ordering::SeqCst);
        let uri = path.parse().expect("canonical shared route");
        let names = request
            .headers()
            .keys()
            .filter(|name| !FORWARD.contains(&name.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        for name in names {
            request.headers_mut().remove(name);
        }
        *request.uri_mut() = uri;
        let address = connection.peer.to_string();
        let client = self.ledger.shared_client(&address).unwrap_or_else(|| {
            let mut mac = Hmac::<Sha256>::new_from_slice(&self.salt).expect("HMAC salt");
            mac.update(format!("lan-share-client-v1\0{address}").as_bytes());
            format!("{:x}", mac.finalize().into_bytes())[..16].to_owned()
        });
        Ok(Arc::new(Remote {
            _permit: permit,
            preempt: Preempt::new(),
            client,
            max_tokens,
        }))
    }
}
const FORWARD: &[&str] = &[
    "host",
    "content-type",
    "content-length",
    "content-encoding",
    "accept",
    "user-agent",
    "connection",
    "origin",
    "sec-fetch-site",
    "sec-fetch-mode",
    "sec-fetch-dest",
    "sec-fetch-user",
    "x-session-id",
    "session-id",
];
async fn discover() -> Result<Vec<IpAddr>, String> {
    let mut command = tokio::process::Command::new("ip");
    command
        .args(["-j", "addr", "show"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(5), command.output())
        .await
        .map_err(|_| "Could not list network addresses: TimeoutExpired")?
        .map_err(|_| "Could not list network addresses: OSError")?;
    if !output.status.success() {
        return Err("Could not list network addresses: CalledProcessError".into());
    }
    let text = String::from_utf8(output.stdout)
        .map_err(|_| "Could not list network addresses: UnicodeDecodeError")?;
    let data: Value = serde_json::from_str(&text)
        .map_err(|_| "Could not list network addresses: JSONDecodeError")?;
    // The reference queries routes even for tailscale discovery.
    let mut routes = tokio::process::Command::new("ip");
    routes
        .args(["-j", "-4", "route", "show", "default"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let result = tokio::time::timeout(Duration::from_secs(5), routes.status())
        .await
        .map_err(|_| "Could not list network addresses: TimeoutExpired")?
        .map_err(|_| "Could not list network addresses: OSError")?;
    if !result.success() {
        return Err("Could not list network addresses: CalledProcessError".into());
    }
    let mut addresses = Vec::new();
    for link in data.as_array().into_iter().flatten() {
        if !link["ifname"]
            .as_str()
            .is_some_and(|s| s.starts_with("tailscale"))
        {
            continue;
        }
        for info in link["addr_info"].as_array().into_iter().flatten() {
            let Some(address) = info["local"]
                .as_str()
                .and_then(|s| s.parse::<IpAddr>().ok())
            else {
                continue;
            };
            let valid = match address {
                IpAddr::V4(a) => {
                    let b = a.octets();
                    b[0] == 100 && (64..128).contains(&b[1])
                }
                IpAddr::V6(a) => a.octets()[..6] == [0xfd, 0x7a, 0x11, 0x5c, 0xa1, 0xe0],
            };
            if valid && !addresses.contains(&address) {
                addresses.push(address);
            }
        }
    }
    if addresses.is_empty() {
        return Err("Tailscale is not up: no 100.64/10 or fd7a:115c:a1e0::/48 address on a tailscale interface.".into());
    }
    Ok(addresses)
}
pub fn payload(value: Value, limit: u64) -> Result<Value, ()> {
    let object = value.as_object().ok_or(())?;
    let mut out = serde_json::Map::new();
    for (name, value) in object {
        if PAYLOAD.contains(&name.as_str()) {
            out.insert(name.clone(), value.clone());
        }
    }
    if let Some(kwargs) = object
        .get("chat_template_kwargs")
        .and_then(Value::as_object)
    {
        let kept = kwargs
            .iter()
            .filter(|(k, v)| {
                ["enable_thinking", "preserve_thinking"].contains(&k.as_str()) && v.is_boolean()
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect::<serde_json::Map<_, _>>();
        if !kept.is_empty() {
            out.insert("chat_template_kwargs".into(), Value::Object(kept));
        }
    }
    if let Some(asked) = ["max_tokens", "max_completion_tokens"]
        .iter()
        .find_map(|k| object.get(*k).and_then(Value::as_u64).filter(|n| *n > 0))
    {
        out.insert("max_tokens".into(), json!(asked.min(limit)));
    }
    for message in out
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        for part in message["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_object)
        {
            for (name, value) in part {
                let url = if value.is_object() {
                    &value["url"]
                } else {
                    value
                };
                if name.ends_with("_url")
                    && !url.as_str().is_some_and(|s| {
                        s.get(..5).is_some_and(|s| s.eq_ignore_ascii_case("data:"))
                    })
                {
                    return Err(());
                }
            }
        }
    }
    Ok(Value::Object(out))
}
const PAYLOAD: &[&str] = &[
    "model",
    "messages",
    "prompt",
    "suffix",
    "echo",
    "stream",
    "stream_options",
    "temperature",
    "top_p",
    "top_k",
    "min_p",
    "stop",
    "seed",
    "presence_penalty",
    "frequency_penalty",
    "repeat_penalty",
    "logit_bias",
    "logprobs",
    "top_logprobs",
    "tools",
    "tool_choice",
    "parallel_tool_calls",
    "response_format",
    "reasoning_effort",
    "user",
];
