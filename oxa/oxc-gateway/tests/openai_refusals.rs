#![cfg(all(feature = "anthropic-client", feature = "ledger"))]

use oxc_gateway::rate_limits::{classify_openai_refusal, openai_limits, Limit, OpenAiLimits, Refusal};
use reqwest::header::HeaderMap;
use serde_json::json;
use std::{
    io::{Read, Write},
    net::TcpListener,
    thread,
    time::{Duration, Instant},
};

const DEADLINE: Duration = Duration::from_secs(5);
const LIMIT_HEADERS: &[(&str, &str)] = &[
    ("X-RateLimit-Limit-Requests", "100"),
    ("x-RATELIMIT-Remaining-Requests", "0"),
    ("X-RateLimit-Reset-Requests", "1m2s3ms"),
    ("x-ratelimit-LIMIT-tokens", "2000"),
    ("X-Ratelimit-Remaining-Tokens", "1999"),
    ("x-RateLimit-Reset-Tokens", "2ms"),
    ("ReTrY-AfTeR", "7"),
];

fn error_body(code: &str, message: &str) -> Vec<u8> {
    // The conflicting type proves classification uses the received error.code.
    serde_json::to_vec(&json!({"error": {
        "type": "synthetic_unrelated_type", "code": code, "message": message
    }}))
    .unwrap()
}

// Raw HTTP preserves header spelling and repeated fields on the wire. The plain
// client below, not a hand-built HeaderMap, performs the HTTP parsing under test.
async fn fetch(status: u16, headers: &[(&str, &str)], body: Vec<u8>) -> (u16, HeaderMap, Vec<u8>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    assert!(address.ip().is_loopback());
    let mut wire = format!(
        "HTTP/1.1 {status} Synthetic\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    )
    .into_bytes();
    for (name, value) in headers {
        wire.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
    }
    wire.extend_from_slice(b"\r\n");
    wire.extend_from_slice(&body);
    let server = thread::spawn(move || {
        let deadline = Instant::now() + DEADLINE;
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "loopback client did not connect");
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("loopback accept: {error}"),
            }
        };
        stream.set_read_timeout(Some(DEADLINE)).unwrap();
        stream.set_write_timeout(Some(DEADLINE)).unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            assert!(request.len() < 8192, "oversized mock request");
            assert!(Instant::now() < deadline, "mock request deadline");
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
        }
        assert!(request.starts_with(b"GET /v1/responses HTTP/1.1\r\n"));
        stream.write_all(&wire).unwrap();
    });
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(DEADLINE)
        .build()
        .unwrap();
    let response = client
        .get(format!("http://{address}/v1/responses"))
        .send()
        .await
        .unwrap();
    let received_status = response.status().as_u16();
    let received_headers = response.headers().clone();
    let received_body = response.bytes().await.unwrap().to_vec();
    server.join().unwrap();
    assert_eq!(received_status, status);
    assert_eq!(received_body, body);
    (received_status, received_headers, received_body)
}

fn expected_limits() -> OpenAiLimits {
    OpenAiLimits {
        requests: Limit {
            limit: Some(100),
            remaining: Some(0),
            reset: Some(Duration::from_millis(62003)),
        },
        tokens: Limit {
            limit: Some(2000),
            remaining: Some(1999),
            reset: Some(Duration::from_millis(2)),
        },
        retry_after: Some(Duration::from_secs(7)),
    }
}

#[tokio::test]
async fn openai_refusals_documented_errors_through_http() {
    for (status, code, expected) in [
        (429, "credit_balance_exhausted", Refusal::OutOfCredit),
        (429, "organization_spend_limit_exceeded", Refusal::OutOfCredit),
        (429, "project_spend_limit_exceeded", Refusal::OutOfCredit),
        (429, "organization_usage_limit_exceeded", Refusal::OutOfCredit),
        (
            429,
            "slow_down",
            Refusal::RateLimited {
                retry_after: Some(Duration::from_secs(7)),
            },
        ),
        (503, "server_is_overloaded", Refusal::Overloaded),
    ] {
        let (status, headers, body) =
            fetch(status, LIMIT_HEADERS, error_body(code, "synthetic refusal")).await;
        assert_eq!(
            classify_openai_refusal(status, &body, &headers),
            Some(expected),
            "{code}"
        );
        assert_eq!(openai_limits(&headers), expected_limits(), "{code}");
    }
}

#[tokio::test]
async fn openai_refusals_duplicate_headers_are_not_first_wins() {
    // Exercise each field independently, including identical duplicate values.
    for (index, (name, value)) in LIMIT_HEADERS.iter().enumerate() {
        let mut wire_headers = LIMIT_HEADERS.to_vec();
        wire_headers.push((name, value));
        let (status, headers, body) =
            fetch(429, &wire_headers, error_body("slow_down", "synthetic refusal")).await;
        assert_eq!(headers.get_all(*name).iter().count(), 2);
        let mut expected = expected_limits();
        match index {
            0 => expected.requests.limit = None,
            1 => expected.requests.remaining = None,
            2 => expected.requests.reset = None,
            3 => expected.tokens.limit = None,
            4 => expected.tokens.remaining = None,
            5 => expected.tokens.reset = None,
            6 => expected.retry_after = None,
            _ => unreachable!(),
        }
        assert_eq!(openai_limits(&headers), expected, "{name}");
        assert_eq!(
            classify_openai_refusal(status, &body, &headers),
            Some(Refusal::RateLimited {
                retry_after: expected.retry_after
            }),
            "{name}"
        );
    }
}

#[tokio::test]
async fn openai_refusals_one_mib_body_keeps_trailing_error_code() {
    // Put error.code at the end: truncating the received message must not turn
    // this non-retryable refusal into the unknown-429 rate-limit fallback.
    let prefix = b"{\"error\":{\"message\":\"";
    let suffix = b"\",\"type\":\"synthetic_unrelated_type\",\"code\":\"credit_balance_exhausted\"}}";
    let mut body = prefix.to_vec();
    body.resize((1 << 20) - suffix.len(), b'x');
    body.extend_from_slice(suffix);
    assert_eq!(body.len(), 1 << 20);
    let (status, headers, body) = fetch(429, LIMIT_HEADERS, body).await;
    assert_eq!(
        classify_openai_refusal(status, &body, &headers),
        Some(Refusal::OutOfCredit)
    );
    assert_eq!(openai_limits(&headers), expected_limits());
}
