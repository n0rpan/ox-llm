//! One authenticated A2A task owning one native long-running goal. Permissions,
//! route and budget are captured locally; wire messages carry goal/control data.
use crate::{
    goal_session::{self, Control, Event as GoalEvent, Report},
    goals::{self, State as GoalState},
    RunSettings, WorkerSpec,
};
use oxa_a2a::{
    server::{error, Backend},
    wire::*,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[derive(Deserialize, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Command {
    Steer { revision: u64, text: String },
    Pause { revision: u64 },
    Resume { revision: u64 },
    Retarget { revision: u64, goal: String },
}
impl Command {
    pub fn revision(&self) -> u64 {
        match self {
            Self::Steer { revision, .. }
            | Self::Pause { revision }
            | Self::Resume { revision }
            | Self::Retarget { revision, .. } => *revision,
        }
    }
    fn control(self) -> Control {
        match self {
            Self::Steer { revision, text } => Control::Steer { revision, text },
            Self::Pause { revision } => Control::Pause { revision },
            Self::Resume { revision } => Control::Resume { revision },
            Self::Retarget { revision, goal } => Control::SetGoal {
                revision,
                text: goal,
                budget: None,
            },
        }
    }
}
struct Inner {
    task: Task,
    goal: Option<goals::Snapshot>,
    initial: Option<(String, String)>,
    messages: BTreeMap<String, String>,
    controls: Option<SyncSender<Control>>,
    thread: Option<JoinHandle<()>>,
    report: Option<Arc<Report>>,
    finished: bool,
}
pub(crate) struct GoalAgent {
    settings: RunSettings,
    spec: WorkerSpec,
    budget: goals::Budget,
    state: Arc<Mutex<Inner>>,
    pub cancel: Arc<AtomicBool>,
    notify: Arc<dyn Fn(GoalEvent) + Send + Sync>,
}
impl GoalAgent {
    pub fn new(
        settings: RunSettings,
        spec: WorkerSpec,
        budget: goals::Budget,
        cancel: Arc<AtomicBool>,
        notify: impl Fn(GoalEvent) + Send + Sync + 'static,
    ) -> Self {
        let task = Task {
            id: settings.id.clone(),
            context_id: format!("ctx-{}", settings.id),
            status: TaskStatus {
                state: TaskState::Submitted,
                message: None,
                timestamp: None,
            },
            artifacts: vec![],
            history: vec![],
        };
        Self {
            settings,
            spec,
            budget,
            state: Arc::new(Mutex::new(Inner {
                task,
                goal: None,
                initial: None,
                messages: BTreeMap::new(),
                controls: None,
                thread: None,
                report: None,
                finished: false,
            })),
            cancel,
            notify: Arc::new(notify),
        }
    }
    pub fn finished(&self) -> bool {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).finished
    }
    pub fn report(&self) -> Option<Arc<Report>> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .report
            .clone()
    }
}
fn fingerprint(value: &impl serde::Serialize) -> Result<String, RpcError> {
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(value).map_err(|_| error(-32602, "Invalid message"))?)
    ))
}
impl Backend for GoalAgent {
    fn card(&self, endpoint: &str) -> AgentCard {
        AgentCard {
            protocol_version: PROTOCOL_VERSION.into(),
            name: "oxa session goal worker".into(),
            description: "One bounded offline goal in an operator-owned native session".into(),
            url: endpoint.into(),
            version: "1".into(),
            preferred_transport: "JSONRPC".into(),
            capabilities: Capabilities {
                streaming: true,
                extensions: vec![Extension {
                    uri: "urn:oxa:a2a:session-goal:1".into(),
                    required: false,
                }],
                ..Default::default()
            },
            default_input_modes: vec!["text/plain".into(), "application/json".into()],
            default_output_modes: vec!["application/json".into()],
            skills: vec![Skill {
                id: "session-goal".into(),
                name: "Long-running session goal".into(),
                description: "Pursue a goal within fixed role, model and grants".into(),
                tags: vec!["local".into(), "goal".into()],
            }],
            security_schemes: BTreeMap::new(),
            security: vec![],
        }
    }
    fn send(&self, params: SendParams) -> Result<Event, RpcError> {
        let MessageObject::Message(message) = params.message;
        if message.role != Role::User
            || message.message_id.is_empty()
            || message.message_id.len() > 128
            || message.message_id.chars().any(char::is_control)
            || message.parts.len() != 1
        {
            return Err(error(-32602, "Invalid goal message"));
        }
        let hash = fingerprint(&message)?;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(task) = &message.task_id {
            if task != &state.task.id || message.context_id.as_deref() != Some(state.task.context_id.as_str())
            {
                return Err(error(-32602, "Unknown goal task/context"));
            }
            if let Some(previous) = state.messages.get(&message.message_id) {
                return if previous == &hash {
                    Ok(Event::Task(state.task.clone()))
                } else {
                    Err(error(-32602, "Message identity conflict"))
                };
            }
            if state.finished || state.messages.len() >= 128 {
                return Err(error(-32602, "Goal stopped or control limit reached"));
            }
            let Part::Data { data } = &message.parts[0] else {
                return Err(error(-32602, "Goal controls require structured data"));
            };
            let command: Command = serde_json::from_value(json!(data))
                .map_err(|_| error(-32602, "Invalid fixed-budget goal control"))?;
            if state
                .goal
                .as_ref()
                .is_none_or(|g| g.revision != command.revision())
            {
                return Err(error(-32602, "Stale goal revision"));
            }
            state
                .controls
                .as_ref()
                .ok_or_else(|| error(-32603, "Goal controls unavailable"))?
                .try_send(Control::Identified {
                    id: message.message_id.clone(),
                    control: Box::new(command.control()),
                })
                .map_err(|_| error(-32603, "Goal control queue unavailable"))?;
            state.messages.insert(message.message_id, hash);
            return Ok(Event::Task(state.task.clone()));
        }
        if message.context_id.is_some() {
            return Err(error(-32602, "Unknown initial goal context"));
        }
        if let Some((id, previous)) = &state.initial {
            return if id == &message.message_id && previous == &hash {
                Ok(Event::Task(state.task.clone()))
            } else {
                Err(error(-32602, "This agent already owns its initial goal"))
            };
        }
        if self.cancel.load(Ordering::Acquire) {
            return Err(error(-32603, "Goal owner stopping"));
        }
        let Part::Text { text } = &message.parts[0] else {
            return Err(error(-32602, "Initial goal requires text data"));
        };
        goals::Goal::new(self.settings.id.clone(), text.clone(), self.budget.clone())
            .map_err(|_| error(-32602, "Invalid bounded goal"))?;
        let mut spec = self.spec.clone();
        spec.task = text.clone();
        let settings = self.settings.clone();
        let budget = self.budget.clone();
        let cancel = self.cancel.clone();
        let shared = self.state.clone();
        let notify = self.notify.clone();
        let (controls, receiver) = mpsc::sync_channel(32);
        state.controls = Some(controls);
        state.initial = Some((message.message_id, hash));
        state.task.history = vec![MessageObject::Message(Message {
            message_id: "accepted-goal".into(),
            role: Role::User,
            parts: vec![Part::Text { text: text.clone() }],
            task_id: Some(state.task.id.clone()),
            context_id: Some(state.task.context_id.clone()),
        })];
        let worker=thread::Builder::new().name("oxa-a2a-goal".into()).spawn(move ||{
            let observed=shared.clone();
            let result=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||goal_session::pursue(settings,spec,budget,receiver,cancel,|event|{
                {let mut state=observed.lock().unwrap_or_else(|e|e.into_inner());state.goal=Some(event.goal.clone());
                    // Terminal wire state is published only after pursue returns
                    // its post-cleanup report, never merely on a declaration.
                    state.task.status.state=match event.goal.state{GoalState::Paused|GoalState::Pausing|GoalState::Blocked=>TaskState::InputRequired,_=>TaskState::Working};}
                notify(event);
            })));
            let mut state=shared.lock().unwrap_or_else(|e|e.into_inner());
            match result {
                Ok(Ok(report))=>{
                    state.task.status.state=match report.goal.state{GoalState::Done=>TaskState::Completed,GoalState::Canceled=>TaskState::Canceled,_=>TaskState::Failed};
                    let hash=format!("{:x}",Sha256::digest(serde_json::to_vec(&report).unwrap_or_default()));
                    let mut reply_end=report.last_reply.len().min(8192);while !report.last_reply.is_char_boundary(reply_end){reply_end-=1;}
                    let value=json!({"report_sha256":hash,"goal":{"id":report.goal.id,"state":report.goal.state,"revision":report.goal.revision},"reply":&report.last_reply[..reply_end],"reply_truncated":reply_end!=report.last_reply.len(),"process_stopped":report.process_stopped});
                    let data=serde_json::from_value(value).unwrap_or_default();
                    state.task.artifacts=vec![Artifact{artifact_id:"goal-result".into(),parts:vec![Part::Data{data}],name:Some("Bounded goal result".into()),description:None}];
                    state.report=Some(Arc::new(report));
                }
                _=>state.task.status.state=TaskState::Failed,
            }
            state.controls=None;state.finished=true;
        });
        match worker {
            Ok(worker) => state.thread = Some(worker),
            Err(_) => {
                state.controls = None;
                state.finished = true;
                state.task.status.state = TaskState::Failed;
                return Err(error(-32603, "Cannot start goal owner"));
            }
        }
        Ok(Event::Task(state.task.clone()))
    }
    fn get(&self, params: TaskParams) -> Result<Task, RpcError> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if params.id != state.task.id {
            return Err(error(-32001, "Task not found"));
        }
        let mut task = state.task.clone();
        if params.history_length.unwrap_or(0) == 0 {
            task.history.clear();
        }
        Ok(task)
    }
    fn cancel(&self, id: &str) -> Result<Task, RpcError> {
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if id != state.task.id {
                return Err(error(-32001, "Task not found"));
            }
            self.cancel.store(true, Ordering::Release);
            if state.initial.is_none() {
                state.task.status.state = TaskState::Canceled;
                state.finished = true;
                return Ok(state.task.clone());
            }
        }
        let started = Instant::now();
        loop {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.finished {
                return Ok(state.task.clone());
            }
            drop(state);
            if started.elapsed() > Duration::from_secs(2) {
                return Err(error(-32603, "Goal cancellation remains unconfirmed"));
            }
            thread::sleep(Duration::from_millis(2));
        }
    }
    fn shutdown(&self) {
        self.cancel.store(true, Ordering::Release);
        let worker = self.state.lock().unwrap_or_else(|e| e.into_inner()).thread.take();
        if let Some(worker) = worker {
            let _ = worker.join();
        }
    }
}
impl Drop for GoalAgent {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LocalInference, PermissionMode, WorkerBudget};

    fn agent() -> GoalAgent {
        // No worker can launch in these protocol-rejection tests.
        let settings = RunSettings {
            id: "owned-worker".into(),
            worker_binary: "/nonexistent/worker".into(),
            bwrap_binary: "/nonexistent/bwrap".into(),
            directory: "/nonexistent/audit".into(),
            parallelism: 1,
            mode: PermissionMode::Plan,
            wall_time: Duration::from_secs(1),
            output_reservation: 65536,
            token_reservation: 100,
            journal_bytes: 65536,
        };
        let spec = WorkerSpec {
            id: "owned-worker".into(),
            role: "reviewer".into(),
            task: String::new(),
            inference: LocalInference::register_verified_local("127.0.0.1:1".parse().unwrap(), "fake".into())
                .unwrap(),
            budget: WorkerBudget {
                wall_time: Duration::from_secs(1),
                output_bytes: 65536,
                reported_tokens: 100,
            },
            workspace: None,
        };
        GoalAgent::new(
            settings,
            spec,
            goals::Budget {
                wall_ms: 1000,
                tokens: Some(100),
                turns: 2,
            },
            Arc::new(AtomicBool::new(false)),
            |_| {},
        )
    }

    fn initial() -> SendParams {
        SendParams {
            message: MessageObject::Message(Message {
                message_id: "initial".into(),
                role: Role::User,
                parts: vec![Part::Text {
                    text: "review the change".into(),
                }],
                task_id: None,
                context_id: None,
            }),
        }
    }

    #[test]
    fn cancel_before_dispatch_is_terminal_and_prevents_late_launch() {
        let agent = agent();
        assert!(agent.cancel("foreign-worker").is_err());
        assert!(!agent.cancel.load(Ordering::Acquire));
        let canceled = agent.cancel("owned-worker").unwrap();
        assert_eq!(canceled.status.state, TaskState::Canceled);
        assert!(agent.finished());
        assert!(agent.report().is_none());
        assert!(agent.send(initial()).is_err());
        assert_eq!(
            agent.cancel("owned-worker").unwrap().status.state,
            TaskState::Canceled
        );
    }

    #[test]
    fn controls_cannot_introduce_routes_budgets_or_foreign_contexts() {
        assert!(
            serde_json::from_value::<Command>(json!({"operation":"resume","revision":0,"model":"paid"}))
                .is_err()
        );
        assert!(serde_json::from_value::<Command>(
            json!({"operation":"retarget","revision":0,"goal":"more","budget":{"tokens":999}})
        )
        .is_err());
        let agent = agent();
        let mut params = initial();
        let MessageObject::Message(ref mut message) = params.message;
        message.context_id = Some("foreign-context".into());
        assert!(agent.send(params).is_err());
        assert!(!agent.finished());
        assert!(agent.report().is_none());
    }

    #[test]
    fn authenticated_wire_cancellation_preserves_task_identity() {
        use oxa_a2a::{
            loopback::Limits,
            server::{Server, ServerLimits},
        };
        let backend = Arc::new(agent());
        let server = Server::start(backend.clone(), ServerLimits::default()).unwrap();
        let mut client = server.client(Limits::default()).unwrap();
        let card = client.card().unwrap();
        assert!(card
            .capabilities
            .extensions
            .iter()
            .any(|e| e.uri == "urn:oxa:a2a:session-goal:1" && !e.required));
        let task = client.cancel("owned-worker").unwrap();
        assert_eq!(task.id, "owned-worker");
        assert_eq!(task.context_id, "ctx-owned-worker");
        assert_eq!(task.status.state, TaskState::Canceled);
        assert!(client.send(initial()).is_err());
        assert!(backend.finished());
        assert!(backend.report().is_none());
    }
}
