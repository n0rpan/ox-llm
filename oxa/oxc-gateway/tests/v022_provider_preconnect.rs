//! Qualification fixture for #881: can the pinned transport warm a selected
//! provider's connection pool? The loopback upstream counts TCP accepts and
//! requests; it never consults the environment, a credential or a provider.
//!
//! No warm-up is implemented. These tests pin the facts the design note
//! (`oxa/docs/design/provider-preconnect.md`) rests on: constructing the
//! gateway opens no connection, a cloud-off/offline refusal touches none, and a
//! completed exchange is what leaves a reusable pool entry.
#![cfg(feature = "anthropic-gateway")]
#[allow(dead_code)]
#[path = "common/anthropic.rs"]
mod anthropic;
#[allow(dead_code)]
#[path = "common/skeleton.rs"]
mod skeleton;
use oxc_gateway::{
    anthropic_client::{Cancellation, Chat, Client, Control, Event},
    catalog::{Access, Model, Provider},
    gateway::Gateway,
    ledger::Context,
    pricing::Price,
    runtime::{Billing, Options, Runtime},
    secrets,
};
use serde_json::{json, Value};
use std::{
    io::{self, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const DEADLINE: Duration = Duration::from_secs(5);
const TICK: Duration = Duration::from_millis(5);

/// A keep-alive HTTP/1.1 upstream that serves the same synthetic Anthropic
/// stream on every request of every connection, counting both.
struct Upstream {
    address: SocketAddr,
    counts: Arc<Counts>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}
#[derive(Default)]
struct Counts {
    accepted: AtomicUsize,
    requests: AtomicUsize,
}
impl Upstream {
    fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let counts = Arc::new(Counts::default());
        let stop = Arc::new(AtomicBool::new(false));
        let worker = counts.clone();
        let stopping = stop.clone();
        let join = thread::spawn(move || {
            let mut peers = vec![];
            while !stopping.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        worker.accepted.fetch_add(1, Ordering::SeqCst);
                        let counts = worker.clone();
                        let stop = stopping.clone();
                        peers.push(thread::spawn(move || {
                            let _ = serve(stream, &counts, &stop);
                        }));
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => thread::sleep(TICK),
                    Err(_) => break,
                }
            }
            for peer in peers {
                let _ = peer.join();
            }
        });
        Self {
            address,
            counts,
            stop,
            join: Some(join),
        }
    }
    fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }
    fn accepted(&self) -> usize {
        self.counts.accepted.load(Ordering::SeqCst)
    }
    fn requests(&self) -> usize {
        self.counts.requests.load(Ordering::SeqCst)
    }
}
impl Drop for Upstream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// One connection serves requests until the client closes it or the test stops.
fn serve(mut stream: TcpStream, counts: &Counts, stop: &AtomicBool) -> io::Result<()> {
    stream.set_write_timeout(Some(DEADLINE))?;
    stream.set_read_timeout(Some(DEADLINE))?;
    let reply = stream_reply();
    let mut first = true;
    loop {
        if !first && !await_request(&stream, stop)? {
            return Ok(());
        }
        first = false;
        match read_request(&mut stream) {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::UnexpectedEof | io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                return Ok(())
            }
            Err(error) => return Err(error),
        }
        counts.requests.fetch_add(1, Ordering::SeqCst);
        // One write keeps the terminating chunk in the same read the client
        // stops on, so reuse depends on the transport, not on test timing.
        stream.write_all(&reply)?;
        stream.flush()?;
    }
}

/// Idle wait for a further request on a pooled connection: bounded, and it
/// returns immediately when the test drops the upstream.
fn await_request(stream: &TcpStream, stop: &AtomicBool) -> io::Result<bool> {
    stream.set_nonblocking(true)?;
    let deadline = Instant::now() + DEADLINE;
    let mut probe = [0_u8; 1];
    loop {
        if stop.load(Ordering::Acquire) || Instant::now() >= deadline {
            return Ok(false);
        }
        match stream.peek(&mut probe) {
            Ok(0) => return Ok(false),
            Ok(_) => {
                stream.set_nonblocking(false)?;
                return Ok(true);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => thread::sleep(TICK),
            Err(error) => return Err(error),
        }
    }
}

fn read_request(stream: &mut TcpStream) -> io::Result<()> {
    let mut bytes = vec![];
    let end = loop {
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
        if bytes.len() >= 32 * 1024 {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let mut next = [0; 1024];
        let read = stream.read(&mut next)?;
        if read == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        bytes.extend_from_slice(&next[..read]);
    };
    let head = std::str::from_utf8(&bytes[..end]).map_err(|_| io::ErrorKind::InvalidData)?;
    let mut length = 0_usize;
    for line in head.split("\r\n").skip(1).filter(|line| !line.is_empty()) {
        let Some((name, value)) = line.split_once(':') else {
            return Err(io::ErrorKind::InvalidData.into());
        };
        if name.eq_ignore_ascii_case("content-length") {
            length = value.trim().parse().map_err(|_| io::ErrorKind::InvalidData)?;
        }
        if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(io::ErrorKind::InvalidData.into());
        }
    }
    if length > 1024 * 1024 {
        return Err(io::ErrorKind::InvalidData.into());
    }
    while bytes.len() < end + length {
        let mut next = [0; 4096];
        let read = stream.read(&mut next)?;
        if read == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        bytes.extend_from_slice(&next[..read]);
    }
    Ok(())
}

/// The same event sequence the shared Anthropic mock scripts, as one body.
fn stream_reply() -> Vec<u8> {
    let usage = json!({"input_tokens":11,"cache_read_input_tokens":7,
        "cache_creation_input_tokens":3,"output_tokens":0});
    let start = json!({"id":"msg_synthetic","type":"message","role":"assistant",
        "model":"synthetic-anthropic","content":[],"stop_reason":null,"usage":usage});
    let events = [
        json!({"type":"message_start","message":start}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Synthetic"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},
            "usage":{"output_tokens":5}}),
        json!({"type":"message_stop"}),
    ];
    let mut body = vec![];
    for event in events {
        body.extend_from_slice(
            format!("event: {}\ndata: {event}\n\n", event["type"].as_str().unwrap()).as_bytes(),
        );
    }
    let mut reply = Vec::with_capacity(body.len() + 128);
    reply.extend_from_slice(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
    );
    for chunk in body.chunks(4096) {
        reply.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
        reply.extend_from_slice(chunk);
        reply.extend_from_slice(b"\r\n");
    }
    reply.extend_from_slice(b"0\r\n\r\n");
    reply
}

fn payload() -> Value {
    json!({"model":skeleton::MODEL,"messages":[{"role":"user","content":"Synthetic request"}],
        "max_tokens":1000,"stream":true,"stream_options":{"include_usage":true}})
}
fn chat<'a>(base: &'a str, payload: &'a Value) -> Chat<'a> {
    Chat {
        base_url: base,
        upstream_model: skeleton::UPSTREAM,
        alias: skeleton::MODEL,
        max_output: Some(16),
        payload,
        created: 1700000000,
    }
}
fn gateway(fixture: &skeleton::Fixture, access: Access) -> Gateway {
    let configured = &fixture.config["cloud"]["models"][skeleton::MODEL];
    let model = Model::new(
        skeleton::MODEL,
        Provider::Anthropic,
        skeleton::UPSTREAM,
        configured["base_url"].as_str().unwrap(),
        1024,
        16,
    )
    .unwrap();
    let runtime = Runtime::new(
        [(model, Price::from_model(configured).map(Billing::Metered))],
        fixture.key_path.clone(),
        fixture.ledger_dir.clone(),
        Options {
            access,
            daily_budget_usd: Some(1.0),
            ledger_enabled: true,
            subscription: None,
            subscription_limits: None,
        },
    )
    .unwrap();
    Gateway::new(runtime).unwrap()
}
fn request<'a>(prefix: &'a oxa_proto::FrozenPrefix) -> oxa_proto::ChatRequest<'a> {
    oxa_proto::ChatRequest::new(prefix, &[], skeleton::MODEL)
}
fn empty_prefix() -> oxa_proto::FrozenPrefix {
    oxa_proto::FrozenPrefix {
        sections: vec![],
        tools_json: "[]".into(),
        sha256: [0; 32],
    }
}

/// An enabled gateway with a configured loopback provider still has an empty
/// pool until a real request. This is the "no implicit warm-up" invariant a
/// future preconnect would have to opt into explicitly.
#[tokio::test]
async fn constructing_the_gateway_opens_no_connection() {
    let upstream = Upstream::start();
    let fixture = skeleton::Fixture::for_endpoint(&upstream.base_url());
    let _gateway = gateway(&fixture, Access::Enabled);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        (upstream.accepted(), upstream.requests()),
        (0, 0),
        "gateway construction must not open or send anything"
    );
}

/// Cloud-off and offline refuse before the transport, so any warm-up that
/// ignores the access policy would fail here.
#[tokio::test]
async fn offline_and_cloud_off_refuse_without_touching_the_pool() {
    for (access, code) in [(Access::Offline, "offline"), (Access::Disabled, "cloud_disabled")] {
        let upstream = Upstream::start();
        let fixture = skeleton::Fixture::for_endpoint(&upstream.base_url());
        let prefix = empty_prefix();
        let request = request(&prefix);
        let error = gateway(&fixture, access)
            .chat(
                &request,
                Context::default(),
                &Cancellation::default(),
                &mut |_| panic!("a refused request must not forward"),
            )
            .await
            .err()
            .unwrap()
            .refusal();
        assert_eq!((error.status, error.code), (403, code));
        assert_eq!(
            (upstream.accepted(), upstream.requests()),
            (0, 0),
            "{code} must refuse before opening a connection"
        );
    }
}

/// A completed exchange is the only thing that fills the pool, and the next
/// real turn reuses that entry. This is the warm connection a preconnect would
/// have had to create before the first turn, without a request.
#[tokio::test]
async fn a_completed_exchange_leaves_one_reusable_connection() {
    let upstream = Upstream::start();
    let fixture = skeleton::Fixture::for_endpoint(&upstream.base_url());
    let key = secrets::read(&fixture.key_path, "anthropic").unwrap();
    let base = upstream.base_url();
    let payload = payload();
    let client = Client::with_read_timeout(Some(DEADLINE)).unwrap();
    for turn in 0..2 {
        let mut attempts = 0;
        let completed = client
            .stream(
                chat(&base, &payload),
                &key,
                &Cancellation::default(),
                &mut |event| {
                    if matches!(event, Event::Sending) {
                        attempts += 1;
                    }
                    Control::Continue
                },
            )
            .await
            .unwrap();
        assert_eq!(attempts, 1, "turn {turn} must forward exactly once");
        assert_eq!(completed.assistant["content"], "Synthetic");
    }
    assert_eq!(upstream.requests(), 2);
    assert_eq!(
        upstream.accepted(),
        1,
        "the second turn must reuse the pooled connection"
    );
}
