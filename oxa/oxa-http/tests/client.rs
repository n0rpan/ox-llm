//! The client facade against raw `TcpListener` fixtures.
#![cfg(feature = "client")]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use oxa_http::client::{reqwest_builder, Client, Error, Flow, Phase, Profile, Request};
use oxa_proto::CancelToken;

fn serve(handler: impl FnOnce(TcpStream) + Send + 'static) -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    thread::spawn(move || {
        if let Ok((s, _)) = l.accept() {
            handler(s);
        }
    });
    port
}

fn read_head(s: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut b = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        if s.read(&mut b).unwrap_or(0) == 0 {
            break;
        }
        buf.push(b[0]);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn client() -> Client {
    Client::new(reqwest_builder(Profile::Loopback)).unwrap()
}

fn get<'a>(url: &'a str) -> Request<'a> {
    Request {
        method: "GET",
        url,
        headers: &[],
        body: b"",
        connect_timeout: Duration::from_secs(2),
        silence: Duration::from_secs(5),
        total: None,
        max_body: 1 << 20,
    }
}

#[test]
fn send_returns_status_lowercased_headers_and_content_length_body() {
    let port = serve(|mut s| {
        read_head(&mut s);
        s.write_all(b"HTTP/1.1 200 OK\r\nX-Thing: Yes\r\nContent-Length: 5\r\n\r\nhello")
            .unwrap();
    });
    let url = format!("http://127.0.0.1:{port}/x");
    let r = client().send(&get(&url), &CancelToken::new()).unwrap();
    assert_eq!(r.status, 200);
    assert!(r.headers.contains(&("x-thing".to_string(), "Yes".to_string())));
    assert_eq!(r.body, b"hello");
}

#[test]
fn send_decodes_chunked_answers() {
    let port = serve(|mut s| {
        read_head(&mut s);
        s.write_all(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n",
        )
        .unwrap();
    });
    let url = format!("http://127.0.0.1:{port}/");
    let r = client().send(&get(&url), &CancelToken::new()).unwrap();
    assert_eq!(r.body, b"hello world");
}

#[test]
fn send_posts_headers_and_body() {
    let port = serve(|mut s| {
        let head = read_head(&mut s).to_ascii_lowercase();
        assert!(head.starts_with("post /p "), "{head}");
        assert!(head.contains("x-key: secret"), "{head}");
        let mut body = [0u8; 3];
        s.read_exact(&mut body).unwrap();
        s.write_all(
            format!(
                "HTTP/1.1 201 Created\r\nContent-Length: 3\r\n\r\n{}",
                String::from_utf8_lossy(&body)
            )
            .as_bytes(),
        )
        .unwrap();
    });
    let url = format!("http://127.0.0.1:{port}/p");
    let headers = [("x-key", "secret".to_string())];
    let req = Request {
        method: "POST",
        headers: &headers,
        body: b"abc",
        ..get(&url)
    };
    let r = client().send(&req, &CancelToken::new()).unwrap();
    assert_eq!((r.status, r.body.as_slice()), (201, b"abc".as_slice()));
}

#[test]
fn body_over_max_is_too_large() {
    let port = serve(|mut s| {
        read_head(&mut s);
        s.write_all(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n5\r\nhello\r\n0\r\n\r\n",
        )
        .unwrap();
    });
    let url = format!("http://127.0.0.1:{port}/");
    let req = Request {
        max_body: 6,
        ..get(&url)
    };
    assert_eq!(
        client().send(&req, &CancelToken::new()).unwrap_err(),
        Error::TooLarge
    );
}

#[test]
fn declared_length_over_max_is_too_large() {
    let port = serve(|mut s| {
        read_head(&mut s);
        let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nabc");
        thread::sleep(Duration::from_millis(500));
    });
    let url = format!("http://127.0.0.1:{port}/");
    let req = Request {
        max_body: 10,
        ..get(&url)
    };
    assert_eq!(
        client().send(&req, &CancelToken::new()).unwrap_err(),
        Error::TooLarge
    );
}

#[test]
fn bodyless_answers_ignore_representation_content_length() {
    for (method, status) in [("HEAD", "200 OK"), ("GET", "304 Not Modified")] {
        let port = serve(move |mut s| {
            let head = read_head(&mut s);
            assert!(head.starts_with(method), "{head}");
            s.write_all(
                format!("HTTP/1.1 {status}\r\nContent-Length: 1000000\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .unwrap();
        });
        let url = format!("http://127.0.0.1:{port}/");
        let req = Request {
            method,
            max_body: 0,
            ..get(&url)
        };
        let response = client().send(&req, &CancelToken::new()).unwrap();
        assert_eq!(response.body, b"");
    }
}

#[test]
fn refused_connection_is_connect_error_without_url() {
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let url = format!("http://127.0.0.1:{port}/secret?token=abc");
    let err = client().send(&get(&url), &CancelToken::new()).unwrap_err();
    match &err {
        Error::Connect(kind, msg) => {
            assert_eq!(*kind, std::io::ErrorKind::ConnectionRefused);
            assert!(!msg.contains("token") && !msg.contains("127.0.0.1"), "{msg}");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn silent_server_is_head_timeout() {
    let port = serve(|s| {
        thread::sleep(Duration::from_secs(3));
        drop(s);
    });
    let url = format!("http://127.0.0.1:{port}/");
    let req = Request {
        connect_timeout: Duration::from_millis(200),
        silence: Duration::from_millis(300),
        ..get(&url)
    };
    assert_eq!(
        client().send(&req, &CancelToken::new()).unwrap_err(),
        Error::Timeout(Phase::Head)
    );
}

fn stalled_body_port() -> u16 {
    serve(|mut s| {
        read_head(&mut s);
        s.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\na\r\n")
            .unwrap();
        thread::sleep(Duration::from_secs(3));
    })
}

#[test]
fn stalled_body_is_body_timeout() {
    let url = format!("http://127.0.0.1:{}/", stalled_body_port());
    let req = Request {
        silence: Duration::from_millis(300),
        ..get(&url)
    };
    assert_eq!(
        client().send(&req, &CancelToken::new()).unwrap_err(),
        Error::Timeout(Phase::Body)
    );
}

#[test]
fn total_bounds_the_whole_call() {
    let url = format!("http://127.0.0.1:{}/", stalled_body_port());
    let req = Request {
        total: Some(Duration::from_millis(300)),
        ..get(&url)
    };
    let t = Instant::now();
    assert_eq!(
        client().send(&req, &CancelToken::new()).unwrap_err(),
        Error::Timeout(Phase::Body)
    );
    assert!(t.elapsed() < Duration::from_secs(2));
}

#[test]
fn cancel_during_stalled_stream_returns_within_a_second() {
    let url = format!("http://127.0.0.1:{}/", stalled_body_port());
    let cancel = CancelToken::new();
    let c2 = cancel.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(300));
        c2.cancel();
    });
    let t = Instant::now();
    let r = client().stream(&get(&url), &cancel, &mut |_, _| Flow::Continue, &mut |_| {
        Flow::Continue
    });
    assert_eq!(r, Err(Error::Cancelled));
    assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
}

#[test]
fn redirect_is_returned_not_followed() {
    let port = serve(|mut s| {
        read_head(&mut s);
        s.write_all(
            b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/elsewhere\r\nContent-Length: 0\r\n\r\n",
        )
        .unwrap();
    });
    let url = format!("http://127.0.0.1:{port}/");
    let r = client().send(&get(&url), &CancelToken::new()).unwrap();
    assert_eq!(r.status, 302);
    assert!(r
        .headers
        .contains(&("location".to_string(), "http://127.0.0.1:1/elsewhere".to_string())));
}

/// The parent starts this test binary again with `HTTP_PROXY` set; the child makes the call.
#[test]
fn loopback_ignores_proxy_environment() {
    if let Ok(url) = std::env::var("OXA_HTTP_TEST_URL") {
        let r = client().send(&get(&url), &CancelToken::new()).unwrap();
        assert_eq!(r.status, 200);
        return;
    }
    let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
    proxy.set_nonblocking(true).unwrap();
    let proxy_port = proxy.local_addr().unwrap().port();
    let (tx, rx) = std::sync::mpsc::channel();
    let port = serve(move |mut s| {
        let head = read_head(&mut s);
        let _ = tx.send(head);
        s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
            .unwrap();
    });
    let status = Command::new(std::env::current_exe().unwrap())
        .args([
            "loopback_ignores_proxy_environment",
            "--exact",
            "--test-threads=1",
        ])
        .env("HTTP_PROXY", format!("http://127.0.0.1:{proxy_port}"))
        .env("http_proxy", format!("http://127.0.0.1:{proxy_port}"))
        .env("OXA_HTTP_TEST_URL", format!("http://127.0.0.1:{port}/direct"))
        .status()
        .unwrap();
    assert!(status.success());
    let head = rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(head.starts_with("GET /direct "), "{head}");
    assert!(proxy.accept().is_err(), "the proxy was contacted");
}

#[test]
fn stream_delivers_chunks_as_they_arrive_and_stops_on_request() {
    let port = serve(|mut s| {
        read_head(&mut s);
        s.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\na\r\n")
            .unwrap();
        thread::sleep(Duration::from_millis(200));
        s.write_all(b"1\r\nb\r\n").unwrap();
        thread::sleep(Duration::from_millis(200));
        s.write_all(b"1\r\nc\r\n0\r\n\r\n").unwrap();
    });
    let url = format!("http://127.0.0.1:{port}/");
    let mut got: Vec<(Instant, Vec<u8>)> = Vec::new();
    let mut status = 0;
    let r = client().stream(
        &get(&url),
        &CancelToken::new(),
        &mut |s, _| {
            status = s;
            Flow::Continue
        },
        &mut |b| {
            got.push((Instant::now(), b.to_vec()));
            if got.len() == 2 {
                Flow::Stop
            } else {
                Flow::Continue
            }
        },
    );
    assert_eq!(r, Ok(()));
    assert_eq!(status, 200);
    assert_eq!(got.len(), 2, "stopped after the second chunk");
    assert_eq!(
        (got[0].1.as_slice(), got[1].1.as_slice()),
        (b"a".as_slice(), b"b".as_slice())
    );
    assert!(got[1].0 - got[0].0 >= Duration::from_millis(150));
}

#[test]
fn send_from_inside_a_runtime_thread_does_not_panic() {
    let port = serve(|mut s| {
        read_head(&mut s);
        s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
            .unwrap();
    });
    let url = format!("http://127.0.0.1:{port}/");
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let r = rt
        .block_on(async { client().send(&get(&url), &CancelToken::new()) })
        .unwrap();
    assert_eq!(r.body, b"ok");
}
