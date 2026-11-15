//! A1 acceptance tests: HTTP/1.1 client and SSE stream reader.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.
//!
//! The network tests talk to a loopback server on an ephemeral port that this file starts; nothing
//! outside 127.0.0.1 is contacted.

use oxa_engine::{http_request, post_sse, Flow, HttpRequest, SseEnd, SseEvent, SseParser, StreamOpts, Url};
use oxa_proto::{codes, CancelToken, OxaError};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

const SSE_HEAD: &str = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
const PATH: &str = "/v1/chat/completions";

/// Start a loopback server that runs `f` on the first accepted connection.
fn serve<F>(f: F) -> u16
where
    F: FnOnce(TcpStream) + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            f(stream);
        }
    });
    port
}

/// Read one request (head plus its `Content-Length` body) from the client.
fn read_request(s: &mut TcpStream) -> Vec<u8> {
    let mut acc = Vec::new();
    let mut b = [0u8; 1];
    while !acc.ends_with(b"\r\n\r\n") {
        if s.read(&mut b).unwrap() == 0 {
            break;
        }
        acc.push(b[0]);
    }
    let head = String::from_utf8_lossy(&acc).to_ascii_lowercase();
    let len = head
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; len];
    s.read_exact(&mut body).unwrap();
    acc.extend_from_slice(&body);
    acc
}

/// One HTTP chunk on the wire.
fn chunk(data: &[u8]) -> Vec<u8> {
    let mut out = format!("{:x}\r\n", data.len()).into_bytes();
    out.extend_from_slice(data);
    out.extend_from_slice(b"\r\n");
    out
}

fn url_for(port: u16) -> Url {
    Url::parse(&format!("http://127.0.0.1:{port}")).unwrap()
}

fn opts(silence_ms: u64) -> StreamOpts {
    StreamOpts {
        connect_timeout_ms: 2_000,
        silence_limit_ms: silence_ms,
    }
}

/// POST to the test server and collect every event the callback saw.
fn run_sse(port: u16, o: StreamOpts, cancel: &CancelToken) -> (Result<SseEnd, OxaError>, Vec<SseEvent>) {
    let mut events = Vec::new();
    let res = post_sse(
        &url_for(port),
        PATH,
        &[],
        b"{}",
        &o,
        cancel,
        &mut |e: SseEvent| {
            events.push(e);
            Flow::Continue
        },
    );
    (res, events)
}

fn s(text: &str) -> String {
    text.to_string()
}

/// a fixed SSE byte stream fed split at every offset yields the same events
#[test]
fn sse_split_at_every_byte_offset() {
    // A comment, a multi-byte UTF-8 payload with CRLF, multi-line data, a comment after a CRLF
    // and the end marker.
    let stream: &[u8] =
        b": prefill 10/200\n\ndata: {\"x\":\"\xc3\xa9\"}\r\n\r\ndata: a\ndata: b\n\n: keep\r\ndata: [DONE]\n\n";
    let want = vec![
        SseEvent::Comment(s("prefill 10/200")),
        SseEvent::Data(s("{\"x\":\"\u{e9}\"}")),
        SseEvent::Data(s("a\nb")),
        SseEvent::Comment(s("keep")),
        SseEvent::Done,
    ];
    let mut whole = SseParser::new();
    assert_eq!(whole.feed(stream), want);
    for k in 0..=stream.len() {
        let mut p = SseParser::new();
        let mut got = p.feed(&stream[..k]);
        got.extend(p.feed(&stream[k..]));
        assert_eq!(got, want, "split at byte {k}");
    }
    let mut bytewise = SseParser::new();
    let mut got = Vec::new();
    for b in stream {
        got.extend(bytewise.feed(std::slice::from_ref(b)));
    }
    assert_eq!(got, want, "one byte at a time");
}

/// `: prefill 10/200` arrives as SseEvent::Comment
#[test]
fn prefill_comments_are_delivered() {
    let port = serve(|mut s| {
        read_request(&mut s);
        let mut out = SSE_HEAD.as_bytes().to_vec();
        out.extend(chunk(b": prefill 10/200\n\n"));
        out.extend(chunk(b"data: [DONE]\n\n"));
        out.extend_from_slice(b"0\r\n\r\n");
        let _ = s.write_all(&out);
    });
    let cancel = CancelToken::new();
    let (res, events) = run_sse(port, opts(5_000), &cancel);
    let end = res.unwrap();
    assert_eq!(end.status, 200);
    assert!(end.done);
    assert!(
        events.contains(&SseEvent::Comment(s("prefill 10/200"))),
        "events: {events:?}"
    );
}

/// server closes right after [DONE] -> Ok, done = true
#[test]
fn early_close_after_done_is_clean() {
    // Read-until-close framing: the server writes the events and drops the socket at once.
    let plain = serve(|mut s| {
        read_request(&mut s);
        let _ = s.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n\
              data: {\"a\":1}\n\ndata: [DONE]\n\n",
        );
    });
    let cancel = CancelToken::new();
    let (res, events) = run_sse(plain, opts(5_000), &cancel);
    let end = res.unwrap();
    assert!(end.done);
    assert!(end.got_data);
    assert_eq!(events.last(), Some(&SseEvent::Done));

    // Chunked framing with the terminal chunk missing: still clean once [DONE] arrived.
    let chunked = serve(|mut s| {
        read_request(&mut s);
        let mut out = SSE_HEAD.as_bytes().to_vec();
        out.extend(chunk(b"data: [DONE]\n\n"));
        let _ = s.write_all(&out);
    });
    let (res, _) = run_sse(chunked, opts(5_000), &CancelToken::new());
    assert!(res.unwrap().done);
}

/// comments every second past the silence limit -> no abort; total silence -> stream_broken
#[test]
fn comments_count_as_liveness() {
    // Silence limit 300 ms, comments every 150 ms for 1.8 s: six times the limit, no abort.
    let alive = serve(|mut s| {
        read_request(&mut s);
        let _ = s.write_all(SSE_HEAD.as_bytes());
        for _ in 0..12 {
            let _ = s.write_all(&chunk(b": queued\n\n"));
            thread::sleep(Duration::from_millis(150));
        }
        let _ = s.write_all(&chunk(b"data: [DONE]\n\n"));
        let _ = s.write_all(b"0\r\n\r\n");
    });
    let (res, events) = run_sse(alive, opts(300), &CancelToken::new());
    assert!(res.unwrap().done, "comments kept the stream alive");
    assert!(events
        .iter()
        .any(|e| matches!(e, SseEvent::Comment(c) if c == "queued")));

    // Total silence after the headers: the same limit gives stream_broken.
    let silent = serve(|mut s| {
        read_request(&mut s);
        let _ = s.write_all(SSE_HEAD.as_bytes());
        let _ = s.write_all(&chunk(b": queued\n\n"));
        thread::sleep(Duration::from_millis(2_500));
    });
    let started = Instant::now();
    let (res, _) = run_sse(silent, opts(300), &CancelToken::new());
    let err = res.unwrap_err();
    assert_eq!(err.code, codes::STREAM_BROKEN);
    assert!(
        started.elapsed() < Duration::from_millis(2_000),
        "silence detected in {:?}",
        started.elapsed()
    );
}

/// a stream kept alive by comments for longer than the silence limit completes
#[test]
fn no_wall_clock_limit() {
    // 2.5 s of comments against a 300 ms silence limit: well past any wall-clock cap that the
    // silence rule would have to imply.
    let port = serve(|mut s| {
        read_request(&mut s);
        let _ = s.write_all(SSE_HEAD.as_bytes());
        for _ in 0..25 {
            let _ = s.write_all(&chunk(b": prefill 1/2\n\n"));
            thread::sleep(Duration::from_millis(100));
        }
        let _ = s.write_all(&chunk(b"data: {\"done\":true}\n\n"));
        let _ = s.write_all(&chunk(b"data: [DONE]\n\n"));
        let _ = s.write_all(b"0\r\n\r\n");
    });
    let started = Instant::now();
    let (res, events) = run_sse(port, opts(300), &CancelToken::new());
    assert!(started.elapsed() > Duration::from_secs(2));
    let end = res.unwrap();
    assert!(end.done && end.got_data);
    assert!(events.contains(&SseEvent::Data(s("{\"done\":true}"))));
}

/// connect to a closed port -> engine_down + hint
#[test]
fn refused_is_engine_down_with_hint() {
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let (res, _) = run_sse(port, opts(5_000), &CancelToken::new());
    let err = res.unwrap_err();
    assert_eq!(err.code, codes::ENGINE_DOWN);
    let hint = err.hint.expect("engine_down carries a hint");
    assert!(hint.contains("OXA_ENGINE_URL"), "hint: {hint}");

    // The non-streaming request gives the same code for a refused connection.
    let url = url_for(port);
    let req = HttpRequest {
        method: "GET",
        url: &url,
        path: "/v1/models",
        headers: &[],
        body: b"",
        timeout_ms: 2_000,
    };
    let err = http_request(&req).unwrap_err();
    assert_eq!(err.code, codes::ENGINE_DOWN);
    assert!(err.hint.is_some());
}

/// cancel during a held stream -> aborted quickly
#[test]
fn cancel_closes_within_200ms() {
    let port = serve(|mut s| {
        read_request(&mut s);
        let mut out = SSE_HEAD.as_bytes().to_vec();
        out.extend(chunk(b"data: {\"first\":true}\n\n"));
        let _ = s.write_all(&out);
        // Hold the stream open; the client cancels long before this sleep ends.
        thread::sleep(Duration::from_millis(3_000));
    });
    let cancel = CancelToken::new();
    let canceller = {
        let c = cancel.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(400));
            let at = Instant::now();
            c.cancel();
            at
        })
    };
    let (res, events) = run_sse(port, opts(10_000), &cancel);
    let returned = Instant::now();
    let cancelled_at = canceller.join().unwrap();
    assert_eq!(res.unwrap_err().code, codes::ABORTED);
    assert!(events.contains(&SseEvent::Data(s("{\"first\":true}"))));
    let lag = returned.saturating_duration_since(cancelled_at);
    assert!(lag < Duration::from_millis(200), "aborted {lag:?} after cancel");
}

/// both body framings decode
#[test]
fn http_request_content_length_and_chunked() {
    // Content-Length framing, and the request bytes the engine receives.
    let (tx, rx) = std::sync::mpsc::channel();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let req = read_request(&mut s);
        let _ = tx.send(req);
        let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nX-Thing: yes\r\n\r\nhello");
    });
    let base = Url::parse(&format!("http://127.0.0.1:{port}/base/")).unwrap();
    assert_eq!(base.base_path, "/base");
    let headers: Vec<(&str, String)> = vec![("X-Test", s("1"))];
    let req = HttpRequest {
        method: "POST",
        url: &base,
        path: "/v1/x",
        headers: &headers[..],
        body: b"{}",
        timeout_ms: 5_000,
    };
    let resp = http_request(&req).unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, b"hello");
    assert!(resp.headers.contains(&(s("x-thing"), s("yes"))));
    let sent = String::from_utf8(rx.recv().unwrap()).unwrap();
    assert!(
        sent.starts_with("POST /base/v1/x HTTP/1.1\r\n"),
        "request: {sent}"
    );
    assert!(sent.contains("X-Test: 1\r\n"));
    assert!(sent.contains("Content-Length: 2\r\n"));
    assert!(sent.ends_with("\r\n\r\n{}"));

    // Chunked framing with a chunk extension and a trailer.
    let port = serve(|mut s| {
        read_request(&mut s);
        let _ = s.write_all(
            b"HTTP/1.1 201 Created\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6;ext=1\r\n world\r\n0\r\nX-T: 1\r\n\r\n",
        );
    });
    let resp = http_request(&HttpRequest {
        method: "GET",
        url: &url_for(port),
        path: "/v1/models",
        headers: &[],
        body: b"",
        timeout_ms: 5_000,
    })
    .unwrap();
    assert_eq!(resp.status, 201);
    assert_eq!(resp.body, b"hello world");
}

/// Url::parse("https://..") -> invalid + hint
#[test]
fn https_url_rejected() {
    let err = Url::parse("https://127.0.0.1:8080").unwrap_err();
    assert_eq!(err.code, codes::INVALID);
    assert!(err.hint.is_some());
}

// ---- extra coverage beyond the acceptance bullets ----

#[test]
fn url_parse_forms() {
    let u = Url::parse("http://localhost:9000/base/").unwrap();
    assert_eq!(
        (u.host.as_str(), u.port, u.base_path.as_str()),
        ("localhost", 9000, "/base")
    );
    let u = Url::parse("http://example.lan").unwrap();
    assert_eq!((u.port, u.base_path.as_str()), (80, ""));
    let u = Url::parse("http://[::1]:8080").unwrap();
    assert_eq!((u.host.as_str(), u.port), ("::1", 8080));
    assert_eq!(Url::parse("ftp://h").unwrap_err().code, codes::INVALID);
    assert_eq!(Url::parse("http://h:0").unwrap_err().code, codes::INVALID);
    assert_eq!(Url::parse("http://h/x?y=1").unwrap_err().code, codes::INVALID);
}

#[test]
fn server_error_status_is_ok_with_body() {
    let port = serve(|mut s| {
        read_request(&mut s);
        let body = r#"{"error":{"type":"exceed_context_size_error"}}"#;
        let _ = s.write_all(
            format!(
                "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        );
    });
    let (res, events) = run_sse(port, opts(5_000), &CancelToken::new());
    let end = res.unwrap();
    assert_eq!(end.status, 400);
    assert!(end.error_body.unwrap().contains("exceed_context_size_error"));
    assert!(events.is_empty());
}

#[test]
fn caller_stop_is_not_an_error() {
    let port = serve(|mut s| {
        read_request(&mut s);
        let mut out = SSE_HEAD.as_bytes().to_vec();
        out.extend(chunk(b"data: 1\n\ndata: 2\n\n"));
        let _ = s.write_all(&out);
        thread::sleep(Duration::from_millis(500));
    });
    let mut seen = Vec::new();
    let res = post_sse(
        &url_for(port),
        PATH,
        &[],
        b"{}",
        &opts(5_000),
        &CancelToken::new(),
        &mut |e: SseEvent| {
            seen.push(e);
            Flow::Stop
        },
    );
    let end = res.unwrap();
    assert_eq!(end.status, 200);
    assert!(!end.done);
    assert_eq!(seen, vec![SseEvent::Data(s("1"))]);
}

/// A server that accepts and never reads, so a large request fills both socket buffers and
/// the client's write blocks.
fn never_reads() -> u16 {
    serve(|s| {
        thread::sleep(Duration::from_secs(20));
        drop(s);
    })
}

/// A body larger than any loopback socket buffer pair.
fn huge_body() -> Vec<u8> {
    vec![b' '; 32 << 20]
}

/// A cancel ends a request whose write is blocked by an engine that stopped reading within
/// one poll, not after the silence limit (the F1 tool-result prefill relies on it).
#[test]
fn cancel_ends_a_write_blocked_by_a_non_reading_engine() {
    let port = never_reads();
    let cancel = CancelToken::new();
    let c = cancel.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(300));
        c.cancel();
    });
    let started = Instant::now();
    let res = post_sse(
        &url_for(port),
        PATH,
        &[],
        &huge_body(),
        &opts(60_000),
        &cancel,
        &mut |_| Flow::Continue,
    );
    let took = started.elapsed();
    assert_eq!(res.unwrap_err().code, codes::ABORTED);
    assert!(
        took < Duration::from_secs(2),
        "the blocked write ended {took:?} after the start"
    );
}

/// No byte accepted for the silence limit while writing is still the liveness error.
#[test]
fn write_blocked_past_the_silence_limit_is_a_timeout() {
    let port = never_reads();
    let started = Instant::now();
    let res = post_sse(
        &url_for(port),
        PATH,
        &[],
        &huge_body(),
        &opts(600),
        &CancelToken::new(),
        &mut |_| Flow::Continue,
    );
    let took = started.elapsed();
    assert_eq!(res.unwrap_err().code, codes::STREAM_BROKEN);
    assert!(
        took >= Duration::from_millis(600) && took < Duration::from_secs(3),
        "{took:?}"
    );
}
