//! Owned loopback Anthropic Messages upstream for the shared Rust skeleton tests.
//! This module uses synthetic credentials only and never consults the environment.
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

pub const CREDENTIAL: &str = "synthetic-private-key";
const DEADLINE: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct Request {
    pub method: String,
    pub target: String,
    pub body: Value,
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
    bytes: Vec<u8>,
    fragment: usize,
    gate: Option<mpsc::Receiver<()>>,
}
impl Reply {
    /// Cache input is separate from ordinary input on the native wire.
    pub fn text(text: &str, input: u64, cache_read: u64, cache_write: u64, output: u64) -> Self {
        let events = [
            json!({"type":"message_start","message":{"id":"msg_synthetic","type":"message","role":"assistant","model":"synthetic-anthropic","content":[],"stop_reason":null,"usage":{"input_tokens":input,"cache_read_input_tokens":cache_read,"cache_creation_input_tokens":cache_write,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":output}}),
            json!({"type":"message_stop"}),
        ];
        let mut bytes = Vec::new();
        for event in events {
            bytes.extend_from_slice(
                format!("event: {}\ndata: {event}\n\n", event["type"].as_str().unwrap()).as_bytes(),
            );
        }
        Self {
            status: 200,
            content_type: "text/event-stream",
            bytes,
            fragment: 7,
            gate: None,
        }
    }
    pub fn error(status: u16, message: &str) -> Self {
        Self {
            status,
            content_type: "application/json",
            bytes: json!({"type":"error","error":{"type":"overloaded_error","message":message}})
                .to_string()
                .into_bytes(),
            fragment: 7,
            gate: None,
        }
    }
    /// The request is captured before this gate is awaited, so admission tests can
    /// observe a live upstream request before allowing its final usage/settlement.
    pub fn gated(mut self) -> (Self, mpsc::Sender<()>) {
        let (tx, rx) = mpsc::channel();
        self.gate = Some(rx);
        (self, tx)
    }
}

#[derive(Default)]
struct Shared {
    requests: Mutex<Vec<Request>>,
    changed: Condvar,
    errors: Mutex<Vec<&'static str>>,
    stopping: AtomicBool,
}
pub struct MockAnthropic {
    address: SocketAddr,
    shared: Arc<Shared>,
    join: Option<JoinHandle<()>>,
}
impl MockAnthropic {
    pub fn start(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
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
                            if serve(stream, &shared, &replies).is_err()
                                && !shared.stopping.load(Ordering::Acquire)
                            {
                                shared
                                    .errors
                                    .lock()
                                    .unwrap()
                                    .push("mock upstream connection failed");
                            }
                        }));
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5))
                    }
                    Err(_) => {
                        worker.errors.lock().unwrap().push("mock upstream accept failed");
                        break;
                    }
                }
            }
            for peer in peers {
                let _ = peer.join();
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
        assert!(
            self.shared.errors.lock().unwrap().is_empty(),
            "mock upstream reported a connection failure"
        );
    }
}
impl Drop for MockAnthropic {
    fn drop(&mut self) {
        self.shared.stopping.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
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
    } else {
        serde_json::from_slice(&bytes[end..end + length]).map_err(|_| io::ErrorKind::InvalidData)?
    };
    Ok(Request {
        method,
        target,
        body,
        headers,
    })
}
fn serve(mut stream: TcpStream, shared: &Shared, replies: &Mutex<VecDeque<Reply>>) -> io::Result<()> {
    let request = read_request(&mut stream)?;
    let reply = {
        let mut requests = shared.requests.lock().unwrap();
        let reply = replies.lock().unwrap().pop_front().unwrap_or_else(|| {
            shared.errors.lock().unwrap().push("unexpected upstream request");
            Reply::error(500, "unexpected synthetic request")
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
                Err(_) => return Err(io::ErrorKind::TimedOut.into()),
            }
        }
    }
    write!(stream, "HTTP/1.1 {} Synthetic\r\nContent-Type: {}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n", reply.status, reply.content_type)?;
    for bytes in reply.bytes.chunks(reply.fragment) {
        write!(stream, "{:x}\r\n", bytes.len())?;
        stream.write_all(bytes)?;
        stream.write_all(b"\r\n")?;
        stream.flush()?;
    }
    stream.write_all(b"0\r\n\r\n")
}
