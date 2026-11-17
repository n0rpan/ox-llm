#![cfg(feature = "upstream-openai")]

#[path = "common/openai.rs"]
mod openai;

use openai::{MockOpenAi, Reply};
use serde_json::{json, Value};
use std::{
    io::{Read, Write},
    net::TcpStream,
    time::Duration,
};

fn request(mock: &MockOpenAi, body: &Value) -> TcpStream {
    let mut stream = TcpStream::connect(mock.address()).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    stream.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
    let body = body.to_string();
    write!(stream, "POST /v1/responses HTTP/1.1\r\nHost: localhost\r\nX-Synthetic: captured\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
    stream
}

fn response(mut stream: TcpStream) -> (String, Vec<u8>, usize) {
    let mut wire = Vec::new();
    stream.read_to_end(&mut wire).unwrap();
    let end = wire.windows(4).position(|s| s == b"\r\n\r\n").unwrap() + 4;
    let headers = String::from_utf8(wire[..end].to_vec()).unwrap();
    let mut rest = &wire[end..];
    let mut body = Vec::new();
    let mut chunks = 0;
    loop {
        let end = rest.windows(2).position(|s| s == b"\r\n").unwrap();
        let length = usize::from_str_radix(std::str::from_utf8(&rest[..end]).unwrap(), 16).unwrap();
        rest = &rest[end + 2..];
        if length == 0 {
            assert_eq!(rest, b"\r\n");
            break;
        }
        assert!(length <= 7, "mock must fragment the SSE wire");
        body.extend_from_slice(&rest[..length]);
        assert_eq!(&rest[length..length + 2], b"\r\n");
        rest = &rest[length + 2..];
        chunks += 1;
    }
    (headers, body, chunks)
}

fn events(body: &[u8]) -> Vec<Value> {
    std::str::from_utf8(body)
        .unwrap()
        .split("\n\n")
        .filter(|frame| !frame.is_empty())
        .map(|frame| {
            let (event, data) = frame.split_once('\n').unwrap();
            let value: Value = serde_json::from_str(data.strip_prefix("data: ").unwrap()).unwrap();
            assert_eq!(event.strip_prefix("event: ").unwrap(), value["type"]);
            value
        })
        .collect()
}

#[test]
fn openai_mock_scripted_events_capture_and_gate() {
    let expected = vec![json!({"type":"response.created","response":{"id":"resp_synthetic"}})];
    let (reply, release) = Reply::events(expected.clone()).gated();
    let mock = MockOpenAi::start(vec![reply, Reply::events(vec![])]);
    assert!(mock.address().ip().is_loopback());
    assert_eq!(mock.base_url(), format!("http://{}/v1", mock.address()));
    let body = json!({"model":"synthetic-openai","input":[],"stream":true});
    let mut stream = request(&mock, &body);
    let captured = mock.wait_for_requests(1);
    assert_eq!(captured[0].method, "POST");
    assert_eq!(captured[0].target, "/v1/responses");
    assert_eq!(captured[0].body, body);
    assert_eq!(captured[0].header("X-SYNTHETIC"), Some("captured"));
    assert_eq!(captured[0].header("absent"), None);
    let debug = format!("{:?}", captured[0]);
    assert!(!debug.contains("synthetic-openai"));
    assert!(!debug.contains("captured\""));
    stream.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
    let error = stream.read(&mut [0; 1]).unwrap_err();
    assert!(matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    ));
    release.send(()).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let (headers, bytes, chunks) = response(stream);
    assert!(headers.starts_with("HTTP/1.1 200 "));
    assert!(headers.contains("Content-Type: text/event-stream\r\n"));
    assert!(chunks > 1);
    assert_eq!(events(&bytes), expected);
    assert!(response(request(&mock, &json!({"input":"second"}))).1.is_empty());
    assert_eq!(mock.requests().len(), 2);
    mock.assert_healthy();
}

#[test]
fn openai_mock_450_three_argument_shapes() {
    let mock = MockOpenAi::start(vec![
        Reply::multi_tool_full_deltas(),
        Reply::multi_tool_no_deltas(),
        Reply::multi_tool_partial_deltas(),
    ]);
    for shape in ["full", "none", "partial"] {
        let (_, bytes, _) = response(request(&mock, &json!({"stream":true,"store":false})));
        let rows = events(&bytes);
        assert_eq!(rows.first().unwrap()["type"], "response.created");
        let completed = rows.last().unwrap();
        assert_eq!(completed["type"], "response.completed");
        assert_eq!(completed["response"]["output"], json!([]));
        assert_eq!(completed["response"]["status"], "completed");
        for index in 0..3 {
            let for_call: Vec<_> = rows.iter().filter(|row| row["output_index"] == index).collect();
            assert_eq!(for_call.first().unwrap()["type"], "response.output_item.added");
            assert_eq!(for_call.first().unwrap()["item"]["arguments"], "");
            let done = for_call.last().unwrap();
            assert_eq!(done["type"], "response.output_item.done");
            let item = &done["item"];
            assert_eq!(item["type"], "function_call");
            assert_eq!(item["call_id"], format!("call_synthetic_{index}"));
            let arguments = item["arguments"].as_str().unwrap();
            assert!(serde_json::from_str::<Value>(arguments).unwrap().is_object());
            let deltas: String = for_call
                .iter()
                .filter(|row| row["type"] == "response.function_call_arguments.delta")
                .map(|row| row["delta"].as_str().unwrap())
                .collect();
            match shape {
                "full" => assert_eq!(deltas, arguments),
                "none" => assert!(deltas.is_empty()),
                "partial" => {
                    assert!(!deltas.is_empty());
                    assert!(arguments.starts_with(&deltas));
                    assert!(deltas.len() < arguments.len());
                }
                _ => unreachable!(),
            }
            let arguments_done = for_call
                .iter()
                .find(|row| row["type"] == "response.function_call_arguments.done");
            if shape == "none" {
                assert!(
                    arguments_done.is_none(),
                    "arguments arrive only in output_item.done"
                );
            } else {
                assert_eq!(arguments_done.unwrap()["arguments"], arguments);
            }
        }
    }
    assert_eq!(mock.requests().len(), 3);
    mock.assert_healthy();
}

#[test]
fn openai_mock_errors_and_retry_after() {
    let mock = MockOpenAi::start(vec![
        Reply::error(503, "server_error", "synthetic unavailable"),
        Reply::insufficient_quota(17),
    ]);
    let (headers, bytes, _) = response(request(&mock, &json!({})));
    assert!(headers.starts_with("HTTP/1.1 503 "));
    assert!(headers.contains("Content-Type: application/json\r\n"));
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap(),
        json!({"error":{"type":"server_error","code":"server_error","message":"synthetic unavailable"}})
    );
    let (headers, bytes, _) = response(request(&mock, &json!({})));
    assert!(headers.starts_with("HTTP/1.1 429 "));
    assert!(headers.contains("Retry-After: 17\r\n"));
    let error: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(error["error"]["type"], "insufficient_quota");
    assert_eq!(error["error"]["code"], "insufficient_quota");
    mock.assert_healthy();
}

/// Real loopback read failures remain unhealthy and reveal only finite facts.
#[test]
fn mock_read_failure_diagnostics_are_finite_and_health_remains_strict() {
    use std::{
        net::Shutdown,
        panic::{catch_unwind, AssertUnwindSafe},
        time::Instant,
    };
    for (wire, expected) in [
        (
            b"POST /synthetic HTTP/1.1\r\nContent-Length: synthetic-private-header\r\n\r\n".as_slice(),
            "InvalidData",
        ),
        (
            b"POST /synthetic-private-header HTTP/1.1\r\n".as_slice(),
            "UnexpectedEof",
        ),
    ] {
        let mock = MockOpenAi::start(Vec::new());
        let mut peer = TcpStream::connect(mock.address()).unwrap();
        peer.write_all(wire).unwrap();
        peer.shutdown(Shutdown::Write).unwrap();
        let started = Instant::now();
        let panic = loop {
            if let Err(panic) = catch_unwind(AssertUnwindSafe(|| mock.assert_healthy())) {
                break panic;
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "mock failure must be observed"
            );
            std::thread::yield_now();
        };
        let message = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .unwrap();
        assert!(message.contains("mock upstream reported a connection failure"));
        assert!(
            message.contains("phase=Some(RequestRead)"),
            "finite read phase missing"
        );
        assert!(
            message.contains(&format!("kind=Some({expected})")),
            "finite I/O kind missing"
        );
        assert!(
            message.contains("part=None"),
            "non-write failures must not invent a reply part"
        );
        assert!(!message.contains("synthetic-private-header"));
        assert!(
            mock.requests().is_empty(),
            "invalid input never becomes a request"
        );
    }
}

/// Fail a chosen existing write/flush boundary without sockets or timing races.
struct FailingReplyWriter {
    fail_at: usize,
    calls: usize,
    bytes: Vec<u8>,
}
impl FailingReplyWriter {
    fn step(&mut self) -> std::io::Result<()> {
        let at = self.calls;
        self.calls += 1;
        if at == self.fail_at {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "synthetic-private-write",
            ))
        } else {
            Ok(())
        }
    }
}
impl Write for FailingReplyWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.step()?;
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.step()
    }
    fn write_fmt(&mut self, args: std::fmt::Arguments<'_>) -> std::io::Result<()> {
        self.write_all(args.to_string().as_bytes())
    }
}
fn diagnostic_reply() -> Reply {
    Reply::raw(
        200,
        "text/plain",
        b"abc".to_vec(),
        vec![("X-Synthetic", "bounded".into())],
    )
}
#[test]
fn mock_reply_write_diagnostics_identify_each_finite_part_without_error_text() {
    use openai::ReplyPart;
    let cases = [
        (
            false,
            vec![
                ReplyPart::Headers,
                ReplyPart::Headers,
                ReplyPart::HeaderEnd,
                ReplyPart::ChunkHeader,
                ReplyPart::FinalChunk,
                ReplyPart::Flush,
            ],
        ),
        (
            true,
            vec![
                ReplyPart::Headers,
                ReplyPart::Headers,
                ReplyPart::HeaderEnd,
                ReplyPart::ChunkHeader,
                ReplyPart::ChunkPayload,
                ReplyPart::ChunkEnd,
                ReplyPart::Flush,
            ],
        ),
    ];
    for (truncate, expected_parts) in cases {
        for (fail_at, expected) in expected_parts.into_iter().enumerate() {
            let mut writer = FailingReplyWriter {
                fail_at,
                calls: 0,
                bytes: Vec::new(),
            };
            let mut reply = diagnostic_reply();
            reply.truncate_transfer = truncate;
            let failure = openai::write_reply(&mut writer, &reply).unwrap_err();
            assert_eq!(failure.part, Some(expected));
            assert_eq!(failure.kind, Some(std::io::ErrorKind::BrokenPipe));
            assert_eq!(writer.calls, fail_at + 1, "do not continue after failed writes");
            let finite = format!("{failure:?}");
            assert!(finite.contains("ReplyWrite"));
            assert!(!finite.contains("synthetic-private-write"));
            assert!(!finite.contains("X-Synthetic"));
        }
    }
    let mut writer = FailingReplyWriter {
        fail_at: 2,
        calls: 0,
        bytes: Vec::new(),
    };
    let empty = Reply::raw(200, "text/plain", Vec::new(), vec![]);
    let failure = openai::write_reply(&mut writer, &empty).unwrap_err();
    assert_eq!(failure.part, Some(ReplyPart::TransferEnd));
    assert_eq!(failure.kind, Some(std::io::ErrorKind::BrokenPipe));
}
#[test]
fn mock_reply_diagnostics_preserve_wire_flush_and_transfer_truncation() {
    for truncate in [false, true] {
        let mut reply = diagnostic_reply();
        reply.truncate_transfer = truncate;
        let mut writer = FailingReplyWriter {
            fail_at: usize::MAX,
            calls: 0,
            bytes: Vec::new(),
        };
        openai::write_reply(&mut writer, &reply).unwrap();
        let mut expected = b"HTTP/1.1 200 Synthetic\r\nContent-Type: text/plain\r\nTransfer-Encoding: chunked\r\nConnection: close\r\nX-Synthetic: bounded\r\n\r\n3\r\nabc\r\n".to_vec();
        if !truncate {
            expected.extend_from_slice(b"0\r\n\r\n");
        }
        assert_eq!(writer.bytes, expected);
        assert_eq!(writer.calls, if truncate { 7 } else { 6 });
    }
}

/// A peer that closes after a flushed semantic end, before a later HTTP-tail write.
#[derive(Default)]
struct CloseOnDoneFlush {
    bytes: Vec<u8>,
    closed: bool,
}
impl Write for CloseOnDoneFlush {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.closed {
            return Err(std::io::ErrorKind::BrokenPipe.into());
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.closed = self
            .bytes
            .windows(b"data: [DONE]".len())
            .any(|w| w == b"data: [DONE]");
        Ok(())
    }
}
#[test]
fn mock_finishes_http_before_final_done_flush_can_close_the_peer() {
    let reply = Reply::raw(200, "text/event-stream", b"data: [DONE]\r\n\r\n".to_vec(), vec![]);
    let mut peer = CloseOnDoneFlush::default();
    openai::write_reply(&mut peer, &reply).expect("HTTP framing must be complete at final flush");
    assert!(peer.closed);
    assert!(peer.bytes.ends_with(b"\r\n0\r\n\r\n"));
}
#[test]
fn mock_still_rejects_peer_close_before_later_payload_chunks() {
    let mut body = b"data: [DONE]\r\n\r\n".to_vec();
    body.extend(std::iter::repeat_n(b'x', 2048));
    let reply = Reply::raw(200, "text/event-stream", body, vec![]);
    let failure = openai::write_reply(&mut CloseOnDoneFlush::default(), &reply).unwrap_err();
    assert_eq!(failure.kind, Some(std::io::ErrorKind::BrokenPipe));
    assert_eq!(failure.part, Some(openai::ReplyPart::ChunkHeader));
}
