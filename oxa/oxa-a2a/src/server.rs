//! Authenticated loopback JSON-RPC/SSE server for owned agent backends.
//! Backends are trusted local code: methods must return bounded snapshots and
//! must not block indefinitely. Models never implement this trait directly.
use crate::loopback::{authorized, read_head, Capability, Client, DeadlineStream, Limits};
use crate::wire::*;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{self, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub trait Backend: Send + Sync + 'static {
    fn card(&self, endpoint: &str) -> AgentCard;
    fn send(&self, params: SendParams) -> Result<Event, RpcError>;
    fn get(&self, params: TaskParams) -> Result<Task, RpcError>;
    /// Return a verified canceled/terminal task or an error, never an
    /// unverified cancellation acknowledgment disguised as a canceled task.
    fn cancel(&self, id: &str) -> Result<Task, RpcError>;
    /// Stop and join owned work. Called only after HTTP handlers have stopped.
    fn shutdown(&self);
}

#[derive(Clone, Copy, Debug)]
pub struct ServerLimits {
    pub connections: usize,
    pub timeout: Duration,
    pub body_bytes: usize,
    pub event_bytes: usize,
    pub poll_interval: Duration,
}

impl Default for ServerLimits {
    fn default() -> Self {
        Self {
            connections: 16,
            timeout: Duration::from_secs(30),
            body_bytes: 1024 * 1024,
            event_bytes: 256 * 1024,
            poll_interval: Duration::from_millis(10),
        }
    }
}

type Sockets = Arc<Mutex<BTreeMap<u64, TcpStream>>>;

pub struct Server<B: Backend> {
    address: SocketAddr,
    capability: Capability,
    backend: Arc<B>,
    stopped: Arc<AtomicBool>,
    sockets: Sockets,
    listener: Option<JoinHandle<()>>,
}

impl<B: Backend> Server<B> {
    pub fn start(backend: Arc<B>, limits: ServerLimits) -> io::Result<Self> {
        Self::start_with_capability(backend, limits, Capability::generate()?)
    }

    /// Explicit operator-owned authentication for a separately hosted agent.
    /// Callers must supply an unpredictable private capability; cards/results
    /// never select it. Binding remains literal loopback on an ephemeral port.
    pub fn start_with_capability(
        backend: Arc<B>,
        limits: ServerLimits,
        capability: Capability,
    ) -> io::Result<Self> {
        if !(2..=32).contains(&limits.connections)
            || limits.timeout.is_zero()
            || limits.timeout > Duration::from_secs(60)
            || limits.body_bytes == 0
            || limits.body_bytes > 16 * 1024 * 1024
            || limits.event_bytes == 0
            || limits.event_bytes > limits.body_bytes
            || limits.poll_interval < Duration::from_millis(1)
            || limits.poll_interval > Duration::from_millis(100)
        {
            return Err(invalid("invalid server limits"));
        }
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let stopped = Arc::new(AtomicBool::new(false));
        let sockets = Arc::new(Mutex::new(BTreeMap::new()));
        let (stop, connections, auth, service) = (
            stopped.clone(),
            sockets.clone(),
            capability.clone(),
            backend.clone(),
        );
        let handle = thread::Builder::new()
            .name("oxa-a2a-listener".into())
            .spawn(move || {
                let mut handlers: Vec<JoinHandle<()>> = Vec::new();
                let streams = Arc::new(AtomicUsize::new(0));
                let mut next_id = 0u64;
                while !stop.load(Ordering::Acquire) {
                    let mut i = 0;
                    while i < handlers.len() {
                        if handlers[i].is_finished() {
                            let _ = handlers.swap_remove(i).join();
                        } else {
                            i += 1;
                        }
                    }
                    match listener.accept() {
                        Ok((socket, _)) => {
                            if handlers.len() == limits.connections {
                                let _ = socket.shutdown(Shutdown::Both);
                                continue;
                            }
                            let Some(id) = next_id.checked_add(1) else {
                                break;
                            };
                            next_id = id;
                            let Ok(owned) = socket.try_clone() else {
                                continue;
                            };
                            connections
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .insert(id, owned);
                            let lease = Connection {
                                id,
                                sockets: connections.clone(),
                            };
                            let (backend, stop, auth) = (service.clone(), stop.clone(), auth.clone());
                            let streams = streams.clone();
                            match thread::Builder::new().name("oxa-a2a-http".into()).spawn(move || {
                                let _lease = lease;
                                let _ =
                                    serve(socket, address, &auth, backend.as_ref(), limits, &stop, &streams);
                            }) {
                                Ok(handle) => handlers.push(handle),
                                Err(_) => {
                                    connections.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
                                }
                            }
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            thread::park_timeout(Duration::from_millis(2))
                        }
                        Err(_) => break,
                    }
                }
                for handle in handlers {
                    let _ = handle.join();
                }
            })?;
        Ok(Self {
            address,
            capability,
            backend,
            stopped,
            sockets,
            listener: Some(handle),
        })
    }

    pub fn address(&self) -> SocketAddr {
        self.address
    }

    pub fn client(&self, limits: Limits) -> io::Result<Client> {
        Client::new(self.address, self.capability.clone(), limits)
    }
}

impl<B: Backend> Drop for Server<B> {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        for socket in self.sockets.lock().unwrap_or_else(|e| e.into_inner()).values() {
            let _ = socket.shutdown(Shutdown::Both);
        }
        if let Some(handle) = self.listener.take() {
            handle.thread().unpark();
            let _ = handle.join();
        }
        self.backend.shutdown();
    }
}

struct Connection {
    id: u64,
    sockets: Sockets,
}
impl Drop for Connection {
    fn drop(&mut self) {
        self.sockets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.id);
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
pub fn error(code: i32, message: &'static str) -> RpcError {
    RpcError {
        code,
        message: message.into(),
        data: None,
    }
}

fn serve<B: Backend>(
    socket: TcpStream,
    address: SocketAddr,
    auth: &Capability,
    backend: &B,
    limits: ServerLimits,
    stopped: &AtomicBool,
    streams: &AtomicUsize,
) -> io::Result<()> {
    if stopped.load(Ordering::Acquire) {
        return Ok(());
    }
    let deadline = Instant::now() + limits.timeout;
    let mut reader = BufReader::new(DeadlineStream {
        socket,
        deadline,
        budget: None,
    });
    let head = read_head(&mut reader)?;
    if !authorized(&head, auth) {
        return http_json(
            reader.get_mut(),
            "401 Unauthorized",
            &json!({}),
            limits.body_bytes,
        );
    }
    if head.first == "GET /.well-known/agent-card.json HTTP/1.1" {
        let mut card = backend.card(&format!("http://{address}/"));
        card.url = format!("http://{address}/");
        card.protocol_version = PROTOCOL_VERSION.into();
        card.preferred_transport = "JSONRPC".into();
        card.capabilities.streaming = true;
        card.capabilities.push_notifications = false;
        card.security_schemes =
            BTreeMap::from([("capability".into(), json!({"type":"http","scheme":"bearer"}))]);
        card.security = vec![BTreeMap::from([("capability".into(), vec![])])];
        return http_json(reader.get_mut(), "200 OK", &card, limits.body_bytes);
    }
    if head.first != "POST / HTTP/1.1"
        || head.headers.get("content-type").and_then(|v| v.split(';').next()) != Some("application/json")
    {
        return http_json(reader.get_mut(), "400 Bad Request", &json!({}), limits.body_bytes);
    }
    let length = head.length(limits.body_bytes)?;
    let mut bytes = vec![0u8; length];
    reader.read_exact(&mut bytes)?;
    let value: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) => {
            return http_json(
                reader.get_mut(),
                "200 OK",
                &json!({"jsonrpc":"2.0","id":null,"error":error(-32700,"Parse error")}),
                limits.body_bytes,
            )
        }
    };
    let request: Request = match serde_json::from_value(value) {
        Ok(request) => request,
        Err(_) => {
            return http_json(
                reader.get_mut(),
                "200 OK",
                &json!({"jsonrpc":"2.0","id":null,"error":error(-32600,"Invalid request")}),
                limits.body_bytes,
            )
        }
    };
    let id = request.id.clone();
    let streaming = matches!(request.method.as_str(), "message/stream" | "tasks/resubscribe");
    // Keep one handler slot available for queries and cancellation instead of
    // allowing long-lived streams to occupy every connection.
    let _slot = if streaming {
        acquire_stream(streams, limits.connections - 1).then(|| StreamSlot(streams))
    } else {
        None
    };
    let result = if streaming && _slot.is_none() {
        Err(error(-32004, "Stream capacity reached"))
    } else {
        dispatch(backend, request)
    };
    if streaming {
        write!(reader.get_mut(),"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n")?;
        let mut writer = Events {
            stream: reader.get_mut(),
            limits,
            bytes: 0,
            id: &id,
        };
        match result {
            Ok(event) => stream_task(backend, &mut writer, event, deadline, stopped),
            Err(error) => writer.write(&json!({"jsonrpc":"2.0","id":id,"error":error})),
        }
    } else {
        let value = match result {
            Ok(event) => json!({"jsonrpc":"2.0","id":id,"result":event}),
            Err(error) => json!({"jsonrpc":"2.0","id":id,"error":error}),
        };
        http_json(reader.get_mut(), "200 OK", &value, limits.body_bytes)
    }
}

struct StreamSlot<'a>(&'a AtomicUsize);
impl Drop for StreamSlot<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn acquire_stream(streams: &AtomicUsize, max: usize) -> bool {
    let mut value = streams.load(Ordering::Acquire);
    while value < max {
        match streams.compare_exchange_weak(value, value + 1, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return true,
            Err(actual) => value = actual,
        }
    }
    false
}

fn dispatch<B: Backend>(backend: &B, request: Request) -> Result<Event, RpcError> {
    request.validate().map_err(|_| error(-32600, "Invalid request"))?;
    match request.method.as_str() {
        "message/send" | "message/stream" => {
            if request.params.get("configuration").is_some() {
                return Err(error(-32004, "Send configuration is unsupported"));
            }
            let params: SendParams =
                serde_json::from_value(request.params).map_err(|_| error(-32602, "Invalid params"))?;
            let result = backend.send(params)?;
            if !matches!(result, Event::Task(_) | Event::Message(_)) {
                return Err(error(-32006, "Invalid agent response"));
            }
            Ok(result)
        }
        "tasks/get" | "tasks/cancel" | "tasks/resubscribe" => {
            let params: TaskParams =
                serde_json::from_value(request.params).map_err(|_| error(-32602, "Invalid params"))?;
            let id = params.id.clone();
            let result = if request.method == "tasks/cancel" {
                backend.cancel(&id)?
            } else {
                backend.get(params)?
            };
            if result.id != id {
                return Err(error(-32006, "Invalid task response"));
            }
            if request.method == "tasks/cancel" && !result.status.state.is_terminal() {
                return Err(error(-32603, "Cancellation is not confirmed"));
            }
            Ok(Event::Task(result))
        }
        _ => Err(error(-32601, "Method not found")),
    }
}

fn stream_task<B: Backend>(
    backend: &B,
    writer: &mut Events<'_>,
    initial: Event,
    deadline: Instant,
    stopped: &AtomicBool,
) -> io::Result<()> {
    writer.event(&initial)?;
    let Event::Task(mut previous) = initial else {
        return Ok(());
    };
    loop {
        if previous.status.state.is_terminal() {
            return Ok(());
        }
        if matches!(
            previous.status.state,
            TaskState::InputRequired | TaskState::AuthRequired
        ) {
            writer.event(&Event::Status(StatusUpdate {
                task_id: previous.id,
                context_id: previous.context_id,
                status: previous.status,
                final_event: true,
            }))?;
            return Ok(());
        }
        if stopped.load(Ordering::Acquire) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "agent stream deadline exceeded",
            ));
        }
        thread::sleep(writer.limits.poll_interval);
        let next = match backend.get(TaskParams {
            id: previous.id.clone(),
            history_length: Some(0),
        }) {
            Ok(task) => task,
            Err(error) => {
                writer.write(&json!({"jsonrpc":"2.0","id":writer.id,"error":error}))?;
                return Ok(());
            }
        };
        if next.id != previous.id || next.context_id != previous.context_id {
            return Err(invalid("backend changed task identity"));
        }
        for artifact in &next.artifacts {
            if previous
                .artifacts
                .iter()
                .find(|a| a.artifact_id == artifact.artifact_id)
                != Some(artifact)
            {
                writer.event(&Event::Artifact(ArtifactUpdate {
                    task_id: next.id.clone(),
                    context_id: next.context_id.clone(),
                    artifact: artifact.clone(),
                    append: false,
                    last_chunk: true,
                }))?;
            }
        }
        if next.status != previous.status {
            let final_event = next.status.state.is_terminal()
                || matches!(
                    next.status.state,
                    TaskState::InputRequired | TaskState::AuthRequired
                );
            writer.event(&Event::Status(StatusUpdate {
                task_id: next.id.clone(),
                context_id: next.context_id.clone(),
                status: next.status.clone(),
                final_event,
            }))?;
            if final_event {
                return Ok(());
            }
        }
        previous = next;
    }
}

struct Events<'a> {
    stream: &'a mut DeadlineStream,
    limits: ServerLimits,
    bytes: usize,
    id: &'a RequestId,
}
impl Events<'_> {
    fn event(&mut self, event: &Event) -> io::Result<()> {
        self.write(&json!({"jsonrpc":"2.0","id":self.id,"result":event}))
    }
    fn write(&mut self, value: &Value) -> io::Result<()> {
        let bytes = encode(value, self.limits.event_bytes.saturating_sub(8))?;
        let n = bytes.len() + 8;
        if n > self.limits.body_bytes.saturating_sub(self.bytes) {
            return Err(invalid("stream output limit exceeded"));
        }
        self.stream.write_all(b"data: ")?;
        self.stream.write_all(&bytes)?;
        self.stream.write_all(b"\n\n")?;
        self.stream.flush()?;
        self.bytes += n;
        Ok(())
    }
}

fn http_json(writer: &mut impl Write, status: &str, value: &impl Serialize, max: usize) -> io::Result<()> {
    let bytes = encode(value, max)?;
    write!(writer,"HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",bytes.len())?;
    writer.write_all(&bytes)
}

fn encode(value: &impl Serialize, max: usize) -> io::Result<Vec<u8>> {
    struct Bounded {
        bytes: Vec<u8>,
        max: usize,
    }
    impl Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > self.max.saturating_sub(self.bytes.len()) {
                return Err(invalid("response size limit exceeded"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut output = Bounded {
        bytes: Vec::new(),
        max,
    };
    serde_json::to_writer(&mut output, value).map_err(|_| invalid("response encoding limit exceeded"))?;
    Ok(output.bytes)
}
