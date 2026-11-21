//! The lead web page attached to a running owner session (#2075), against the real binary.
//! The test process plays the TUI: it creates the session in another project directory, holds
//! its writer lock and consumes the inbox with `Inbox::watch`, the consumer the TUI uses.
use oxa_config::Dirs;
use oxa_fake::TestEnv;
use oxa_proto::{Entry, EntryBody, ToolCall, WireMsg};
use oxa_session::{new_entry_id, FsStore, Inbox, SessionRef, SessionStore};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::fs::{MetadataExt, PermissionsExt},
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

const AUTHORITY: &str = "lead.example.invalid";
const OWNER: &str = "Tailscale-User-Login: owner@example.invalid\r\n";
const GUEST: &str = "Tailscale-User-Login: guest@example.invalid\r\n";
const SECRET: &str = "SYNTHETICSECRETVALUE";

struct Running(Child);
impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Fixture {
    test: TestEnv,
    dirs: Dirs,
    port: u16,
    session: SessionRef,
    // Holding the store keeps the session's writer lock, as a running TUI does.
    store: FsStore,
}

fn entry(body: EntryBody) -> Entry {
    Entry {
        id: new_entry_id(&|_| false),
        parent: None,
        ts_ms: oxa_proto::time::now_ms(),
        body,
    }
}

fn message(wire: WireMsg) -> Entry {
    entry(EntryBody::Msg {
        wire,
        parts: Vec::new(),
        tool: None,
    })
}

fn private(path: &std::path::Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

impl Fixture {
    /// A session in `other-project` (not the service's directory) with a user message, an
    /// assistant message and a tool call that carries a synthetic credential.
    fn new() -> Self {
        let test = TestEnv::new();
        let dirs = Dirs::under(test.root());
        private(&dirs.agent_dir, 0o700);
        let other = test.root().join("other-project");
        std::fs::create_dir(&other).unwrap();
        let store = FsStore::new(dirs.sessions_dir.clone());
        let session = store.create(&other.canonicalize().unwrap(), None, None).unwrap();
        store.acquire(&session).unwrap();
        store.commit_lock();
        store
            .append(
                &session,
                &message(WireMsg::user("what is the state of the board")),
            )
            .unwrap();
        store
            .append(
                &session,
                &message(WireMsg::assistant("the board has three open items")),
            )
            .unwrap();
        let mut call = WireMsg::assistant("running a check");
        call.tool_calls.push(ToolCall {
            id: "call-1".into(),
            name: "bash".into(),
            args_raw: format!(r#"{{"command":"curl -H x token={SECRET} && ls"}}"#),
        });
        store.append(&session, &message(call)).unwrap();
        let reservation = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = reservation.local_addr().unwrap().port();
        drop(reservation);
        let fixture = Self {
            test,
            dirs,
            port,
            session,
            store,
        };
        fixture.settings(Some(&fixture.session.id.clone()), None);
        fixture
    }

    /// Another session of the same user in its own project directory, holding its writer lock
    /// when `hold` (the returned store keeps it, as a running oxa does).
    fn extra(&self, project: &str, text: &str, hold: bool) -> (SessionRef, FsStore) {
        let directory = self.test.root().join(project);
        std::fs::create_dir(&directory).unwrap();
        let store = FsStore::new(self.dirs.sessions_dir.clone());
        let session = store
            .create(&directory.canonicalize().unwrap(), None, None)
            .unwrap();
        if hold {
            store.acquire(&session).unwrap();
            store.commit_lock();
        }
        store.append(&session, &message(WireMsg::user(text))).unwrap();
        (session, store)
    }

    fn uid(&self) -> u32 {
        std::fs::metadata(self.test.root()).unwrap().uid()
    }

    fn settings(&self, attach: Option<&str>, dedicated: Option<&str>) {
        let mut lead = serde_json::json!({
            "serve": {"enabled": true, "port": self.port, "identity": "tailscale", "owner_chat": true,
                "allowed_identities": ["owner@example.invalid"],
                "proxy": {"uid": self.uid(), "authority": AUTHORITY}}
        });
        if let Some(id) = attach {
            lead["attachSessionId"] = id.into();
        }
        if let Some(id) = dedicated {
            lead["sessionId"] = id.into();
        }
        let path = self.test.write(
            "home/.oxa/agent/settings.json",
            &serde_json::json!({"oxa": {"lead": lead}}).to_string(),
        );
        private(&path, 0o600);
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_oxa"));
        command
            .env_clear()
            .envs(self.test.envs("http://127.0.0.1:1"))
            .env("TZ", "UTC0")
            .current_dir(self.test.work())
            .stdin(Stdio::null())
            .args(["lead", "serve"])
            .stdout(Stdio::piped())
            .stderr(std::fs::File::create(self.test.root().join("lead.stderr")).unwrap());
        command
    }

    fn start(&self) -> Running {
        let mut child = Running(self.command().spawn().unwrap());
        let stdout = child.0.stdout.take().unwrap();
        let (send, receive) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let line = line.unwrap_or_default();
                let found = line.contains("listening");
                let _ = send.send(line);
                if found {
                    break;
                }
            }
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let line = receive
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("lead serve did not report its listener");
            if line.contains("listening") {
                return child;
            }
        }
    }

    /// The service refuses to start: it exits unsuccessfully and never listens.
    fn refuses_to_start(&self) {
        let mut child = Running(self.command().spawn().unwrap());
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                assert!(!status.success());
                let mut out = String::new();
                child.0.stdout.take().unwrap().read_to_string(&mut out).unwrap();
                assert!(!out.contains("listening"), "{out}");
                return;
            }
            assert!(Instant::now() < deadline, "the service kept running");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn get(&self, path: &str, identity: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: {AUTHORITY}\r\n{identity}\r\n"
        )
        .unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).unwrap();
        reply
    }

    fn post(&self, identity: &str, body: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        write!(
            stream,
            "POST /chat HTTP/1.1\r\nHost: {AUTHORITY}\r\nOrigin: https://{AUTHORITY}\r\nSec-Fetch-Site: same-origin\r\nContent-Type: application/x-www-form-urlencoded\r\n{identity}Content-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut input = BufReader::new(stream);
        let mut reply = String::new();
        while !reply.ends_with("\r\n\r\n") {
            assert!(input.read_line(&mut reply).unwrap() > 0);
        }
        reply
    }

    fn nonce(&self) -> String {
        let page = self.get("/chat", OWNER);
        page.split("name=csrf value=\"")
            .nth(1)
            .expect("the attached page has its message form")
            .split('"')
            .next()
            .unwrap()
            .to_owned()
    }

    /// What the session's own inbox consumer receives within `wait`.
    fn inbox(&self, wait: Duration) -> Vec<String> {
        self.inbox_of(&self.session, wait)
    }

    fn inbox_of(&self, session: &SessionRef, wait: Duration) -> Vec<String> {
        let (send, receive) = mpsc::channel();
        let _watch = Inbox::watch(&self.dirs.agent_dir, &session.id, move |text| {
            let _ = send.send(text);
            true
        })
        .unwrap();
        let mut got = Vec::new();
        while let Ok(text) = receive.recv_timeout(wait) {
            got.push(text);
        }
        got
    }
}

#[test]
fn page_shows_the_running_session_and_sends_through_its_inbox() {
    let fixture = Fixture::new();
    // No dedicated Lead session exists: the service starts on the attached one alone.
    let _service = fixture.start();
    let page = fixture.get("/chat", OWNER);
    assert!(page.starts_with("HTTP/1.1 200"), "{page}");
    for text in [
        "what is the state of the board",
        "the board has three open items",
        "running a check",
    ] {
        assert!(page.contains(text), "{text}: {page}");
    }
    assert!(page.contains("Status: running"), "{page}");
    assert!(!page.contains(SECRET), "credential leaked: {page}");
    assert!(page.contains(&fixture.session.id[..8]));

    // A guest never sees the transcript.
    assert!(fixture.get("/chat", GUEST).starts_with("HTTP/1.1 403"));

    // A new entry the session appends shows up on the next load.
    fixture
        .store
        .append(
            &fixture.session,
            &message(WireMsg::assistant("a fresh answer arrived")),
        )
        .unwrap();
    assert!(fixture.get("/chat", OWNER).contains("a fresh answer arrived"));
    let before = std::fs::read(&fixture.session.path).unwrap();

    // The owner's message reaches the session's inbox exactly once, while the session still
    // holds its writer lock.
    let reply = fixture.post(
        OWNER,
        &format!("csrf={}&message=continue-with-the-review-please", fixture.nonce()),
    );
    assert!(reply.starts_with("HTTP/1.1 303"), "{reply}");
    assert!(reply.contains("Location: /chat\r\n"), "{reply}");
    assert!(fixture.get("/chat", OWNER).contains("Sent."));
    assert_eq!(
        fixture.inbox(Duration::from_secs(3)),
        vec!["continue-with-the-review-please".to_owned()]
    );

    // Reading and sending changed nothing in the session file.
    assert_eq!(std::fs::read(&fixture.session.path).unwrap(), before);
}

#[test]
fn bad_posts_and_unsafe_selections_deliver_nothing() {
    let fixture = Fixture::new();
    {
        let _service = fixture.start();
        let nonce = fixture.nonce();
        let wrong_csrf = fixture.post(OWNER, "csrf=wrong&message=must-not-arrive");
        assert!(wrong_csrf.starts_with("HTTP/1.1 403"), "{wrong_csrf}");
        let guest = fixture.post(GUEST, &format!("csrf={nonce}&message=must-not-arrive"));
        assert!(guest.starts_with("HTTP/1.1 403"), "{guest}");
        let journal = std::fs::read_to_string(fixture.test.root().join("lead.stderr")).unwrap();
        assert!(!journal.contains("must-not-arrive") && !journal.contains(&nonce));
        assert!(fixture.inbox(Duration::from_millis(700)).is_empty());
    }

    // A session file readable by group or others is refused at start.
    private(&fixture.session.path, 0o640);
    fixture.refuses_to_start();
    private(&fixture.session.path, 0o600);

    // An id chosen only in a project settings file selects nothing: the page starts on the
    // list alone, shows no transcript and delivers no message.
    fixture.settings(None, None);
    fixture.test.write(
        "work/.oxa/settings.json",
        &serde_json::json!({"oxa": {"lead": {"attachSessionId": fixture.session.id}}}).to_string(),
    );
    let _service = fixture.start();
    let page = fixture.get("/chat", OWNER);
    assert!(page.contains("No session is attached"), "{page}");
    assert!(!page.contains("what is the state of the board"), "{page}");
    let reply = fixture.post(
        OWNER,
        &format!("csrf={}&message=must-not-arrive", fixture.nonce()),
    );
    assert!(reply.starts_with("HTTP/1.1 503"), "{reply}");
    assert!(fixture.inbox(Duration::from_millis(700)).is_empty());
}

#[test]
fn owner_lists_running_sessions_switches_and_returns() {
    let fixture = Fixture::new();
    let project = fixture.test.work().canonicalize().unwrap();
    let lead = oxa_cli::LeadOwnerBinding::create(&fixture.dirs, &project, fixture.uid()).unwrap();
    fixture.settings(None, Some(&lead));
    let first = fixture.session.clone();
    let (second, second_lock) = fixture.extra("second-project", "second session question", true);
    let (idle, _idle_store) = fixture.extra("idle-project", "idle session question", false);
    let _service = fixture.start();
    let short = |session: &SessionRef| session.id[..8].to_owned();

    // The service starts on the dedicated Lead chat and lists exactly the running sessions.
    let page = fixture.get("/chat", OWNER);
    // The page's own title; its body is the thread, not a second "Chat with Lead" section.
    assert!(page.contains(">Chat with lead</h1>"), "{page}");
    assert!(
        page.contains(&short(&first)) && page.contains(&short(&second)),
        "{page}"
    );
    assert!(
        page.contains("second-project") && !page.contains(&short(&idle)),
        "{page}"
    );
    assert!(!page.contains("Back to Lead chat"), "{page}");
    assert!(fixture.get("/chat", GUEST).starts_with("HTTP/1.1 403"));

    let attach = |identity: &str, value: &str| {
        fixture.post(identity, &format!("csrf={}&attach={value}", fixture.nonce()))
    };
    // Choosing the second session shows its transcript, not the first one's, and its inbox
    // receives the owner's message.
    let reply = attach(OWNER, &second.id);
    assert!(reply.starts_with("HTTP/1.1 303"), "{reply}");
    let page = fixture.get("/chat", OWNER);
    assert!(page.contains("second session question"), "{page}");
    assert!(!page.contains("what is the state of the board"), "{page}");
    assert!(
        page.contains("Back to Lead chat") && page.contains("Status: running"),
        "{page}"
    );
    let reply = fixture.post(
        OWNER,
        &format!("csrf={}&message=for-the-second-session", fixture.nonce()),
    );
    assert!(reply.starts_with("HTTP/1.1 303"), "{reply}");
    assert_eq!(
        fixture.inbox_of(&second, Duration::from_secs(3)),
        vec!["for-the-second-session".to_owned()]
    );
    assert!(fixture.inbox_of(&first, Duration::from_millis(500)).is_empty());

    // A session that is not running, an unknown id, a path-like value, a bad CSRF and a
    // non-owner all leave the target as it was.
    let nonce = fixture.nonce();
    assert!(attach(OWNER, &idle.id).starts_with("HTTP/1.1 404"));
    assert!(attach(OWNER, "unknown-session-id").starts_with("HTTP/1.1 404"));
    assert!(attach(OWNER, "..%2F..%2Fetc").starts_with("HTTP/1.1 403"));
    let bad_csrf = fixture.post(OWNER, &format!("csrf=wrong&attach={}", first.id));
    assert!(bad_csrf.starts_with("HTTP/1.1 403"), "{bad_csrf}");
    let guest = fixture.post(GUEST, &format!("csrf={nonce}&attach={}", first.id));
    assert!(guest.starts_with("HTTP/1.1 403"), "{guest}");
    assert!(fixture.get("/chat", OWNER).contains("second session question"));
    assert!(fixture.inbox_of(&first, Duration::from_millis(500)).is_empty());
    assert!(fixture.inbox_of(&idle, Duration::from_millis(500)).is_empty());

    // When the attached session stops, the page says so and a message waits in its inbox.
    drop(second_lock);
    let page = fixture.get("/chat", OWNER);
    assert!(page.contains("not running"), "{page}");
    let reply = fixture.post(
        OWNER,
        &format!("csrf={}&message=queued-for-later", fixture.nonce()),
    );
    assert!(reply.starts_with("HTTP/1.1 303"), "{reply}");
    assert_eq!(
        fixture.inbox_of(&second, Duration::from_secs(3)),
        vec!["queued-for-later".to_owned()]
    );

    // An empty choice goes back to the dedicated Lead chat.
    assert!(attach(OWNER, "").starts_with("HTTP/1.1 303"));
    let page = fixture.get("/chat", OWNER);
    assert!(
        page.contains("Chat with Lead") && !page.contains("second session question"),
        "{page}"
    );
}
