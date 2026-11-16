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
    headers: Vec<(&'static str, String)>,
    bytes: Vec<u8>,
    fragment: usize,
    terminal: bool,
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
        Self::events(events.into())
    }
    /// Native events for provider-specific continuation regressions, using the
    /// same bounded, fragmented loopback peer as the walking skeleton.
    pub fn events(events: Vec<Value>) -> Self {
        let terminal = events.last().is_some_and(|event| event["type"] == "message_stop");
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
            terminal,
            gate: None,
        }
    }
    pub fn error(status: u16, message: &str) -> Self {
        Self {
            status,
            content_type: "application/json",
            headers: Vec::new(),
            bytes: json!({"type":"error","error":{"type":"overloaded_error","message":message}})
                .to_string()
                .into_bytes(),
            fragment: 7,
            terminal: false,
            gate: None,
        }
    }
    /// Malformed/truncated protocol cases use the same fragmented peer.
    pub fn wire(content_type: &'static str, bytes: &[u8]) -> Self {
        Self {
            status: 200,
            content_type,
            headers: Vec::new(),
            bytes: bytes.to_vec(),
            fragment: 7,
            terminal: false,
            gate: None,
        }
    }
    pub fn redirect(location: &str) -> Self {
        Self {
            status: 307,
            headers: vec![("Location", location.to_owned())],
            ..Self::wire("text/event-stream", b"")
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
                            if serve(stream, &shared, &replies).is_err() {
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
                if peer.join().is_err() {
                    worker.errors.lock().unwrap().push("mock upstream peer panicked");
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
    /// Stop accepting, drain every peer, then check health. A snapshot before
    /// joining cannot certify success: a peer may still be reading or writing.
    pub fn finish(mut self) {
        self.drain();
        let healthy = self.shared.errors.lock().unwrap().is_empty();
        assert!(healthy, "mock upstream reported a connection failure");
    }
    fn drain(&mut self) {
        self.shared.stopping.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            if join.join().is_err() {
                self.shared
                    .errors
                    .lock()
                    .unwrap()
                    .push("mock upstream listener panicked");
            }
        }
    }
}
impl Drop for MockAnthropic {
    fn drop(&mut self) {
        // Cleanup is not a success verdict, especially during unwinding.
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
                return Err(io::ErrorKind::Interrupted.into());
            }
            match gate.recv_timeout(Duration::from_millis(20)) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(_) if start.elapsed() < DEADLINE => {}
                Err(_) => return Err(io::ErrorKind::TimedOut.into()),
            }
        }
    }
    write_reply(&mut stream, &reply)
}

fn write_reply(stream: &mut impl Write, reply: &Reply) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {} Synthetic\r\nContent-Type: {}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n",
        reply.status, reply.content_type
    )?;
    for (name, value) in &reply.headers {
        write!(stream, "{name}: {value}\r\n")?;
    }
    stream.write_all(b"\r\n")?;
    let mut chunks = reply.bytes.chunks(reply.fragment).peekable();
    while let Some(bytes) = chunks.next() {
        write!(stream, "{:x}\r\n", bytes.len())?;
        stream.write_all(bytes)?;
        if reply.terminal && chunks.peek().is_none() {
            // Client::stream legitimately returns at message_stop, without
            // waiting for HTTP EOF. Only framing remains after its last byte;
            // disconnects before here (including partial stop events) are errors.
            let framing = stream.write_all(b"\r\n0\r\n\r\n");
            return match framing {
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
                    ) =>
                {
                    Ok(())
                }
                result => result,
            };
        }
        stream.write_all(b"\r\n")?;
        stream.flush()?;
    }
    stream.write_all(b"0\r\n\r\n")
}

#[cfg(test)]
mod mock_health {
    use super::*;
    use std::{
        net::Shutdown,
        panic::{catch_unwind, AssertUnwindSafe},
    };

    fn request(mock: &MockAnthropic, body: &[u8]) -> TcpStream {
        let mut client = TcpStream::connect(mock.address()).unwrap();
        write!(
            client,
            "POST /v1/messages HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .unwrap();
        client.write_all(body).unwrap();
        client
            .set_read_timeout(Some(DEADLINE + Duration::from_secs(1)))
            .unwrap();
        client
    }

    fn rejects(mock: MockAnthropic) {
        assert!(catch_unwind(AssertUnwindSafe(|| mock.finish())).is_err());
    }

    /// Deterministically model a client closing after any particular wire byte,
    /// including between message_stop and the final HTTP chunk framing.
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

    #[test]
    fn terminal_framing_disconnect_is_healthy_only_after_complete_message_stop() {
        let complete = Reply::text("Synthetic", 1, 0, 0, 1);
        let incomplete = Reply::events(vec![json!({"type":"message_delta"})]);
        let raw = Reply::wire("text/event-stream", b"synthetic raw body");
        for reply in [complete, incomplete, raw] {
            let terminal = reply.bytes.ends_with(b"data: {\"type\":\"message_stop\"}\n\n");
            let mut wire = Vec::new();
            write_reply(&mut wire, &reply).unwrap();
            // Last payload byte precedes the final CRLF and zero chunk.
            let framing = wire.len() - 7;
            for kind in [
                io::ErrorKind::BrokenPipe,
                io::ErrorKind::ConnectionReset,
                io::ErrorKind::TimedOut,
            ] {
                for remaining in [0, framing - 1, framing, framing + 2] {
                    let result = write_reply(&mut DisconnectAfter { remaining, kind }, &reply);
                    let expected = terminal && remaining >= framing && kind != io::ErrorKind::TimedOut;
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
    fn finish_accepts_complete_replies() {
        let mock = MockAnthropic::start(vec![
            Reply::events(vec![json!({"type":"message_stop"})]),
            Reply::text("Synthetic", 1, 0, 0, 1),
        ]);
        for _ in 0..2 {
            let mut response = Vec::new();
            request(&mock, b"{}").read_to_end(&mut response).unwrap();
            assert!(response.ends_with(b"0\r\n\r\n"));
        }
        assert_eq!(mock.requests().len(), 2);
        mock.finish();
    }

    #[test]
    fn finish_rejects_malformed_and_unexpected_requests() {
        for body in [b"{".as_slice(), b"{}".as_slice()] {
            let mock = MockAnthropic::start(vec![]);
            let _ = request(&mock, body).read_to_end(&mut Vec::new());
            rejects(mock);
        }
    }

    #[test]
    fn finish_rejects_unfinished_gate_instead_of_certifying_shutdown() {
        let (reply, _gate) = Reply::text("Synthetic", 1, 0, 0, 1).gated();
        let mock = MockAnthropic::start(vec![reply]);
        let _client = request(&mock, b"{}");
        mock.wait_for_requests(1);
        rejects(mock);
    }

    #[test]
    fn finish_rejects_gate_timeout() {
        let (reply, _gate) = Reply::text("Synthetic", 1, 0, 0, 1).gated();
        let mock = MockAnthropic::start(vec![reply]);
        // EOF comes from the peer's bounded timeout, not from shutdown.
        let mut response = Vec::new();
        request(&mock, b"{}").read_to_end(&mut response).unwrap();
        assert!(response.is_empty());
        rejects(mock);
    }

    #[test]
    fn finish_rejects_disconnect_during_response() {
        let mock = MockAnthropic::start(vec![Reply::wire("text/event-stream", &vec![b'x'; 1024 * 1024])]);
        let mut client = request(&mock, b"{}");
        client.read_exact(&mut [0]).unwrap();
        client.shutdown(Shutdown::Both).unwrap();
        rejects(mock);
    }

    #[test]
    fn finish_records_listener_panic() {
        let mut mock = MockAnthropic::start(vec![]);
        mock.drain();
        mock.join = Some(thread::spawn(|| panic!("synthetic listener failure")));
        rejects(mock);
    }

    #[test]
    fn shutdown_records_in_flight_read_failure() {
        let mock = MockAnthropic::start(vec![Reply::text("Synthetic", 1, 0, 0, 1)]);
        let mut client = TcpStream::connect(mock.address()).unwrap();
        client.write_all(b"POST /v1/messages HTTP/1.1\r\n").unwrap();
        // Acceptance is serial: observing a later connection's request proves
        // the listener already handed the incomplete connection to a peer.
        let mut later = TcpStream::connect(mock.address()).unwrap();
        later
            .write_all(b"POST /v1/messages HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}")
            .unwrap();
        mock.wait_for_requests(1);
        later.read_to_end(&mut Vec::new()).unwrap();
        let shared = mock.shared.clone();
        let closer = thread::spawn(move || {
            while !shared.stopping.load(Ordering::Acquire) {
                thread::yield_now();
            }
            client.shutdown(Shutdown::Both).unwrap();
        });
        let shared = mock.shared.clone();
        rejects(mock);
        closer.join().unwrap();
        assert!(
            !shared.errors.lock().unwrap().is_empty(),
            "shutdown hid a peer read failure"
        );
    }

    #[test]
    fn shutdown_records_peer_panic() {
        let mut reply = Reply::text("Synthetic", 1, 0, 0, 1);
        // A broken fixture must not turn a panicking peer into a passing test.
        reply.fragment = 0;
        let mock = MockAnthropic::start(vec![reply]);
        let mut client = TcpStream::connect(mock.address()).unwrap();
        client
            .write_all(b"POST /v1/messages HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}")
            .unwrap();
        mock.wait_for_requests(1);
        let mut response = Vec::new();
        client.read_to_end(&mut response).unwrap();
        let shared = mock.shared.clone();
        rejects(mock);
        assert!(
            !shared.errors.lock().unwrap().is_empty(),
            "shutdown hid a peer panic"
        );
    }
}
