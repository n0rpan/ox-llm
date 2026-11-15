use oxa_a2a::{
    loopback::Limits,
    server::{error, Backend, Server, ServerLimits},
    wire::*,
};
use std::collections::BTreeMap;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

struct Fixture {
    task: Mutex<Task>,
    sends: AtomicUsize,
    stopped: AtomicBool,
    confirm_cancel: AtomicBool,
}

impl Fixture {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            task: Mutex::new(Task {
                id: "task".into(),
                context_id: "context".into(),
                status: TaskStatus {
                    state: TaskState::Working,
                    message: None,
                    timestamp: None,
                },
                artifacts: vec![],
                history: vec![],
            }),
            sends: AtomicUsize::new(0),
            stopped: AtomicBool::new(false),
            confirm_cancel: AtomicBool::new(true),
        })
    }
}

impl Backend for Fixture {
    fn card(&self, endpoint: &str) -> AgentCard {
        AgentCard {
            protocol_version: PROTOCOL_VERSION.into(),
            name: "synthetic".into(),
            description: "synthetic test agent".into(),
            url: endpoint.into(),
            version: "1".into(),
            preferred_transport: "JSONRPC".into(),
            capabilities: Capabilities::default(),
            default_input_modes: vec!["text/plain".into()],
            default_output_modes: vec!["text/plain".into()],
            skills: vec![],
            security_schemes: BTreeMap::new(),
            security: vec![],
        }
    }
    fn send(&self, _: SendParams) -> Result<Event, RpcError> {
        self.sends.fetch_add(1, Ordering::Relaxed);
        Ok(Event::Task(self.task.lock().unwrap().clone()))
    }
    fn get(&self, params: TaskParams) -> Result<Task, RpcError> {
        let task = self.task.lock().unwrap();
        if params.id != task.id {
            return Err(error(-32001, "Task not found"));
        }
        Ok(task.clone())
    }
    fn cancel(&self, id: &str) -> Result<Task, RpcError> {
        let mut task = self.task.lock().unwrap();
        if id != task.id {
            return Err(error(-32001, "Task not found"));
        }
        if self.confirm_cancel.load(Ordering::Acquire) {
            task.status.state = TaskState::Canceled;
        }
        Ok(task.clone())
    }
    fn shutdown(&self) {
        self.stopped.store(true, Ordering::Release);
    }
}

#[test]
fn cancellation_acknowledgment_without_terminal_state_is_not_success() {
    let backend = Fixture::new();
    backend.confirm_cancel.store(false, Ordering::Release);
    let server = Server::start(backend, ServerLimits::default()).unwrap();
    let mut client = server.client(Limits::default()).unwrap();
    assert!(client.cancel("task").unwrap_err().to_string().contains("-32603"));
    assert_eq!(client.get("task").unwrap().status.state, TaskState::Working);
}

fn prompt() -> SendParams {
    SendParams {
        message: MessageObject::Message(Message {
            message_id: "synthetic".into(),
            role: Role::User,
            parts: vec![Part::Text {
                text: "synthetic".into(),
            }],
            task_id: None,
            context_id: None,
        }),
    }
}

#[test]
fn full_stream_capacity_preserves_query_and_cancel_access() {
    let backend = Fixture::new();
    let server = Server::start(
        backend.clone(),
        ServerLimits {
            connections: 2,
            ..ServerLimits::default()
        },
    )
    .unwrap();
    let mut client = server.client(Limits::default()).unwrap();
    let mut stream = client.stream(prompt()).unwrap();
    assert!(matches!(stream.next_event().unwrap(), Some(Event::Task(_))));
    for _ in 0..2 {
        let mut extra = server.client(Limits::default()).unwrap();
        let mut rejected = extra.stream(prompt()).unwrap();
        assert!(rejected.next_event().unwrap_err().to_string().contains("-32004"));
        // Let the completed HTTP handler release its connection slot. The
        // original SSE handler stays live throughout both rejection attempts.
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        backend.sends.load(Ordering::Relaxed),
        1,
        "rejected streams launch no tasks"
    );
    let mut controller = server.client(Limits::default()).unwrap();
    assert_eq!(controller.get("task").unwrap().status.state, TaskState::Working);
    std::thread::sleep(Duration::from_millis(10));
    assert_eq!(
        controller.cancel("task").unwrap().status.state,
        TaskState::Canceled
    );
    let mut terminal = false;
    while let Some(event) = stream.next_event().unwrap() {
        if let Event::Status(status) = event {
            terminal |= status.final_event && status.status.state == TaskState::Canceled;
        }
    }
    assert!(terminal);
}

#[test]
fn shutdown_interrupts_stalled_connections_and_joins_handlers() {
    let backend = Fixture::new();
    let server = Server::start(backend.clone(), ServerLimits::default()).unwrap();
    let stalled = std::net::TcpStream::connect(server.address()).unwrap();
    std::thread::sleep(Duration::from_millis(20));
    let started = Instant::now();
    drop(server);
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(backend.stopped.load(Ordering::Acquire));
    drop(stalled);
}

#[test]
fn authentication_errors_and_input_limits_do_not_reach_backend() {
    let backend = Fixture::new();
    let server = Server::start(
        backend.clone(),
        ServerLimits {
            body_bytes: 512,
            event_bytes: 512,
            ..ServerLimits::default()
        },
    )
    .unwrap();
    let mut client = server.client(Limits::default()).unwrap();
    assert!(client.call("message/send",serde_json::json!({"message":{"kind":"message","messageId":"bad","role":"user","parts":[{"kind":"file","file":{"uri":"file:///private"}}]}})).unwrap_err().to_string().contains("-32602"));
    let MessageObject::Message(mut message) = prompt().message;
    message.parts = vec![Part::Text {
        text: "x".repeat(1024),
    }];
    assert!(client
        .send(SendParams {
            message: MessageObject::Message(message)
        })
        .is_err());
    let mut unauthenticated = std::net::TcpStream::connect(server.address()).unwrap();
    use std::io::{Read, Write};
    unauthenticated
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    unauthenticated
        .write_all(b"GET /.well-known/agent-card.json HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap();
    let mut reply = String::new();
    unauthenticated.read_to_string(&mut reply).unwrap();
    assert!(reply.starts_with("HTTP/1.1 401"));
    assert_eq!(backend.sends.load(Ordering::Relaxed), 0);
}
