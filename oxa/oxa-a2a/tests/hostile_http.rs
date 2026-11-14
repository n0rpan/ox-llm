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

fn continuation(task: Option<&str>, context: Option<&str>) -> SendParams {
    let mut params = params();
    let MessageObject::Message(message) = &mut params.message;
    message.task_id = task.map(str::to_owned);
    message.context_id = context.map(str::to_owned);
    params
}
fn task_result(task: &str, context: &str) -> serde_json::Value {
    serde_json::json!({"kind":"task","id":task,"contextId":context,"status":{"state":"completed"}})
}
fn message_result(task: Option<&str>, context: Option<&str>) -> serde_json::Value {
    let mut result = serde_json::json!({"kind":"message","messageId":"reply","role":"agent","parts":[{"kind":"text","text":"synthetic reply"}]});
    if let Some(task) = task {
        result["taskId"] = task.into();
    }
    if let Some(context) = context {
        result["contextId"] = context.into();
    }
    result
}
fn envelope(result: serde_json::Value) -> String {
    serde_json::json!({"jsonrpc":"2.0","id":"1","result":result}).to_string()
}
#[test]
fn first_continuation_reply_must_keep_the_requested_task_and_context() {
    for result in [
        task_result("wrong-task", "context-x"),
        task_result("task-a", "wrong-context"),
        message_result(Some("wrong-task"), Some("context-x")),
        message_result(Some("task-a"), Some("wrong-context")),
        message_result(None, Some("context-x")),
        message_result(Some("task-a"), None),
        serde_json::json!({"kind":"status-update","taskId":"wrong-task","contextId":"context-x","status":{"state":"completed"},"final":true}),
        serde_json::json!({"kind":"artifact-update","taskId":"task-a","contextId":"wrong-context","artifact":{"artifactId":"a","parts":[{"kind":"text","text":"synthetic"}]}}),
    ] {
        let body = envelope(result);
        with_server(response("application/json", &body), |client| {
            assert!(client
                .send(continuation(Some("task-a"), Some("context-x")))
                .is_err())
        });
        with_server(
            response("text/event-stream", &format!("data: {body}\n\n")),
            |client| {
                let mut stream = client
                    .stream(continuation(Some("task-a"), Some("context-x")))
                    .unwrap();
                assert!(stream.next_event().is_err());
                assert!(stream.next_event().is_err());
            },
        );
    }
}
#[test]
fn valid_continuations_and_unbound_new_task_replies_remain_supported() {
    for (task, context, result) in [
        (
            Some("task-a"),
            Some("context-x"),
            task_result("task-a", "context-x"),
        ),
        (
            Some("task-a"),
            Some("context-x"),
            message_result(Some("task-a"), Some("context-x")),
        ),
        (Some("task-a"), None, task_result("task-a", "context-new")),
        (Some("task-a"), None, message_result(Some("task-a"), None)),
        (None, Some("context-x"), task_result("new-task", "context-x")),
        (None, Some("context-x"), message_result(None, Some("context-x"))),
        (None, None, task_result("new-task", "new-context")),
        (None, None, message_result(None, None)),
    ] {
        let body = envelope(result);
        with_server(response("application/json", &body), |client| {
            assert!(client.send(continuation(task, context)).is_ok())
        });
        with_server(
            response("text/event-stream", &format!("data: {body}\n\n")),
            |client| {
                let mut stream = client.stream(continuation(task, context)).unwrap();
                assert!(stream.next_event().unwrap().is_some());
                assert!(stream.next_event().unwrap().is_none());
            },
        );
    }
    // Each constraint also applies when it is the only supplied identity field.
    for (task, context, result) in [
        (Some("task-a"), None, task_result("wrong", "any")),
        (None, Some("context-x"), task_result("any", "wrong")),
    ] {
        let body = envelope(result);
        with_server(response("application/json", &body), |client| {
            assert!(client.send(continuation(task, context)).is_err())
        });
        with_server(
            response("text/event-stream", &format!("data: {body}\n\n")),
            |client| {
                assert!(client
                    .stream(continuation(task, context))
                    .unwrap()
                    .next_event()
                    .is_err())
            },
        );
    }
}
