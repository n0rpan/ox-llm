#![cfg(feature = "share")]
use oxc_gateway::share::tailnet::{Error, LocalApi};
use serde_json::{json, Value};
use std::{fs, net::IpAddr, path::PathBuf, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixListener,
    task::JoinHandle,
};

struct Mock {
    root: PathBuf,
    socket: PathBuf,
    task: JoinHandle<Vec<String>>,
}
impl Mock {
    fn start(replies: Vec<Vec<u8>>) -> Self {
        let root = directory();
        let socket = root.join("api");
        let listener = UnixListener::bind(&socket).unwrap();
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            for reply in replies {
                let (mut peer, _) = listener.accept().await.unwrap();
                let mut head = Vec::new();
                while !head.ends_with(b"\r\n\r\n") {
                    assert!(head.len() < 1024);
                    head.push(peer.read_u8().await.unwrap());
                }
                requests.push(String::from_utf8(head).unwrap());
                peer.write_all(&reply).await.unwrap();
            }
            requests
        });
        Self { root, socket, task }
    }
    fn api(&self) -> LocalApi {
        LocalApi::new(self.socket.clone(), Duration::from_secs(1)).unwrap()
    }
    async fn finish(self) -> Vec<String> {
        let requests = self.task.await.unwrap();
        fs::remove_dir_all(self.root).unwrap();
        requests
    }
}
fn directory() -> PathBuf {
    let mut nonce = [0; 16];
    getrandom::fill(&mut nonce).unwrap();
    let name: String = nonce.iter().map(|n| format!("{n:02x}")).collect();
    let root = std::env::temp_dir().join(format!("oxa-tailnet-{name}"));
    fs::create_dir(&root).unwrap();
    root
}

fn response(value: Value) -> Vec<u8> {
    let body = value.to_string();
    format!("HTTP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n{body}", body.len()).into_bytes()
}
fn status() -> Value {
    json!({"BackendState":"Running","TUN":true,"TailscaleIPs":["100.80.1.1","fd7a:115c:a1e0::1"],"Self":{"UserID":42}})
}
fn who(peer: &str) -> Value {
    json!({"Node":{"User":42,"Addresses":[format!("{peer}/{}",if peer.contains(':') {128} else {32})]},"UserProfile":{"ID":42}})
}

#[tokio::test(flavor = "current_thread")]
async fn assigned_addresses_and_same_owner_use_fresh_daemon_state_and_numeric_peer() {
    let peer: IpAddr = "fd7a:115c:a1e0::2".parse().unwrap();
    let mut changed = who(&peer.to_string());
    changed["Node"]["User"] = json!(43);
    let mock = Mock::start(vec![
        response(status()),
        response(status()),
        response(who(&peer.to_string())),
        response(status()),
        response(changed),
    ]);
    let api = mock.api();
    assert_eq!(format!("{api:?}"), "TailnetLocalApi([private])");
    let ips = api.addresses().await.unwrap();
    assert_eq!(ips.len(), 2);
    assert!(ips.contains(&"100.80.1.1".parse().unwrap()));
    assert!(api.same_owner(peer).await.unwrap());
    assert!(!api.same_owner(peer).await.unwrap());
    let requests = mock.finish().await;
    assert_eq!(requests.len(), 5);
    assert!(requests[0].starts_with("GET /localapi/v0/status?peers=false HTTP/1.0\r\n"));
    assert!(requests[2].starts_with("GET /localapi/v0/whois?addr=fd7a%3A115c%3Aa1e0%3A%3A2 HTTP/1.0\r\n"));
    assert!(requests
        .iter()
        .all(|r| r.contains("Host: local-tailscaled.sock\r\n") && !r.contains("Authorization")));
}

#[tokio::test(flavor = "current_thread")]
async fn absent_stopped_userspace_or_invalid_addresses_never_produce_a_binding() {
    for (field, value) in [
        ("BackendState", json!("NeedsLogin")),
        ("TUN", json!(false)),
        ("TailscaleIPs", json!([])),
        ("TailscaleIPs", json!(["0.0.0.0"])),
        ("TailscaleIPs", json!(["127.0.0.1"])),
        ("TailscaleIPs", json!(["ff02::1"])),
        ("TailscaleIPs", json!(["not-an-address"])),
        ("TailscaleIPs", json!(vec!["100.80.1.1"; 9])),
    ] {
        let mut body = status();
        body[field] = value;
        let mock = Mock::start(vec![response(body)]);
        assert!(mock.api().addresses().await.is_err());
        mock.finish().await;
    }
    let mock = Mock::start(vec![]);
    let api = mock.api();
    mock.finish().await;
    assert_eq!(api.addresses().await, Err(Error::Unavailable));
}

#[tokio::test(flavor = "current_thread")]
async fn peer_checks_refuse_shared_tagged_malformed_and_mismatched_identity() {
    let peer = "100.80.1.2";
    for (pointer, value) in [
        ("/Node/User", json!(43)),
        ("/UserProfile/ID", json!(43)),
        ("/Node/Tags", json!(["tag:synthetic"])),
        ("/Node/Sharer", json!(43)),
        ("/Node/Sharer", json!("invalid")),
        ("/Node/Addresses", json!(["100.80.1.3/32"])),
        ("/Node/Addresses", json!(["100.80.1.2/24"])),
        ("/Node", Value::Null),
    ] {
        let mut body = who(peer);
        if pointer == "/Node/Sharer" || pointer == "/Node/Tags" {
            body["Node"][pointer.rsplit('/').next().unwrap()] = value;
        } else {
            *body.pointer_mut(pointer).unwrap() = value;
        }
        let mock = Mock::start(vec![response(status()), response(body)]);
        assert!(!mock
            .api()
            .same_owner(peer.parse().unwrap())
            .await
            .unwrap_or(false));
        mock.finish().await;
    }
    for field in ["UserID", "Tags"] {
        let mut body = status();
        body["Self"][field] = if field == "UserID" {
            json!(0)
        } else {
            json!(["tag:synthetic"])
        };
        let mock = Mock::start(vec![response(body)]);
        assert!(!mock
            .api()
            .same_owner(peer.parse().unwrap())
            .await
            .unwrap_or(false));
        assert_eq!(mock.finish().await.len(), 1);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn http_bounds_framing_redirects_and_error_bodies_fail_closed() {
    for bytes in [
        b"HTTP/1.0 302 Found\r\nLocation: http://example.invalid\r\n\r\n{}".to_vec(),
        b"HTTP/1.0 403 Forbidden\r\n\r\nprivate synthetic body".to_vec(),
        b"HTTP/1.0 200 OK\r\nContent-Length: 2\r\nContent-Length: 2\r\n\r\n{}".to_vec(),
        b"HTTP/1.0 200 OK\r\nContent-Length: 3\r\n\r\n{}".to_vec(),
        b"HTTP/1.0 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n".to_vec(),
        b"HTTP/1.0 200 OK\r\n bad: header\r\n\r\n{}".to_vec(),
        b"HTTP/1.0 200 OK\r\n\r\n{".to_vec(),
        format!("HTTP/1.0 200 OK\r\nOversized: {}\r\n\r\n{{}}", "x".repeat(8192)).into_bytes(),
        format!("HTTP/1.0 200 OK\r\n\r\n{}", " ".repeat(65537)).into_bytes(),
    ] {
        let mock = Mock::start(vec![bytes]);
        let error = mock.api().addresses().await.unwrap_err();
        assert!(matches!(error, Error::Invalid | Error::Unavailable));
        assert!(!error.to_string().contains("private synthetic"));
        mock.finish().await;
    }
    // Close-delimited responses are intentional for the HTTP/1.0 request.
    let mock = Mock::start(vec![format!("HTTP/1.0 200 OK\r\n\r\n{}", status()).into_bytes()]);
    assert_eq!(mock.api().addresses().await.unwrap().len(), 2);
    mock.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn deadlines_cover_stalls_without_blocking_runtime_or_leaking_slots() {
    let root = directory();
    let socket = root.join("api");
    let listener = UnixListener::bind(&socket).unwrap();
    let api = LocalApi::new(socket.clone(), Duration::from_millis(40)).unwrap();
    let waiting = api.addresses();
    tokio::pin!(waiting);
    let mut held = Vec::new();
    tokio::select! {
        _ = &mut waiting => panic!("must wait for a daemon reply"),
        peer = listener.accept() => held.push(peer.unwrap().0),
    }
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert_eq!(waiting.await, Err(Error::Unavailable));
    for _ in 0..10 {
        assert_eq!(api.addresses().await, Err(Error::Unavailable));
        held.push(listener.accept().await.unwrap().0);
    }
    drop(held);
    drop(listener);
    fs::remove_dir_all(socket.parent().unwrap()).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn concurrent_checks_are_bounded_and_cancellation_closes_sockets_and_releases_slots() {
    let root = directory();
    let socket = root.join("api");
    let listener = UnixListener::bind(&socket).unwrap();
    let api = std::sync::Arc::new(LocalApi::new(socket, Duration::from_secs(5)).unwrap());
    let mut tasks = tokio::task::JoinSet::new();
    let mut peers = Vec::new();
    for _ in 0..8 {
        let api = api.clone();
        tasks.spawn(async move { api.addresses().await });
        peers.push(listener.accept().await.unwrap().0);
    }
    assert_eq!(api.addresses().await, Err(Error::Busy));
    tasks.abort_all();
    while let Some(task) = tasks.join_next().await {
        assert!(task.unwrap_err().is_cancelled());
    }
    for mut peer in peers {
        let mut bytes = Vec::new();
        tokio::time::timeout(Duration::from_secs(1), peer.read_to_end(&mut bytes))
            .await
            .unwrap()
            .unwrap();
    }
    let next = {
        let api = api.clone();
        tokio::spawn(async move { api.addresses().await })
    };
    let (peer, _) = listener.accept().await.unwrap();
    next.abort();
    assert!(next.await.unwrap_err().is_cancelled());
    drop(peer);
    drop(listener);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn construction_is_inert_and_rejects_unbounded_or_invalid_configuration() {
    let root = directory();
    let socket = root.join("not-created");
    LocalApi::new(socket.clone(), Duration::from_secs(1)).unwrap();
    assert!(!socket.exists());
    for (path, deadline) in [
        (PathBuf::from("relative"), Duration::from_secs(1)),
        (socket.clone(), Duration::ZERO),
        (socket, Duration::from_secs(31)),
    ] {
        assert_eq!(LocalApi::new(path, deadline).unwrap_err(), Error::Invalid);
    }
    assert!(fs::read_dir(&root).unwrap().next().is_none());
    fs::remove_dir(root).unwrap();
}
