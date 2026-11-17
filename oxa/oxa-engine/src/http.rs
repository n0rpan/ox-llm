//! A1: minimal HTTP/1.1 client over TcpStream.
use oxa_proto::{codes, CancelToken, OxaError};
use std::fmt::Write as _;
use std::io::{self, ErrorKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

/// How often a blocked socket call wakes up to poll cancellation and the silence limit.
pub(crate) const POLL: Duration = Duration::from_millis(100);
/// Largest response head (status line + headers) or chunk-size line we accept.
const MAX_HEAD: usize = 64 * 1024;
const ENGINE_HINT: &str = "start the engine or set OXA_ENGINE_URL";

/// A parsed `http://host[:port][/base]` URL. `https` is rejected with `invalid` and a hint
/// (the engine is plain HTTP).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    pub host: String,
    pub port: u16,
    /// Path prefix without a trailing slash (`""` or `/base`).
    pub base_path: String,
}

impl Url {
    pub fn parse(s: &str) -> Result<Url, OxaError> {
        let s = s.trim();
        let bad = |msg: String, hint: &str| OxaError::new(codes::INVALID, msg).with_hint(hint);
        if s.get(..8).is_some_and(|p| p.eq_ignore_ascii_case("https://")) {
            return Err(bad(
                format!("https is not supported for the engine URL: {s}"),
                "the engine speaks plain HTTP; use http://host:port (TLS is not supported)",
            ));
        }
        let rest = match s.get(..7) {
            Some(p) if p.eq_ignore_ascii_case("http://") => &s[7..],
            _ => {
                return Err(bad(
                    format!("engine URL must start with http://: {s}"),
                    "use http://host:port, for example http://127.0.0.1:8092",
                ))
            }
        };
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        if path.chars().any(|c| c == '?' || c == '#' || c.is_whitespace()) {
            return Err(bad(
                format!("engine URL has a query, fragment or whitespace: {s}"),
                "the engine URL is http://host:port plus an optional base path",
            ));
        }
        if authority.contains('@') {
            return Err(bad(
                format!("credentials in the engine URL are not supported: {s}"),
                "remove the user part; the engine is local or on the LAN without auth",
            ));
        }
        let (host, port_text): (&str, Option<&str>) = match authority.strip_prefix('[') {
            Some(inner) => {
                let end = inner
                    .find(']')
                    .ok_or_else(|| bad(format!("unclosed [ in engine URL: {s}"), "use http://[::1]:8080"))?;
                let after = &inner[end + 1..];
                match after.strip_prefix(':') {
                    Some(p) => (&inner[..end], Some(p)),
                    None if after.is_empty() => (&inner[..end], None),
                    None => {
                        return Err(bad(
                            format!("bad engine URL authority: {s}"),
                            "use http://host:port",
                        ))
                    }
                }
            }
            None => match authority.rsplit_once(':') {
                Some((h, p)) => (h, Some(p)),
                None => (authority, None),
            },
        };
        if host.is_empty() || host.chars().any(|c| c.is_whitespace() || c == '[' || c == ']') {
            return Err(bad(
                format!("engine URL has no host: {s}"),
                "use http://host:port",
            ));
        }
        let port = match port_text {
            None => 80,
            Some(p) => p.parse::<u16>().ok().filter(|&p| p != 0).ok_or_else(|| {
                bad(
                    format!("invalid port in engine URL: {p:?}"),
                    "use http://host:port",
                )
            })?,
        };
        Ok(Url {
            host: host.to_string(),
            port,
            base_path: path.trim_end_matches('/').to_string(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest<'a> {
    pub method: &'a str,
    pub url: &'a Url,
    /// Path below `url.base_path`, e.g. `/v1/models`.
    pub path: &'a str,
    pub headers: &'a [(&'a str, String)],
    pub body: &'a [u8],
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    /// Header names lowercased.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// One non-streaming request (`Connection: close`; handles `Content-Length` and chunked
/// bodies). Connection refused or unreachable = `engine_down` with the hint
/// "start the engine or set OXA_ENGINE_URL"; timeout = `timeout`.
///
/// `timeout_ms` is the silence limit: no byte from the engine for that long is a `timeout`.
pub fn http_request(r: &HttpRequest) -> Result<HttpResponse, OxaError> {
    let never = CancelToken::new();
    let stream = connect(r.url, r.timeout_ms, &never)?;
    let mut conn = Conn::new(stream, &never, r.timeout_ms, codes::TIMEOUT, codes::ENGINE_HTTP)?;
    let req = build_request(r.method, r.url, r.path, r.headers, r.body)?;
    conn.write_all(&req)?;
    let (head, rest) = conn.read_head()?;
    let parsed = parse_head(&head).map_err(|m| OxaError::new(codes::ENGINE_HTTP, m))?;
    let framing = framing_of(&parsed.headers).map_err(|m| OxaError::new(codes::ENGINE_HTTP, m))?;
    let mut dec = BodyDecoder::new(framing);
    let mut body = Vec::new();
    dec.feed(&rest, &mut body)?;
    let mut buf = vec![0u8; 16 * 1024];
    while !dec.done() {
        let n = conn.read(&mut buf)?;
        if n == 0 {
            break;
        }
        dec.feed(&buf[..n], &mut body)?;
    }
    if !dec.complete() {
        return Err(OxaError::new(
            codes::ENGINE_HTTP,
            "the engine closed the connection before the response body was complete",
        ));
    }
    Ok(HttpResponse {
        status: parsed.status,
        headers: parsed.headers,
        body,
    })
}

// ---- crate-internal plumbing shared with `sse.rs` ----

/// Open a TCP connection to `url`, polling `cancel` while the connect runs. The connect itself
/// runs on a helper thread so a cancel never waits for the connect timeout.
pub(crate) fn connect(url: &Url, timeout_ms: u64, cancel: &CancelToken) -> Result<TcpStream, OxaError> {
    if cancel.is_cancelled() {
        return Err(aborted());
    }
    // P4: offline, every connection of the harness is to loopback only.
    oxa_proto::check_connect("a connection", &url.host)?;
    let timeout = Duration::from_millis(timeout_ms.max(1));
    let (tx, rx) = mpsc::channel();
    let (host, port) = (url.host.clone(), url.port);
    thread::Builder::new()
        .name("oxa-connect".into())
        .spawn(move || {
            // A dropped receiver (cancelled caller) closes the stream when `res` is dropped.
            let res = connect_now(&host, port, timeout);
            let _ = tx.send(res);
        })
        .map_err(|e| OxaError::io("spawn connect thread", &e))?;
    loop {
        match rx.recv_timeout(POLL) {
            Ok(Ok(stream)) => return Ok(stream),
            Ok(Err(e)) => return Err(map_connect_err(url, &e)),
            Err(RecvTimeoutError::Timeout) => {
                if cancel.is_cancelled() {
                    return Err(aborted());
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(OxaError::new(
                    codes::INTERNAL,
                    "connect thread ended without a result",
                ))
            }
        }
    }
}

fn connect_now(host: &str, port: u16, timeout: Duration) -> io::Result<TcpStream> {
    let mut last = None;
    for addr in (host, port).to_socket_addrs()? {
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(stream) => return Ok(stream),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::new(ErrorKind::NotFound, "no address for the engine host")))
}

fn map_connect_err(url: &Url, e: &io::Error) -> OxaError {
    let at = format!("{}:{}", url.host, url.port);
    if e.kind() == ErrorKind::TimedOut {
        OxaError::new(
            codes::TIMEOUT,
            format!("connecting to the engine at {at} timed out"),
        )
        .with_hint("the engine did not accept the connection in time; check OXA_ENGINE_URL")
    } else {
        OxaError::new(
            codes::ENGINE_DOWN,
            format!("cannot reach the engine at {at}: {e}"),
        )
        .with_hint(ENGINE_HINT)
    }
}

fn aborted() -> OxaError {
    OxaError::new(codes::ABORTED, "request cancelled")
}

/// Serialise one request: request line, `Host`, `Connection: close`, `Content-Length` (always),
/// the caller's headers (`Host`, `Content-Length`, `Connection` and `Transfer-Encoding` are
/// ours), a blank line, then the body.
pub(crate) fn build_request(
    method: &str,
    url: &Url,
    path: &str,
    headers: &[(&str, String)],
    body: &[u8],
) -> Result<Vec<u8>, OxaError> {
    let bad = |what: &str| OxaError::new(codes::INVALID, format!("invalid HTTP request: {what}"));
    if method.is_empty() || !method.bytes().all(|b| b.is_ascii_uppercase()) {
        return Err(bad("method"));
    }
    if !path.starts_with('/') || path.bytes().any(|b| b <= b' ') {
        return Err(bad("path must start with / and contain no spaces"));
    }
    let host = if url.host.contains(':') {
        format!("[{}]:{}", url.host, url.port)
    } else {
        format!("{}:{}", url.host, url.port)
    };
    let mut head = String::with_capacity(256);
    let _ = write!(
        head,
        "{method} {}{path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nContent-Length: {}\r\n",
        url.base_path,
        body.len()
    );
    for (name, value) in headers {
        if name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(bad("header name"));
        }
        if value.contains(['\r', '\n']) {
            return Err(bad("header value contains a line break"));
        }
        if matches!(
            name.to_ascii_lowercase().as_str(),
            "host" | "content-length" | "connection" | "transfer-encoding"
        ) {
            continue;
        }
        let _ = write!(head, "{name}: {value}\r\n");
    }
    head.push_str("\r\n");
    let mut out = head.into_bytes();
    out.extend_from_slice(body);
    Ok(out)
}

/// A connection with the liveness rule: any byte resets the silence clock, a poll tick with no
/// byte for `silence` is `silent_code`, and cancellation is seen within one [`POLL`].
pub(crate) struct Conn<'a> {
    stream: TcpStream,
    cancel: &'a CancelToken,
    silence: Duration,
    last: Instant,
    silent_code: &'static str,
    broken_code: &'static str,
}

impl<'a> Conn<'a> {
    pub(crate) fn new(
        stream: TcpStream,
        cancel: &'a CancelToken,
        silence_ms: u64,
        silent_code: &'static str,
        broken_code: &'static str,
    ) -> Result<Self, OxaError> {
        let setup = |e: io::Error| OxaError::io("socket setup", &e);
        stream.set_nodelay(true).map_err(setup)?;
        let silence = Duration::from_millis(silence_ms.max(1));
        stream.set_read_timeout(Some(POLL)).map_err(setup)?;
        // Writes wake every POLL too: an engine that stopped reading must not hold a cancelled
        // request inside one blocking `write_all` for the whole silence limit.
        stream.set_write_timeout(Some(POLL)).map_err(setup)?;
        Ok(Conn {
            stream,
            cancel,
            silence,
            last: Instant::now(),
            silent_code,
            broken_code,
        })
    }

    /// Write all of `bytes`, polling the cancel token every [`POLL`] while the engine is not
    /// reading; no byte accepted for the silence limit is `silent_code`.
    pub(crate) fn write_all(&mut self, bytes: &[u8]) -> Result<(), OxaError> {
        let mut off = 0;
        let mut progress = Instant::now();
        while off < bytes.len() {
            if self.cancel.is_cancelled() {
                return Err(aborted());
            }
            match self.stream.write(&bytes[off..]) {
                Ok(0) => {
                    return Err(OxaError::new(
                        self.broken_code,
                        "request write failed: the engine closed the connection",
                    ))
                }
                Ok(n) => {
                    off += n;
                    progress = Instant::now();
                }
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    if progress.elapsed() >= self.silence {
                        return Err(self.silent_error());
                    }
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => {
                    return Err(OxaError::new(
                        self.broken_code,
                        format!("request write failed: {e}"),
                    ))
                }
            }
        }
        self.last = Instant::now();
        Ok(())
    }

    /// Read the next bytes. `Ok(0)` is end of stream.
    pub(crate) fn read(&mut self, buf: &mut [u8]) -> Result<usize, OxaError> {
        loop {
            if self.cancel.is_cancelled() {
                return Err(aborted());
            }
            match self.stream.read(buf) {
                Ok(0) => return Ok(0),
                Ok(n) => {
                    self.last = Instant::now();
                    return Ok(n);
                }
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    if self.last.elapsed() >= self.silence {
                        return Err(self.silent_error());
                    }
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => {
                    return Err(OxaError::new(
                        self.broken_code,
                        format!("connection read failed: {e}"),
                    ))
                }
            }
        }
    }

    /// Read until the blank line ending the response head. Returns the head (without the blank
    /// line) and whatever body bytes arrived with it.
    pub(crate) fn read_head(&mut self) -> Result<(Vec<u8>, Vec<u8>), OxaError> {
        let mut acc = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            if let Some(pos) = find(&acc, b"\r\n\r\n") {
                let rest = acc.split_off(pos + 4);
                acc.truncate(pos);
                return Ok((acc, rest));
            }
            if acc.len() > MAX_HEAD {
                return Err(OxaError::new(
                    self.broken_code,
                    "engine response head is too large",
                ));
            }
            let n = self.read(&mut chunk)?;
            if n == 0 {
                return Err(OxaError::new(
                    self.broken_code,
                    "the engine closed the connection before the response headers",
                ));
            }
            acc.extend_from_slice(&chunk[..n]);
        }
    }

    fn silent_error(&self) -> OxaError {
        OxaError::new(
            self.silent_code,
            format!("no bytes from the engine for {} ms", self.silence.as_millis()),
        )
    }
}

/// Status and headers of a response.
pub(crate) struct Head {
    pub status: u16,
    /// Names lowercased, values trimmed.
    pub headers: Vec<(String, String)>,
}

/// Parse a head as returned by [`Conn::read_head`] (no trailing blank line).
pub(crate) fn parse_head(head: &[u8]) -> Result<Head, String> {
    let text = String::from_utf8_lossy(head);
    let mut lines = text.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let mut parts = status_line.split_whitespace();
    let version = parts.next().unwrap_or("");
    let status = parts.next().and_then(|s| s.parse::<u16>().ok());
    if !version.starts_with("HTTP/") {
        return Err(format!("malformed status line: {status_line:?}"));
    }
    let status = status.ok_or_else(|| format!("malformed status line: {status_line:?}"))?;
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        match line.split_once(':') {
            Some((name, value)) => headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string())),
            None => return Err(format!("malformed header line: {line:?}")),
        }
    }
    Ok(Head { status, headers })
}

/// Case-insensitive header lookup on a list whose names are already lowercased.
pub(crate) fn header<'h>(headers: &'h [(String, String)], name: &str) -> Option<&'h str> {
    headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
}

/// How the body of a response ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Framing {
    /// Read until the server closes.
    Eof,
    /// Exactly this many bytes.
    Length(usize),
    Chunked,
}

/// Framing from the response headers. Chunked wins over `Content-Length`.
pub(crate) fn framing_of(headers: &[(String, String)]) -> Result<Framing, String> {
    if let Some(te) = header(headers, "transfer-encoding") {
        if te.to_ascii_lowercase().contains("chunked") {
            return Ok(Framing::Chunked);
        }
    }
    match header(headers, "content-length") {
        Some(cl) => cl
            .parse::<usize>()
            .map(Framing::Length)
            .map_err(|_| format!("bad content-length: {cl:?}")),
        None => Ok(Framing::Eof),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Chunk {
    Size,
    Data(usize),
    DataEnd,
    Trailer,
}

/// Incremental body decoder for all three framings. Feed it raw bytes in any split; it hands
/// back payload bytes as they become available.
pub(crate) struct BodyDecoder {
    framing: Framing,
    chunk: Chunk,
    /// Raw bytes not yet consumed (chunked framing only).
    buf: Vec<u8>,
    remaining: usize,
    done: bool,
}

impl BodyDecoder {
    pub(crate) fn new(framing: Framing) -> Self {
        BodyDecoder {
            framing,
            chunk: Chunk::Size,
            buf: Vec::new(),
            remaining: match framing {
                Framing::Length(n) => n,
                _ => 0,
            },
            done: framing == Framing::Length(0),
        }
    }

    /// The framing reached its end marker (last chunk, all bytes of a length body).
    pub(crate) fn done(&self) -> bool {
        self.done
    }

    /// The body is complete: its end marker was seen, or the framing is read-until-close.
    pub(crate) fn complete(&self) -> bool {
        self.done || self.framing == Framing::Eof
    }

    /// Append the payload bytes that `raw` completes to `out`.
    pub(crate) fn feed(&mut self, raw: &[u8], out: &mut Vec<u8>) -> Result<(), OxaError> {
        if self.done {
            return Ok(());
        }
        match self.framing {
            Framing::Eof => out.extend_from_slice(raw),
            Framing::Length(_) => {
                let n = raw.len().min(self.remaining);
                out.extend_from_slice(&raw[..n]);
                self.remaining -= n;
                self.done = self.remaining == 0;
            }
            Framing::Chunked => {
                self.buf.extend_from_slice(raw);
                self.decode_chunked(out)?;
            }
        }
        Ok(())
    }

    fn decode_chunked(&mut self, out: &mut Vec<u8>) -> Result<(), OxaError> {
        loop {
            match self.chunk {
                Chunk::Size => {
                    let pos = match find(&self.buf, b"\r\n") {
                        Some(p) => p,
                        None if self.buf.len() > MAX_HEAD => return Err(bad_chunk("size line too long")),
                        None => return Ok(()),
                    };
                    let line = String::from_utf8_lossy(&self.buf[..pos]).into_owned();
                    let size_text = line.split(';').next().unwrap_or("").trim();
                    let size = usize::from_str_radix(size_text, 16)
                        .map_err(|_| bad_chunk(&format!("bad chunk size {size_text:?}")))?;
                    self.buf.drain(..pos + 2);
                    self.chunk = if size == 0 {
                        Chunk::Trailer
                    } else {
                        Chunk::Data(size)
                    };
                }
                Chunk::Data(rem) => {
                    if self.buf.is_empty() {
                        return Ok(());
                    }
                    let n = rem.min(self.buf.len());
                    out.extend_from_slice(&self.buf[..n]);
                    self.buf.drain(..n);
                    self.chunk = if n == rem {
                        Chunk::DataEnd
                    } else {
                        Chunk::Data(rem - n)
                    };
                }
                Chunk::DataEnd => {
                    if self.buf.len() < 2 {
                        return Ok(());
                    }
                    if &self.buf[..2] != b"\r\n" {
                        return Err(bad_chunk("missing CRLF after chunk data"));
                    }
                    self.buf.drain(..2);
                    self.chunk = Chunk::Size;
                }
                Chunk::Trailer => {
                    if self.buf.len() < 2 {
                        return Ok(());
                    }
                    if self.buf.starts_with(b"\r\n") {
                        self.buf.drain(..2);
                        self.done = true;
                        return Ok(());
                    }
                    match find(&self.buf, b"\r\n\r\n") {
                        Some(p) => {
                            self.buf.drain(..p + 4);
                            self.done = true;
                            return Ok(());
                        }
                        None if self.buf.len() > MAX_HEAD => return Err(bad_chunk("trailer too long")),
                        None => return Ok(()),
                    }
                }
            }
        }
    }
}

fn bad_chunk(what: &str) -> OxaError {
    OxaError::new(codes::STREAM_BROKEN, format!("malformed chunked body: {what}"))
}

/// Position of the first `needle` in `hay`.
pub(crate) fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}
