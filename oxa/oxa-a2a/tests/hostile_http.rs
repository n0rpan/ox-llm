use oxa_a2a::loopback::{Capability, Client, Limits};
use oxa_a2a::wire::*;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;
use std::time::{Duration, Instant};

fn params() -> SendParams {
    SendParams {
        message: MessageObject::Message(Message {
            message_id: "m".into(),
            role: Role::User,
            parts: vec![Part::Text {
                text: "synthetic".into(),
            }],
            task_id: None,
            context_id: None,
        }),
    }
}

fn with_server(response: Vec<u8>, run: impl FnOnce(&mut Client)) {
    let server = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut client = Client::new(
        server.local_addr().unwrap(),
        Capability::generate().unwrap(),
        Limits::default(),
    )
    .unwrap();
    let handle = thread::spawn(move || {
        let (mut socket, _) = server.accept().unwrap();
        socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut buffer = Vec::new();
        loop {
            let mut byte = [0];
            socket.read_exact(&mut byte).unwrap();
            buffer.push(byte[0]);
            if buffer.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let text = std::str::from_utf8(&buffer).unwrap();
        let length: usize = text
            .lines()
            .find_map(|s| s.strip_prefix("Content-Length: "))
            .unwrap()
            .parse()
            .unwrap();
        socket.read_exact(&mut vec![0; length]).unwrap();
        let _ = socket.write_all(&response);
    });
    run(&mut client);
    handle.join().unwrap();
}

fn response(content_type: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

#[test]
fn redirects_duplicate_lengths_and_truncated_bodies_are_rejected() {
    for bytes in [
        b"HTTP/1.1 302 Found\r\nLocation: http://192.0.2.1/\r\nContent-Length: 0\r\n\r\n".to_vec(),
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nContent-Length: 999\r\n\r\n{}".to_vec(),
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 100\r\n\r\n{}".to_vec(),
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nContent-Length: 2\r\n\r\n{}".to_vec(),
    ] { with_server(bytes, |client| assert!(client.send(params()).is_err())); }
}

#[test]
fn streaming_rejects_cross_task_events_and_eof_without_final_event() {
    let first = "data: {\"jsonrpc\":\"2.0\",\"id\":\"1\",\"result\":{\"kind\":\"task\",\"id\":\"t\",\"contextId\":\"c\",\"status\":{\"state\":\"working\"}}}\n\n";
    let wrong = "data: {\"jsonrpc\":\"2.0\",\"id\":\"1\",\"result\":{\"kind\":\"status-update\",\"taskId\":\"other\",\"contextId\":\"c\",\"status\":{\"state\":\"completed\"},\"final\":true}}\n\n";
    for body in [first.to_string(), format!("{first}{wrong}")] {
        with_server(response("text/event-stream", &body), |client| {
            let mut stream = client.stream(params()).unwrap();
            assert!(matches!(stream.next_event().unwrap(), Some(Event::Task(_))));
            assert!(stream.next_event().is_err());
            assert!(stream.next_event().is_err());
        });
    }
}

#[test]
fn stalled_headers_are_bounded_by_the_absolute_deadline() {
    let server = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut client = Client::new(
        server.local_addr().unwrap(),
        Capability::generate().unwrap(),
        Limits {
            timeout: Duration::from_millis(50),
            ..Limits::default()
        },
    )
    .unwrap();
    let handle = thread::spawn(move || {
        let (mut socket, _) = server.accept().unwrap();
        // Dribble bytes often enough to defeat a per-read-only timeout.
        for _ in 0..20 {
            if socket.write_all(b"H").is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
    });
    let start = Instant::now();
    assert!(client.send(params()).is_err());
    assert!(start.elapsed() < Duration::from_secs(2));
    handle.join().unwrap();
}

#[test]
fn close_delimited_sse_requires_explicit_framing_and_protocol_completion() {
    let message = "data: {\"jsonrpc\":\"2.0\",\"id\":\"1\",\"result\":{\"kind\":\"message\",\"messageId\":\"m\",\"role\":\"agent\",\"parts\":[{\"kind\":\"text\",\"text\":\"synthetic\"}]}}\n\n";
    let framed =
        format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{message}");
    with_server(framed.into_bytes(), |client| {
        let mut stream = client.stream(params()).unwrap();
        assert!(matches!(stream.next_event().unwrap(), Some(Event::Message(_))));
        assert!(stream.next_event().unwrap().is_none());
    });
    for head in [
        "Content-Type: text/event-stream\r\n",
        "Content-Type: text/event-stream\r\nConnection: close\r\nTransfer-Encoding: chunked\r\n",
    ] {
        with_server(
            format!("HTTP/1.1 200 OK\r\n{head}\r\n{message}").into_bytes(),
            |client| assert!(client.stream(params()).is_err()),
        );
    }
    with_server(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: incomplete"
            .to_vec(),
        |client| {
            let mut stream = client.stream(params()).unwrap();
            assert!(stream.next_event().is_err());
        },
    );
}
