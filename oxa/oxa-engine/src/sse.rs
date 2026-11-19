//! A1: SSE parsing and the streaming POST.
use crate::http::{build_request, connect, framing_of, header, parse_head, BodyDecoder, Conn};
use crate::Url;
use oxa_proto::{codes, CancelToken, OxaError};

/// One parsed SSE item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SseEvent {
    /// The payload of a `data:` event (multi-line data joined with `\n`), `[DONE]` excluded.
    Data(String),
    /// A comment line without the leading `:` and one optional space (`prefill 10/200`).
    Comment(String),
    /// `data: [DONE]`.
    Done,
}

/// Incremental SSE parser. Correct for any split of the byte stream, including inside a UTF-8
/// sequence and between `\r` and `\n` (acceptance: split at every byte offset).
///
/// Lines end at `\n`, `\r\n` or a lone `\r`. A blank line dispatches the pending `data:` lines
/// as one event; a comment is delivered as soon as its line ends. Other fields (`event:`, `id:`,
/// `retry:`) are ignored: the controller's in-stream error is `event: error` + `data: {"error":..}`
/// and is recognised by its data (F3).
#[derive(Debug, Default)]
pub struct SseParser {
    buf: Vec<u8>,
    /// The last byte was `\r`, so a following `\n` belongs to the same line break.
    pending_cr: bool,
    /// `data:` values of the event being assembled.
    data: Vec<String>,
    data_bytes: usize,
}

impl SseParser {
    pub fn new() -> SseParser {
        SseParser::default()
    }

    /// Bound incomplete lines and multiline event data before buffering another
    /// byte. Complete events may continue indefinitely without accumulating.
    pub fn feed_bounded(&mut self, bytes: &[u8], maximum: usize) -> Result<Vec<SseEvent>, OxaError> {
        let mut events = Vec::new();
        for byte in bytes {
            if self.buf.len().saturating_add(self.data_bytes) >= maximum {
                return Err(broken("SSE event exceeds its buffer limit"));
            }
            events.extend(self.feed(std::slice::from_ref(byte)));
        }
        Ok(events)
    }

    /// Feed bytes; returns the events completed by them, in order.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        let mut events = Vec::new();
        for &b in bytes {
            match b {
                b'\n' if self.pending_cr => self.pending_cr = false,
                b'\r' => {
                    self.pending_cr = true;
                    let line = self.take_line();
                    self.line(line, &mut events);
                }
                b'\n' => {
                    self.pending_cr = false;
                    let line = self.take_line();
                    self.line(line, &mut events);
                }
                _ => {
                    self.pending_cr = false;
                    self.buf.push(b);
                }
            }
        }
        events
    }

    fn take_line(&mut self) -> String {
        let bytes = std::mem::take(&mut self.buf);
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn line(&mut self, line: String, events: &mut Vec<SseEvent>) {
        if line.is_empty() {
            self.data_bytes = 0;
            if self.data.is_empty() {
                return;
            }
            let joined = self.data.join("\n");
            self.data.clear();
            events.push(if joined == "[DONE]" {
                SseEvent::Done
            } else {
                SseEvent::Data(joined)
            });
            return;
        }
        if let Some(rest) = line.strip_prefix(':') {
            events.push(SseEvent::Comment(
                rest.strip_prefix(' ').unwrap_or(rest).to_string(),
            ));
            return;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line.as_str(), ""),
        };
        if field == "data" {
            self.data_bytes = self.data_bytes.saturating_add(line.len());
            self.data.push(value.to_string());
        }
    }
}

/// Whether the caller wants more events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Continue,
    /// Stop reading and close the connection (not an error).
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamOpts {
    pub connect_timeout_ms: u64,
    /// Maximum time without receiving any byte (headers, data or comment). Default 900 000.
    pub silence_limit_ms: u64,
}

impl Default for StreamOpts {
    fn default() -> Self {
        StreamOpts {
            connect_timeout_ms: 5_000,
            silence_limit_ms: 900_000,
        }
    }
}

/// How a stream ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEnd {
    pub status: u16,
    /// At least one `Data` event was delivered (after this, no retry is allowed).
    pub got_data: bool,
    /// `[DONE]` was received. A server that closes right after `[DONE]` is a clean end.
    pub done: bool,
    /// Body of a non-2xx response.
    pub error_body: Option<String>,
    /// `Retry-After` of a non-2xx response, in whole seconds (the delta-seconds form only; an
    /// HTTP-date is ignored). The controller sends `Retry-After: 30` with its queue-full 503.
    pub retry_after: Option<u32>,
}

/// POST `body` and stream the SSE response to `on`.
///
/// * Non-2xx: returns `Ok` with `status` and `error_body` (the caller maps 400
///   `exceed_context_size_error` to `context_overflow`).
/// * Refused / unreachable: `Err(engine_down)` + hint. Silence past the limit or a close before
///   `[DONE]`: `Err(stream_broken)`. Cancel: `Err(aborted)`, the socket is closed at once.
/// * Polls `cancel` at least every 100 ms (socket read timeout).
///
/// `[DONE]` ends the call at once with `done = true`; the server may close or keep the socket.
/// Comments count as liveness, so there is no wall-clock limit.
pub fn post_sse(
    url: &Url,
    path: &str,
    headers: &[(&str, String)],
    body: &[u8],
    opts: &StreamOpts,
    cancel: &CancelToken,
    on: &mut dyn FnMut(SseEvent) -> Flow,
) -> Result<SseEnd, OxaError> {
    post_with_policy(
        url,
        path,
        headers,
        body,
        ReadPolicy {
            stream: opts,
            maximum: None,
        },
        cancel,
        on,
    )
}

/// The existing SSE transport with a cap on buffered events and HTTP error bodies.
pub struct BoundedStreamOpts {
    pub stream: StreamOpts,
    pub max_event_bytes: usize,
}
pub fn post_sse_bounded(
    url: &Url,
    path: &str,
    headers: &[(&str, String)],
    body: &[u8],
    opts: &BoundedStreamOpts,
    cancel: &CancelToken,
    on: &mut dyn FnMut(SseEvent) -> Flow,
) -> Result<SseEnd, OxaError> {
    if opts.max_event_bytes == 0 {
        return Err(broken("SSE buffer limit must be positive"));
    }
    post_with_policy(
        url,
        path,
        headers,
        body,
        ReadPolicy {
            stream: &opts.stream,
            maximum: Some(opts.max_event_bytes),
        },
        cancel,
        on,
    )
}

struct ReadPolicy<'a> {
    stream: &'a StreamOpts,
    maximum: Option<usize>,
}
fn post_with_policy(
    url: &Url,
    path: &str,
    headers: &[(&str, String)],
    body: &[u8],
    policy: ReadPolicy<'_>,
    cancel: &CancelToken,
    on: &mut dyn FnMut(SseEvent) -> Flow,
) -> Result<SseEnd, OxaError> {
    let opts = policy.stream;
    let mut all: Vec<(&str, String)> = headers.to_vec();
    if !has_header(headers, "content-type") {
        all.push(("Content-Type", "application/json".to_string()));
    }
    if !has_header(headers, "accept") {
        all.push(("Accept", "text/event-stream".to_string()));
    }
    let req = build_request("POST", url, path, &all, body)?;

    let stream = connect(url, opts.connect_timeout_ms, cancel)?;
    let mut conn = Conn::new(
        stream,
        cancel,
        opts.silence_limit_ms,
        codes::STREAM_BROKEN,
        codes::STREAM_BROKEN,
    )?;
    conn.write_all(&req)?;

    let (head, rest) = conn.read_head()?;
    let parsed = parse_head(&head).map_err(|m| broken(&m))?;
    let status = parsed.status;
    let framing = framing_of(&parsed.headers).map_err(|m| broken(&m))?;
    let mut dec = BodyDecoder::new(framing);

    if !(200..300).contains(&status) {
        // Read the error body to the end; a broken body still reports the status.
        let mut body = Vec::new();
        let mut failure = dec.feed(&rest, &mut body).err();
        if policy.maximum.is_some_and(|limit| body.len() > limit) {
            return Err(broken("HTTP error body exceeds its buffer limit"));
        }
        let mut buf = [0u8; 4096];
        while failure.is_none() && !dec.done() {
            match conn.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if let Err(e) = dec.feed(&buf[..n], &mut body) {
                        failure = Some(e);
                    }
                    if policy.maximum.is_some_and(|limit| body.len() > limit) {
                        return Err(broken("HTTP error body exceeds its buffer limit"));
                    }
                }
                Err(e) => failure = Some(e),
            }
        }
        if let Some(e) = failure {
            if e.code == codes::ABORTED {
                return Err(e);
            }
        }
        let retry_after = header(&parsed.headers, "retry-after").and_then(|v| v.trim().parse::<u32>().ok());
        return Ok(SseEnd {
            status,
            got_data: false,
            done: false,
            error_body: Some(String::from_utf8_lossy(&body).into_owned()),
            retry_after,
        });
    }

    let mut parser = SseParser::new();
    let mut end = SseEnd {
        status,
        got_data: false,
        done: false,
        error_body: None,
        retry_after: None,
    };
    if deliver(&rest, &mut dec, &mut parser, &mut end, policy.maximum, on)? {
        return Ok(end);
    }
    let mut chunk = vec![0u8; 16 * 1024];
    while !dec.done() {
        let n = conn.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        if deliver(&chunk[..n], &mut dec, &mut parser, &mut end, policy.maximum, on)? {
            return Ok(end);
        }
    }
    if end.done {
        return Ok(end);
    }
    Err(broken("the engine closed the stream before [DONE]"))
}

/// Decode `bytes`, parse them and hand each event to `on`. Returns `true` when the stream is
/// over (`[DONE]` seen, or the caller asked to stop).
fn deliver(
    bytes: &[u8],
    dec: &mut BodyDecoder,
    parser: &mut SseParser,
    end: &mut SseEnd,
    maximum: Option<usize>,
    on: &mut dyn FnMut(SseEvent) -> Flow,
) -> Result<bool, OxaError> {
    let mut payload = Vec::new();
    dec.feed(bytes, &mut payload)?;
    let events = match maximum {
        Some(limit) => parser.feed_bounded(&payload, limit)?,
        None => parser.feed(&payload),
    };
    for ev in events {
        let is_done = ev == SseEvent::Done;
        match ev {
            SseEvent::Data(_) => end.got_data = true,
            SseEvent::Done => end.done = true,
            SseEvent::Comment(_) => {}
        }
        let flow = on(ev);
        if is_done || flow == Flow::Stop {
            return Ok(true);
        }
    }
    Ok(false)
}

fn has_header(headers: &[(&str, String)], name: &str) -> bool {
    headers.iter().any(|(n, _)| n.eq_ignore_ascii_case(name))
}

fn broken(what: &str) -> OxaError {
    OxaError::new(codes::STREAM_BROKEN, what.to_string())
}
