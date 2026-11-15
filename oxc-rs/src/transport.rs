//! HTTP/1 transport for the controller's existing framing contract.
//!
//! Python reports malformed Content-Length through the normal JSON/auth/Host
//! gates. Hyper's wire decoder rejects those headers before an axum handler can
//! see them, so parse the bounded head here and dispatch through the same router.
use axum::{
    body::Body,
    http::{header, HeaderValue, Request, Version},
    Router,
};
use futures_util::StreamExt;
use std::{
    future::Future,
    io,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf},
    sync::{Mutex, Notify},
};
use tower::ServiceExt;

type Reader = Arc<Mutex<BufReader<Timed<Box<dyn AsyncRead + Send + Unpin>>>>>;

// A deadline measures one stalled I/O operation, not total request or generation
// time. Progress resets it, matching the shared Python socket's timeout.
struct Timed<T> {
    inner: T,
    timeout: Option<Duration>,
    read: Option<Pin<Box<tokio::time::Sleep>>>,
    write: Option<Pin<Box<tokio::time::Sleep>>>,
    read_timed_out: bool,
}
impl<T> Timed<T> {
    fn new(inner: T, timeout: Option<Duration>) -> Self {
        Self {
            inner,
            timeout,
            read: None,
            write: None,
            read_timed_out: false,
        }
    }
}
fn deadline(
    timer: &mut Option<Pin<Box<tokio::time::Sleep>>>,
    timeout: Option<Duration>,
    cx: &mut Context<'_>,
) -> Poll<io::Result<()>> {
    let Some(timeout) = timeout else {
        return Poll::Pending;
    };
    let timer = timer.get_or_insert_with(|| Box::pin(tokio::time::sleep(timeout)));
    if timer.as_mut().poll(cx).is_ready() {
        Poll::Ready(Err(io::ErrorKind::TimedOut.into()))
    } else {
        Poll::Pending
    }
}
impl<T: AsyncRead + Unpin> AsyncRead for Timed<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_read(cx, buf) {
            Poll::Ready(value) => {
                this.read = None;
                Poll::Ready(value)
            }
            Poll::Pending => {
                let value = deadline(&mut this.read, this.timeout, cx);
                if value.is_ready() {
                    this.read = None;
                    this.read_timed_out = true;
                }
                value
            }
        }
    }
}
impl<T: AsyncWrite + Unpin> AsyncWrite for Timed<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_write(cx, buf) {
            Poll::Ready(value) => {
                this.write = None;
                Poll::Ready(value)
            }
            Poll::Pending => deadline(&mut this.write, this.timeout, cx).map_ok(|()| 0),
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_flush(cx) {
            Poll::Ready(value) => {
                this.write = None;
                Poll::Ready(value)
            }
            Poll::Pending => deadline(&mut this.write, this.timeout, cx),
        }
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_shutdown(cx) {
            Poll::Ready(value) => {
                this.write = None;
                Poll::Ready(value)
            }
            Poll::Pending => deadline(&mut this.write, this.timeout, cx),
        }
    }
}

#[derive(Clone)]
pub struct CloseConnection;
#[derive(Clone)]
pub struct NoReply;

#[derive(Clone)]
struct Peer {
    reader: Reader,
    done: Arc<AtomicBool>,
    notify: Arc<Notify>,
    reading: Arc<Notify>,
    monitor: bool,
    reusable: bool,
}
impl Peer {
    fn body_done(&self) {
        self.done.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
    async fn disconnected(&self) {
        // The reference deliberately omits its peek-based disconnect monitor
        // for TLS sockets. Preserve that admission behavior for encrypted peers.
        if !self.monitor {
            std::future::pending::<()>().await;
        }
        loop {
            let ready = self.notify.notified();
            if self.done.load(Ordering::SeqCst) {
                break;
            }
            ready.await;
        }
        loop {
            let mut reader = self.reader.lock().await;
            // Leave pipelined bytes for the next request. Peeking must never
            // consume a subsequent request while an inference is running.
            if reader.buffer().is_empty() {
                match reader.fill_buf().await {
                    Ok([]) => return,
                    Err(e) if e.kind() == io::ErrorKind::TimedOut => {
                        // A peek watches for EOF during backend generation; it
                        // does not impose a request-wide generation deadline.
                        reader.get_mut().read_timed_out = false;
                        continue;
                    }
                    Err(_) => return,
                    _ => {}
                }
            }
            drop(reader);
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

async fn head(reader: &Reader) -> io::Result<Option<Vec<u8>>> {
    let mut head = Vec::new();
    let mut reader = reader.lock().await;
    reader.get_mut().read = None;
    for index in 0..101 {
        let mut line = Vec::new();
        let count = (&mut *reader)
            .take(65537)
            .read_until(b'\n', &mut line)
            .await?;
        if count == 0 && index == 0 {
            return Ok(None);
        }
        if count == 0 || count > 65536 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid HTTP header line",
            ));
        }
        let end = line == b"\r\n" || line == b"\n";
        head.extend_from_slice(&line);
        if end {
            return Ok(Some(head));
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "too many HTTP headers",
    ))
}

fn parse(raw: &[u8], reader: Reader, monitor: bool) -> io::Result<(Request<Body>, Peer)> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid HTTP request");
    let mut headers = [httparse::EMPTY_HEADER; 100];
    let mut parsed = httparse::Request::new(&mut headers);
    if !parsed.parse(raw).map_err(|_| invalid())?.is_complete() {
        return Err(invalid());
    }
    let target = parsed.path.ok_or_else(invalid)?;
    // Python's HTTP parser collapses leading slashes before the controller's
    // canonical-target and admission gates see the request.
    let target = if target.starts_with("//") {
        format!("/{}", target.trim_start_matches('/'))
    } else {
        target.to_owned()
    };
    let mut request = Request::builder()
        .method(parsed.method.ok_or_else(invalid)?)
        .uri(target)
        .version(if parsed.version == Some(0) {
            Version::HTTP_10
        } else {
            Version::HTTP_11
        });
    let mut length = 0;
    let mut length_seen = false;
    let mut length_valid = true;
    let mut length_fields = 0;
    let mut chunked = false;
    let mut lines = raw.split(|byte| *byte == b'\n').skip(1);
    for field in parsed.headers {
        // httparse validates the field syntax but trims trailing whitespace.
        // Python keeps it: in particular, "Bearer " must block alternative
        // key headers rather than becoming a different authorization scheme.
        let line = lines.next().ok_or_else(invalid)?;
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let colon = line
            .iter()
            .position(|byte| *byte == b':')
            .ok_or_else(invalid)?;
        let value = &line[colon + 1..];
        let start = value
            .iter()
            .position(|byte| !matches!(byte, b' ' | b'\t'))
            .unwrap_or(value.len());
        let value = &value[start..];
        if field.name.eq_ignore_ascii_case("content-length") {
            length_fields += 1;
        }
        if field.name.eq_ignore_ascii_case("content-length") && !length_seen {
            // Keep the original header intact. The handler decides whether it
            // is valid, after authentication, without reading a rejected body.
            let parsed_length = std::str::from_utf8(value)
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok())
                .filter(|n| *n <= 16 * 1024 * 1024);
            length_valid = parsed_length.is_some();
            length = parsed_length.unwrap_or(0);
            length_seen = true;
        }
        if field.name.eq_ignore_ascii_case("transfer-encoding") {
            chunked = true;
        }
        request = request.header(field.name, value);
    }
    let peer = Peer {
        reader: reader.clone(),
        done: Arc::new(AtomicBool::new(false)),
        notify: Arc::new(Notify::new()),
        reading: Arc::new(Notify::new()),
        monitor,
        reusable: !chunked && length_fields <= 1 && length_valid,
    };
    let body = if length == 0 || chunked {
        peer.body_done();
        Body::empty()
    } else {
        let stream = futures_util::stream::unfold(
            (reader, length, peer.clone()),
            |(reader, remaining, peer)| async move {
                if remaining == 0 {
                    return None;
                }
                peer.reading.notify_one();
                let mut bytes = vec![0_u8; remaining.min(8192) as usize];
                let result = {
                    let mut locked = reader.lock().await;
                    locked.read(&mut bytes).await
                };
                let count = match result {
                    Ok(0) => {
                        peer.body_done();
                        return None;
                    }
                    Ok(count) => count,
                    Err(error) => {
                        peer.body_done();
                        return Some((Err(error), (reader, 0, peer)));
                    }
                };
                bytes.truncate(count);
                if count as u64 == remaining {
                    peer.body_done();
                }
                Some((Ok(bytes), (reader, remaining - count as u64, peer)))
            },
        );
        Body::from_stream(stream)
    };
    request
        .body(body)
        .map(|request| (request, peer))
        .map_err(|_| invalid())
}

pub async fn serve<S: AsyncRead + AsyncWrite + Send + Unpin + 'static>(stream: S, app: Router) {
    serve_with_timeout(stream, app, None, true).await;
}
pub async fn serve_with_timeout<S: AsyncRead + AsyncWrite + Send + Unpin + 'static>(
    stream: S,
    app: Router,
    timeout: Option<Duration>,
    monitor: bool,
) {
    serve_inner(stream, app, timeout, monitor).await;
}
async fn serve_inner<S: AsyncRead + AsyncWrite + Send + Unpin + 'static>(
    stream: S,
    app: Router,
    timeout: Option<Duration>,
    monitor: bool,
) {
    let (read, write) = tokio::io::split(stream);
    let read: Box<dyn AsyncRead + Send + Unpin> = Box::new(read);
    let mut write = Timed::new(write, timeout);
    let reader = Arc::new(Mutex::new(BufReader::new(Timed::new(read, timeout))));
    loop {
        let raw = match head(&reader).await {
            Ok(Some(raw)) => raw,
            _ => break,
        };
        let (request, peer) = match parse(&raw, reader.clone(), monitor) {
            Ok(request) => request,
            Err(_) => {
                let _=write.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                break;
            }
        };
        let close = request
            .headers()
            .get(header::CONNECTION)
            .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"close"))
            || (request.version() == Version::HTTP_10
                && !request
                    .headers()
                    .get(header::CONNECTION)
                    .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"keep-alive")));
        let http10 = request.version() == Version::HTTP_10;
        let head_request = request.method() == axum::http::Method::HEAD;
        let mut expect_continue = !http10
            && request
                .headers()
                .get(header::EXPECT)
                .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"100-continue"));
        let handling = app.clone().oneshot(request);
        tokio::pin!(handling);
        let handled = loop {
            tokio::select! {
                response = &mut handling => break response,
                _ = peer.reading.notified(), if expect_continue => {
                    // Admission may reject without reading. Send permission only
                    // once the handler actually asks for the request body.
                    expect_continue = false;
                    if write.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").await.is_err()
                        || write.flush().await.is_err() {
                        return;
                    }
                }
                _ = peer.disconnected() => return,
            }
        };
        let response = match handled {
            Ok(response) => response,
            Err(error) => match error {},
        };
        if response.extensions().get::<NoReply>().is_some() {
            break;
        }
        // Python closes a shared socket whose body read timed out, without
        // emitting the JSON parse error built from that incomplete body.
        if reader.lock().await.get_mut().read_timed_out {
            break;
        }
        let (mut parts, body) = response.into_parts();
        let (has_transfer, mut chunked) = {
            let mut codings = Vec::new();
            for value in parts.headers.get_all(header::TRANSFER_ENCODING) {
                let Ok(value) = value.to_str() else { return };
                codings.extend(value.split(',').map(str::trim));
            }
            let chunked = codings
                .last()
                .is_some_and(|c| c.eq_ignore_ascii_case("chunked"));
            // Invalid handler framing must not be copied to a reusable wire.
            if codings.iter().enumerate().any(|(i, c)| {
                c.is_empty() || (c.eq_ignore_ascii_case("chunked") && i + 1 != codings.len())
            }) || (http10 && codings.iter().any(|c| !c.eq_ignore_ascii_case("chunked")))
            {
                return;
            }
            (!codings.is_empty(), chunked)
        };
        if has_transfer {
            // Transfer-Encoding defines framing; emitting both is ambiguous.
            parts.headers.remove(header::CONTENT_LENGTH);
        }
        if http10 && chunked {
            // HTTP/1.0 has no chunked coding. Send the stream until close.
            parts.headers.remove(header::TRANSFER_ENCODING);
            chunked = false;
        }
        let mut lengths = parts.headers.get_all(header::CONTENT_LENGTH).iter();
        let length = lengths.next();
        if lengths.next().is_some() {
            return;
        }
        let fixed_length = if let Some(value) = length {
            let Ok(value) = value.to_str() else { return };
            let value = value.trim_matches([' ', '\t']);
            if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                return;
            }
            let Ok(length) = value.parse::<u64>() else {
                return;
            };
            Some(length)
        } else {
            None
        };
        if let Some(length) = fixed_length {
            parts
                .headers
                .insert(header::CONTENT_LENGTH, HeaderValue::from(length));
        }
        let bodyless = head_request
            || matches!(parts.status.as_u16(), 204 | 304)
            || parts.status.is_informational();
        let close = close
            || !peer.reusable
            || !peer.done.load(Ordering::SeqCst)
            || parts.extensions.get::<CloseConnection>().is_some()
            || (!parts.headers.contains_key(header::CONTENT_LENGTH)
                && !chunked
                && !matches!(parts.status.as_u16(), 204 | 304))
            || parts
                .headers
                .get(header::CONNECTION)
                .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"close"));
        if close {
            parts
                .headers
                .insert(header::CONNECTION, HeaderValue::from_static("close"));
        }
        parts.headers.insert(
            header::DATE,
            HeaderValue::from_str(&httpdate::fmt_http_date(std::time::SystemTime::now()))
                .expect("HTTP date"),
        );
        let mut output = format!(
            "HTTP/1.{} {} {}\r\n",
            1,
            parts.status.as_u16(),
            parts.status.canonical_reason().unwrap_or("")
        );
        for (name, value) in &parts.headers {
            let Ok(value) = value.to_str() else {
                return;
            };
            output.push_str(name.as_str());
            output.push_str(": ");
            output.push_str(value);
            output.push_str("\r\n");
        }
        output.push_str("\r\n");
        if write.write_all(output.as_bytes()).await.is_err() || write.flush().await.is_err() {
            break;
        }
        let mut body = body.into_data_stream();
        let mut written = 0_u64;
        loop {
            if bodyless {
                break;
            }
            // A client may close immediately after reading Content-Length
            // bytes. The response's finalizer still owns completed work.
            let chunk = if fixed_length.is_some_and(|length| written >= length) {
                body.next().await
            } else {
                tokio::select! {chunk=body.next()=>chunk,_=peer.disconnected()=>return}
            };
            let Some(chunk) = chunk else {
                break;
            };
            let Ok(chunk) = chunk else {
                return;
            };
            if chunk.is_empty() {
                continue;
            }
            if fixed_length
                .is_some_and(|length| written.saturating_add(chunk.len() as u64) > length)
            {
                return;
            }
            if chunked
                && write
                    .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
                    .await
                    .is_err()
            {
                return;
            }
            if write.write_all(&chunk).await.is_err() {
                return;
            }
            written = written.saturating_add(chunk.len() as u64);
            if chunked && write.write_all(b"\r\n").await.is_err() {
                return;
            }
            if write.flush().await.is_err() {
                return;
            }
        }
        if !bodyless && fixed_length.is_some_and(|length| written != length) {
            return;
        }
        if !bodyless && chunked && write.write_all(b"0\r\n\r\n").await.is_err() {
            return;
        }
        if write.flush().await.is_err() || close {
            break;
        }
    }
    let _ = write.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpStream;
    #[tokio::test]
    async fn delivered_response_finalizes_after_client_closes() {
        assert!(close_after(b"ok").await);
    }
    #[tokio::test]
    async fn unfinished_response_is_cancelled_after_client_closes() {
        assert!(!close_after(b"o").await);
    }
    async fn close_after(first: &'static [u8]) -> bool {
        let (completed, completion) = tokio::sync::oneshot::channel();
        let sender = Arc::new(std::sync::Mutex::new(Some(completed)));
        let app = Router::new().fallback(move || {
            let completed = sender.lock().expect("test sender").take();
            async move {
                let stream = futures_util::stream::unfold(
                    (0_u8, completed),
                    move |(stage, completed)| async move {
                        if stage > 0 {
                            tokio::time::sleep(Duration::from_millis(20)).await;
                            if stage == 1 && first.len() < 2 {
                                return Some((Ok::<_, io::Error>(b"k".to_vec()), (2, completed)));
                            }
                            if let Some(completed) = completed {
                                let _ = completed.send(());
                            }
                            None
                        } else {
                            Some((Ok::<_, io::Error>(first.to_vec()), (1, completed)))
                        }
                    },
                );
                axum::http::Response::builder()
                    .header("content-length", "2")
                    .body(Body::from_stream(stream))
                    .expect("test response")
            }
        });
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("test listener");
        let address = listener.local_addr().expect("test address");
        let serving = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("test accept");
            serve(stream, app).await;
        });
        let mut client = TcpStream::connect(address).await.expect("test connect");
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .expect("test request");
        let mut client = BufReader::new(client);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let mut line = String::new();
                client.read_line(&mut line).await.expect("response head");
                if line == "\r\n" {
                    break;
                }
            }
            let mut body = vec![0_u8; first.len()];
            client.read_exact(&mut body).await.expect("response body");
            assert_eq!(&body, first);
        })
        .await
        .expect("response deadline");
        drop(client);
        let finalized = tokio::time::timeout(Duration::from_secs(2), completion)
            .await
            .expect("finalizer deadline")
            .is_ok();
        serving.await.expect("test server");
        finalized
    }
}
