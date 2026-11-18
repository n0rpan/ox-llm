#![cfg(feature = "share")]
#[path = "common/share.rs"]
mod support;
use oxc_gateway::share::{
    keys::{Limits, Store},
    listener::*,
};
use serde_json::{json, Value};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use support::{limits, Directory};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::Notify,
};

struct Fake {
    models: Vec<Model>,
    calls: AtomicUsize,
    messages: Mutex<Vec<Vec<Value>>>,
    guests: Mutex<Vec<(String, String)>>,
    hold: bool,
    started: Notify,
    cancelled: Notify,
    release: Notify,
}
impl Fake {
    fn new(hold: bool) -> Self {
        Self {
            models: vec![
                Model {
                    alias: "local-model".into(),
                    context_window: 100,
                    max_output: 10,
                    access: Access::Local,
                },
                Model {
                    alias: "metered-model".into(),
                    context_window: 100,
                    max_output: 10,
                    access: Access::Metered,
                },
                Model {
                    alias: "subscription-model".into(),
                    context_window: 100,
                    max_output: 10,
                    access: Access::Subscription,
                },
            ],
            calls: AtomicUsize::new(0),
            messages: Mutex::new(Vec::new()),
            guests: Mutex::new(Vec::new()),
            hold,
            started: Notify::new(),
            cancelled: Notify::new(),
            release: Notify::new(),
        }
    }
}
impl Answer for Fake {
    fn models(&self) -> &[Model] {
        &self.models
    }
    async fn chat(
        &self,
        request: Request,
        guest: Guest<'_>,
        output: Output,
        cancel: CancelToken,
    ) -> Result<Completed, Failure> {
        assert!(request.max_output <= 10);
        assert_eq!(format!("{guest:?}"), "Guest([authenticated])");
        let trusted = (guest.key_id().to_owned(), guest.key_name().to_owned());
        self.guests.lock().unwrap().push(trusted.clone());
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.messages.lock().unwrap().push(request.messages);
        self.started.notify_one();
        if self.hold {
            cancel.cancelled().await;
            self.cancelled.notify_one();
            self.release.notified().await; // Simulated model ownership during cleanup.
            assert_eq!(
                (guest.key_id(), guest.key_name()),
                (trusted.0.as_str(), trusted.1.as_str())
            );
            return Err(Failure::Failed);
        }
        output.text("Synthetic reply").await?;
        Ok(Completed {
            usage: Some(Usage { input: 5, output: 3 }),
            finish: Finish::Stop,
        })
    }
}
struct Network {
    addresses: Vec<IpAddr>,
    owner: AtomicBool,
    peers: Mutex<Vec<IpAddr>>,
    hold_check: AtomicBool,
    check_started: Notify,
    check_release: Notify,
}
impl Network {
    fn local() -> Self {
        Self {
            addresses: vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
            owner: AtomicBool::new(true),
            peers: Mutex::new(Vec::new()),
            hold_check: AtomicBool::new(false),
            check_started: Notify::new(),
            check_release: Notify::new(),
        }
    }
}
impl Tailnet for Network {
    async fn addresses(&self) -> Result<Vec<IpAddr>, Failure> {
        Ok(self.addresses.clone())
    }
    async fn same_owner(&self, peer: IpAddr) -> Result<bool, Failure> {
        self.peers.lock().unwrap().push(peer);
        self.check_started.notify_one();
        if self.hold_check.load(Ordering::SeqCst) {
            self.check_release.notified().await;
        }
        Ok(self.owner.load(Ordering::SeqCst))
    }
}
struct Running {
    address: SocketAddr,
    stop: CancelToken,
    task: tokio::task::JoinHandle<Result<(), Failure>>,
    status: oxc_gateway::share::status::Status,
    keys: Arc<Store>,
    fake: Arc<Fake>,
    network: Arc<Network>,
    bearer: String,
    id: String,
    _directory: Directory,
}
impl Running {
    async fn start(config: Config, limits: Limits, hold: bool) -> Self {
        let d = Directory::new();
        let keys = Arc::new(d.store());
        let key = keys.create("Device", limits).unwrap();
        let (fake, network) = (Arc::new(Fake::new(hold)), Arc::new(Network::local()));
        let server = Server::bind(keys.clone(), fake.clone(), network.clone(), config)
            .await
            .unwrap();
        let address = server.address().unwrap();
        let status = server.status();
        let stop = CancelToken::new();
        let task = tokio::spawn(server.serve(stop.clone()));
        Self {
            address,
            stop,
            task,
            status,
            keys,
            fake,
            network,
            bearer: key.bearer.expose().into(),
            id: key.info.id,
            _directory: d,
        }
    }
    async fn close(self) {
        self.stop.cancel();
        tokio::time::timeout(Duration::from_secs(3), self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let snapshot = self.status.snapshot().unwrap();
        assert_eq!(snapshot.connections, 0);
        assert!(snapshot.keys.is_empty());
    }
    fn header(&self, method: &str, path: &str, extra: &str) -> String {
        format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\n{extra}",
            self.bearer
        )
    }
    async fn models(&self) -> String {
        exchange(
            self.address,
            format!("{}\r\n", self.header("GET", "/v1/models", "")),
        )
        .await
    }
    async fn chat(&self, body: Value) -> String {
        let body = body.to_string();
        let headers = format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        );
        exchange(
            self.address,
            format!(
                "{}\r\n{body}",
                self.header("POST", "/v1/chat/completions", &headers)
            ),
        )
        .await
    }
}
async fn exchange(address: SocketAddr, request: String) -> String {
    let mut stream = TcpStream::connect(address).await.unwrap();
    let _ = stream.write_all(request.as_bytes()).await;
    let mut bytes = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(3), stream.read_to_end(&mut bytes))
        .await
        .unwrap();
    String::from_utf8(bytes).unwrap()
}
fn body(model: &str) -> Value {
    json!({"model":model,"messages":[{"role":"user","content":"Synthetic request"}],"stream":true,"max_tokens":10,"stream_options":{"include_usage":true}})
}
fn config() -> Config {
    Config {
        read_timeout: Duration::from_millis(200),
        write_timeout: Duration::from_millis(200),
        ..Config::default()
    }
}

#[tokio::test(flavor = "current_thread")]
async fn authenticated_stream_is_guest_only_settles_usage_and_revocation_is_fresh() {
    let r = Running::start(config(), limits(), false).await;
    let models = r.models().await;
    assert!(models.starts_with("HTTP/1.1 200"));
    assert!(models.contains("local-model"));
    assert!(!models.contains("Device") && !models.contains(&r.id));
    assert!(!models.contains("metered-model") && !models.contains("subscription-model"));
    let stream = r.chat(body("local-model")).await;
    assert!(stream.starts_with("HTTP/1.1 200"));
    assert!(stream.contains("Synthetic reply") && stream.contains("data: [DONE]"));
    assert!(stream.contains("\"prompt_tokens\":5") && stream.contains("\"completion_tokens\":3"));
    assert!(!stream.contains(&r.bearer) && !stream.contains("Device") && !stream.contains(&r.id));
    assert_eq!(
        *r.fake.guests.lock().unwrap(),
        vec![(r.id.clone(), "Device".into())]
    );
    assert_eq!(
        r.fake.messages.lock().unwrap()[0],
        json!([{"role":"user","content":"Synthetic request"}])
            .as_array()
            .unwrap()
            .clone()
    );
    let day = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        / 86400;
    assert_eq!(r.keys.guest_usage(day, &r.id).unwrap(), (1, 8));
    r.keys.revoke(&r.id).unwrap();
    assert!(r.models().await.starts_with("HTTP/1.1 401"));
    r.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn tool_session_file_fields_and_subscription_routes_are_refused_before_backend() {
    let mut c = config();
    c.remote_models
        .extend(["metered-model".into(), "subscription-model".into()]);
    let r = Running::start(c, limits(), false).await;
    let listed = r.models().await;
    assert!(listed.contains("metered-model") && !listed.contains("subscription-model"));
    assert!(r
        .chat(body("subscription-model"))
        .await
        .starts_with("HTTP/1.1 403"));
    for (field, value) in [
        ("tools", json!([])),
        ("tool_choice", json!("none")),
        ("guest_key_name", json!("untrusted-claim")),
        ("guest", json!({"key_name":"untrusted-claim"})),
        ("origin", json!("oxa")),
        ("session", json!("owner")),
        ("resume", json!(true)),
        ("file", json!("/synthetic/path")),
    ] {
        let mut request = body("local-model");
        request[field] = value;
        assert!(r.chat(request).await.starts_with("HTTP/1.1 400"));
    }
    for message in [
        json!({"role":"tool","content":"result"}),
        json!({"role":"user","content":[{"type":"image_url","image_url":{"url":"file:///synthetic"}}]}),
    ] {
        let mut request = body("local-model");
        request["messages"] = json!([message]);
        assert!(r.chat(request).await.starts_with("HTTP/1.1 400"));
    }
    assert_eq!(r.fake.calls.load(Ordering::SeqCst), 0);
    r.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn peer_check_uses_transport_identity_and_failure_never_forwards() {
    let r = Running::start(
        Config {
            same_owner: true,
            ..config()
        },
        limits(),
        false,
    )
    .await;
    r.network.owner.store(false, Ordering::SeqCst);
    let request = format!(
        "{}\r\n",
        r.header("GET", "/v1/models", "X-Forwarded-For: 192.0.2.1\r\n")
    );
    assert!(exchange(r.address, request).await.starts_with("HTTP/1.1 403"));
    assert_eq!(
        *r.network.peers.lock().unwrap(),
        vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]
    );
    assert_eq!(r.fake.calls.load(Ordering::SeqCst), 0);
    r.network.owner.store(true, Ordering::SeqCst);
    assert!(r.models().await.starts_with("HTTP/1.1 200"));
    r.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn daily_peer_and_key_limits_refuse_further_work() {
    let r = Running::start(
        config(),
        Limits {
            daily_requests: 1,
            ..limits()
        },
        false,
    )
    .await;
    assert!(r.chat(body("local-model")).await.contains("[DONE]"));
    assert!(r.chat(body("local-model")).await.starts_with("HTTP/1.1 429"));
    assert_eq!(r.fake.calls.load(Ordering::SeqCst), 1);
    r.close().await;
    for c in [
        Config {
            requests_per_minute: 1,
            ..config()
        },
        Config {
            peer_requests_per_minute: 1,
            ..config()
        },
    ] {
        let r = Running::start(c, limits(), false).await;
        assert!(r.models().await.starts_with("HTTP/1.1 200"));
        assert!(r.models().await.starts_with("HTTP/1.1 429"));
        r.close().await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn malformed_oversized_and_unauthenticated_http_is_bounded() {
    let r = Running::start(
        Config {
            max_body: 128,
            ..config()
        },
        limits(),
        false,
    )
    .await;
    assert!(exchange(
        r.address,
        "GET /v1/models HTTP/1.1\r\nHost: localhost\r\n\r\n".into()
    )
    .await
    .starts_with("HTTP/1.1 401"));
    for headers in [
        "Content-Length: 2\r\nContent-Length: 2\r\n",
        "Transfer-Encoding: chunked\r\n",
        "Content-Length: -1\r\n",
    ] {
        let request = format!("{}\r\n{{}}", r.header("POST", "/v1/chat/completions", headers));
        assert!(exchange(r.address, request).await.starts_with("HTTP/1.1 400"));
    }
    assert!(r.chat(body("local-model")).await.starts_with("HTTP/1.1 413"));
    let header = format!(
        "{}X-Large: {}\r\n\r\n",
        r.header("GET", "/v1/models", ""),
        "x".repeat(17000)
    );
    assert!(exchange(r.address, header).await.starts_with("HTTP/1.1 413"));
    assert_eq!(r.fake.calls.load(Ordering::SeqCst), 0);
    r.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn shutdown_and_deadline_join_model_cleanup_instead_of_dropping_ownership() {
    for deadline in [false, true] {
        let c = if deadline {
            Config {
                request_timeout: Duration::from_millis(20),
                ..config()
            }
        } else {
            config()
        };
        let r = Running::start(c, limits(), true).await;
        let request = body("local-model").to_string();
        let header = r.header(
            "POST",
            "/v1/chat/completions",
            &format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                request.len()
            ),
        );
        let address = r.address;
        let client = tokio::spawn(exchange(address, format!("{header}\r\n{request}")));
        tokio::time::timeout(Duration::from_secs(2), r.fake.started.notified())
            .await
            .unwrap();
        if !deadline {
            r.stop.cancel();
        }
        tokio::time::timeout(Duration::from_secs(2), r.fake.cancelled.notified())
            .await
            .unwrap();
        assert!(
            !r.task.is_finished(),
            "shutdown must retain backend cleanup ownership"
        );
        r.fake.release.notify_one();
        assert!(!client.await.unwrap().contains("[DONE]"));
        r.close().await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn tailnet_absence_or_unspecified_detection_never_falls_back_to_wildcard() {
    let d = Directory::new();
    let keys = Arc::new(d.store());
    for addresses in [vec![], vec![IpAddr::V4(Ipv4Addr::UNSPECIFIED)]] {
        let network = Arc::new(Network {
            addresses,
            owner: AtomicBool::new(true),
            peers: Mutex::new(Vec::new()),
            hold_check: AtomicBool::new(false),
            check_started: Notify::new(),
            check_release: Notify::new(),
        });
        assert!(
            Server::bind(keys.clone(), Arc::new(Fake::new(false)), network, config())
                .await
                .is_err()
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn disconnect_cancels_a_silent_backend_and_waits_for_its_cleanup() {
    let r = Running::start(config(), limits(), true).await;
    let body = body("local-model").to_string();
    let header = r.header(
        "POST",
        "/v1/chat/completions",
        &format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        ),
    );
    let mut socket = TcpStream::connect(r.address).await.unwrap();
    socket
        .write_all(format!("{header}\r\n{body}").as_bytes())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), r.fake.started.notified())
        .await
        .unwrap();
    let live = r.status.snapshot().unwrap();
    assert_eq!(live.connections, 1);
    assert_eq!(live.keys.len(), 1);
    assert_eq!(live.keys[0].id, r.id);
    assert_eq!(live.keys[0].name, "Device");
    assert_eq!(live.keys[0].active_requests, 1);
    assert!(!format!("{live:?}").contains(&r.bearer));
    drop(socket);
    let observed = tokio::time::timeout(Duration::from_secs(2), r.fake.cancelled.notified()).await;
    assert!(!r.task.is_finished());
    assert_eq!(r.status.snapshot().unwrap(), live);
    r.stop.cancel();
    r.fake.release.notify_one();
    r.close().await;
    assert!(observed.is_ok(), "disconnect did not wake a silent model request");
}

#[tokio::test(flavor = "current_thread")]
async fn owner_status_groups_by_key_id_and_releases_each_request_after_cleanup() {
    for distinct in [false, true] {
        let r = Running::start(
            Config {
                connections: 2,
                ..config()
            },
            limits(),
            true,
        )
        .await;
        assert_eq!(r.status.snapshot().unwrap().connections, 0);
        let second = if distinct {
            Some(r.keys.create("Tablet", limits()).unwrap())
        } else {
            None
        };
        let second_id = second.as_ref().map(|k| &k.info.id).unwrap_or(&r.id);
        let mut clients = Vec::new();
        for bearer in [
            &r.bearer[..],
            second.as_ref().map(|k| k.bearer.expose()).unwrap_or(&r.bearer),
        ] {
            let body = body("local-model").to_string();
            let header = r
                .header(
                    "POST",
                    "/v1/chat/completions",
                    &format!(
                        "Content-Type: application/json\r\nContent-Length: {}\r\n",
                        body.len()
                    ),
                )
                .replace(&r.bearer, bearer);
            let mut socket = TcpStream::connect(r.address).await.unwrap();
            socket
                .write_all(format!("{header}\r\n{body}").as_bytes())
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(2), r.fake.started.notified())
                .await
                .unwrap();
            clients.push(socket);
        }
        assert_eq!(
            *r.fake.guests.lock().unwrap(),
            vec![
                (r.id.clone(), "Device".into()),
                (
                    second_id.clone(),
                    if distinct {
                        "Tablet".into()
                    } else {
                        "Device".into()
                    }
                ),
            ]
        );
        let live = r.status.snapshot().unwrap();
        assert_eq!(live.connections, 2);
        assert_eq!(live.keys.len(), if distinct { 2 } else { 1 });
        assert!(live
            .keys
            .iter()
            .all(|key| key.name == if key.id == r.id { "Device" } else { "Tablet" }));
        assert_eq!(live.keys.iter().map(|key| key.active_requests).sum::<usize>(), 2);
        assert_eq!(
            live.keys
                .iter()
                .map(|key| key.id.as_str())
                .collect::<std::collections::BTreeSet<_>>(),
            [&r.id[..], &second_id[..]].into_iter().collect()
        );
        drop(clients.remove(0));
        tokio::time::timeout(Duration::from_secs(2), r.fake.cancelled.notified())
            .await
            .unwrap();
        assert_eq!(r.status.snapshot().unwrap(), live);
        r.fake.release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), async {
            while r.status.snapshot().unwrap().connections != 1 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        let remaining = r.status.snapshot().unwrap();
        assert_eq!(remaining.keys.len(), 1);
        assert_eq!(&remaining.keys[0].id, second_id);
        assert_eq!(remaining.keys[0].active_requests, 1);
        drop(clients);
        tokio::time::timeout(Duration::from_secs(2), r.fake.cancelled.notified())
            .await
            .unwrap();
        r.stop.cancel();
        r.fake.release.notify_one();
        r.close().await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn owner_status_never_attributes_a_key_before_optional_peer_authorization() {
    let r = Running::start(
        Config {
            same_owner: true,
            ..config()
        },
        limits(),
        false,
    )
    .await;
    r.network.hold_check.store(true, Ordering::SeqCst);
    r.network.owner.store(false, Ordering::SeqCst);
    let address = r.address;
    let request = format!("{}\r\n", r.header("GET", "/v1/models", ""));
    let client = tokio::spawn(exchange(address, request));
    tokio::time::timeout(Duration::from_secs(2), r.network.check_started.notified())
        .await
        .unwrap();
    let pending = r.status.snapshot().unwrap();
    assert_eq!(pending.connections, 1);
    assert!(pending.keys.is_empty());
    r.network.check_release.notify_one();
    assert!(client.await.unwrap().starts_with("HTTP/1.1 403"));
    assert!(r.status.snapshot().unwrap().keys.is_empty());
    r.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn revoked_key_between_auth_and_admission_never_reaches_backend_metadata() {
    let r = Running::start(
        Config {
            same_owner: true,
            read_timeout: Duration::from_secs(2),
            ..config()
        },
        limits(),
        false,
    )
    .await;
    r.network.hold_check.store(true, Ordering::SeqCst);
    let body = body("local-model").to_string();
    let header = r.header(
        "POST",
        "/v1/chat/completions",
        &format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        ),
    );
    let address = r.address;
    let client = tokio::spawn(exchange(address, format!("{header}\r\n{body}")));
    tokio::time::timeout(Duration::from_secs(2), r.network.check_started.notified())
        .await
        .unwrap();
    r.keys.revoke(&r.id).unwrap();
    r.network.check_release.notify_one();
    assert!(client.await.unwrap().starts_with("HTTP/1.1 401"));
    assert_eq!(r.fake.calls.load(Ordering::SeqCst), 0);
    assert!(r.fake.guests.lock().unwrap().is_empty());
    r.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn successful_stream_retries_contended_settlement_and_finishes() {
    struct Locked {
        fake: Fake,
        path: std::path::PathBuf,
        lock: Mutex<Option<nix::fcntl::Flock<std::fs::File>>>,
        ready: Notify,
    }
    impl Answer for Locked {
        fn models(&self) -> &[Model] {
            self.fake.models()
        }
        async fn chat(
            &self,
            request: Request,
            guest: Guest<'_>,
            output: Output,
            cancel: CancelToken,
        ) -> Result<Completed, Failure> {
            let file = std::fs::File::open(&self.path).unwrap();
            *self.lock.lock().unwrap() =
                Some(nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusive).unwrap());
            let result = self.fake.chat(request, guest, output, cancel).await;
            self.ready.notify_one();
            result
        }
    }
    for release_lock in [true, false] {
        let d = Directory::new();
        let keys = Arc::new(d.store());
        let key = keys.create("Device", limits()).unwrap();
        let backend = Arc::new(Locked {
            fake: Fake::new(false),
            path: d.0.join("share-keys.lock"),
            lock: Mutex::new(None),
            ready: Notify::new(),
        });
        let server = Server::bind(
            keys.clone(),
            backend.clone(),
            Arc::new(Network::local()),
            config(),
        )
        .await
        .unwrap();
        let address = server.address().unwrap();
        let stop = CancelToken::new();
        let serving = tokio::spawn(server.serve(stop.clone()));
        let payload = body("local-model").to_string();
        let request = format!("POST /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n{}", key.bearer.expose(), payload.len(), payload);
        let client = tokio::spawn(exchange(address, request));
        backend.ready.notified().await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!client.is_finished());
        if release_lock {
            backend.lock.lock().unwrap().take();
        }
        let reply = client.await.unwrap();
        assert!(reply.contains("data: [DONE]"), "{reply}");
        assert!(reply.contains("total_tokens\":8"), "{reply}");
        let day = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            / 86400;
        backend.lock.lock().unwrap().take();
        assert_eq!(
            keys.guest_usage(day, &key.info.id).unwrap(),
            (1, if release_lock { 8 } else { 110 })
        );
        stop.cancel();
        serving.await.unwrap().unwrap();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn authenticated_native_bridge_streams_only_guest_text_and_settles_usage() {
    use oxa_proto::{AssistantTurn, ChatRequest, StreamEvent, ThinkingLevel, WireMsg};
    use oxc_gateway::share::native::Native;

    let directory = Directory::new();
    let keys = Arc::new(directory.store());
    let key = keys.create("Device", limits()).unwrap();
    let answer = Native::new(
        vec![Model {
            alias: "local-model".into(),
            context_window: 100,
            max_output: 10,
            access: Access::Local,
        }],
        |request: &ChatRequest<'_>, _: &CancelToken, on: &mut dyn FnMut(StreamEvent)| {
            assert_eq!(request.model, "local-model");
            assert_eq!(request.prefix.tools_json, "[]");
            assert_eq!(request.prefix.system_text(), "Synthetic guest instructions");
            assert_eq!(request.messages, &[WireMsg::user("Synthetic request")]);
            assert_eq!(request.max_tokens, 10);
            on(StreamEvent::Reasoning("Synthetic private reasoning".into()));
            on(StreamEvent::Text("Synthetic native reply".into()));
            Ok(AssistantTurn {
                msg: WireMsg::assistant("Synthetic native reply"),
                finish: oxa_proto::Finish::Stop,
                usage: oxa_proto::Usage {
                    prompt: 3,
                    completion: 2,
                    ..oxa_proto::Usage::default()
                },
                timings: None,
                model: "local-model".into(),
                level: ThinkingLevel::Off,
            })
        },
    )
    .unwrap();
    #[cfg(feature = "ledger")]
    let ledger_path = directory.0.join("request-ledger");
    #[cfg(feature = "ledger")]
    let answer = answer.with_ledger(oxc_gateway::ledger::Ledger::open(&ledger_path).unwrap());
    let server = Server::bind(
        keys.clone(),
        Arc::new(answer),
        Arc::new(Network::local()),
        config(),
    )
    .await
    .unwrap();
    let address = server.address().unwrap();
    let stop = CancelToken::new();
    let task = tokio::spawn(server.serve(stop.clone()));
    let mut request = body("local-model");
    request["messages"].as_array_mut().unwrap().insert(
        0,
        json!({"role":"system","content":"Synthetic guest instructions"}),
    );
    let request = request.to_string();
    let response = exchange(
        address,
        format!(
            "POST /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{request}",
            key.bearer.expose(), request.len()
        ),
    ).await;
    assert!(response.starts_with("HTTP/1.1 200"));
    assert!(response.contains("Synthetic native reply"));
    assert!(response.contains("data: [DONE]"));
    assert!(response.contains("\"prompt_tokens\":3"));
    assert!(response.contains("\"completion_tokens\":2"));
    assert!(!response.contains("Synthetic private reasoning"));
    assert!(!response.contains(key.bearer.expose()));
    #[cfg(feature = "ledger")]
    {
        let paths: Vec<_> = std::fs::read_dir(ledger_path)
            .unwrap()
            .map(|item| item.unwrap().path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "jsonl"))
            .collect();
        assert_eq!(paths.len(), 1);
        let row = std::fs::read_to_string(&paths[0]).unwrap();
        for private in [
            key.bearer.expose(),
            "Device",
            "Synthetic request",
            "Synthetic native reply",
            "Synthetic private reasoning",
        ] {
            assert!(!row.contains(private));
        }
        let row: Value = serde_json::from_str(&row).unwrap();
        assert_eq!(row["model"], "local-model");
        assert_eq!(row["origin"], "api");
        assert_eq!(row["remote"], false);
        assert_eq!(row["guest_key"].as_str().unwrap().len(), 32);
        assert_eq!(row["prompt_tokens"], 3);
        assert_eq!(row["completion_tokens"], 2);
    }

    stop.cancel();
    tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let day = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        / 86400;
    assert_eq!(keys.guest_usage(day, &key.info.id).unwrap(), (1, 5));
}
