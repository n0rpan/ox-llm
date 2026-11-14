//! Loopback mock agent. No models, paid calls, filesystem tools or ambient keys.
use crate::loopback::{authorized, read_head, Capability, Client, DeadlineStream, Limits};
use crate::wire::*;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{self, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
pub enum Behavior {
    Complete,
    AwaitInput,
}

pub struct MockAgent {
    address: SocketAddr,
    capability: Capability,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl MockAgent {
    pub fn start(behavior: Behavior) -> io::Result<Self> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let capability = Capability::generate()?;
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let auth = capability.clone();
        let handle = thread::Builder::new()
            .name("a2a-loopback-mock".into())
            .spawn(move || {
                let mut state = State {
                    tasks: BTreeMap::new(),
                    next_task: 0,
                    behavior,
                };
                while !stopped.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((socket, _)) => {
                            let _ = serve(socket, address, &auth, &mut state);
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            thread::park_timeout(Duration::from_millis(2))
                        }
                        Err(_) => break,
                    }
                }
            })?;
        Ok(Self {
            address,
            capability,
            stop,
            thread: Some(handle),
        })
    }

    pub fn address(&self) -> SocketAddr {
        self.address
    }
    pub fn client(&self, limits: Limits) -> io::Result<Client> {
        Client::new(self.address, self.capability.clone(), limits)
    }
}

impl Drop for MockAgent {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.thread.take() {
            handle.thread().unpark();
            let _ = handle.join();
        }
    }
}

struct State {
    tasks: BTreeMap<String, Task>,
    next_task: u64,
    behavior: Behavior,
}

enum Reply {
    Json(Value),
    Stream(Vec<Value>),
}

fn serve(socket: TcpStream, address: SocketAddr, auth: &Capability, state: &mut State) -> io::Result<()> {
    let mut reader = BufReader::new(DeadlineStream {
        socket,
        deadline: Instant::now() + Duration::from_millis(500),
    });
    let head = read_head(&mut reader)?;
    if !authorized(&head, auth) {
        return write_http(reader.get_mut(), "401 Unauthorized", "application/json", b"{}");
    }
    if head.first == "GET /.well-known/agent-card.json HTTP/1.1" {
        let card = card(address);
        return write_http(
            reader.get_mut(),
            "200 OK",
            "application/json",
            &serde_json::to_vec(&card)?,
        );
    }
    if head.first != "POST / HTTP/1.1"
        || head.headers.get("content-type").map(String::as_str) != Some("application/json")
    {
        return write_http(reader.get_mut(), "400 Bad Request", "application/json", b"{}");
    }
    let length = head.length(64 * 1024)?;
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes)?;
    let request: Request = match serde_json::from_slice(&bytes) {
        Ok(request) => request,
        Err(_) => {
            return write_http(
                reader.get_mut(),
                "200 OK",
                "application/json",
                &serde_json::to_vec(
                    &json!({"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"Invalid request"}}),
                )?,
            )
        }
    };
    let id = request.id.clone();
    let reply = match dispatch(request, state) {
        Ok(reply) => reply,
        Err((code, message)) => {
            Reply::Json(json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}}))
        }
    };
    match reply {
        Reply::Json(value) => write_http(
            reader.get_mut(),
            "200 OK",
            "application/json",
            &serde_json::to_vec(&value)?,
        ),
        Reply::Stream(values) => {
            let mut body = Vec::new();
            for value in values {
                body.extend_from_slice(b"data: ");
                serde_json::to_writer(&mut body, &value)?;
                body.extend_from_slice(b"\n\n");
            }
            write_http(reader.get_mut(), "200 OK", "text/event-stream", &body)
        }
    }
}

fn write_http(writer: &mut impl Write, status: &str, content_type: &str, bytes: &[u8]) -> io::Result<()> {
    write!(writer, "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", bytes.len())?;
    writer.write_all(bytes)
}

fn card(address: SocketAddr) -> AgentCard {
    AgentCard {
        protocol_version: PROTOCOL_VERSION.into(),
        name: "oxa loopback mock".into(),
        description: "Synthetic model-free task agent".into(),
        url: format!("http://{address}/"),
        version: "1".into(),
        preferred_transport: "JSONRPC".into(),
        capabilities: Capabilities {
            streaming: true,
            ..Capabilities::default()
        },
        default_input_modes: vec!["text/plain".into()],
        default_output_modes: vec!["text/plain".into()],
        skills: vec![Skill {
            id: "echo".into(),
            name: "echo".into(),
            description: "Echo synthetic input".into(),
            tags: vec!["mock".into()],
        }],
        security_schemes: BTreeMap::from([("capability".into(), json!({"type":"http","scheme":"bearer"}))]),
        security: vec![BTreeMap::from([("capability".into(), vec![])])],
    }
}

type ProtocolResult = Result<Reply, (i32, &'static str)>;

fn dispatch(request: Request, state: &mut State) -> ProtocolResult {
    request.validate().map_err(|_| (-32600, "Invalid request"))?;
    let wrap = |event: Event| json!({"jsonrpc":"2.0","id":request.id,"result":event});
    match request.method.as_str() {
        "message/send" | "message/stream" => {
            if request.params.get("configuration").is_some() {
                return Err((-32004, "Send configuration is unsupported by this mock"));
            }
            let SendParams {
                message: MessageObject::Message(message),
            } = serde_json::from_value(request.params).map_err(|_| (-32602, "Invalid params"))?;
            if message.role != Role::User || message.message_id.is_empty() || message.parts.is_empty() {
                return Err((-32602, "Invalid params"));
            }
            let task = if let Some(task_id) = &message.task_id {
                let existing = state.tasks.get(task_id).ok_or((-32001, "Task not found"))?;
                if existing.status.state.is_terminal()
                    || message.context_id.as_ref() != Some(&existing.context_id)
                {
                    return Err((-32602, "Task cannot accept message"));
                }
                let mut task = existing.clone();
                task.history.push(MessageObject::Message(message.clone()));
                task
            } else {
                if message.context_id.is_some() {
                    return Err((-32602, "Unknown context"));
                }
                if state.tasks.len() >= 128 {
                    return Err((-32006, "Mock task limit reached"));
                }
                state.next_task += 1;
                Task {
                    id: format!("task-{}", state.next_task),
                    context_id: format!("context-{}", state.next_task),
                    status: TaskStatus {
                        state: TaskState::Submitted,
                        message: None,
                        timestamp: None,
                    },
                    artifacts: vec![],
                    history: vec![MessageObject::Message(message.clone())],
                }
            };
            let mut task = task;
            // Bound retained histories as well as request bodies.
            if serde_json::to_vec(&task)
                .map_err(|_| (-32603, "Internal error"))?
                .len()
                > 64 * 1024
            {
                return Err((-32602, "Mock task history limit reached"));
            }
            task.status.state = TaskState::Working;
            let initial = wrap(Event::Task(task.clone()));
            task.status.state = match state.behavior {
                Behavior::Complete => TaskState::Completed,
                Behavior::AwaitInput => TaskState::InputRequired,
            };
            let mut events = vec![initial];
            if matches!(state.behavior, Behavior::Complete) {
                let artifact = Artifact {
                    artifact_id: format!("{}-result", task.id),
                    parts: message.parts,
                    name: Some("mock result".into()),
                    description: None,
                };
                events.push(wrap(Event::Artifact(ArtifactUpdate {
                    task_id: task.id.clone(),
                    context_id: task.context_id.clone(),
                    artifact: artifact.clone(),
                    append: false,
                    last_chunk: true,
                })));
                task.artifacts = vec![artifact];
            }
            events.push(wrap(Event::Status(StatusUpdate {
                task_id: task.id.clone(),
                context_id: task.context_id.clone(),
                status: task.status.clone(),
                final_event: true,
            })));
            state.tasks.insert(task.id.clone(), task.clone());
            if request.method == "message/stream" {
                Ok(Reply::Stream(events))
            } else {
                Ok(Reply::Json(wrap(Event::Task(task))))
            }
        }
        "tasks/get" | "tasks/cancel" | "tasks/resubscribe" => {
            let params: TaskParams =
                serde_json::from_value(request.params).map_err(|_| (-32602, "Invalid params"))?;
            let task = state
                .tasks
                .get_mut(&params.id)
                .ok_or((-32001, "Task not found"))?;
            if request.method == "tasks/cancel" {
                if task.status.state.is_terminal() {
                    return Err((-32002, "Task not cancelable"));
                }
                task.status.state = TaskState::Canceled;
            }
            let mut output = task.clone();
            if let Some(length) = params.history_length {
                let skip = output.history.len().saturating_sub(length as usize);
                output.history.drain(..skip);
            }
            if request.method == "tasks/resubscribe" {
                Ok(Reply::Stream(vec![
                    wrap(Event::Task(output)),
                    wrap(Event::Status(StatusUpdate {
                        task_id: task.id.clone(),
                        context_id: task.context_id.clone(),
                        status: task.status.clone(),
                        final_event: true,
                    })),
                ]))
            } else {
                Ok(Reply::Json(wrap(Event::Task(output))))
            }
        }
        _ => Err((-32601, "Method not found")),
    }
}
