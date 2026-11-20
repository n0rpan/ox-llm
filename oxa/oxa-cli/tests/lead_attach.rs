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
        fixture.settings(Some(&fixture.session.id.clone()));
        fixture
    }

    fn uid(&self) -> u32 {
        std::fs::metadata(self.test.root()).unwrap().uid()
    }

    fn settings(&self, attach: Option<&str>) {
        let mut lead = serde_json::json!({
            "serve": {"enabled": true, "port": self.port, "identity": "tailscale", "owner_chat": true,
                "allowed_identities": ["owner@example.invalid"],
                "proxy": {"uid": self.uid(), "authority": AUTHORITY}}
        });
        if let Some(id) = attach {
            lead["attachSessionId"] = id.into();
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
        let (send, receive) = mpsc::channel();
        let _watch = Inbox::watch(&self.dirs.agent_dir, &self.session.id, move |text| {
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

    // An id chosen only in a project settings file selects nothing.
    fixture.settings(None);
    fixture.test.write(
        "work/.oxa/settings.json",
        &serde_json::json!({"oxa": {"lead": {"attachSessionId": fixture.session.id}}}).to_string(),
    );
    fixture.refuses_to_start();
    assert!(fixture.inbox(Duration::from_millis(700)).is_empty());
}
