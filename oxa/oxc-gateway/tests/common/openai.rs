//! Scripted loopback OpenAI Responses upstream for tests only.
//! Never consults credentials, sign-ins, the environment, or an external service.
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, VecDeque},
    fmt,
    io::{self, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Condvar, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const DEADLINE: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct Request {
    pub method: String,
    pub target: String,
    pub body: Value,
    #[allow(dead_code)] // Used by replay transport consumers of this shared fixture.
    pub body_bytes: Vec<u8>,
    headers: BTreeMap<String, String>,
}
impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(&name.to_ascii_lowercase()).map(String::as_str)
    }
}
impl fmt::Debug for Request {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.debug_struct("Request")
            .field("method", &self.method)
            .field("target", &self.target)
            .field("body", &"[captured synthetic JSON]")
            .field("headers", &self.headers.keys().collect::<Vec<_>>())
            .finish()
    }
}

pub struct Reply {
    status: u16,
    content_type: &'static str,
    headers: Vec<(&'static str, String)>,
    bytes: Vec<u8>,
    fragment: usize,
    pub truncate_transfer: bool,
    gate: Option<mpsc::Receiver<()>>,
}
impl Reply {
    #[allow(dead_code)] // Shared fixture constructor; not every consumer refreshes tokens.
    pub fn json(value: Value) -> Self {
        Self {
            status: 200,
            content_type: "application/json",
            headers: vec![],
            bytes: value.to_string().into_bytes(),
            fragment: 7,
            truncate_transfer: false,
            gate: None,
        }
    }
    #[allow(dead_code)] // Shared fixture constructor for transport boundary cases.
    pub fn raw(
        status: u16,
        content_type: &'static str,
        bytes: Vec<u8>,
        headers: Vec<(&'static str, String)>,
    ) -> Self {
        Self {
            status,
            content_type,
            bytes,
            headers,
            fragment: 1024,
            truncate_transfer: false,
            gate: None,
        }
    }

    /// Script native Responses events, fragmented across HTTP chunks.
    pub fn events(events: Vec<Value>) -> Self {
        let mut bytes = Vec::new();
        for event in events {
            bytes.extend_from_slice(
                format!("event: {}\ndata: {event}\n\n", event["type"].as_str().unwrap()).as_bytes(),
            );
        }
        Self {
            status: 200,
            content_type: "text/event-stream",
            headers: Vec::new(),
            bytes,
            fragment: 7,
            truncate_transfer: false,
            gate: None,
        }
    }
    pub fn error(status: u16, kind: &str, message: &str) -> Self {
        Self {
            status,
            content_type: "application/json",
            headers: Vec::new(),
            bytes: json!({"error":{"type":kind,"code":kind,"message":message}})
                .to_string()
                .into_bytes(),
            fragment: 7,
            truncate_transfer: false,
            gate: None,
        }
    }
    /// A Responses quota refusal, including the server's retry delay in seconds.
    pub fn insufficient_quota(retry_after_seconds: u64) -> Self {
        Self {
            headers: vec![("Retry-After", retry_after_seconds.to_string())],
            ..Self::error(429, "insufficient_quota", "synthetic quota exhausted")
        }
    }
    /// Synthetic private subscription usage reply; no credentials or live service.
    #[allow(dead_code)] // Shared constructor; only usage-parser tests consume it.
    pub fn chatgpt_usage() -> Self {
        Self::json(json!({
            "plan_type": "synthetic-plan",
            "rate_limit": {
                "primary_window": {"used_percent": 12.5, "reset_at": 1_800_000_000},
                "secondary_window": {"used_percent": 75, "reset_at": 1_800_600_000}
            },
            "credits": {"has_credits": true, "unlimited": false, "balance": "42.50"}
        }))
    }
    /// #450: three parallel calls with all arguments delivered as deltas.
    /// Completed output is empty; output_item.done is authoritative.
    pub fn multi_tool_full_deltas() -> Self {
        Self::multi_tool(ArgumentDeltas::Full)
    }
    /// #450 reproduction: no argument events at all. Arguments exist only in
    /// output_item.done, and response.completed has an empty output array.
    pub fn multi_tool_no_deltas() -> Self {
        Self::multi_tool(ArgumentDeltas::None)
    }
    /// #450: only a prefix arrives as deltas; arguments.done and item.done carry
    /// the full arguments. Consumers must emit the missing suffix exactly once.
    pub fn multi_tool_partial_deltas() -> Self {
        Self::multi_tool(ArgumentDeltas::Partial)
    }
    fn multi_tool(shape: ArgumentDeltas) -> Self {
        let mut events = vec![json!({"type":"response.created","response":{
            "id":"resp_synthetic","object":"response","status":"in_progress",
            "model":"synthetic-openai","output":[]
        }})];
        let items: Vec<Value> = [
            ("read", json!({"path":"fixture.txt"})),
            ("bash", json!({"command":"printf synthetic"})),
            ("read", json!({"path":"another-fixture.txt"})),
        ]
        .into_iter()
        .enumerate()
        .map(|(index, (name, arguments))| {
            json!({"id":format!("fc_synthetic_{index}"),"type":"function_call",
                "call_id":format!("call_synthetic_{index}"),"name":name,
                "arguments":arguments.to_string(),"status":"completed"})
        })
        .collect();
        // Open all three calls before closing any, rather than three serial turns.
        for (index, item) in items.iter().enumerate() {
            let mut opening = item.clone();
            opening["arguments"] = json!("");
            opening["status"] = json!("in_progress");
            events.push(json!({"type":"response.output_item.added","output_index":index,"item":opening}));
        }
        for (index, item) in items.iter().enumerate() {
            let arguments = item["arguments"].as_str().unwrap();
            let split = arguments.len() / 2; // All fixture arguments are ASCII.
            let chunks: &[&str] = match shape {
                ArgumentDeltas::Full => &[&arguments[..split], &arguments[split..]],
                ArgumentDeltas::Partial => &[&arguments[..split]],
                ArgumentDeltas::None => &[],
            };
            for chunk in chunks {
                events.push(json!({"type":"response.function_call_arguments.delta",
                    "output_index":index,"item_id":item["id"],"delta":chunk}));
            }
            if !matches!(shape, ArgumentDeltas::None) {
                events.push(json!({"type":"response.function_call_arguments.done",
                    "output_index":index,"item_id":item["id"],"arguments":arguments}));
            }
            events.push(json!({"type":"response.output_item.done","output_index":index,"item":item}));
        }
        events.push(json!({"type":"response.completed","response":{
            "id":"resp_synthetic","object":"response","status":"completed",
            "model":"synthetic-openai","output":[],
            "usage":{"input_tokens":12,"output_tokens":9,"total_tokens":21,
                "input_tokens_details":{"cached_tokens":0},
                "output_tokens_details":{"reasoning_tokens":0}}
        }}));
        Self::events(events)
    }
    /// The request is captured before this gate is awaited, so admission tests can
    /// observe a live upstream request before allowing its final usage/settlement.
    pub fn gated(mut self) -> (Self, mpsc::Sender<()>) {
        let (tx, rx) = mpsc::channel();
        self.gate = Some(rx);
        (self, tx)
    }
    /// A stream reply whose last SSE event is `[DONE]`: the native consumer returns
    /// there without reading the HTTP transfer end (`post_sse`).
    fn ends_at_done(&self) -> bool {
        self.content_type == "text/event-stream"
            && String::from_utf8_lossy(&self.bytes)
                .lines()
                .rev()
                .find(|line| !line.trim().is_empty())
                .is_some_and(|line| line.trim() == "data: [DONE]")
    }
}

#[derive(Clone, Copy)]
enum ArgumentDeltas {
    Full,
    None,
    Partial,
}

// Closed test-only facts; never retain io::Error display text or request data.
#[derive(Clone, Copy, Debug)]
enum IoPhase {
    Accept,
    RequestRead,
    ReplyGate,
    ReplyWrite,
    UnexpectedRequest,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReplyPart {
    Headers,
    HeaderEnd,
    ChunkHeader,
    ChunkPayload,
    ChunkEnd,
    Flush,
    TransferEnd,
}
#[derive(Clone, Copy, Debug)]
pub(super) struct HealthFailure {
    phase: IoPhase,
    pub(super) kind: Option<io::ErrorKind>,
    pub(super) part: Option<ReplyPart>,
}
impl HealthFailure {
    fn io(phase: IoPhase, error: io::Error) -> Self {
        Self {
            phase,
            kind: Some(error.kind()),
            part: None,
        }
    }
    fn reply(part: ReplyPart, error: io::Error) -> Self {
        Self {
            part: Some(part),
            ..Self::io(IoPhase::ReplyWrite, error)
        }
    }
    fn peer_panic() -> Self {
        Self {
            phase: IoPhase::Accept,
            kind: None,
            part: None,
        }
    }
}

#[derive(Default)]
struct Shared {
    requests: Mutex<Vec<Request>>,
    changed: Condvar,
    errors: Mutex<Vec<HealthFailure>>,
    stopping: AtomicBool,
}
pub struct MockOpenAi {
    address: SocketAddr,
    shared: Arc<Shared>,
    join: Option<JoinHandle<()>>,
}
impl MockOpenAi {
    pub fn start(replies: Vec<Reply>) -> Self {
        Self::start_with(|_| replies)
    }
    /// Discovery fixtures advertise the exact already-bound loopback origin.
    pub fn start_with(replies: impl FnOnce(&str) -> Vec<Reply>) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let replies = replies(&format!("http://{address}"));
        let shared = Arc::new(Shared::default());
        let worker = shared.clone();
        let replies = Arc::new(Mutex::new(VecDeque::from(replies)));
        let join = thread::spawn(move || {
            let mut peers = vec![];
            while !worker.stopping.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let shared = worker.clone();
                        let replies = replies.clone();
                        peers.push(thread::spawn(move || {
                            if let Err(failure) = serve(stream, &shared, &replies) {
                                shared.errors.lock().unwrap().push(failure);
                            }
                        }));
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5))
                    }
                    Err(error) => {
                        worker
                            .errors
                            .lock()
                            .unwrap()
                            .push(HealthFailure::io(IoPhase::Accept, error));
                        break;
                    }
                }
            }
            for peer in peers {
                if peer.join().is_err() {
                    worker.errors.lock().unwrap().push(HealthFailure::peer_panic());
                }
            }
        });
        Self {
            address,
            shared,
            join: Some(join),
        }
    }
    pub fn base_url(&self) -> String {
        format!("http://{}/v1", self.address)
    }
    pub fn address(&self) -> SocketAddr {
        self.address
    }
    pub fn requests(&self) -> Vec<Request> {
        self.shared.requests.lock().unwrap().clone()
    }
    pub fn wait_for_requests(&self, count: usize) -> Vec<Request> {
        let (requests, _) = self
            .shared
            .changed
            .wait_timeout_while(self.shared.requests.lock().unwrap(), DEADLINE, |rows| {
                rows.len() < count
            })
            .unwrap();
        assert!(
            requests.len() >= count,
            "mock upstream did not receive the expected request count"
        );
        requests.clone()
    }
    pub fn assert_healthy(&self) {
        let (empty, first) = {
            let errors = self.shared.errors.lock().unwrap();
            (errors.is_empty(), errors.first().copied())
        };
        assert!(
            empty,
            "mock upstream reported a connection failure phase={:?} kind={:?} part={:?}",
            first.map(|failure| failure.phase),
            first.and_then(|failure| failure.kind),
            first.and_then(|failure| failure.part),
        );
    }
    /// Stop accepting, drain every live peer, then check health. A snapshot taken
    /// while a peer is still reading or writing cannot certify its own success.
    pub fn finish(mut self) {
        self.drain();
        self.assert_healthy();
    }
    fn drain(&mut self) {
        self.shared.stopping.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            if join.join().is_err() {
                self.shared
                    .errors
                    .lock()
                    .unwrap()
                    .push(HealthFailure::peer_panic());
            }
        }
    }
}
impl Drop for MockOpenAi {
    fn drop(&mut self) {
        // Cleanup only: the health verdict belongs to `finish`.
        self.drain();
    }
}

fn read_request(stream: &mut TcpStream) -> io::Result<Request> {
    stream.set_read_timeout(Some(DEADLINE))?;
    stream.set_write_timeout(Some(DEADLINE))?;
    let mut bytes = vec![];
    let end = loop {
        if let Some(end) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
            break end + 4;
        }
        if bytes.len() >= 32 * 1024 {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let mut next = [0; 1024];
        let n = stream.read(&mut next)?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        bytes.extend_from_slice(&next[..n]);
    };
    let head = std::str::from_utf8(&bytes[..end]).map_err(|_| io::ErrorKind::InvalidData)?;
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or_default().split_whitespace();
    let method = first.next().ok_or(io::ErrorKind::InvalidData)?.to_owned();
    let target = first.next().ok_or(io::ErrorKind::InvalidData)?.to_owned();
    let mut headers = BTreeMap::new();
    for line in lines.filter(|line| !line.is_empty()) {
        let (name, value) = line.split_once(':').ok_or(io::ErrorKind::InvalidData)?;
        headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
    }
    let length: usize = headers
        .get("content-length")
        .map(String::as_str)
        .unwrap_or("0")
        .parse()
        .map_err(|_| io::ErrorKind::InvalidData)?;
    if length > 1024 * 1024 || headers.contains_key("transfer-encoding") {
        return Err(io::ErrorKind::InvalidData.into());
    }
    while bytes.len() < end + length {
        let mut next = [0; 4096];
        let n = stream.read(&mut next)?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        bytes.extend_from_slice(&next[..n]);
    }
    let body = if length == 0 {
        Value::Null
    } else if headers
        .get("content-type")
        .is_some_and(|v| v.starts_with("application/x-www-form-urlencoded"))
    {
        #[cfg(feature = "transport")]
        {
            Value::Object(
                url::form_urlencoded::parse(&bytes[end..end + length])
                    .map(|(k, v)| (k.into_owned(), Value::String(v.into_owned())))
                    .collect(),
            )
        }
        #[cfg(not(feature = "transport"))]
        {
            return Err(io::ErrorKind::InvalidData.into());
        }
    } else {
        serde_json::from_slice(&bytes[end..end + length]).map_err(|_| io::ErrorKind::InvalidData)?
    };
    Ok(Request {
        method,
        target,
        body,
        body_bytes: bytes[end..end + length].to_vec(),
        headers,
    })
}
fn serve(
    mut stream: TcpStream,
    shared: &Shared,
    replies: &Mutex<VecDeque<Reply>>,
) -> Result<(), HealthFailure> {
    let request =
        read_request(&mut stream).map_err(|error| HealthFailure::io(IoPhase::RequestRead, error))?;
    let reply = {
        let mut requests = shared.requests.lock().unwrap();
        let reply = replies.lock().unwrap().pop_front().unwrap_or_else(|| {
            shared.errors.lock().unwrap().push(HealthFailure {
                phase: IoPhase::UnexpectedRequest,
                kind: None,
                part: None,
            });
            Reply::error(500, "server_error", "unexpected synthetic request")
        });
        requests.push(request);
        reply
    };
    shared.changed.notify_all();
    if let Some(gate) = &reply.gate {
        let start = Instant::now();
        loop {
            if shared.stopping.load(Ordering::Acquire) {
                return Ok(());
            }
            match gate.recv_timeout(Duration::from_millis(20)) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(_) if start.elapsed() < DEADLINE => {}
                Err(_) => {
                    return Err(HealthFailure::io(
                        IoPhase::ReplyGate,
                        io::ErrorKind::TimedOut.into(),
                    ))
                }
            }
        }
    }
    write_reply(&mut stream, &reply)
}
pub(super) fn write_reply(stream: &mut impl Write, reply: &Reply) -> Result<(), HealthFailure> {
    write!(
        stream,
        "HTTP/1.1 {} Synthetic\r\nContent-Type: {}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n",
        reply.status, reply.content_type
    )
    .map_err(|error| HealthFailure::reply(ReplyPart::Headers, error))?;
    for (name, value) in &reply.headers {
        write!(stream, "{name}: {value}\r\n")
            .map_err(|error| HealthFailure::reply(ReplyPart::Headers, error))?;
    }
    stream
        .write_all(b"\r\n")
        .map_err(|error| HealthFailure::reply(ReplyPart::HeaderEnd, error))?;
    let mut chunks = reply.bytes.chunks(reply.fragment).peekable();
    while let Some(bytes) = chunks.next() {
        write!(stream, "{:x}\r\n", bytes.len())
            .map_err(|error| HealthFailure::reply(ReplyPart::ChunkHeader, error))?;
        stream
            .write_all(bytes)
            .map_err(|error| HealthFailure::reply(ReplyPart::ChunkPayload, error))?;
        if chunks.peek().is_none() && reply.ends_at_done() {
            // The native client returns at `[DONE]` without waiting for HTTP EOF
            // (`post_sse`), so only framing remains after this last payload byte.
            // A peer disconnect here is expected: accept it only after the whole
            // payload, and only for BrokenPipe/ConnectionReset. Partial payloads,
            // nonterminal replies and timeouts stay strict.
            let tail: &[u8] = if reply.truncate_transfer {
                b"\r\n"
            } else {
                b"\r\n0\r\n\r\n"
            };
            return match stream.write_all(tail) {
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
                    ) =>
                {
                    Ok(())
                }
                result => {
                    result.map_err(|error| HealthFailure::reply(ReplyPart::TransferEnd, error))
                }
            };
        }
        stream
            .write_all(b"\r\n")
            .map_err(|error| HealthFailure::reply(ReplyPart::ChunkEnd, error))?;
        stream
            .flush()
            .map_err(|error| HealthFailure::reply(ReplyPart::Flush, error))?;
    }
    if reply.truncate_transfer {
        Ok(())
    } else {
        stream
            .write_all(b"0\r\n\r\n")
            .map_err(|error| HealthFailure::reply(ReplyPart::TransferEnd, error))
    }
}

#[cfg(all(test, feature = "upstream-openai"))]
mod mock_health {
    use super::*;

    /// Deterministically model a client closing after any particular wire byte,
    /// including between the `[DONE]` payload and the final HTTP chunk framing.
    struct DisconnectAfter {
        remaining: usize,
        kind: io::ErrorKind,
    }
    impl Write for DisconnectAfter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.remaining == 0 {
                return Err(self.kind.into());
            }
            let count = bytes.len().min(self.remaining);
            self.remaining -= count;
            Ok(count)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn done_stream() -> Reply {
        Reply::raw(
            200,
            "text/event-stream",
            b"data: {\"choices\":[]}\n\ndata: [DONE]\n\n".to_vec(),
            Vec::new(),
        )
    }

    #[test]
    fn terminal_framing_disconnect_is_healthy_only_after_complete_done() {
        let mut truncated = done_stream();
        truncated.truncate_transfer = true;
        let incomplete = Reply::raw(
            200,
            "text/event-stream",
            b"data: {\"choices\":[]}\n\n".to_vec(),
            Vec::new(),
        );
        let json = Reply::json(json!({"output":[]}));
        for reply in [done_stream(), truncated, incomplete, json] {
            let terminal = reply.ends_at_done();
            let tail: &[u8] = if reply.truncate_transfer {
                b"\r\n"
            } else {
                b"\r\n0\r\n\r\n"
            };
            let mut wire = Vec::new();
            write_reply(&mut wire, &reply).unwrap();
            let framing = wire.len() - tail.len();
            for kind in [
                io::ErrorKind::BrokenPipe,
                io::ErrorKind::ConnectionReset,
                io::ErrorKind::TimedOut,
            ] {
                for remaining in [0, framing - 1, framing, framing + 2] {
                    let result = write_reply(&mut DisconnectAfter { remaining, kind }, &reply);
                    let expected = terminal
                        && remaining >= framing
                        && kind != io::ErrorKind::TimedOut;
                    assert_eq!(
                        result.is_ok(),
                        expected,
                        "terminal={terminal}, byte={remaining}, error={kind:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn finish_accepts_a_client_that_returns_at_done() {
        let mock = MockOpenAi::start(vec![done_stream(), done_stream()]);
        for _ in 0..2 {
            // The native consumer returns at `[DONE]`, skipping the transfer end.
            let mut stream = TcpStream::connect(mock.address()).unwrap();
            stream
                .write_all(b"POST /v1/chat/completions HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}")
                .unwrap();
            let mut wire = Vec::new();
            let mut chunk = [0u8; 512];
            while !wire.windows(12).any(|part| part == b"data: [DONE]") {
                let n = stream.read(&mut chunk).unwrap();
                assert!(n > 0, "the mock closed before [DONE]");
                wire.extend_from_slice(&chunk[..n]);
            }
            stream.shutdown(std::net::Shutdown::Both).unwrap();
        }
        assert_eq!(mock.requests().len(), 2);
        mock.finish();
    }
}
