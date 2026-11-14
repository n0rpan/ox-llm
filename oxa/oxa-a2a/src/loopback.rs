//! Explicitly registered loopback-only HTTP adapter for model-free development.
//!
//! It cannot reach remote hosts, resolve DNS, follow redirects, use proxies or
//! load credentials from the environment. This is not a production TLS client.
//! JSON responses require Content-Length. SSE also permits explicitly
//! close-delimited bodies; chunked encoding is not supported yet.
use crate::sse::SseReader;
use crate::wire::*;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Take, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

const HEADER_LIMIT: usize = 16 * 1024;

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// A short-lived transport capability, never a cloud/provider API key.
/// Deliberately has no serialization or revealing Debug implementation.
#[derive(Clone)]
pub struct Capability(String);

impl Capability {
    pub fn new(value: String) -> io::Result<Self> {
        if value.len() < 32 || value.len() > 256 || !value.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err(invalid("invalid loopback capability"));
        }
        Ok(Self(value))
    }

    pub fn generate() -> io::Result<Self> {
        let mut bytes = [0u8; 32];
        std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
        Self::new(bytes.iter().map(|b| format!("{b:02x}")).collect())
    }
}

impl fmt::Debug for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Capability([redacted])")
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub timeout: Duration,
    pub body_bytes: usize,
    pub event_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(5),
            body_bytes: 1024 * 1024,
            event_bytes: 64 * 1024,
        }
    }
}

/// Address construction is the network policy: literal loopback only. This
/// route is for mocks, not evidence that an arbitrary local proxy is offline.
pub struct Client {
    address: SocketAddr,
    capability: Capability,
    limits: Limits,
    next_id: u64,
}

impl Client {
    pub fn new(address: SocketAddr, capability: Capability, limits: Limits) -> io::Result<Self> {
        if !address.ip().is_loopback() || address.port() == 0 {
            return Err(invalid("only explicit loopback mock endpoints are supported"));
        }
        if limits.timeout.is_zero() || limits.body_bytes == 0 || limits.event_bytes == 0 {
            return Err(invalid("limits must be positive"));
        }
        Ok(Self {
            address,
            capability,
            limits,
            next_id: 0,
        })
    }

    pub fn url(&self) -> String {
        format!("http://{}/", self.address)
    }

    pub fn card(&self) -> io::Result<AgentCard> {
        let response = self.open("GET", "/.well-known/agent-card.json", &[], "application/json")?;
        let value = read_json(response.body)?;
        let card: AgentCard = serde_json::from_value(value).map_err(|_| invalid("invalid agent card"))?;
        card.validate_for(&self.url()).map_err(invalid)?;
        // No unsupported auth alternative can silently become an unauthenticated call.
        if card.security != vec![BTreeMap::from([("capability".into(), vec![])])]
            || card.security_schemes.get("capability") != Some(&json!({"type":"http","scheme":"bearer"}))
        {
            return Err(invalid("unsupported agent authentication scheme"));
        }
        Ok(card)
    }

    fn request(&mut self, method: &str, params: Value) -> io::Result<(RequestId, Vec<u8>)> {
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| invalid("request ID exhausted"))?;
        let id = RequestId::Text(self.next_id.to_string());
        let bytes = serde_json::to_vec(&Request::new(id.clone(), method, params))
            .map_err(|_| invalid("request encoding failed"))?;
        if bytes.len() > self.limits.body_bytes {
            return Err(invalid("request size limit exceeded"));
        }
        Ok((id, bytes))
    }

    pub fn send(&mut self, params: SendParams) -> io::Result<Event> {
        let result = self.call(
            "message/send",
            serde_json::to_value(params).map_err(|_| invalid("invalid params"))?,
        )?;
        let event: Event = serde_json::from_value(result).map_err(|_| invalid("invalid send result"))?;
        if !matches!(event, Event::Message(_) | Event::Task(_)) {
            return Err(invalid("invalid send result kind"));
        }
        Ok(event)
    }

    pub fn get(&mut self, task: &str) -> io::Result<Task> {
        self.task_call("tasks/get", task)
    }
    pub fn cancel(&mut self, task: &str) -> io::Result<Task> {
        self.task_call("tasks/cancel", task)
    }

    fn task_call(&mut self, method: &str, task: &str) -> io::Result<Task> {
        let result = self.call(method, json!({"id": task}))?;
        match serde_json::from_value::<Event>(result) {
            Ok(Event::Task(t)) if t.id == task => Ok(t),
            _ => Err(invalid("invalid task response")),
        }
    }

    pub fn call(&mut self, method: &str, params: Value) -> io::Result<Value> {
        let (id, bytes) = self.request(method, params)?;
        let response = self.open("POST", "/", &bytes, "application/json")?;
        response_result(read_json(response.body)?, &id).map_err(rpc_error)
    }

    pub fn stream(&mut self, params: SendParams) -> io::Result<EventStream> {
        self.open_stream(
            "message/stream",
            serde_json::to_value(params).map_err(|_| invalid("invalid params"))?,
        )
    }

    pub fn resubscribe(&mut self, task: &str) -> io::Result<EventStream> {
        let mut stream = self.open_stream("tasks/resubscribe", json!({"id": task}))?;
        stream.expected_task = Some(task.into());
        Ok(stream)
    }

    fn open_stream(&mut self, method: &str, params: Value) -> io::Result<EventStream> {
        let (id, bytes) = self.request(method, params)?;
        let response = self.open("POST", "/", &bytes, "text/event-stream")?;
        Ok(EventStream {
            reader: SseReader::new(response.body, self.limits.event_bytes, self.limits.body_bytes),
            id,
            binding: None,
            expected_task: None,
            done: false,
            failed: false,
        })
    }

    fn open(&self, method: &str, path: &str, body: &[u8], content_type: &str) -> io::Result<Response> {
        let deadline = Instant::now()
            .checked_add(self.limits.timeout)
            .ok_or_else(|| invalid("invalid deadline"))?;
        let socket = TcpStream::connect_timeout(&self.address, self.limits.timeout)?;
        socket.set_write_timeout(Some(self.limits.timeout))?;
        let mut stream = DeadlineStream { socket, deadline };
        write!(stream, "{method} {path} HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nAccept: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", self.address, self.capability.0, body.len())?;
        stream.write_all(body)?;
        let mut reader = BufReader::new(stream);
        let head = read_head(&mut reader)?;
        if head.first != "HTTP/1.1 200 OK" && head.first != "HTTP/1.0 200 OK" {
            return Err(invalid("agent HTTP request rejected"));
        }
        if head.headers.get("content-type").and_then(|s| s.split(';').next()) != Some(content_type) {
            return Err(invalid("unexpected agent content type"));
        }
        let length = if content_type == "text/event-stream"
            && !head.headers.contains_key("content-length")
            && !head.headers.contains_key("transfer-encoding")
            && head
                .headers
                .get("connection")
                .is_some_and(|s| s.eq_ignore_ascii_case("close"))
        {
            self.limits
                .body_bytes
                .checked_add(1)
                .ok_or_else(|| invalid("invalid body limit"))?
        } else {
            head.length(self.limits.body_bytes)?
        };
        Ok(Response {
            body: reader.take(length as u64),
        })
    }
}

fn rpc_error(error: RpcError) -> io::Error {
    // Preserve the protocol code without disclosing arbitrary remote error text.
    io::Error::other(format!("agent JSON-RPC error {}", error.code))
}

pub struct EventStream {
    reader: SseReader<Body>,
    id: RequestId,
    binding: Option<(String, String)>,
    expected_task: Option<String>,
    done: bool,
    failed: bool,
}

impl EventStream {
    /// A clean transport EOF before a protocol-final record is an error.
    pub fn next_event(&mut self) -> io::Result<Option<Event>> {
        if self.failed {
            return Err(invalid("event stream already failed"));
        }
        if self.done {
            return Ok(None);
        }
        let result = self.read_event();
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn read_event(&mut self) -> io::Result<Option<Event>> {
        let data = self
            .reader
            .next_data()?
            .ok_or_else(|| invalid("agent stream disconnected before final event"))?;
        let value: Value = serde_json::from_str(&data).map_err(|_| invalid("invalid streaming JSON"))?;
        let event: Event = serde_json::from_value(response_result(value, &self.id).map_err(rpc_error)?)
            .map_err(|_| invalid("invalid streaming result"))?;
        if let Some((task, context)) = &self.binding {
            if !event.belongs_to(task, context) {
                return Err(invalid("stream changed task identity"));
            }
        } else {
            let (task, context) = match &event {
                Event::Task(t) => (&t.id, &t.context_id),
                Event::Status(e) => (&e.task_id, &e.context_id),
                Event::Artifact(e) => (&e.task_id, &e.context_id),
                Event::Message(_) if self.expected_task.is_none() => {
                    self.done = true;
                    return Ok(Some(event));
                }
                _ => return Err(invalid("stream has no task identity")),
            };
            if self.expected_task.as_deref().is_some_and(|id| id != task) {
                return Err(invalid("unexpected resubscribed task"));
            }
            self.binding = Some((task.clone(), context.clone()));
        }
        self.done = match &event {
            Event::Task(t) => t.status.state.is_terminal(),
            Event::Status(e) => e.final_event,
            _ => false,
        };
        Ok(Some(event))
    }
}

pub(crate) struct DeadlineStream {
    pub(crate) socket: TcpStream,
    pub(crate) deadline: Instant,
}

impl DeadlineStream {
    fn remaining(&self) -> io::Result<Duration> {
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "agent deadline exceeded"))
    }
}

impl Read for DeadlineStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.socket.set_read_timeout(Some(self.remaining()?))?;
        self.socket.read(buf)
    }
}

impl Write for DeadlineStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.socket.set_write_timeout(Some(self.remaining()?))?;
        self.socket.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.socket.flush()
    }
}

type Body = Take<BufReader<DeadlineStream>>;
struct Response {
    body: Body,
}

fn read_json(mut body: Body) -> io::Result<Value> {
    let mut bytes = Vec::new();
    body.read_to_end(&mut bytes)?;
    if body.limit() != 0 {
        return Err(invalid("truncated HTTP body"));
    }
    serde_json::from_slice(&bytes).map_err(|_| invalid("invalid agent JSON"))
}

pub(crate) struct Head {
    pub(crate) first: String,
    pub(crate) headers: BTreeMap<String, String>,
}

impl Head {
    pub(crate) fn length(&self, max: usize) -> io::Result<usize> {
        if self.headers.contains_key("transfer-encoding") {
            return Err(invalid("transfer encoding is unsupported"));
        }
        let value = self
            .headers
            .get("content-length")
            .ok_or_else(|| invalid("missing content length"))?;
        if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
            return Err(invalid("invalid content length"));
        }
        let length = value
            .parse::<usize>()
            .map_err(|_| invalid("invalid content length"))?;
        if length > max {
            return Err(invalid("HTTP body size limit exceeded"));
        }
        Ok(length)
    }
}

pub(crate) fn read_head(reader: &mut impl BufRead) -> io::Result<Head> {
    let mut bytes = Vec::new();
    loop {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            return Err(invalid("truncated HTTP headers"));
        }
        // Consume only through the terminating header boundary, not body bytes.
        let byte = buf[0];
        reader.consume(1);
        bytes.push(byte);
        if bytes.len() > HEADER_LIMIT {
            return Err(invalid("HTTP header size limit exceeded"));
        }
        if bytes.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| invalid("invalid HTTP headers"))?;
    let mut lines = text[..text.len() - 4].split("\r\n");
    let first = lines
        .next()
        .ok_or_else(|| invalid("missing HTTP start line"))?
        .into();
    let mut headers = BTreeMap::new();
    for line in lines {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| invalid("invalid HTTP header"))?;
        if name.is_empty()
            || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || value.bytes().any(|b| b.is_ascii_control() && b != b'\t')
            || headers
                .insert(name.to_ascii_lowercase(), value.trim().into())
                .is_some()
        {
            return Err(invalid("invalid or duplicate HTTP header"));
        }
    }
    Ok(Head { first, headers })
}

pub(crate) fn authorized(head: &Head, capability: &Capability) -> bool {
    let expected = format!("Bearer {}", capability.0);
    let actual = head
        .headers
        .get("authorization")
        .map_or(&[][..], |s| s.as_bytes());
    let mut difference = expected.len() ^ actual.len();
    for (i, byte) in expected.bytes().enumerate() {
        difference |= (byte ^ actual.get(i).copied().unwrap_or(0)) as usize;
    }
    difference == 0
}
