//! Bounded task/artifact collection. Wire completion and task completion remain
//! distinct, and direct Message results never receive invented task IDs.
use crate::wire::*;
use serde::Serialize;
use std::collections::BTreeSet;
use std::io::{self, Write};

#[derive(Clone)]
pub struct Collector {
    task: Option<Task>,
    message: Option<MessageObject>,
    closed: BTreeSet<String>,
    max_bytes: usize,
    stream_final: bool,
    failed: bool,
}

impl Collector {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            task: None,
            message: None,
            closed: BTreeSet::new(),
            max_bytes,
            stream_final: false,
            failed: false,
        }
    }
    pub fn task(&self) -> Option<&Task> {
        self.task.as_ref()
    }
    pub fn message(&self) -> Option<&MessageObject> {
        self.message.as_ref()
    }
    pub fn stream_final(&self) -> bool {
        self.stream_final
    }
    pub fn is_failed(&self) -> bool {
        self.failed
    }

    pub fn apply(&mut self, event: Event) -> io::Result<()> {
        if self.failed {
            return Err(invalid("collector already failed"));
        }
        self.stage(event, false)
    }

    // The retained state stays within its bound even after a rejected update.
    // Staging temporarily duplicates only the previously bounded state; the
    // incoming event and candidate result are each checked before publication.
    fn stage(&mut self, event: Event, reconnect: bool) -> io::Result<()> {
        let mut candidate = self.clone();
        if reconnect {
            candidate.stream_final = false;
        }
        match candidate.apply_inner(event) {
            Ok(()) => {
                *self = candidate;
                Ok(())
            }
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
    }

    /// A query snapshot reconciles a disconnected stream; it does not resend
    /// task input. Previously received artifacts are preserved when omitted.
    pub fn reconcile(&mut self, task: Task) -> io::Result<()> {
        if self.failed {
            return Err(invalid("collector already failed"));
        }
        if task.status.state.is_terminal()
            && self.task.as_ref().is_some_and(|previous| {
                previous.artifacts.iter().any(|artifact| {
                    !self.closed.contains(&artifact.artifact_id)
                        && !task
                            .artifacts
                            .iter()
                            .any(|a| a.artifact_id == artifact.artifact_id)
                })
            })
        {
            self.failed = true;
            return Err(invalid("terminal snapshot omits an incomplete artifact"));
        }
        self.stage(Event::Task(task), true)
    }

    fn apply_inner(&mut self, event: Event) -> io::Result<()> {
        if self.stream_final {
            return Err(invalid("event after stream completion"));
        }
        bounded_size(&event, self.max_bytes)?;
        if let Event::Message(message) = event {
            if self.task.is_some()
                || self.message.is_some()
                || message.role != Role::Agent
                || message.message_id.is_empty()
                || message.parts.is_empty()
            {
                return Err(invalid("invalid direct agent message"));
            }
            self.message = Some(MessageObject::Message(message));
            self.stream_final = true;
            return Ok(());
        }
        if self.message.is_some() {
            return Err(invalid("task after direct message"));
        }
        let (id, context) = match &event {
            Event::Task(t) => (&t.id, &t.context_id),
            Event::Status(e) => (&e.task_id, &e.context_id),
            Event::Artifact(e) => (&e.task_id, &e.context_id),
            _ => unreachable!(),
        };
        if id.is_empty() || context.is_empty() {
            return Err(invalid("empty task identity"));
        }
        if let Some(task) = &self.task {
            if &task.id != id || &task.context_id != context {
                return Err(invalid("task identity changed"));
            }
        } else {
            self.task = Some(Task {
                id: id.clone(),
                context_id: context.clone(),
                status: TaskStatus {
                    state: TaskState::Unknown,
                    message: None,
                    timestamp: None,
                },
                artifacts: vec![],
                history: vec![],
            });
        }
        let task = self.task.as_mut().unwrap();
        match event {
            Event::Task(snapshot) => {
                let mut seen = BTreeSet::new();
                for artifact in snapshot.artifacts {
                    if artifact.artifact_id.is_empty() || !seen.insert(artifact.artifact_id.clone()) {
                        return Err(invalid("invalid artifact identity"));
                    }
                    if snapshot.status.state.is_terminal() {
                        self.closed.insert(artifact.artifact_id.clone());
                    } else if task
                        .artifacts
                        .iter()
                        .find(|a| a.artifact_id == artifact.artifact_id)
                        != Some(&artifact)
                    {
                        self.closed.remove(&artifact.artifact_id);
                    }
                    replace(&mut task.artifacts, artifact);
                }
                task.status = snapshot.status;
                task.history = snapshot.history;
                self.stream_final = task.status.state.is_terminal();
            }
            Event::Status(update) => {
                if update.status.state.is_terminal() && !update.final_event {
                    return Err(invalid("terminal status without final flag"));
                }
                task.status = update.status;
                self.stream_final = update.final_event;
            }
            Event::Artifact(update) => {
                let id = update.artifact.artifact_id.clone();
                if id.is_empty() {
                    return Err(invalid("empty artifact identity"));
                }
                if update.append {
                    if self.closed.contains(&id) {
                        return Err(invalid("append after final artifact chunk"));
                    }
                    let existing = task
                        .artifacts
                        .iter_mut()
                        .find(|a| a.artifact_id == id)
                        .ok_or_else(|| invalid("append without initial artifact"))?;
                    existing.parts.extend(update.artifact.parts);
                } else {
                    self.closed.remove(&id);
                    replace(&mut task.artifacts, update.artifact);
                }
                if update.last_chunk {
                    self.closed.insert(id);
                }
            }
            _ => unreachable!(),
        }
        bounded_size(&Event::Task(task.clone()), self.max_bytes)
    }
}

fn replace(artifacts: &mut Vec<Artifact>, artifact: Artifact) {
    if let Some(existing) = artifacts
        .iter_mut()
        .find(|a| a.artifact_id == artifact.artifact_id)
    {
        *existing = artifact;
    } else {
        artifacts.push(artifact);
    }
}

fn bounded_size(value: &impl Serialize, max: usize) -> io::Result<()> {
    struct Count {
        bytes: usize,
        max: usize,
    }
    impl Write for Count {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > self.max.saturating_sub(self.bytes) {
                return Err(invalid("collected result limit exceeded"));
            }
            self.bytes += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Count { bytes: 0, max }, value)
        .map_err(|_| invalid("collected result limit exceeded"))
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
