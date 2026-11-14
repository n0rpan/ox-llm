//! Bounded fan-out/fan-in through registered local-only A2A development agents.
//! Agent results remain data. No card or result grants local tools/permissions.
use crate::{journal::Journal, Control, Outcome};
use oxa_a2a::{
    collect::Collector,
    loopback::{Client, Limits, ResponseBudget},
    wire::*,
};
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, VecDeque};
use std::fs::DirBuilder;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, SyncSender},
    Arc, Mutex,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// A trusted operator assertion, never an assertion from a discovered card.
/// The endpoint must not forward to cloud/billable inference while offline.
pub struct LocalAgent {
    client: Client,
}
impl LocalAgent {
    pub fn register_verified_local(client: Client) -> Self {
        Self { client }
    }
}

pub struct A2AWorkerSpec {
    pub id: String,
    pub role: String,
    pub task: String,
    pub agent: LocalAgent,
    pub wall_time: Duration,
    /// Aggregate incoming wire bytes, including discovery, polling and cleanup.
    pub response_bytes: usize,
    /// Optional observed usage cap, not a pre-request spend guarantee. Requires
    /// the native reported-usage extension; unknown usage cannot satisfy a cap.
    pub reported_tokens: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct A2ARunSettings {
    pub id: String,
    pub directory: PathBuf,
    pub parallelism: usize,
    pub wall_time: Duration,
    pub response_reservation: usize,
    pub journal_bytes: usize,
    pub request_timeout: Duration,
    pub poll_interval: Duration,
    pub cancel_grace: Duration,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StopConfirmation {
    NotNeeded,
    Confirmed,
    Unconfirmed,
}

#[derive(Clone, Debug, Serialize)]
pub struct A2AWorkerReport {
    pub worker: String,
    pub outcome: Outcome,
    #[serde(serialize_with = "optional_task")]
    pub task: Option<Task>,
    pub message: Option<MessageObject>,
    pub response_bytes: usize,
    pub reported_tokens: Option<u64>,
    pub stop: StopConfirmation,
}

#[derive(Clone, Debug, Serialize)]
pub struct A2ARunReport {
    pub run: String,
    pub workers: Vec<A2AWorkerReport>,
}
impl A2ARunReport {
    pub fn succeeded(&self) -> bool {
        !self.workers.is_empty() && self.workers.iter().all(|w| w.outcome == Outcome::Completed)
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct A2AStatus {
    pub worker: String,
    pub state: String,
    pub task_id: Option<String>,
    pub response_bytes: usize,
    pub reported_tokens: Option<u64>,
}

#[derive(Default)]
struct Snapshot {
    task: Option<Task>,
    state: String,
    tokens: Option<u64>,
    version: u64,
    notices: VecDeque<(&'static str, serde_json::Value)>,
}

struct ActiveJobs(Vec<Active>);
impl std::ops::Deref for ActiveJobs {
    type Target = Vec<Active>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl std::ops::DerefMut for ActiveJobs {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl Drop for ActiveJobs {
    fn drop(&mut self) {
        for job in &self.0 {
            job.cancel.store(true, Ordering::Release);
        }
    }
}

struct Active {
    id: String,
    shared: Arc<Mutex<Snapshot>>,
    cancel: Arc<AtomicBool>,
    controls: SyncSender<Control>,
    thread: Option<JoinHandle<A2AWorkerReport>>,
    emergency: Client,
    read_budget: ResponseBudget,
    stop_budget: ResponseBudget,
    seen: u64,
    binding: Option<(String, String)>,
    cancel_reason: Option<Outcome>,
}

impl Drop for Active {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            if thread.join().is_err() {
                let _ = self.read_budget.transfer_remaining_to(&self.stop_budget);
                if let Some(task) = self.shared.lock().unwrap_or_else(|e| e.into_inner()).task.clone() {
                    let _ = self.emergency.cancel(&task.id);
                }
            }
        }
    }
}

/// The status callback is trusted/nonblocking. Controls should use a bounded
/// sync_channel. Cancellation has its own flag and cannot be starved by steer.
pub fn run_a2a(
    settings: A2ARunSettings,
    plan: Vec<A2AWorkerSpec>,
    controls: Receiver<Control>,
    cancel: Arc<AtomicBool>,
    mut observe: impl FnMut(A2AStatus),
) -> io::Result<A2ARunReport> {
    validate(&settings, &plan)?;
    let deadline = Instant::now() + settings.wall_time;
    let parent = settings
        .directory
        .parent()
        .ok_or_else(|| invalid("missing run directory parent"))?
        .canonicalize()?;
    let directory = parent.join(
        settings
            .directory
            .file_name()
            .ok_or_else(|| invalid("missing run directory name"))?,
    );
    DirBuilder::new().mode(0o700).create(directory.clone())?;
    let mut journal = Journal::create(&directory.join("transcript.jsonl"), settings.journal_bytes)?;
    journal.record(None,"run_started",json!({"run":settings.id,"transport":"A2A-0.3.0","workers":plan.len(),"parallelism":settings.parallelism,"offline":true}))?;
    for spec in &plan {
        journal.record(Some(&spec.id),"queued",json!({"roleSha256":hash(&spec.role),"taskSha256":hash(&spec.task),"reservedBytes":spec.response_bytes}))?;
        observe(A2AStatus {
            worker: spec.id.clone(),
            state: "queued".into(),
            task_id: None,
            response_bytes: 0,
            reported_tokens: None,
        });
    }
    let mut pending = VecDeque::from(plan);
    let mut active = ActiveJobs(Vec::new());
    let mut reports = Vec::new();
    let mut cancel_all = false;
    while !pending.is_empty() || !active.is_empty() {
        cancel_all |= cancel.load(Ordering::Acquire);
        for _ in 0..32 {
            match controls.try_recv() {
                Ok(Control::CancelAll) => cancel_all = true,
                Ok(Control::Cancel { worker }) => {
                    if let Some(index) = pending.iter().position(|s| s.id == worker) {
                        let spec = pending.remove(index).unwrap();
                        let report = empty(&spec.id, Outcome::Canceled);
                        finish(&mut journal, &report, &mut observe)?;
                        reports.push(report);
                    } else if let Some(job) = active.iter_mut().find(|a| a.id == worker) {
                        job.cancel_reason = Some(Outcome::Canceled);
                        job.cancel.store(true, Ordering::Release);
                    }
                }
                Ok(control @ (Control::Steer { .. } | Control::SteerChecked { .. })) => {
                    let id = match &control {
                        Control::Steer { worker, .. } | Control::SteerChecked { worker, .. } => worker,
                        _ => unreachable!(),
                    };
                    if let Some(job) = active
                        .iter()
                        .find(|a| &a.id == id && !a.cancel.load(Ordering::Acquire))
                    {
                        let text = match &control {
                            Control::Steer { text, .. } | Control::SteerChecked { text, .. } => text,
                            _ => unreachable!(),
                        };
                        if text.is_empty() || text.len() > 16 * 1024 {
                            reject_control(control, &mut journal)?;
                            continue;
                        }
                        journal.record(Some(id), "steer_requested", json!({"textSha256":hash(text)}))?;
                        if let Err(error) = job.controls.try_send(control) {
                            let control = match error {
                                mpsc::TrySendError::Full(control)
                                | mpsc::TrySendError::Disconnected(control) => control,
                            };
                            reject_control(control, &mut journal)?;
                        }
                    } else {
                        reject_control(control, &mut journal)?;
                    }
                }
                Err(_) => break,
            }
        }
        let expired = Instant::now() >= deadline;
        if cancel_all || expired {
            let reason = if expired {
                Outcome::Deadline
            } else {
                Outcome::Canceled
            };
            for job in active.iter_mut() {
                job.cancel_reason = Some(reason.clone());
                job.cancel.store(true, Ordering::Release);
            }
            while let Some(spec) = pending.pop_front() {
                let report = empty(&spec.id, reason.clone());
                finish(&mut journal, &report, &mut observe)?;
                reports.push(report);
            }
        }
        while active.len() < settings.parallelism && !cancel_all && Instant::now() < deadline {
            let Some(mut spec) = pending.pop_front() else {
                break;
            };
            let reserve = (spec.response_bytes / 4).min(16 * 1024);
            let read_budget = ResponseBudget::new(spec.response_bytes - reserve);
            let stop_budget = ResponseBudget::new(reserve);
            let limits = Limits {
                timeout: settings.request_timeout,
                body_bytes: spec.response_bytes,
                event_bytes: spec.response_bytes,
            };
            spec.agent.client.constrain(limits)?;
            let emergency = spec.agent.client.fork(limits, stop_budget.clone())?;
            let cleanup = spec.agent.client.fork(limits, stop_budget.clone())?;
            let worker_deadline = (Instant::now() + spec.wall_time).min(deadline);
            let id = spec.id.clone();
            let shared = Arc::new(Mutex::new(Snapshot::default()));
            let worker_shared = shared.clone();
            let canceled = Arc::new(AtomicBool::new(false));
            let worker_cancel = canceled.clone();
            let (tx, rx) = mpsc::sync_channel(32);
            let args = WorkerRun {
                run: settings.id.clone(),
                deadline: worker_deadline,
                request_timeout: settings.request_timeout,
                poll_interval: settings.poll_interval,
                cancel_grace: settings.cancel_grace,
            };
            let (read, stop) = (read_budget.clone(), stop_budget.clone());
            journal.record(Some(&id), "starting", json!({}))?;
            let thread = thread::Builder::new()
                .name("oxa-a2a-worker".into())
                .spawn(move || worker(spec, args, rx, worker_cancel, worker_shared, read, stop, cleanup))?;
            active.push(Active {
                id,
                shared,
                cancel: canceled,
                controls: tx,
                thread: Some(thread),
                emergency,
                read_budget,
                stop_budget,
                seen: 0,
                binding: None,
                cancel_reason: None,
            });
        }
        let mut i = 0;
        while i < active.len() {
            let job = &mut active[i];
            {
                let (identity, version, state, tokens, notices) = {
                    let mut snapshot = job.shared.lock().unwrap_or_else(|e| e.into_inner());
                    (
                        snapshot
                            .task
                            .as_ref()
                            .map(|t| (t.id.clone(), t.context_id.clone())),
                        snapshot.version,
                        snapshot.state.clone(),
                        snapshot.tokens,
                        snapshot.notices.drain(..).collect::<Vec<_>>(),
                    )
                };
                if let Some((id, context)) = &identity {
                    if job.binding.is_none() {
                        journal.record(
                            Some(&job.id),
                            "task_bound",
                            json!({"taskId":id,"contextId":context}),
                        )?;
                        job.binding = Some((id.clone(), context.clone()));
                    }
                }
                for (kind, details) in notices {
                    journal.record(Some(&job.id), kind, details)?;
                }
                if version != job.seen {
                    job.seen = version;
                    journal.record(Some(&job.id),"status",json!({"state":state,"reportedTokens":tokens,"responseBytes":job.read_budget.used()+job.stop_budget.used()}))?;
                    observe(A2AStatus {
                        worker: job.id.clone(),
                        state,
                        task_id: identity.map(|t| t.0),
                        response_bytes: job.read_budget.used() + job.stop_budget.used(),
                        reported_tokens: tokens,
                    });
                }
            }
            if job.thread.as_ref().unwrap().is_finished() {
                let mut job = active.remove(i);
                let mut report = match job.thread.take().unwrap().join() {
                    Ok(report) => report,
                    Err(_) => {
                        let _ = job.read_budget.transfer_remaining_to(&job.stop_budget);
                        let mut report = empty(&job.id, Outcome::Lost);
                        report.task = job.shared.lock().unwrap_or_else(|e| e.into_inner()).task.clone();
                        report.stop = if let Some(task) = &report.task {
                            match job.emergency.cancel(&task.id) {
                                Ok(final_task)
                                    if final_task.context_id == task.context_id
                                        && final_task.status.state.is_terminal() =>
                                {
                                    report.task = Some(final_task);
                                    StopConfirmation::Confirmed
                                }
                                _ => StopConfirmation::Unconfirmed,
                            }
                        } else {
                            StopConfirmation::Unconfirmed
                        };
                        report
                    }
                };
                if let Some(reason) = job.cancel_reason.take() {
                    if matches!(report.outcome, Outcome::Canceled | Outcome::Deadline) {
                        report.outcome = reason;
                    }
                }
                report.response_bytes = job.read_budget.used() + job.stop_budget.used();
                let notices: Vec<_> = job
                    .shared
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .notices
                    .drain(..)
                    .collect();
                for (kind, details) in notices {
                    journal.record(Some(&job.id), kind, details)?;
                }
                if job.binding.is_none() {
                    if let Some(task) = &report.task {
                        journal.record(
                            Some(&job.id),
                            "task_bound",
                            json!({"taskId":task.id,"contextId":task.context_id}),
                        )?;
                    }
                }
                finish(&mut journal, &report, &mut observe)?;
                reports.push(report);
            } else {
                i += 1;
            }
        }
        if !active.is_empty() {
            thread::sleep(Duration::from_millis(2));
        }
    }
    reports.sort_by(|a, b| a.worker.cmp(&b.worker));
    journal.record(None,"run_finished",json!({"workers":reports.len(),"completed":reports.iter().filter(|r|r.outcome==Outcome::Completed).count()}))?;
    Ok(A2ARunReport {
        run: settings.id,
        workers: reports,
    })
}

struct WorkerRun {
    run: String,
    deadline: Instant,
    request_timeout: Duration,
    poll_interval: Duration,
    cancel_grace: Duration,
}

fn worker(
    mut spec: A2AWorkerSpec,
    args: WorkerRun,
    controls: Receiver<Control>,
    cancel: Arc<AtomicBool>,
    shared: Arc<Mutex<Snapshot>>,
    read: ResponseBudget,
    stop: ResponseBudget,
    mut cleanup: Client,
) -> A2AWorkerReport {
    let mut collector = Collector::new(spec.response_bytes);
    let mut sent = false;
    let mut steer = 0u64;
    let mut remote_steers: VecDeque<(String, bool)> = VecDeque::new();
    spec.agent.client = spec.agent.client.with_response_budget(read.clone());
    let mut stream = None;
    let mut started = false;
    let mut outcome = Outcome::Failed;
    loop {
        if cancel.load(Ordering::Acquire) {
            outcome = Outcome::Canceled;
            break;
        }
        if Instant::now() >= args.deadline {
            outcome = Outcome::Deadline;
            break;
        }
        let request_bounds = Limits {
            timeout: args.request_timeout.min(
                args.deadline
                    .saturating_duration_since(Instant::now())
                    .max(Duration::from_nanos(1)),
            ),
            body_bytes: spec.response_bytes,
            event_bytes: spec.response_bytes,
        };
        let _ = spec.agent.client.constrain(request_bounds);
        if !started {
            let card = match spec.agent.client.card() {
                Ok(card) => card,
                Err(_) => {
                    if read.remaining() == 0 {
                        outcome = Outcome::OutputBudget;
                    }
                    break;
                }
            };
            if spec.reported_tokens.is_some()
                && !card
                    .capabilities
                    .extensions
                    .iter()
                    .any(|e| e.uri == "urn:oxa:a2a:reported-usage:1")
            {
                outcome = Outcome::PolicyViolation;
                break;
            }
            if cancel.load(Ordering::Acquire) || Instant::now() >= args.deadline {
                continue;
            }
            let params = initial(&args.run, &spec);
            let _ = spec.agent.client.constrain(Limits {
                timeout: args
                    .deadline
                    .saturating_duration_since(Instant::now())
                    .max(Duration::from_nanos(1)),
                ..request_bounds
            });
            sent = true; // A failed observation never authorizes replaying this send.
            if card.capabilities.streaming {
                match spec.agent.client.stream(params) {
                    Ok(events) => stream = Some(events),
                    Err(_) => {
                        outcome = if read.remaining() == 0 {
                            Outcome::OutputBudget
                        } else {
                            Outcome::Lost
                        };
                        break;
                    }
                }
            } else {
                match spec.agent.client.send(params) {
                    Ok(event) => {
                        if collector.apply(event).is_err() {
                            outcome = Outcome::PolicyViolation;
                            break;
                        }
                    }
                    Err(_) => {
                        outcome = if read.remaining() == 0 {
                            Outcome::OutputBudget
                        } else {
                            Outcome::Lost
                        };
                        break;
                    }
                }
            }
            started = true;
        }
        for _ in 0..32 {
            if cancel.load(Ordering::Acquire) || Instant::now() >= args.deadline {
                break;
            }
            match controls.try_recv() {
                Ok(control @ (Control::Steer { .. } | Control::SteerChecked { .. })) => {
                    let (text, admitted) = match control {
                        Control::Steer { text, .. } => (text, None),
                        Control::SteerChecked { text, admitted, .. } => (text, Some(admitted)),
                        _ => unreachable!(),
                    };
                    let mut accepted = false;
                    if let Some(task) = collector.task().filter(|t| !t.status.state.is_terminal()) {
                        if !text.is_empty() && text.len() <= 16 * 1024 && steer < 128 {
                            steer += 1;
                            let message = Message {
                                message_id: format!("{}:{}:steer-{steer}", args.run, spec.id),
                                role: Role::User,
                                parts: vec![Part::Text { text: text.clone() }],
                                task_id: Some(task.id.clone()),
                                context_id: Some(task.context_id.clone()),
                            };
                            let _ = spec.agent.client.constrain(Limits {
                                timeout: args
                                    .deadline
                                    .saturating_duration_since(Instant::now())
                                    .max(Duration::from_nanos(1)),
                                ..request_bounds
                            });
                            if let Ok(Event::Task(task)) = spec.agent.client.send(SendParams {
                                message: MessageObject::Message(message),
                            }) {
                                accepted = collector.reconcile(task).is_ok();
                                stream = None;
                            }
                        }
                    }
                    if let Some(reply) = admitted {
                        let _ = reply.try_send(accepted);
                    }
                    remote_steers.push_back((hash(&text), accepted));
                }
                Ok(_) => cancel.store(true, Ordering::Release),
                Err(_) => break,
            }
        }
        if collector.is_failed() {
            outcome = Outcome::PolicyViolation;
            break;
        }
        if publish(&shared, &collector, &remote_steers).is_err() {
            outcome = Outcome::OutputBudget;
            break;
        }
        remote_steers.clear();
        if cancel.load(Ordering::Acquire) || Instant::now() >= args.deadline {
            continue;
        }
        if let Some(task) = collector.task() {
            if let Some(cap) = spec.reported_tokens {
                if observed_tokens(task).is_some_and(|n| n > cap) {
                    outcome = Outcome::TokenBudget;
                    break;
                }
            }
            if task.status.state.is_terminal() {
                outcome = match task.status.state {
                    TaskState::Completed
                        if spec.reported_tokens.is_some() && observed_tokens(task).is_none() =>
                    {
                        Outcome::Failed
                    }
                    TaskState::Completed => Outcome::Completed,
                    TaskState::Canceled => Outcome::Canceled,
                    TaskState::Rejected => Outcome::PolicyViolation,
                    _ => Outcome::Failed,
                };
                break;
            }
        } else if collector.message().is_some() {
            outcome = if spec.reported_tokens.is_some() {
                Outcome::Failed
            } else {
                Outcome::Completed
            };
            break;
        }
        if let Some(events) = stream.as_mut() {
            match events.next_event() {
                Ok(Some(event)) => {
                    if collector.apply(event).is_err() {
                        outcome = Outcome::PolicyViolation;
                        break;
                    }
                    if publish(&shared, &collector, &remote_steers).is_err() {
                        outcome = Outcome::OutputBudget;
                        break;
                    }
                    if collector.stream_final() {
                        stream = None;
                    }
                    continue;
                }
                Ok(None) => stream = None,
                Err(_) => {
                    stream = None;
                    if read.remaining() == 0 {
                        outcome = Outcome::OutputBudget;
                        break;
                    }
                    if collector.task().is_none() {
                        outcome = Outcome::Lost;
                        break;
                    }
                }
            }
        }
        let Some(task) = collector.task() else {
            outcome = Outcome::Lost;
            break;
        };
        if cancel.load(Ordering::Acquire) || Instant::now() >= args.deadline {
            continue;
        }
        let _ = spec.agent.client.constrain(Limits {
            timeout: args
                .deadline
                .saturating_duration_since(Instant::now())
                .max(Duration::from_nanos(1)),
            ..request_bounds
        });
        match spec.agent.client.get(&task.id) {
            Ok(task) => {
                if collector.reconcile(task).is_err() {
                    outcome = Outcome::PolicyViolation;
                    break;
                }
                if publish(&shared, &collector, &remote_steers).is_err() {
                    outcome = Outcome::OutputBudget;
                    break;
                }
            }
            Err(error) => {
                if read.remaining() == 0 {
                    outcome = Outcome::OutputBudget;
                    break;
                }
                if oxa_a2a::loopback::rpc_error_code(&error) == Some(-32001) {
                    outcome = Outcome::Lost;
                    break;
                }
            }
        }
        thread::sleep(
            args.poll_interval
                .min(args.deadline.saturating_duration_since(Instant::now())),
        );
    }
    drop(stream);
    let mut task = collector.task().cloned();
    let mut confirmation = StopConfirmation::NotNeeded;
    if let Some(known) = task.as_ref().filter(|t| !t.status.state.is_terminal()) {
        let _ = read.transfer_remaining_to(&stop);
        let id = known.id.clone();
        let context = known.context_id.clone();
        let deadline = Instant::now() + args.cancel_grace;
        confirmation = StopConfirmation::Unconfirmed;
        let limits = Limits {
            timeout: args.request_timeout.min(args.cancel_grace),
            body_bytes: spec.response_bytes,
            event_bytes: spec.response_bytes,
        };
        let _ = cleanup.constrain(limits);
        if let Ok(final_task) = cleanup.cancel(&id) {
            if final_task.context_id == context && final_task.status.state.is_terminal() {
                task = Some(final_task);
                confirmation = StopConfirmation::Confirmed;
            }
        }
        while confirmation == StopConfirmation::Unconfirmed
            && Instant::now() < deadline
            && stop.remaining() > 0
        {
            let _ = cleanup.constrain(Limits {
                timeout: deadline
                    .saturating_duration_since(Instant::now())
                    .max(Duration::from_nanos(1)),
                ..limits
            });
            if let Ok(final_task) = cleanup.get(&id) {
                if final_task.context_id == context && final_task.status.state.is_terminal() {
                    task = Some(final_task);
                    confirmation = StopConfirmation::Confirmed;
                    break;
                }
            }
            thread::sleep(args.poll_interval);
        }
    } else if sent && task.is_none() && collector.message().is_none() {
        confirmation = StopConfirmation::Unconfirmed;
    }
    A2AWorkerReport {
        worker: spec.id,
        outcome,
        reported_tokens: task.as_ref().and_then(observed_tokens),
        task,
        message: collector.message().cloned(),
        response_bytes: read.used() + stop.used(),
        stop: confirmation,
    }
}

fn publish(
    shared: &Mutex<Snapshot>,
    collector: &Collector,
    steers: &VecDeque<(String, bool)>,
) -> io::Result<()> {
    let mut snapshot = shared.lock().unwrap_or_else(|e| e.into_inner());
    let state =
        collector
            .task()
            .map(|t| state_name(&t.status.state))
            .unwrap_or(if collector.message().is_some() {
                "completed"
            } else {
                "starting"
            });
    let tokens = collector.task().and_then(observed_tokens);
    if snapshot.notices.len() + steers.len() > 128 {
        return Err(invalid("A2A audit queue limit exceeded"));
    }
    for (hash, accepted) in steers {
        snapshot.notices.push_back((
            if *accepted {
                "steer_admitted"
            } else {
                "steer_unconfirmed"
            },
            json!({"textSha256":hash}),
        ));
    }
    if snapshot.state != state
        || snapshot.tokens != tokens
        || !steers.is_empty()
        || snapshot.task.as_ref().map(|t| &t.artifacts) != collector.task().map(|t| &t.artifacts)
    {
        snapshot.version += 1;
        snapshot.state = state.into();
        snapshot.tokens = tokens;
    }
    snapshot.task = collector.task().cloned();
    Ok(())
}

fn observed_tokens(task: &Task) -> Option<u64> {
    let mut totals = task
        .artifacts
        .iter()
        .filter(|a| a.name.as_deref() == Some("worker usage"))
        .flat_map(|a| &a.parts)
        .filter_map(|p| match p {
            Part::Data { data } => data.get("reportedTokens").and_then(|v| v.as_u64()),
            _ => None,
        });
    let value = totals.next()?;
    if totals.next().is_some() {
        None
    } else {
        Some(value)
    }
}
fn state_name(state: &TaskState) -> &'static str {
    match state {
        TaskState::Submitted => "submitted",
        TaskState::Working => "working",
        TaskState::InputRequired => "input-required",
        TaskState::AuthRequired => "auth-required",
        TaskState::Completed => "completed",
        TaskState::Failed => "failed",
        TaskState::Canceled => "canceled",
        TaskState::Rejected => "rejected",
        TaskState::Unknown => "unknown",
    }
}
fn initial(run: &str, spec: &A2AWorkerSpec) -> SendParams {
    SendParams {
        message: MessageObject::Message(Message {
            message_id: format!("{run}:{}:initial", spec.id),
            role: Role::User,
            parts: vec![Part::Text {
                text: format!("Assigned role:\n{}\nDelegated goal:\n{}", spec.role, spec.task),
            }],
            task_id: None,
            context_id: None,
        }),
    }
}
fn validate(settings: &A2ARunSettings, plan: &[A2AWorkerSpec]) -> io::Result<()> {
    if !valid_id(&settings.id)
        || plan.is_empty()
        || plan.len() > 128
        || settings.parallelism == 0
        || settings.parallelism > 32
        || settings.wall_time.is_zero()
        || settings.wall_time > Duration::from_secs(86400)
        || settings.request_timeout.is_zero()
        || settings.request_timeout > Duration::from_secs(1)
        || settings.poll_interval < Duration::from_millis(1)
        || settings.poll_interval > Duration::from_millis(100)
        || settings.cancel_grace.is_zero()
        || settings.cancel_grace > Duration::from_secs(2)
        || settings.journal_bytes < 1024
    {
        return Err(invalid("invalid A2A run settings"));
    }
    let mut ids = BTreeSet::new();
    let mut bytes = 0usize;
    for spec in plan {
        if !valid_id(&spec.id)
            || !ids.insert(&spec.id)
            || spec.role.is_empty()
            || spec.role.len() > 8192
            || spec.task.is_empty()
            || spec.task.len() > 32 * 1024
            || spec.wall_time.is_zero()
            || spec.wall_time > Duration::from_secs(86400)
            || spec.response_bytes == 0
            || spec.response_bytes > 16 * 1024 * 1024
            || spec.reported_tokens == Some(0)
            || serde_json::to_vec(&initial(&settings.id, spec))?.len() > 32 * 1024
        {
            return Err(invalid("invalid A2A worker plan"));
        }
        bytes = bytes
            .checked_add(spec.response_bytes)
            .ok_or_else(|| invalid("A2A reservation overflow"))?;
    }
    if bytes > settings.response_reservation {
        return Err(invalid("A2A plan exceeds run reservations"));
    }
    Ok(())
}
fn reject_control(control: Control, journal: &mut Journal) -> io::Result<()> {
    let (worker, reply) = match control {
        Control::Steer { worker, .. } => (worker, None),
        Control::SteerChecked { worker, admitted, .. } => (worker, Some(admitted)),
        _ => unreachable!(),
    };
    if let Some(reply) = reply {
        let _ = reply.try_send(false);
    }
    journal.record(
        Some(&worker),
        "control_rejected",
        json!({"reason":"worker_or_queue_unavailable"}),
    )
}
fn finish(
    journal: &mut Journal,
    report: &A2AWorkerReport,
    observe: &mut impl FnMut(A2AStatus),
) -> io::Result<()> {
    let digest = if let Some(task) = &report.task {
        Some(hash(&serde_json::to_string(&task.artifacts)?))
    } else if let Some(message) = &report.message {
        Some(hash(&serde_json::to_string(message)?))
    } else {
        None
    };
    journal.record(Some(&report.worker),"finished",json!({"outcome":report.outcome,"stop":report.stop,"responseBytes":report.response_bytes,"reportedTokens":report.reported_tokens,"resultSha256":digest}))?;
    observe(A2AStatus {
        worker: report.worker.clone(),
        state: format!("{:?}", report.outcome).to_ascii_lowercase(),
        task_id: report.task.as_ref().map(|t| t.id.clone()),
        response_bytes: report.response_bytes,
        reported_tokens: report.reported_tokens,
    });
    Ok(())
}
fn empty(id: &str, outcome: Outcome) -> A2AWorkerReport {
    A2AWorkerReport {
        worker: id.into(),
        outcome,
        task: None,
        message: None,
        response_bytes: 0,
        reported_tokens: None,
        stop: StopConfirmation::NotNeeded,
    }
}
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
fn invalid(message: &'static str) -> io::Error {
    io::Error::other(message)
}
fn hash(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}
fn optional_task<S: serde::Serializer>(task: &Option<Task>, serializer: S) -> Result<S::Ok, S::Error> {
    match task {
        Some(task) => Event::Task(task.clone()).serialize(serializer),
        None => serializer.serialize_none(),
    }
}
