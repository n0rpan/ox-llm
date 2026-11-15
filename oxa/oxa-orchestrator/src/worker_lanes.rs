//! Operator-funded worker lanes belonging to one shared session.
//!
//! Reservations are lifetime charges. Cancellation, an unknown usage report,
//! and forgetting a result never replenish the operator's allowance.
use crate::{
    goal_agent::{Command, GoalAgent},
    goal_session, goals, RunSettings, WorkerSpec,
};
use oxa_a2a::{
    loopback::{Client, Limits},
    server::{Server, ServerLimits},
    wire::*,
};
use sha2::{Digest, Sha256};
use std::io;
use std::sync::{atomic::AtomicBool, Arc};
use std::time::Duration;
mod manager;
pub(crate) use manager::for_session;
pub use manager::{augment, Manager};

/// Keeps the authenticated transport and native goal owner alive together.
/// Dropping the server cancels and joins the goal even when dispatch failed
/// after the peer accepted it. Initial requests are never automatically replayed.
pub(crate) struct LocalLane {
    client: Client,
    backend: Arc<GoalAgent>,
    _server: Server<GoalAgent>,
    id: String,
    next_control: u64,
}

impl LocalLane {
    pub(crate) fn start(
        settings: RunSettings,
        spec: WorkerSpec,
        budget: goals::Budget,
        cancel: Arc<AtomicBool>,
        notify: impl Fn(goal_session::Event) + Send + Sync + 'static,
    ) -> io::Result<Self> {
        crate::run::validate(&settings, std::slice::from_ref(&spec))?;
        goals::Goal::new(settings.id.clone(), spec.task.clone(), budget.clone())
            .map_err(|_| io::Error::other("invalid worker goal"))?;
        let id = settings.id.clone();
        let goal = spec.task.clone();
        let backend = Arc::new(GoalAgent::new(settings, spec, budget, cancel, notify));
        let server = Server::start(backend.clone(), ServerLimits::default())?;
        let mut client = server.client(Limits {
            timeout: Duration::from_secs(3),
            body_bytes: 1024 * 1024,
            event_bytes: 256 * 1024,
        })?;
        let event = client.send(SendParams {
            message: MessageObject::Message(Message {
                message_id: "initial-goal".into(),
                role: Role::User,
                parts: vec![Part::Text { text: goal }],
                task_id: None,
                context_id: None,
            }),
        })?;
        let Event::Task(task) = event else {
            return Err(io::Error::other("missing worker task"));
        };
        if task.id != id || task.context_id != format!("ctx-{id}") {
            return Err(io::Error::other("worker task identity mismatch"));
        }
        Ok(Self {
            client,
            backend,
            _server: server,
            id,
            next_control: 0,
        })
    }

    /// This response proves queueing only; goal events carry admission results.
    pub(crate) fn control(&mut self, command: Command) -> io::Result<()> {
        self.next_control = self
            .next_control
            .checked_add(1)
            .ok_or_else(|| io::Error::other("worker control identity exhausted"))?;
        let data = serde_json::from_value(serde_json::to_value(command)?)?;
        self.client.send(SendParams {
            message: MessageObject::Message(Message {
                message_id: format!("control-{}", self.next_control),
                role: Role::User,
                parts: vec![Part::Data { data }],
                task_id: Some(self.id.clone()),
                context_id: Some(format!("ctx-{}", self.id)),
            }),
        })?;
        Ok(())
    }

    /// A wire terminal declaration alone is insufficient. Match its artifact to
    /// the locally owned report produced after process cleanup.
    pub(crate) fn result(&mut self) -> io::Result<Option<Arc<goal_session::Report>>> {
        if !self.backend.finished() {
            return Ok(None);
        }
        let report = self
            .backend
            .report()
            .ok_or_else(|| io::Error::other("worker ended without a cleanup report"))?;
        let task = self.client.get(&self.id)?;
        let expected = match report.goal.state {
            goals::State::Done => TaskState::Completed,
            goals::State::Canceled => TaskState::Canceled,
            _ => TaskState::Failed,
        };
        let hash = format!("{:x}", Sha256::digest(serde_json::to_vec(&*report)?));
        let valid_artifact = task.artifacts.iter().any(|artifact| {
            artifact.artifact_id == "goal-result" && artifact.parts.iter().any(|part| {
                matches!(part, Part::Data { data } if data.get("report_sha256").and_then(|v|v.as_str()) == Some(hash.as_str())
                    && data.get("process_stopped").and_then(|v|v.as_bool()) == Some(true))
            })
        });
        if task.id != self.id
            || task.context_id != format!("ctx-{}", self.id)
            || task.status.state != expected
            || !report.process_stopped
            || !valid_artifact
        {
            return Err(io::Error::other("worker cleanup/result verification failed"));
        }
        Ok(Some(report))
    }
}

#[derive(Debug)]
pub(crate) struct Reservations {
    tokens: u64,
    output: usize,
    token_limit: u64,
    output_limit: usize,
}

impl Reservations {
    pub(crate) fn new(token_limit: u64, output_limit: usize) -> Self {
        Self {
            tokens: 0,
            output: 0,
            token_limit,
            output_limit,
        }
    }

    /// Commit both allowances together, before launching a worker. A failed
    /// admission cannot consume only one side of the reservation.
    pub(crate) fn reserve(&mut self, tokens: u64, output: usize) -> io::Result<()> {
        let next_tokens = self.tokens.checked_add(tokens);
        let next_output = self.output.checked_add(output);
        match (next_tokens, next_output) {
            (Some(next_tokens), Some(next_output))
                if tokens > 0
                    && output > 0
                    && next_tokens <= self.token_limit
                    && next_output <= self.output_limit =>
            {
                self.tokens = next_tokens;
                self.output = next_output;
                Ok(())
            }
            _ => Err(io::Error::other("worker lifetime allowance exhausted")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_admission_is_atomic_and_exact_remaining_capacity_is_usable() {
        let mut pool = Reservations::new(100, 1000);
        pool.reserve(60, 500).unwrap();
        assert!(pool.reserve(40, 501).is_err());
        assert_eq!((pool.tokens, pool.output), (60, 500));
        pool.reserve(40, 500).unwrap();
        assert!(pool.reserve(1, 1).is_err());
    }

    #[test]
    fn overflow_and_zero_are_rejected_without_charging() {
        let mut pool = Reservations::new(u64::MAX, usize::MAX);
        assert!(pool.reserve(0, 1).is_err());
        assert!(pool.reserve(1, 0).is_err());
        pool.reserve(u64::MAX, usize::MAX).unwrap();
        assert!(pool.reserve(1, 1).is_err());
        assert_eq!((pool.tokens, pool.output), (u64::MAX, usize::MAX));
    }
}
