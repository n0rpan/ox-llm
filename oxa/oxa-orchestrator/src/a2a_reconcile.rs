//! Explicit original-agent observation/cancellation under a journal lease.
use crate::a2a_recovery::{Binding, ExclusiveRun, WorkerSnapshot};
use crate::a2a_run::{LocalAgent, StopConfirmation};
use crate::supervisor::Outcome;
use oxa_a2a::{
    collect::Collector,
    loopback::{rpc_error_code, Client, Limits, ResponseBudget},
    wire::{Event, Task, TaskState},
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::io;
use std::time::{Duration, Instant};
impl ExclusiveRun {
    /// The operator re-registers the ORIGINAL agent instance/capability. An
    /// endpoint hash prevents accidental route changes; it is not authentication.
    /// Only discover/get/cancel are used, never a send or replay of task input.
    pub fn reconcile(
        &self,
        worker: &str,
        agent: LocalAgent,
        options: ReconcileOptions,
    ) -> io::Result<Reconciliation> {
        let snapshot = self.inspected().snapshot();
        if options.timeout.is_zero()
            || options.timeout > Duration::from_secs(5)
            || !(1024..=16 * 1024 * 1024).contains(&options.response_bytes)
        {
            return Err(invalid("invalid reconciliation transport or limits"));
        }
        let worker = snapshot
            .workers
            .iter()
            .find(|w| w.worker == worker)
            .ok_or_else(|| invalid("worker is absent from inspected journal"))?;
        let binding = worker
            .binding
            .as_ref()
            .ok_or_else(|| invalid("no durable task identity; input must not be replayed"))?;
        let mut client = agent.into_client();
        if worker.agent_url_sha256 != hash(client.url().as_bytes()) {
            return Err(invalid("original registered endpoint does not match the journal"));
        }
        let budget = ResponseBudget::new(options.response_bytes);
        client = client.with_response_budget(budget.clone());
        let deadline = Instant::now() + options.timeout;
        let limits = Limits {
            timeout: options.timeout,
            body_bytes: options.response_bytes,
            event_bytes: options.response_bytes,
        };
        let mut result = Reconciliation {
            journal_sha256: snapshot.journal_sha256.clone(),
            worker: worker.worker.clone(),
            recorded_outcome: worker.recorded_outcome,
            disposition: Disposition::Unavailable,
            remote_stop: StopConfirmation::Unconfirmed,
            task: None,
            response_bytes: 0,
        };
        if !constrain(&mut client, limits, deadline)? {
            return Ok(result);
        }
        if client.card().is_err() {
            result.disposition = failure(&budget);
            result.response_bytes = budget.used();
            return Ok(result);
        }
        if !constrain(&mut client, limits, deadline)? {
            result.response_bytes = budget.used();
            return Ok(result);
        }
        let task = match client.get(&binding.task_id) {
            Ok(task) => task,
            Err(error) => {
                result.disposition = if rpc_error_code(&error) == Some(-32001) {
                    Disposition::NotFound
                } else {
                    failure(&budget)
                };
                result.response_bytes = budget.used();
                return Ok(result);
            }
        };
        if let Err(disposition) = validate_task(&task, binding, worker, options.response_bytes) {
            result.disposition = disposition;
            result.response_bytes = budget.used();
            return Ok(result);
        }
        let terminal = task.status.state.is_terminal();
        result.task = Some(task);
        result.disposition = Disposition::Observed;
        if terminal {
            result.remote_stop = StopConfirmation::Confirmed;
        }
        if options.action == Action::Cancel && !terminal {
            result.disposition = Disposition::CancellationUnconfirmed;
            if constrain(&mut client, limits, deadline)? {
                if let Ok(task) = client.cancel(&binding.task_id) {
                    match validate_task(&task, binding, worker, options.response_bytes) {
                        Ok(()) => {
                            result.remote_stop = if task.status.state.is_terminal() {
                                StopConfirmation::Confirmed
                            } else {
                                StopConfirmation::Unconfirmed
                            };
                            result.task = Some(task);
                        }
                        Err(disposition) => result.disposition = disposition,
                    }
                }
            }
            // A lost cancel response may still have stopped the task. Query
            // once within the SAME absolute deadline and aggregate allowance.
            if result.remote_stop != StopConfirmation::Confirmed
                && result.disposition == Disposition::CancellationUnconfirmed
                && constrain(&mut client, limits, deadline)?
            {
                if let Ok(task) = client.get(&binding.task_id) {
                    match validate_task(&task, binding, worker, options.response_bytes) {
                        Ok(()) => {
                            if task.status.state.is_terminal() {
                                result.remote_stop = StopConfirmation::Confirmed;
                            }
                            result.task = Some(task);
                        }
                        Err(disposition) => result.disposition = disposition,
                    }
                }
            }
            if result.remote_stop == StopConfirmation::Confirmed {
                result.disposition = Disposition::Observed;
            } else if budget.remaining() == 0 {
                result.disposition = Disposition::OutputBudget;
            }
        }
        result.response_bytes = budget.used();
        Ok(result)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Observe,
    Cancel,
}
#[derive(Clone, Copy, Debug)]
pub struct ReconcileOptions {
    pub action: Action,
    pub timeout: Duration,
    pub response_bytes: usize,
}
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    Observed,
    Unavailable,
    NotFound,
    IdentityMismatch,
    InvalidResponse,
    ResultMismatch,
    OutputBudget,
    CancellationUnconfirmed,
}
#[derive(Debug, Serialize)]
pub struct Reconciliation {
    pub journal_sha256: String,
    pub worker: String,
    pub recorded_outcome: Option<Outcome>,
    pub disposition: Disposition,
    pub remote_stop: StopConfirmation,
    #[serde(serialize_with = "crate::a2a_run::optional_task")]
    pub task: Option<Task>,
    pub response_bytes: usize,
}
fn validate_task(
    task: &Task,
    binding: &Binding,
    worker: &WorkerSnapshot,
    max: usize,
) -> Result<(), Disposition> {
    if task.id != binding.task_id || task.context_id != binding.context_id {
        return Err(Disposition::IdentityMismatch);
    }
    if worker
        .recorded_task_state
        .as_ref()
        .is_some_and(|state| state.is_terminal() && state != &task.status.state)
        || (worker.recorded_outcome == Some(Outcome::Completed) && task.status.state != TaskState::Completed)
        || (worker.recorded_stop == Some(StopConfirmation::Confirmed) && !task.status.state.is_terminal())
    {
        return Err(Disposition::ResultMismatch);
    }
    let mut collector = Collector::new(max);
    collector
        .apply(Event::Task(task.clone()))
        .map_err(|_| Disposition::InvalidResponse)?;
    let recorded_terminal = worker
        .recorded_task_state
        .as_ref()
        .is_some_and(TaskState::is_terminal)
        || worker.recorded_stop == Some(StopConfirmation::Confirmed)
        || worker.recorded_outcome == Some(Outcome::Completed);
    if recorded_terminal && task.status.state.is_terminal() {
        if let Some(expected) = &worker.result_sha256 {
            let actual = serde_json::to_vec(&task.artifacts).map_err(|_| Disposition::InvalidResponse)?;
            if expected != &hash(&actual) {
                return Err(Disposition::ResultMismatch);
            }
        }
    }
    Ok(())
}
fn constrain(client: &mut Client, limits: Limits, deadline: Instant) -> io::Result<bool> {
    if Instant::now() >= deadline {
        return Ok(false);
    }
    client.constrain(Limits {
        timeout: deadline
            .saturating_duration_since(Instant::now())
            .max(Duration::from_nanos(1)),
        ..limits
    })?;
    Ok(true)
}
fn failure(budget: &ResponseBudget) -> Disposition {
    if budget.remaining() == 0 {
        Disposition::OutputBudget
    } else {
        Disposition::Unavailable
    }
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
#[cfg(test)]
#[path = "a2a_reconcile_tests.rs"]
mod tests;
