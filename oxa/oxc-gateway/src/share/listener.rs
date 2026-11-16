//! Bounded, explicit guest HTTP serving. Uses the caller's Tokio runtime;
//! imports start no runtime, listener, refresh worker or model supervisor.
use super::{
    admission::Admission,
    keys::{Error as KeyError, Store},
    protocol,
    status::{Activity, Status, MAX_CONNECTIONS},
};
pub use oxa_proto::CancelToken;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::{mpsc, Semaphore},
    task::JoinSet,
    time::timeout,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    BadRequest,
    Unauthorized,
    Forbidden,
    NotFound,
    TooLarge,
    Limited,
    Busy,
    Failed,
}
impl Failure {
    fn status(self) -> u16 {
        match self {
            Self::BadRequest => 400,
            Self::Unauthorized => 401,
            Self::Forbidden => 403,
            Self::NotFound => 404,
            Self::TooLarge => 413,
            Self::Limited => 429,
            Self::Busy => 503,
            Self::Failed => 502,
        }
    }
}
impl From<KeyError> for Failure {
    fn from(e: KeyError) -> Self {
        match e {
            KeyError::RequestLimit | KeyError::TokenLimit => Self::Limited,
            _ => Self::Busy,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Local,
    Metered,
    Subscription,
}
#[derive(Clone, Debug)]
pub struct Model {
    pub alias: String,
    pub context_window: u32,
    pub max_output: u32,
    pub access: Access,
}
/// Only validated guest text/history is present, never owner tools or context.
pub struct Request {
    pub model: String,
    pub messages: Vec<Value>,
    pub max_output: u32,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub include_usage: bool,
}
#[derive(Clone, Copy, Debug)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
}
#[derive(Clone, Copy, Debug)]
pub enum Finish {
    Stop,
    Length,
    Refusal,
}
pub struct Completed {
    pub usage: Option<Usage>,
    pub finish: Finish,
}
/// Bounded backpressure; a backend must stop sending and clean up on cancellation.
pub struct Output {
    send: mpsc::Sender<String>,
    cancel: CancelToken,
}
impl Output {
    pub async fn text(&self, text: &str) -> Result<(), Failure> {
        if text.len() > 65536 {
            return Err(Failure::TooLarge);
        }
        tokio::select! {
            _ = self.cancel.cancelled() => Err(Failure::Failed),
            result = self.send.send(text.into()) => result.map_err(|_| Failure::Failed),
        }
    }
}
/// The future must retain model ownership until generation/cancellation cleanup
/// has stopped, and never detach background model work. The listener joins it.
pub trait Answer: Send + Sync + 'static {
    fn models(&self) -> &[Model];
    fn chat(
        &self,
        request: Request,
        output: Output,
        cancel: CancelToken,
    ) -> impl Future<Output = Result<Completed, Failure>> + Send;
}
/// Production detection/LocalAPI access is injected; tests use loopback fakes.
pub trait Tailnet: Send + Sync + 'static {
    fn addresses(&self) -> impl Future<Output = Result<Vec<IpAddr>, Failure>> + Send;
    fn same_owner(&self, peer: IpAddr) -> impl Future<Output = Result<bool, Failure>> + Send;
}
#[derive(Clone, Copy, Default)]
pub enum Binding {
    #[default]
    Tailnet,
    Explicit(IpAddr),
}
#[derive(Clone)]
pub struct Config {
    pub binding: Binding,
    pub port: u16,
    pub same_owner: bool,
    /// Local models are eligible; metered routes require this separate allowlist.
    pub remote_models: BTreeSet<String>,
    pub connections: usize,
    pub max_body: usize,
    pub requests_per_minute: u32,
    pub peer_requests_per_minute: u32,
    pub read_timeout: Duration,
    pub write_timeout: Duration,
    pub request_timeout: Duration,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            binding: Binding::Tailnet,
            port: 0,
            same_owner: false,
            remote_models: BTreeSet::new(),
            connections: 8,
            max_body: 1024 * 1024,
            requests_per_minute: 60,
            peer_requests_per_minute: 120,
            read_timeout: Duration::from_secs(10),
            write_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(1800),
        }
    }
}
pub struct Server<A, T> {
    listener: TcpListener,
    status: Status,
    keys: Arc<Store>,
    answer: Arc<A>,
    tailnet: Arc<T>,
    config: Config,
}
impl<A: Answer, T: Tailnet> Server<A, T> {
    pub async fn bind(
        keys: Arc<Store>,
        answer: Arc<A>,
        tailnet: Arc<T>,
        config: Config,
    ) -> Result<Self, Failure> {
        if !(1..=MAX_CONNECTIONS).contains(&config.connections)
            || !(1..=1024 * 1024).contains(&config.max_body)
            || config.requests_per_minute == 0
            || config.peer_requests_per_minute == 0
            || [config.read_timeout, config.write_timeout, config.request_timeout]
                .iter()
                .any(Duration::is_zero)
            || answer.models().len() > 64
        {
            return Err(Failure::BadRequest);
        }
        let mut seen = BTreeSet::new();
        for model in answer.models() {
            if model.alias.is_empty()
                || model.alias.len() > 256
                || model.context_window == 0
                || model.max_output == 0
                || !seen.insert(&model.alias)
            {
                return Err(Failure::BadRequest);
            }
        }
        let address = match config.binding {
            Binding::Explicit(ip) => ip, // Includes wildcard only on explicit owner selection.
            Binding::Tailnet => {
                let mut addresses = timeout(config.read_timeout, tailnet.addresses())
                    .await
                    .map_err(|_| Failure::Busy)??;
                if addresses.len() > 8 {
                    return Err(Failure::BadRequest);
                }
                addresses.sort_unstable();
                addresses.dedup();
                if addresses
                    .iter()
                    .any(|ip| ip.is_unspecified() || ip.is_multicast())
                {
                    return Err(Failure::BadRequest);
                }
                let v4: Vec<_> = addresses.iter().filter(|ip| ip.is_ipv4()).copied().collect();
                match (v4.as_slice(), addresses.as_slice()) {
                    ([ip], _) => *ip,
                    ([], [ip]) => *ip,
                    _ => return Err(Failure::Forbidden),
                }
            }
        };
        let listener = TcpListener::bind((address, config.port))
            .await
            .map_err(|_| Failure::Busy)?;
        Ok(Self {
            listener,
            status: Status::new(),
            keys,
            answer,
            tailnet,
            config,
        })
    }
    pub fn status(&self) -> Status {
        self.status.clone()
    }

    pub fn address(&self) -> Result<SocketAddr, Failure> {
        self.listener.local_addr().map_err(|_| Failure::Failed)
    }
    /// No permanent worker pool: at most `connections` tasks, plus this accept
    /// future. Shutdown wakes network waits and joins backend cleanup.
    pub async fn serve(self, stop: CancelToken) -> Result<(), Failure> {
        let permits = Arc::new(Semaphore::new(self.config.connections));
        let rates = Arc::new(Mutex::new(Rates::default()));
        let storage = Storage {
            keys: self.keys.clone(),
            slots: Arc::new(Semaphore::new(self.config.connections)),
        };
        let mut tasks = JoinSet::new();
        let mut failure = None;
        loop {
            while tasks.try_join_next().is_some() {}
            let permit = tokio::select! { biased;
                _ = stop.cancelled() => break,
                p = permits.clone().acquire_owned() => match p { Ok(p) => p, Err(_) => { failure = Some(Failure::Failed); break; } },
            };
            let (socket, _) = tokio::select! { biased;
                _ = stop.cancelled() => break,
                r = self.listener.accept() => match r { Ok(v) => v, Err(_) => { failure = Some(Failure::Failed); break; } },
            };
            let (answer, tailnet, config, rates) = (
                self.answer.clone(),
                self.tailnet.clone(),
                self.config.clone(),
                rates.clone(),
            );
            let global = stop.clone();
            let status = self.status.clone();
            let storage = storage.clone();
            tasks.spawn(async move {
                let _permit = permit;
                let Ok(activity) = status.open() else {
                    return;
                };
                let cancel = CancelToken::new();
                let future = connection(
                    socket,
                    &storage,
                    answer.as_ref(),
                    tailnet.as_ref(),
                    &config,
                    &rates,
                    ConnectionContext {
                        cancel: cancel.clone(),
                        activity,
                    },
                );
                tokio::pin!(future);
                tokio::select! {
                    _ = &mut future => (),
                    _ = global.cancelled() => { cancel.cancel(); future.await; }
                }
            });
        }
        stop.cancel();
        while tasks.join_next().await.is_some() {}
        failure.map_or(Ok(()), Err)
    }
}

// A running operation owns its slot even if its waiting connection is cancelled.
// Thus dropped waits cannot create an unbounded spawn_blocking backlog.
#[derive(Clone)]
struct Storage {
    keys: Arc<Store>,
    slots: Arc<Semaphore>,
}
impl Storage {
    async fn run<R: Send + 'static>(
        &self,
        operation: impl FnOnce(&Store) -> Result<R, KeyError> + Send + 'static,
    ) -> Result<R, KeyError> {
        let keys = self.keys.clone();
        blocking(&self.slots, move || operation(&keys)).await
    }

    async fn settle(&self, mut admission: Admission, total: u64, limit: Duration) {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            let result = self
                .run(move |_| {
                    let result = admission.settle(total);
                    Ok((admission, result))
                })
                .await;
            match result {
                Ok((held, Err(KeyError::Busy))) if tokio::time::Instant::now() < deadline => {
                    admission = held;
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                // Unknown storage outcomes retain the conservative charge. They
                // must not truncate a successfully generated OpenAI stream.
                _ => return,
            }
        }
    }
}

async fn blocking<R: Send + 'static>(
    slots: &Arc<Semaphore>,
    operation: impl FnOnce() -> Result<R, KeyError> + Send + 'static,
) -> Result<R, KeyError> {
    let permit = slots.clone().try_acquire_owned().map_err(|_| KeyError::Busy)?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        operation()
    })
    .await
    .map_err(|_| KeyError::Unavailable)?
}

#[cfg(test)]
mod storage_tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn stalled_storage_preserves_runtime_progress_and_bounds_work() {
        let slots = Arc::new(Semaphore::new(1));
        let worker_slots = slots.clone();
        let (release, wait) = std::sync::mpsc::channel();
        let (started, ready) = tokio::sync::oneshot::channel();
        let began = Instant::now();
        let worker = tokio::spawn(async move {
            blocking(&worker_slots, move || {
                started.send(()).unwrap();
                let _ = wait.recv_timeout(Duration::from_secs(1));
                Ok(())
            })
            .await
        });
        ready.await.unwrap();
        // This timer and cancellation must progress on the same single thread
        // while the filesystem operation is deliberately held elsewhere.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(began.elapsed() < Duration::from_millis(500));
        let stop = CancelToken::new();
        stop.cancel();
        stop.cancelled().await;
        assert_eq!(blocking(&slots, || Ok(())).await, Err(KeyError::Busy));
        assert!(!worker.is_finished());
        release.send(()).unwrap();
        worker.await.unwrap().unwrap();
        blocking(&slots, || Ok(())).await.unwrap();
    }
}

#[derive(Default)]
struct Rates {
    peers: BTreeMap<IpAddr, (Instant, u32)>,
    keys: BTreeMap<String, (Instant, u32)>,
}
fn rate<K: Ord>(
    map: &mut BTreeMap<K, (Instant, u32)>,
    key: K,
    cap: usize,
    limit: u32,
) -> Result<(), Failure> {
    let now = Instant::now();
    map.retain(|_, (at, _)| now.duration_since(*at) < Duration::from_secs(60));
    if !map.contains_key(&key) && map.len() >= cap {
        return Err(Failure::Limited);
    }
    let row = map.entry(key).or_insert((now, 0));
    if row.1 >= limit {
        return Err(Failure::Limited);
    }
    row.1 += 1;
    Ok(())
}
fn eligible(model: &Model, config: &Config) -> bool {
    model.access == Access::Local
        || (model.access == Access::Metered && config.remote_models.contains(&model.alias))
}
struct ConnectionContext {
    cancel: CancelToken,
    activity: Activity,
}

async fn connection<A: Answer, T: Tailnet>(
    socket: TcpStream,
    keys: &Storage,
    answer: &A,
    tailnet: &T,
    config: &Config,
    rates: &Mutex<Rates>,
    context: ConnectionContext,
) {
    let ConnectionContext { cancel, mut activity } = context;
    let peer = match socket.peer_addr() {
        Ok(peer) => peer.ip(),
        Err(_) => return,
    };
    let mut reader = BufReader::new(socket);
    let result = {
        let result = async {
            rate(
                &mut rates.lock().map_err(|_| Failure::Busy)?.peers,
                peer,
                256,
                config.peer_requests_per_minute,
            )?;
            let head = timeout(config.read_timeout, async {
                let mut bytes = Vec::new();
                while !bytes.ends_with(b"\r\n\r\n") {
                    if bytes.len() >= 16384 {
                        return Err(Failure::TooLarge);
                    }
                    bytes.push(reader.read_u8().await.map_err(|_| Failure::BadRequest)?);
                }
                protocol::head(&bytes, config.max_body)
            })
            .await
            .map_err(|_| Failure::BadRequest)??;
            let bearer = head.bearer.clone();
            let key = keys
                .run(move |store| store.try_authenticate(&bearer))
                .await?
                .ok_or(Failure::Unauthorized)?;
            rate(
                &mut rates.lock().map_err(|_| Failure::Busy)?.keys,
                key.id.clone(),
                64,
                config.requests_per_minute,
            )?;
            if config.same_owner
                && !timeout(config.read_timeout, tailnet.same_owner(peer))
                    .await
                    .map_err(|_| Failure::Forbidden)??
            {
                return Err(Failure::Forbidden);
            }
            activity.authorize(&key)?;
            if !head.chat {
                let models: Vec<_> = answer.models().iter().filter(|m| eligible(m, config))
                .map(|m| json!({"id":m.alias,"object":"model","context_window":m.context_window,"remote":m.access!=Access::Local})).collect();
                return Ok((head, None, Some(json!({"object":"list","data":models}))));
            }
            let mut body = vec![0; head.length];
            timeout(config.read_timeout, reader.read_exact(&mut body))
                .await
                .map_err(|_| Failure::BadRequest)?
                .map_err(|_| Failure::BadRequest)?;
            let request = protocol::request(&body, |alias| {
                answer
                    .models()
                    .iter()
                    .find(|m| m.alias == alias && eligible(m, config))
                    .map(|m| m.max_output)
            })?;
            Ok((head, Some(request), None))
        };
        tokio::pin!(result);
        tokio::select! {
            _ = cancel.cancelled() => {
                // Join any admitted storage operation; shutdown never abandons its
                // charge ownership. Network reads retain their existing deadline.
                let _ = result.await;
                return;
            },
            r = &mut result => r,
        }
    };
    let mut socket = reader.into_inner();
    let (head, request, models) = match result {
        Ok(value) => value,
        Err(failure) => {
            let _ = response(
                &mut socket,
                failure.status(),
                &json!({"error":{"message":"Guest request refused","code":failure.status()}}),
                config,
            )
            .await;
            return;
        }
    };
    if let Some(models) = models {
        let _ = response(&mut socket, 200, &models, config).await;
        return;
    }
    let Some(request) = request else {
        return;
    };
    let Some(model) = answer.models().iter().find(|m| m.alias == request.model) else {
        return;
    };
    let reserved = u64::from(model.context_window) + u64::from(request.max_output);
    let created = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(t) => t.as_secs(),
        Err(_) => return,
    };
    let mut random = [0; 16];
    if getrandom::fill(&mut random).is_err() {
        return;
    }
    let stream = StreamMeta {
        model: request.model.clone(),
        created,
        id: random.iter().map(|b| format!("{b:02x}")).collect(),
    };
    let bearer = head.bearer;
    let admission = match keys
        .run(move |store| store.admit(&bearer, created / 86400, reserved))
        .await
    {
        Ok(Some(a)) => a,
        other => {
            let failure = other.err().map(Failure::from).unwrap_or(Failure::Unauthorized);
            let _ = response(
                &mut socket,
                failure.status(),
                &json!({"error":{"message":"Guest admission refused","code":failure.status()}}),
                config,
            )
            .await;
            return;
        }
    };
    if send(&mut socket, b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n", config).await.is_err() {
        keys.settle(admission, 0, config.write_timeout).await;
        return;
    }
    let (mut incoming, mut socket) = socket.into_split();
    if event(
        &mut socket,
        &stream.chunk(json!({"role":"assistant"}), Value::Null),
        config,
    )
    .await
    .is_err()
    {
        keys.settle(admission, 0, config.write_timeout).await;
        return;
    }
    let (tx, mut rx) = mpsc::channel(4);
    let include_usage = request.include_usage;
    let output_cap = request.max_output;
    let backend = async {
        let future = answer.chat(
            request,
            Output {
                send: tx,
                cancel: cancel.clone(),
            },
            cancel.clone(),
        );
        tokio::pin!(future);
        tokio::select! {
            r = &mut future => r,
            _ = tokio::time::sleep(config.request_timeout) => {
                cancel.cancel();
                // Cancellation is a request; retain ownership until the backend
                // acknowledges cleanup rather than dropping a live future.
                let _ = future.await;
                Err(Failure::Failed)
            }
        }
    };
    let writer = async {
        loop {
            let text = tokio::select! { _ = cancel.cancelled() => return Err(Failure::Failed), text = rx.recv() => text };
            let Some(text) = text else {
                return Ok(());
            };
            let value = stream.chunk(json!({"content":text}), Value::Null);
            let written = tokio::select! {
                _ = cancel.cancelled() => Err(Failure::Failed),
                r = event(&mut socket,&value,config) => r,
            };
            if written.is_err() {
                cancel.cancel();
                return Err(Failure::Failed);
            }
        }
    };
    let (completed, delivered) = {
        let work = async { tokio::join!(backend, writer) };
        tokio::pin!(work);
        tokio::select! {
            result = &mut work => result,
            _ = incoming.read_u8() => {
                // A streaming connection accepts one request only. EOF, reset,
                // or pipelined bytes stop generation; join its cleanup as well.
                cancel.cancel();
                work.await
            }
        }
    };
    let Ok(completed) = completed else {
        return;
    };
    if let Some(usage) = completed.usage {
        if usage.input > u64::from(model.context_window) || usage.output > u64::from(output_cap) {
            return;
        }
        let Some(total) = usage.input.checked_add(usage.output) else {
            return;
        };
        keys.settle(admission, total, config.write_timeout).await;
    } // Unknown usage leaves the conservative charge in place.
    if delivered.is_err() || cancel.is_cancelled() {
        return;
    }
    let finish = match completed.finish {
        Finish::Stop => "stop",
        Finish::Length => "length",
        Finish::Refusal => "content_filter",
    };
    if event(&mut socket, &stream.chunk(json!({}), json!(finish)), config)
        .await
        .is_err()
    {
        return;
    }
    if include_usage {
        if let Some(u) = completed.usage {
            let value = json!({"id":stream.id,"created":stream.created,"object":"chat.completion.chunk","model":stream.model,"choices":[],"usage":{"prompt_tokens":u.input,"completion_tokens":u.output,"total_tokens":u.input+u.output}});
            if event(&mut socket, &value, config).await.is_err() {
                return;
            }
        }
    }
    let _ = send(&mut socket, b"data: [DONE]\n\n", config).await;
}
struct StreamMeta {
    model: String,
    id: String,
    created: u64,
}
impl StreamMeta {
    fn chunk(&self, delta: Value, finish: Value) -> Value {
        json!({"id":self.id,"created":self.created,"object":"chat.completion.chunk","model":self.model,"choices":[{"index":0,"delta":delta,"finish_reason":finish}]})
    }
}
async fn send<W: AsyncWrite + Unpin>(socket: &mut W, bytes: &[u8], config: &Config) -> Result<(), Failure> {
    timeout(config.write_timeout, socket.write_all(bytes))
        .await
        .map_err(|_| Failure::Failed)?
        .map_err(|_| Failure::Failed)
}
async fn event<W: AsyncWrite + Unpin>(socket: &mut W, value: &Value, config: &Config) -> Result<(), Failure> {
    let mut bytes = b"data: ".to_vec();
    serde_json::to_writer(&mut bytes, value).map_err(|_| Failure::Failed)?;
    bytes.extend_from_slice(b"\n\n");
    send(socket, &bytes, config).await
}
async fn response(
    socket: &mut TcpStream,
    status: u16,
    value: &Value,
    config: &Config,
) -> Result<(), Failure> {
    let body = serde_json::to_vec(value).map_err(|_| Failure::Failed)?;
    let header = format!("HTTP/1.1 {status} Result\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
    send(socket, header.as_bytes(), config).await?;
    send(socket, &body, config).await
}
