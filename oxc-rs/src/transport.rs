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
use std::{io, sync::Arc};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{tcp::OwnedReadHalf, TcpStream},
    sync::Mutex,
};
use tower::ServiceExt;

type Reader = Arc<Mutex<BufReader<OwnedReadHalf>>>;

#[derive(Clone)]
pub struct CloseConnection;

async fn head(reader: &Reader) -> io::Result<Option<Vec<u8>>> {
    let mut head = Vec::new();
    let mut reader = reader.lock().await;
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

fn parse(raw: &[u8], reader: Reader) -> io::Result<Request<Body>> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid HTTP request");
    let mut headers = [httparse::EMPTY_HEADER; 100];
    let mut parsed = httparse::Request::new(&mut headers);
    if !parsed.parse(raw).map_err(|_| invalid())?.is_complete() {
        return Err(invalid());
    }
    let mut request = Request::builder()
        .method(parsed.method.ok_or_else(invalid)?)
        .uri(parsed.path.ok_or_else(invalid)?)
        .version(if parsed.version == Some(0) {
            Version::HTTP_10
        } else {
            Version::HTTP_11
        });
    let mut length = 0;
    let mut length_seen = false;
    let mut chunked = false;
    for field in parsed.headers {
        if field.name.eq_ignore_ascii_case("content-length") && !length_seen {
            // Keep the original header intact. The handler decides whether it
            // is valid, after authentication, without reading a rejected body.
            length = std::str::from_utf8(field.value)
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok())
                .filter(|n| *n <= 16 * 1024 * 1024)
                .unwrap_or(0);
            length_seen = true;
        }
        if field.name.eq_ignore_ascii_case("transfer-encoding") {
            chunked = true;
        }
        request = request.header(field.name, field.value);
    }
    let body = if length == 0 || chunked {
        Body::empty()
    } else {
        let stream =
            futures_util::stream::unfold((reader, length), |(reader, remaining)| async move {
                if remaining == 0 {
                    return None;
                }
                let mut bytes = vec![0_u8; remaining.min(8192) as usize];
                let result = {
                    let mut locked = reader.lock().await;
                    locked.read(&mut bytes).await
                };
                let count = match result {
                    Ok(0) => return None,
                    Ok(count) => count,
                    Err(error) => return Some((Err(error), (reader, 0))),
                };
                bytes.truncate(count);
                Some((Ok(bytes), (reader, remaining - count as u64)))
            });
        Body::from_stream(stream)
    };
    request.body(body).map_err(|_| invalid())
}

pub async fn serve(stream: TcpStream, app: Router) {
    let (read, mut write) = stream.into_split();
    let reader = Arc::new(Mutex::new(BufReader::new(read)));
    loop {
        let raw = match head(&reader).await {
            Ok(Some(raw)) => raw,
            _ => break,
        };
        let request = match parse(&raw, reader.clone()) {
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
        let response = match app.clone().oneshot(request).await {
            Ok(response) => response,
            Err(error) => match error {},
        };
        let (mut parts, body) = response.into_parts();
        let close = close
            || parts.extensions.get::<CloseConnection>().is_some()
            || (!parts.headers.contains_key(header::CONTENT_LENGTH)
                && !matches!(parts.status.as_u16(), 204 | 304))
            || parts
                .headers
                .get(header::CONNECTION)
                .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"close"));
        parts.headers.insert(
            header::DATE,
            HeaderValue::from_str(&httpdate::fmt_http_date(std::time::SystemTime::now()))
                .expect("HTTP date"),
        );
        let mut output = format!(
            "HTTP/1.1 {} {}\r\n",
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
        if write.write_all(output.as_bytes()).await.is_err() {
            break;
        }
        let mut body = body.into_data_stream();
        while let Some(chunk) = body.next().await {
            let Ok(chunk) = chunk else {
                return;
            };
            if write.write_all(&chunk).await.is_err() {
                return;
            }
        }
        if write.flush().await.is_err() || close {
            break;
        }
    }
    let _ = write.shutdown().await;
}
