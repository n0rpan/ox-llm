#![cfg(feature = "http-transport")]
use axum::{
    body::{to_bytes, Body},
    extract::Request,
    http::Response,
    Router,
};
use oxc::transport;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn exchange(wire: &[u8], app: Router) -> Vec<u8> {
    let (mut client, server) = tokio::io::duplex(128 * 1024);
    let serving = tokio::spawn(transport::serve_with_timeout(
        server,
        app,
        Some(Duration::from_millis(200)),
        true,
    ));
    client.write_all(wire).await.unwrap();
    let mut reply = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), client.read_to_end(&mut reply))
        .await
        .unwrap()
        .unwrap();
    serving.await.unwrap();
    reply
}
fn reply(body: Body) -> Response<Body> {
    Response::builder()
        .header("content-length", "2")
        .body(body)
        .unwrap()
}

#[tokio::test]
async fn fixed_body_reaches_handler_without_consuming_next_request() {
    let app = Router::new().fallback(|request: Request| async move {
        let expected = if request.uri().path() == "/first" {
            "hello"
        } else {
            ""
        };
        let bytes = to_bytes(request.into_body(), 100).await.unwrap();
        assert_eq!(bytes, expected);
        reply(Body::from("ok"))
    });
    let response=exchange(b"POST /first HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\n\r\nhelloGET /second HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",app).await;
    assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
    assert!(response.ends_with(b"\r\n\r\nok"));
    assert_eq!(
        String::from_utf8(response)
            .unwrap()
            .matches("HTTP/1.1 200 OK\r\n")
            .count(),
        2
    );
}

#[tokio::test]
async fn duplicate_and_malformed_lengths_and_authorization_reach_policy_intact() {
    let app = Router::new().fallback(|request: Request| async move {
        assert_eq!(
            request.headers().get_all("content-length").iter().count(),
            2
        );
        assert_eq!(request.headers()["content-length"], "invalid");
        assert_eq!(request.headers()["authorization"].as_bytes(), b"Bearer ");
        reply(Body::from("ok"))
    });
    let response=exchange(b"POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: invalid\r\nContent-Length: 5\r\nAuthorization: Bearer \r\nConnection: close\r\n\r\n",app).await;
    assert!(response.ends_with(b"\r\n\r\nok"));
}

#[tokio::test]
async fn sse_data_uses_chunk_framing_and_a_terminal_chunk() {
    let app = Router::new().fallback(|| async {
        let chunks = futures_util::stream::iter([
            Ok::<_, std::io::Error>("data: one\n\n"),
            Ok("data: [DONE]\n\n"),
        ]);
        Response::builder()
            .header("transfer-encoding", "chunked")
            .header("content-type", "text/event-stream")
            .body(Body::from_stream(chunks))
            .unwrap()
    });
    let response = exchange(
        b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        app,
    )
    .await;
    assert!(response.ends_with(b"\r\n\r\nb\r\ndata: one\n\n\r\ne\r\ndata: [DONE]\n\n\r\n0\r\n\r\n"));
}

#[tokio::test]
async fn stalled_header_times_out_without_dispatch() {
    let app = Router::new().fallback(|| async { "unexpected dispatch" });
    assert!(exchange(b"GET / HTTP/1.1\r\nHost: ", app).await.is_empty());
}

#[tokio::test]
async fn invalid_request_syntax_closes_with_400() {
    let app = Router::new().fallback(|| async { "unexpected dispatch" });
    let response = exchange(b"invalid request line\r\n\r\n", app).await;
    assert!(response.starts_with(b"HTTP/1.1 400 Bad Request\r\n"));
}

#[tokio::test]
async fn head_response_has_no_body_even_when_handler_supplies_one() {
    let app = Router::new().fallback(|| async { reply(Body::from("ok")) });
    let response = exchange(
        b"HEAD / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        app,
    )
    .await;
    assert!(response.ends_with(b"\r\n\r\n"));
    assert!(String::from_utf8(response)
        .unwrap()
        .contains("content-length: 2\r\n"));
}

#[tokio::test]
async fn response_body_error_never_emits_a_successful_terminal_chunk() {
    let app = Router::new().fallback(|| async {
        let chunks = futures_util::stream::iter([
            Ok::<_, std::io::Error>("data: partial\n\n"),
            Err(std::io::ErrorKind::UnexpectedEof.into()),
        ]);
        Response::builder()
            .header("transfer-encoding", "chunked")
            .body(Body::from_stream(chunks))
            .unwrap()
    });
    let response = exchange(
        b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        app,
    )
    .await;
    assert!(!response.ends_with(b"0\r\n\r\n"));
    assert!(response.ends_with(b"data: partial\n\n\r\n"));
}
