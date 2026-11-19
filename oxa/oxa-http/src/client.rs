//! One HTTP client on reqwest: builder profiles and a sync facade.
//!
//! [`reqwest_builder`] is the one place that sets transport policy. [`Client`] runs requests on
//! one process-wide runtime ([`runtime`]) and is callable from any thread, also from inside
//! another tokio runtime: the call spawns the future and waits on a std channel, it never
//! `block_on`s. Errors are mapped by category only and never carry header values or the URL.

use std::io::ErrorKind;
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use oxa_proto::CancelToken;

/// How often a waiting call looks at its cancel token.
const POLL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// Plain HTTP to a local server: HTTP/1.1 only, one connection per request.
    Loopback,
    /// Anything else (TLS capable).
    Remote,
}

/// Both profiles: no ambient proxy, no redirects. Loopback also: HTTP/1.1 only and no idle pool.
pub fn reqwest_builder(profile: Profile) -> reqwest::ClientBuilder {
    let b = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none());
    match profile {
        Profile::Loopback => b.http1_only().pool_max_idle_per_host(0),
        Profile::Remote => b,
    }
}

/// The process-wide runtime (one worker, io and time).
pub fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("oxa-http")
            .enable_all()
            .build()
            .unwrap_or_else(|e| panic!("oxa-http: cannot start the runtime: {e}"))
    })
}

pub struct Request<'a> {
    pub method: &'a str,
    pub url: &'a str,
    pub headers: &'a [(&'a str, String)],
    pub body: &'a [u8],
    /// Bound for connecting. Connect and head share one timer, so a silent head is reported
    /// after `connect_timeout + silence`.
    pub connect_timeout: Duration,
    /// Longest time with no byte: for the head and between body chunks.
    pub silence: Duration,
    /// Bound for the whole call when set.
    pub total: Option<Duration>,
    /// `send` only: larger bodies are `Error::TooLarge`.
    pub max_body: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    /// Names lowercased.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Connect,
    Head,
    Body,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Connect(ErrorKind, String),
    Timeout(Phase),
    Cancelled,
    TooLarge,
    Protocol(String),
    /// Reserved for callers that follow redirects with their own policy; the facade never
    /// produces it (a 3xx answer is a normal [`Response`]).
    Redirect(u16, Option<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Continue,
    Stop,
}

/// Called once with the status and the lowercased headers.
pub type HeadFn<'a> = dyn FnMut(u16, &[(String, String)]) -> Flow + 'a;
/// Called for each body chunk as it arrives.
pub type BytesFn<'a> = dyn FnMut(&[u8]) -> Flow + 'a;

pub struct Client {
    inner: reqwest::Client,
}

enum Event {
    Head(u16, Vec<(String, String)>),
    Bytes(Vec<u8>),
    Done(Result<(), Error>),
}

struct Owned {
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    connect: Duration,
    silence: Duration,
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl Client {
    pub fn new(builder: reqwest::ClientBuilder) -> Result<Self, Error> {
        // Building a client starts no I/O, but reqwest may touch the runtime's reactor handle.
        let _guard = runtime().enter();
        let inner = builder.build().map_err(|e| map_error(&e))?;
        Ok(Self { inner })
    }

    pub fn send(&self, req: &Request<'_>, cancel: &CancelToken) -> Result<Response, Error> {
        let mut status = 0;
        let mut headers = Vec::new();
        let mut body: Vec<u8> = Vec::new();
        self.pump(req, cancel, &mut |ev| {
            match ev {
                Event::Head(s, h) => {
                    let declared = h
                        .iter()
                        .find(|(k, _)| k == "content-length")
                        .and_then(|(_, v)| v.trim().parse::<usize>().ok());
                    if declared.is_some_and(|n| n > req.max_body) {
                        return Err(Error::TooLarge);
                    }
                    status = s;
                    headers = h;
                }
                Event::Bytes(b) => {
                    if body.len().saturating_add(b.len()) > req.max_body {
                        return Err(Error::TooLarge);
                    }
                    body.extend_from_slice(&b);
                }
                Event::Done(_) => {}
            }
            Ok(Flow::Continue)
        })?;
        Ok(Response {
            status,
            headers,
            body,
        })
    }

    /// Calls `on_head` once, then `on_bytes` for each chunk as it arrives. `Flow::Stop` ends the
    /// call with `Ok(())` and drops the connection.
    pub fn stream(
        &self,
        req: &Request<'_>,
        cancel: &CancelToken,
        on_head: &mut HeadFn<'_>,
        on_bytes: &mut BytesFn<'_>,
    ) -> Result<(), Error> {
        self.pump(req, cancel, &mut |ev| {
            Ok(match ev {
                Event::Head(s, h) => on_head(s, &h),
                Event::Bytes(b) => on_bytes(&b),
                Event::Done(_) => Flow::Continue,
            })
        })
    }

    /// Spawn the request on the runtime and feed its events to `handle` on this thread, looking
    /// at the cancel token and the total deadline every [`POLL`].
    fn pump(
        &self,
        req: &Request<'_>,
        cancel: &CancelToken,
        handle: &mut dyn FnMut(Event) -> Result<Flow, Error>,
    ) -> Result<(), Error> {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let owned = Owned {
            method: req.method.to_string(),
            url: req.url.to_string(),
            headers: req
                .headers
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
            body: req.body.to_vec(),
            connect: req.connect_timeout,
            silence: req.silence,
        };
        let (tx, rx) = mpsc::sync_channel::<Event>(16);
        let client = self.inner.clone();
        let _task = AbortOnDrop(runtime().spawn(async move {
            let done = drive(&client, owned, &tx).await;
            emit(&tx, Event::Done(done)).await;
        }));
        let start = Instant::now();
        let mut head_seen = false;
        loop {
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            if req.total.is_some_and(|t| start.elapsed() >= t) {
                return Err(Error::Timeout(if head_seen { Phase::Body } else { Phase::Head }));
            }
            match rx.recv_timeout(POLL) {
                Ok(Event::Done(r)) => return r,
                Ok(ev) => {
                    head_seen |= matches!(ev, Event::Head(..));
                    if handle(ev)? == Flow::Stop {
                        return Ok(());
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(Error::Protocol("request task ended".into()))
                }
            }
        }
    }
}

/// Send with back-pressure that never blocks a runtime thread.
async fn emit(tx: &SyncSender<Event>, mut ev: Event) -> bool {
    loop {
        match tx.try_send(ev) {
            Ok(()) => return true,
            Err(TrySendError::Disconnected(_)) => return false,
            Err(TrySendError::Full(back)) => {
                ev = back;
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
    }
}

async fn drive(client: &reqwest::Client, r: Owned, tx: &SyncSender<Event>) -> Result<(), Error> {
    let method = reqwest::Method::from_bytes(r.method.as_bytes())
        .map_err(|_| Error::Protocol("invalid method".into()))?;
    let mut rb = client.request(method, r.url.as_str());
    for (k, v) in &r.headers {
        rb = rb.header(k.as_str(), v.as_str());
    }
    if !r.body.is_empty() {
        rb = rb.body(r.body);
    }
    let mut resp = match tokio::time::timeout(r.connect.saturating_add(r.silence), rb.send()).await {
        Err(_) => return Err(Error::Timeout(Phase::Head)),
        Ok(Err(e)) => return Err(map_error(&e)),
        Ok(Ok(resp)) => resp,
    };
    let headers = resp
        .headers()
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_ascii_lowercase(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect();
    if !emit(tx, Event::Head(resp.status().as_u16(), headers)).await {
        return Ok(());
    }
    loop {
        match tokio::time::timeout(r.silence, resp.chunk()).await {
            Err(_) => return Err(Error::Timeout(Phase::Body)),
            Ok(Err(e)) => {
                return Err(if e.is_timeout() {
                    Error::Timeout(Phase::Body)
                } else {
                    Error::Protocol("body read failed".into())
                })
            }
            Ok(Ok(None)) => return Ok(()),
            Ok(Ok(Some(chunk))) => {
                if !emit(tx, Event::Bytes(chunk.to_vec())).await {
                    return Ok(());
                }
            }
        }
    }
}

/// Category only: no URL, no header value, no reqwest text.
fn map_error(e: &reqwest::Error) -> Error {
    if e.is_connect() {
        if e.is_timeout() {
            return Error::Timeout(Phase::Connect);
        }
        let mut kind = ErrorKind::Other;
        let mut source: Option<&(dyn std::error::Error + 'static)> = Some(e);
        while let Some(s) = source {
            if let Some(io) = s.downcast_ref::<std::io::Error>() {
                kind = io.kind();
                break;
            }
            source = s.source();
        }
        return Error::Connect(kind, "connect failed".into());
    }
    if e.is_timeout() {
        return Error::Timeout(Phase::Head);
    }
    if e.is_builder() {
        return Error::Protocol("invalid request".into());
    }
    if e.is_body() || e.is_decode() {
        return Error::Protocol("body error".into());
    }
    Error::Protocol("request failed".into())
}
