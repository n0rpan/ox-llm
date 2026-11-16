use super::*;
use crate::secrets;
use serde_json::json;
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
    thread,
    time::Instant,
};

const CREDENTIAL: &str = "synthetic-model-discovery-credential";
static NEXT: AtomicUsize = AtomicUsize::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "openai-models-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path.join("credentials.json"))
            .unwrap();
        write!(file, "{}", json!({"openai": CREDENTIAL})).unwrap();
        Self(path)
    }
    fn key(&self) -> Key {
        secrets::read(&self.0.join("credentials.json"), "openai").unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

struct Reply {
    path: &'static str,
    status: u16,
    headers: String,
    body: String,
    etag: Option<&'static str>,
    chunked: bool,
    delay: Duration,
}
impl Reply {
    fn ok(path: &'static str, body: &str) -> Self {
        Self {
            path,
            status: 200,
            headers: String::new(),
            body: body.into(),
            etag: None,
            chunked: false,
            delay: Duration::ZERO,
        }
    }
}
fn mock(replies: Vec<Reply>) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let task = thread::spawn(move || {
        for reply in replies {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "expected discovery request");
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(e) => panic!("{e}"),
                }
            };
            socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            socket.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                socket.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
                assert!(request.len() < 16384);
            }
            let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
            assert!(request.starts_with(&format!("get {} http/1.1\r\n", reply.path.to_ascii_lowercase())));
            assert!(request.contains(&format!("authorization: bearer {CREDENTIAL}\r\n")));
            match reply.etag {
                Some(etag) => assert!(request.contains(&format!("if-none-match: {etag}\r\n"))),
                None => assert!(!request.contains("if-none-match:")),
            }
            let framing = if reply.chunked {
                "Transfer-Encoding: chunked".into()
            } else {
                format!("Content-Length: {}", reply.body.len())
            };
            let _ = write!(
                socket,
                "HTTP/1.1 {} Mock\r\n{framing}\r\n{}Connection: close\r\n\r\n",
                reply.status, reply.headers
            );
            thread::sleep(reply.delay);
            if reply.chunked {
                for chunk in reply.body.as_bytes().chunks(16384) {
                    if write!(socket, "{:x}\r\n", chunk.len()).is_err()
                        || socket.write_all(chunk).is_err()
                        || socket.write_all(b"\r\n").is_err()
                    {
                        break;
                    }
                }
                let _ = socket.write_all(b"0\r\n\r\n");
            } else {
                let _ = socket.write_all(reply.body.as_bytes());
            }
        }
    });
    (base, task)
}
fn client(base: &str) -> Client {
    Client::new(base, Duration::from_secs(2)).unwrap()
}

#[tokio::test]
async fn api_ids_and_shutdown_dates_without_inventing_limits() {
    let fixture = Fixture::new();
    let (base, task) = mock(vec![Reply::ok(
        "/v1/models",
        r#"{"data":[{"id":"synthetic-api","shutdown_date":"2027-01-01","created":1,"owned_by":"synthetic"},{"id":"synthetic-no-date"},{"id":"synthetic-null","shutdown_date":null}]}"#,
    )]);
    let models = client(&base).api_models(&fixture.key()).await.unwrap();
    assert_eq!(
        models,
        vec![
            ApiModel {
                id: "synthetic-api".into(),
                shutdown_date: Some("2027-01-01".into())
            },
            ApiModel {
                id: "synthetic-no-date".into(),
                shutdown_date: None
            },
            ApiModel {
                id: "synthetic-null".into(),
                shutdown_date: None
            },
        ]
    );
    task.join().unwrap();
}

#[tokio::test]
async fn subscription_metadata_and_conditional_refresh() {
    let fixture = Fixture::new();
    let mut first = Reply::ok(
        "/v1/models?client_version=1.2%2Btest+build",
        r#"{"models":[{"slug":"synthetic-subscription","context_window":200000,"auto_compact_token_limit":180000,"supported_reasoning_levels":[{"effort":"low","description":"Fast"},{"effort":"future","description":"New"}],"input_modalities":["text","image"],"visibility":"list","unknown":true},{"slug":"synthetic-hidden","context_window":8192,"auto_compact_token_limit":null,"supported_reasoning_levels":[],"input_modalities":["text"],"visibility":"hide"}]}"#,
    );
    first.headers = "ETag: W/\"revision-1\"\r\n".into();
    let mut second = Reply::ok("/v1/models?client_version=1.2%2Btest+build", "");
    second.status = 304;
    second.etag = Some("w/\"revision-1\"");
    let (base, task) = mock(vec![first, second]);
    let client = client(&base);
    let key = fixture.key();
    let SubscriptionModels::Modified { models, etag } = client
        .subscription_models(&key, "1.2+test build", None)
        .await
        .unwrap()
    else {
        panic!("expected models")
    };
    assert_eq!(etag.as_deref(), Some("W/\"revision-1\""));
    assert_eq!(models.len(), 2);
    assert_eq!(models[0].slug, "synthetic-subscription");
    assert_eq!(models[0].context_window, 200000);
    assert_eq!(models[0].auto_compact_token_limit, Some(180000));
    assert_eq!(
        models[0].supported_reasoning_levels,
        vec![
            ReasoningLevel {
                effort: "low".into(),
                description: "Fast".into()
            },
            ReasoningLevel {
                effort: "future".into(),
                description: "New".into()
            },
        ]
    );
    assert_eq!(models[0].input_modalities, ["text", "image"]);
    assert_eq!(models[0].visibility, "list");
    assert_eq!(models[1].auto_compact_token_limit, None);
    assert_eq!(models[1].visibility, "hide");
    assert_eq!(
        client
            .subscription_models(&key, "1.2+test build", etag.as_deref())
            .await
            .unwrap(),
        SubscriptionModels::NotModified { etag: None }
    );
    task.join().unwrap();
}

#[tokio::test]
async fn unauthorized_and_redirects_never_leak_credentials() {
    let fixture = Fixture::new();
    // A redirect target must receive no request or bearer credential.
    let target = TcpListener::bind("127.0.0.1:0").unwrap();
    target.set_nonblocking(true).unwrap();
    for status in [401, 302, 307] {
        let mut reply = Reply::ok("/v1/models", CREDENTIAL);
        reply.status = status;
        reply.headers = format!("Location: http://{}/stolen\r\n", target.local_addr().unwrap());
        let (base, task) = mock(vec![reply]);
        let client = client(&base);
        let key = fixture.key();
        let error = client.api_models(&key).await.unwrap_err();
        assert_eq!(error, Error::Http(status));
        assert!(!format!("{error} {error:?} {client:?} {key:?}").contains(CREDENTIAL));
        task.join().unwrap();
        assert_eq!(
            target.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}

#[tokio::test]
async fn oversized_content_length_and_chunked_bodies_are_capped() {
    let fixture = Fixture::new();
    for subscription in [false, true] {
        for chunked in [false, true] {
            let path = if subscription {
                "/v1/models?client_version=1"
            } else {
                "/v1/models"
            };
            let mut reply = Reply::ok(path, "");
            reply.body = "x".repeat(MAX_BODY + 1);
            reply.chunked = chunked;
            let (base, task) = mock(vec![reply]);
            let client = client(&base);
            let key = fixture.key();
            let error = if subscription {
                client.subscription_models(&key, "1", None).await.unwrap_err()
            } else {
                client.api_models(&key).await.unwrap_err()
            };
            assert_eq!(error, Error::TooLarge);
            task.join().unwrap();
        }
    }
}

#[tokio::test]
async fn malformed_responses_and_unsolicited_304_are_protocol_errors() {
    let fixture = Fixture::new();
    for body in [
        CREDENTIAL,
        "{}",
        r#"{"data":[{"id":""}]}"#,
        r#"{"data":[{"id":"synthetic","shutdown_date":42}]}"#,
    ] {
        let (base, task) = mock(vec![Reply::ok("/v1/models", body)]);
        let error = client(&base).api_models(&fixture.key()).await.unwrap_err();
        assert_eq!(error, Error::Protocol);
        assert!(!format!("{error} {error:?}").contains(CREDENTIAL));
        task.join().unwrap();
    }
    for body in [
        "{}",
        r#"{"models":[{"slug":"synthetic"}]}"#,
        r#"{"models":[{"slug":"synthetic","context_window":1,"supported_reasoning_levels":[{"effort":2}],"input_modalities":[],"visibility":"list"}]}"#,
    ] {
        let (base, task) = mock(vec![Reply::ok("/v1/models?client_version=1", body)]);
        assert_eq!(
            client(&base)
                .subscription_models(&fixture.key(), "1", None)
                .await
                .unwrap_err(),
            Error::Protocol
        );
        task.join().unwrap();
    }
    for subscription in [false, true] {
        let mut reply = Reply::ok(
            if subscription {
                "/v1/models?client_version=1"
            } else {
                "/v1/models"
            },
            "",
        );
        reply.status = 304;
        let (base, task) = mock(vec![reply]);
        let client = client(&base);
        let key = fixture.key();
        if subscription {
            assert_eq!(
                client.subscription_models(&key, "1", None).await.unwrap_err(),
                Error::Protocol
            );
        } else {
            assert_eq!(client.api_models(&key).await.unwrap_err(), Error::Http(304));
        }
        task.join().unwrap();
    }
}

#[tokio::test]
async fn deadline_includes_response_body() {
    let fixture = Fixture::new();
    let mut reply = Reply::ok("/v1/models", r#"{"data":[]}"#);
    reply.delay = Duration::from_millis(300);
    let (base, task) = mock(vec![reply]);
    assert_eq!(
        Client::new(&base, Duration::from_millis(100))
            .unwrap()
            .api_models(&fixture.key())
            .await
            .unwrap_err(),
        Error::Transport
    );
    task.join().unwrap();
}

#[tokio::test]
async fn subscription_root_empty_list_and_updated_304_etag() {
    let fixture = Fixture::new();
    let first = Reply::ok("/v1/subscription/models?client_version=1", r#"{"models":[]}"#);
    let mut second = Reply::ok("/v1/subscription/models?client_version=1", "");
    second.status = 304;
    second.etag = Some("\"previous\"");
    second.headers = "ETag: \"next\"\r\n".into();
    let (base, task) = mock(vec![first, second]);
    let client = client(&format!("{base}/subscription/"));
    let key = fixture.key();
    assert_eq!(
        client.subscription_models(&key, "1", None).await.unwrap(),
        SubscriptionModels::Modified {
            models: vec![],
            etag: None
        }
    );
    assert_eq!(
        client
            .subscription_models(&key, "1", Some("\"previous\""))
            .await
            .unwrap(),
        SubscriptionModels::NotModified {
            etag: Some("\"next\"".into())
        }
    );
    task.join().unwrap();
}

#[tokio::test]
async fn subscription_401_and_invalid_request_headers_are_safe() {
    let fixture = Fixture::new();
    let mut reply = Reply::ok("/v1/models?client_version=1", CREDENTIAL);
    reply.status = 401;
    let (base, task) = mock(vec![reply]);
    let key = fixture.key();
    let error = client(&base)
        .subscription_models(&key, "1", None)
        .await
        .unwrap_err();
    assert_eq!(error, Error::Http(401));
    assert!(!format!("{error:?} {error}").contains(CREDENTIAL));
    task.join().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let client = client(&format!("http://{}/v1", listener.local_addr().unwrap()));
    for etag in ["", "bad\r\nInjected: value"] {
        assert_eq!(
            client
                .subscription_models(&key, "1", Some(etag))
                .await
                .unwrap_err(),
            Error::Setup
        );
    }
    assert_eq!(
        client.subscription_models(&key, "", None).await.unwrap_err(),
        Error::Setup
    );
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[test]
fn endpoint_validation_and_debug_are_safe() {
    for base in [
        "http://example.invalid/v1".to_owned(),
        "file:///v1".to_owned(),
        format!("https://{CREDENTIAL}@example.invalid/v1"),
        format!("https://example.invalid/v1?key={CREDENTIAL}"),
        "https://example.invalid/v1#fragment".to_owned(),
    ] {
        let error = Client::new(&base, Duration::from_secs(1)).unwrap_err();
        assert_eq!(error, Error::Endpoint);
        assert!(!format!("{error:?} {error}").contains(CREDENTIAL));
    }
    assert_eq!(
        Client::new("https://example.invalid/v1", Duration::ZERO).unwrap_err(),
        Error::Endpoint
    );
    for base in [
        "https://example.invalid/v1",
        "http://127.0.0.1:1/v1",
        "http://[::1]:1/v1",
        "http://[::ffff:127.0.0.1]:1/v1",
    ] {
        assert!(!format!("{:?}", client(base)).contains(base));
    }
}
