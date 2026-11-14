//! Private Unix-socket attachment to one running goal owner, never a second runtime.
//! One bounded JSON line/request per connection; slow clients never hold owner locks.
use crate::goal_session::{Control, Event};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{SyncSender, TrySendError},
    Arc, Mutex,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const REQUEST_BYTES: usize = 32 * 1024;
const HISTORY_BYTES: usize = 512 * 1024;
const HISTORY_EVENTS: usize = 128;
const CLIENTS: usize = 8;
const CLIENT_TIME: Duration = Duration::from_secs(2);

#[derive(Deserialize)]
#[serde(tag = "method", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Status {
        id: String,
    },
    Events {
        id: String,
        after: u64,
    },
    Pause {
        id: String,
        revision: u64,
    },
    Resume {
        id: String,
        revision: u64,
    },
    Steer {
        id: String,
        revision: u64,
        text: String,
    },
    Cancel {
        id: String,
        revision: u64,
    },
    SetGoal {
        id: String,
        revision: u64,
        text: String,
        budget: Option<crate::goals::Budget>,
    },
    Close {
        id: String,
    },
}
impl Request {
    fn id(&self) -> &str {
        match self {
            Self::Status { id }
            | Self::Events { id, .. }
            | Self::Pause { id, .. }
            | Self::Resume { id, .. }
            | Self::Steer { id, .. }
            | Self::SetGoal { id, .. }
            | Self::Close { id }
            | Self::Cancel { id, .. } => id,
        }
    }
}
#[derive(Default)]
struct State {
    latest: Option<Value>,
    sequence: u64,
    events: VecDeque<(u64, Value, usize)>,
    bytes: usize,
    terminal: bool,
    session: Option<Value>,
}
/// The session owner publishes without waiting for socket clients. No raw payload is persisted.
#[derive(Clone)]
pub struct Publisher(Arc<Mutex<State>>);
impl Publisher {
    pub fn publish(&self, event: &Event) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.latest = Some(json!({"goal":event.goal,"kind":event.kind,"session":state.session}));
        if event.kind == "goal_status" {
            return;
        }
        // A bounded preview is explicit; large replies remain available in the final report.
        let reply = event.reply.as_deref().map(|text| {
            let mut end = text.len().min(16 * 1024);
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            json!({"text":&text[..end],"truncated":end != text.len()})
        });
        let value = json!({"kind":event.kind,"goal":event.goal,"reply":reply,"request_id":event.request_id,"session":state.session});
        let bytes = value.to_string().len();
        state.sequence += 1;
        let seq = state.sequence;
        state.events.push_back((seq, value, bytes));
        state.bytes += bytes;
        while state.events.len() > HISTORY_EVENTS || state.bytes > HISTORY_BYTES {
            if let Some((_, _, bytes)) = state.events.pop_front() {
                state.bytes -= bytes;
            }
        }
    }
    pub fn bind_session(&self, id: &str, model: &str, mode: &str, busy: bool) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).session =
            Some(json!({"id":id,"model":model,"mode":mode,"busy":busy}));
    }
}
/// Lifetime is owned by the foreground session; drop joins the bounded I/O loop.
/// Socket is mode 0600 in an owner-only directory, granting local operator authority.
pub struct Server {
    publisher: Publisher,
    stop: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    _socket: SocketPath,
}
struct SocketPath {
    path: PathBuf,
    identity: (u64, u64),
}
impl Drop for SocketPath {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.path).is_ok_and(|m| (m.dev(), m.ino()) == self.identity) {
            let _ = fs::remove_file(&self.path);
        }
    }
}
impl Server {
    pub fn start(directory: &Path, controls: SyncSender<Control>) -> io::Result<Self> {
        let metadata = fs::symlink_metadata(directory)?;
        if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0
        {
            return Err(io::Error::other("control directory must be private and owned"));
        }
        let path = directory.join("session.sock");
        // Never remove or adopt an existing attachment, even a stale socket.
        let listener = UnixListener::bind(&path)?;
        let metadata = fs::symlink_metadata(&path)?;
        let socket = SocketPath {
            path: path.clone(),
            identity: (metadata.dev(), metadata.ino()),
        };
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        let publisher = Publisher(Arc::new(Mutex::new(State::default())));
        let stop = Arc::new(AtomicBool::new(false));
        let shutdown = stop.clone();
        let closed = Arc::new(AtomicBool::new(false));
        let close_owner = closed.clone();
        let state = publisher.clone();
        let thread = thread::Builder::new()
            .name("oxa-session-control".into())
            .spawn(move || serve(listener, controls, state, shutdown, close_owner))?;
        Ok(Self {
            publisher,
            stop,
            closed,
            thread: Some(thread),
            _socket: socket,
        })
    }
    pub fn publisher(&self) -> Publisher {
        self.publisher.clone()
    }
    /// Called only after the native owner returned a post-cleanup report. Keep
    /// terminal replies/events available until an explicit client close.
    pub fn complete(&self) {
        self.publisher
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .terminal = true;
    }
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
    #[cfg(feature = "interactive-control")]
    pub(crate) fn stop_handle(&self) -> Arc<AtomicBool> {
        self.stop.clone()
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
struct Client {
    stream: UnixStream,
    input: Vec<u8>,
    output: Option<Vec<u8>>,
    written: usize,
    started: Instant,
    close_owner: bool,
}
fn serve(
    listener: UnixListener,
    controls: SyncSender<Control>,
    publisher: Publisher,
    stop: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
) {
    let mut clients: Vec<Client> = Vec::new();
    while !stop.load(Ordering::Acquire) {
        for _ in 0..CLIENTS {
            if clients.len() == CLIENTS {
                break;
            }
            match listener.accept() {
                Ok((stream, _)) => {
                    if stream.set_nonblocking(true).is_ok() {
                        clients.push(Client {
                            stream,
                            input: Vec::new(),
                            output: None,
                            written: 0,
                            started: Instant::now(),
                            close_owner: false,
                        });
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
        clients.retain_mut(|client| {
            if client.started.elapsed() >= CLIENT_TIME {
                return false;
            }
            if client.output.is_none() {
                let mut bytes = [0; 4096];
                // At most one bounded chunk per client/tick, never monopolize the owner.
                match client.stream.read(&mut bytes) {
                    Ok(0) => return false,
                    Ok(n) => {
                        client.input.extend_from_slice(&bytes[..n]);
                        if client.input.len() > REQUEST_BYTES {
                            return false;
                        }
                        if let Some(end) = client.input.iter().position(|b| *b == b'\n') {
                            let response = handle(&client.input[..end], &controls, &publisher);
                            client.close_owner =
                                response.get("closed").and_then(Value::as_bool) == Some(true);
                            let mut wire = response.to_string().into_bytes();
                            wire.push(b'\n');
                            client.output = Some(wire);
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(_) => return false,
                }
            }
            if let Some(bytes) = client.output.as_ref() {
                match client.stream.write(&bytes[client.written..]) {
                    Ok(0) => return false,
                    Ok(n) => client.written += n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(_) => return false,
                }
                if client.written == bytes.len() {
                    if client.close_owner {
                        closed.store(true, Ordering::Release);
                    }
                    return false;
                }
            }
            true
        });
        thread::sleep(Duration::from_millis(2));
    }
}
fn handle(bytes: &[u8], controls: &SyncSender<Control>, publisher: &Publisher) -> Value {
    let request: Request = match serde_json::from_slice(bytes) {
        Ok(request) => request,
        Err(_) => return json!({"error":"invalid_request"}),
    };
    let id = request.id().to_owned();
    if id.is_empty() || id.len() > 128 || id.chars().any(char::is_control) {
        return json!({"error":"invalid_id"});
    }
    let control = match request {
        Request::Status { .. } => {
            let state = publisher.0.lock().unwrap_or_else(|e| e.into_inner());
            return json!({"id":id,"sequence":state.sequence,"status":state.latest,"runtime_stopped":state.terminal});
        }
        Request::Events { after, .. } => {
            let state = publisher.0.lock().unwrap_or_else(|e| e.into_inner());
            if after > state.sequence {
                return json!({"id":id,"error":"invalid_cursor"});
            }
            let first = state.events.front().map_or(state.sequence + 1, |e| e.0);
            // At most 16 events/page; response stays bounded even with full reply previews.
            let events: Vec<_> = state
                .events
                .iter()
                .filter(|e| e.0 > after)
                .take(16)
                .map(|(seq, event, _)| json!({"sequence":seq,"event":event}))
                .collect();
            let next = events
                .last()
                .and_then(|e| e["sequence"].as_u64())
                .unwrap_or(after);
            return json!({"id":id,"events":events,"next":next,"latest":state.sequence,
                "gap":after.saturating_add(1) < first});
        }
        Request::Pause { revision, .. } => Control::Pause { revision },
        Request::Resume { revision, .. } => Control::Resume { revision },
        Request::Cancel { revision, .. } => Control::Cancel { revision },
        Request::SetGoal {
            revision,
            text,
            budget,
            ..
        } => Control::SetGoal {
            revision,
            text,
            budget,
        },
        Request::Close { .. } => {
            let state = publisher.0.lock().unwrap_or_else(|e| e.into_inner());
            return if state.terminal {
                json!({"id":id,"closed":true})
            } else {
                json!({"id":id,"error":"runtime_still_active"})
            };
        }
        Request::Steer { revision, text, .. } => {
            if text.is_empty() || text.len() > 16 * 1024 || text.contains('\0') {
                return json!({"id":id,"error":"invalid_steer"});
            }
            Control::Steer { revision, text }
        }
    };
    let control = Control::Identified {
        id: id.clone(),
        control: Box::new(control),
    };
    match controls.try_send(control) {
        Ok(()) => json!({"id":id,"queued":true}),
        Err(TrySendError::Full(_)) => json!({"id":id,"error":"control_queue_full"}),
        Err(TrySendError::Disconnected(_)) => json!({"id":id,"error":"session_stopped"}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    fn request(path: &Path, value: Value) -> Value {
        let mut stream = UnixStream::connect(path).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        writeln!(stream, "{value}").unwrap();
        serde_json::from_reader(stream).unwrap()
    }
    #[test]
    fn concurrent_clients_bounded_queue_and_owned_shutdown() {
        let env = oxa_fake::TestEnv::new();
        let path = env.root().join("control");
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        let (tx, rx) = mpsc::sync_channel(1);
        let server = Server::start(&path, tx).unwrap();
        let socket = path.join("session.sock");
        assert_eq!(fs::metadata(&socket).unwrap().mode() & 0o777, 0o600);
        assert!(Server::start(&path, mpsc::sync_channel(1).0).is_err());
        let mut slow = UnixStream::connect(&socket).unwrap();
        slow.write_all(b"{\"method\":").unwrap();
        let a = request(&socket, json!({"method":"pause","id":"one","revision":1}));
        assert_eq!(a["queued"], true);
        let b = request(&socket, json!({"method":"resume","id":"two","revision":1}));
        assert_eq!(b["error"], "control_queue_full");
        assert!(matches!(rx.recv().unwrap(),Control::Identified{id,..} if id == "one"));
        assert!(request(&socket, json!({"method":"status","id":"three"}))["status"].is_null());
        assert_eq!(
            request(&socket, json!({"method":"status","id":"bad","permission":"yolo"}))["error"],
            "invalid_request"
        );
        let began = Instant::now();
        drop(server);
        assert!(began.elapsed() < Duration::from_millis(500));
        assert!(!socket.exists());
        assert!(matches!(
            rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Disconnected)
        ));
        fs::remove_dir(path).unwrap();
    }
    #[test]
    fn history_is_bounded_and_gaps_explicit() {
        let publisher = Publisher(Arc::new(Mutex::new(State::default())));
        let goal = crate::goals::Goal::new(
            "test".into(),
            "goal".into(),
            crate::goals::Budget {
                wall_ms: 10_000,
                tokens: None,
                turns: 1000,
            },
        )
        .unwrap();
        for n in 0..200 {
            publisher.publish(&Event {
                kind: format!("event-{n}"),
                goal: goal.snapshot(),
                reply: Some("é".repeat(20_000)),
                request_id: None,
            });
        }
        let (tx, _rx) = mpsc::sync_channel(1);
        let response = handle(br#"{"method":"events","id":"read","after":0}"#, &tx, &publisher);
        assert_eq!(response["gap"], true);
        assert_eq!(response["events"].as_array().unwrap().len(), 16);
        assert_eq!(response["events"][0]["event"]["reply"]["truncated"], true);
        let state = publisher.0.lock().unwrap();
        assert!(state.bytes <= HISTORY_BYTES && state.events.len() <= HISTORY_EVENTS);
        drop(state);
        assert_eq!(
            handle(
                br#"{"method":"events","id":"future","after":201}"#,
                &tx,
                &publisher
            )["error"],
            "invalid_cursor"
        );
    }
}
