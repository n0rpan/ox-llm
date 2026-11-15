//! Explicit tool-free fan-out/fan-in. No CLI hook, replay or ambient provider.
use crate::journal::Journal;
use crate::native::{checked_program, Launch, NativeWorker, VerifiedLocal};
use crate::rpc::{Outcome as RpcOutcome, RpcSession};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub struct WorkerSpec {
    pub id: String,
    pub role: String,
    pub task: String,
    pub inference: VerifiedLocal,
    pub wall_time: Duration,
    pub output_bytes: usize,
    pub tokens: u64,
}
pub struct Settings {
    pub id: String,
    pub worker_binary: PathBuf,
    pub bwrap_binary: PathBuf,
    /// New journal under an existing private operator-owned parent.
    pub journal: PathBuf,
    pub journal_bytes: usize,
    pub parallelism: usize,
    pub wall_time: Duration,
    pub output_reservation: usize,
    pub token_reservation: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Completed,
    Failed,
    Truncated,
    Canceled,
    Deadline,
    OutputBudget,
    TokenBudget,
    PolicyViolation,
    Lost,
}
impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Truncated => "truncated",
            Self::Canceled => "canceled",
            Self::Deadline => "deadline",
            Self::OutputBudget => "output_budget",
            Self::TokenBudget => "token_budget",
            Self::PolicyViolation => "policy_violation",
            Self::Lost => "lost",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cleanup {
    NotStarted,
    Stopped,
    Unconfirmed,
}
#[derive(Clone)]
pub struct WorkerReport {
    pub worker: String,
    pub outcome: Outcome,
    pub cleanup: Cleanup,
    /// Last valid observations; aborted requests can still lack final usage.
    pub observed_tokens: Option<u64>,
    pub output_bytes: usize,
    pub result: String,
}
pub struct RunReport {
    pub run: String,
    pub workers: Vec<WorkerReport>,
}
impl RunReport {
    pub fn succeeded(&self) -> bool {
        !self.workers.is_empty()
            && self
                .workers
                .iter()
                .all(|w| w.outcome == Outcome::Completed && w.cleanup == Cleanup::Stopped)
    }
    pub fn cleanup_confirmed(&self) -> bool {
        self.workers.iter().all(|w| w.cleanup != Cleanup::Unconfirmed)
    }
}
/// Metadata only. Observers are trusted and must return promptly; they must not
/// perform blocking UI/network work on the supervisor thread.
pub struct Status {
    pub worker: String,
    pub state: &'static str,
    pub observed_tokens: Option<u64>,
    pub output_bytes: usize,
}
enum Control {
    Cancel(String),
    Steer {
        worker: String,
        text: String,
        admitted: SyncSender<bool>,
    },
}
#[derive(Clone)]
pub struct Controller {
    sender: SyncSender<Control>,
    cancel_all: Arc<AtomicBool>,
}
pub struct Inbox {
    receiver: Receiver<Control>,
    cancel_all: Arc<AtomicBool>,
}
pub fn controls() -> (Controller, Inbox) {
    let (sender, receiver) = mpsc::sync_channel(32);
    let cancel_all = Arc::new(AtomicBool::new(false));
    (
        Controller {
            sender,
            cancel_all: cancel_all.clone(),
        },
        Inbox { receiver, cancel_all },
    )
}
impl Controller {
    /// Sticky out-of-band cancellation remains admissible when the queue is full.
    pub fn cancel_all(&self) {
        self.cancel_all.store(true, Ordering::Release);
    }
    pub fn cancel(&self, worker: String) -> io::Result<()> {
        if !valid_id(&worker) {
            return Err(invalid("invalid worker identity"));
        }
        self.send(Control::Cancel(worker))
    }
    /// The returned acknowledgement concerns bounded RPC queue admission only.
    pub fn steer(&self, worker: String, text: String) -> io::Result<Receiver<bool>> {
        if !valid_id(&worker) || text.trim().is_empty() || text.len() > 64 * 1024 {
            return Err(invalid("invalid worker steering"));
        }
        let (admitted, reply) = mpsc::sync_channel(1);
        self.send(Control::Steer {
            worker,
            text,
            admitted,
        })?;
        Ok(reply)
    }
    fn send(&self, control: Control) -> io::Result<()> {
        self.sender.try_send(control).map_err(|e| match e {
            TrySendError::Full(_) => io::Error::new(io::ErrorKind::WouldBlock, "control queue full"),
            TrySendError::Disconnected(_) => {
                io::Error::new(io::ErrorKind::BrokenPipe, "run no longer accepts controls")
            }
        })
    }
}
struct Active {
    spec: WorkerSpec,
    process: NativeWorker,
    rpc: RpcSession,
    forced: Option<Outcome>,
}

/// Run one explicit bounded plan. Dropping the controller does not cancel a
/// run; cancel_all is explicit. No user text, model output or credentials are
/// copied into the journal. The route assertion remains the operator's duty.
pub fn run(
    settings: Settings,
    plan: Vec<WorkerSpec>,
    inbox: Inbox,
    mut observe: impl FnMut(Status),
) -> io::Result<RunReport> {
    validate(&settings, &plan)?;
    let binary = checked_program(&settings.worker_binary)?;
    let bwrap = checked_program(&settings.bwrap_binary)?;
    let deadline = Instant::now()
        .checked_add(settings.wall_time)
        .ok_or_else(|| invalid("invalid run deadline"))?;
    let order: BTreeMap<_, _> = plan.iter().enumerate().map(|(i, w)| (w.id.clone(), i)).collect();
    let mut journal = Journal::create(&settings.journal, settings.journal_bytes)?;
    journal.record(None,"run_started",json!({"run":settings.id,"workers":plan.len(),"parallelism":settings.parallelism,"mode":"plan","offline":true,"toolPolicy":"none","profile":"tool-free-rpc-v2"}))?;
    for spec in &plan {
        journal.record(Some(&spec.id),"queued",json!({"roleSha256":hash(&spec.role),"taskSha256":hash(&spec.task),"route":"verified-local","reservedTokens":spec.tokens,"reservedBytes":spec.output_bytes,"tools":[]}))?;
        observe(status(spec, "queued", None, 0));
    }
    let mut pending = VecDeque::from(plan);
    let mut active: Vec<Active> = Vec::new();
    let mut reports = Vec::new();
    let mut forced = None;
    while !pending.is_empty() || !active.is_empty() {
        if inbox.cancel_all.load(Ordering::Acquire) {
            forced.get_or_insert(Outcome::Canceled);
        }
        if Instant::now() >= deadline {
            forced.get_or_insert(Outcome::Deadline);
        }
        for _ in 0..32 {
            match inbox.receiver.try_recv() {
                Ok(Control::Cancel(id)) => {
                    if let Some(index) = pending.iter().position(|w| w.id == id) {
                        let spec = pending.remove(index).unwrap();
                        publish(
                            &mut journal,
                            &mut observe,
                            not_started(spec, Outcome::Canceled),
                            &mut reports,
                        )?;
                    } else if let Some(worker) = active.iter_mut().find(|w| w.spec.id == id) {
                        worker.forced = Some(Outcome::Canceled);
                    } else {
                        journal.record(None, "control_rejected", json!({"reason":"worker_not_live"}))?;
                    }
                }
                Ok(Control::Steer {
                    worker: id,
                    text,
                    admitted,
                }) => {
                    let mut accepted = false;
                    if forced.is_none() {
                        if let Some(worker) =
                            active.iter_mut().find(|w| w.spec.id == id && w.forced.is_none())
                        {
                            if let Ok(command) = worker.rpc.steer(&text) {
                                accepted = worker.process.queue(&command).is_ok();
                                if !accepted {
                                    worker.forced = Some(Outcome::Failed);
                                }
                            }
                        }
                    }
                    let _ = admitted.try_send(accepted);
                    if accepted {
                        journal.record(Some(&id), "steer_admitted", json!({"textSha256":hash(&text)}))?;
                    } else {
                        journal.record(
                            None,
                            "control_rejected",
                            json!({"reason":"steering_not_admitted","textSha256":hash(&text)}),
                        )?;
                    }
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        // Never start queued work after global cancellation/deadline/budget.
        if let Some(outcome) = forced {
            while let Some(spec) = pending.pop_front() {
                publish(
                    &mut journal,
                    &mut observe,
                    not_started(spec, outcome),
                    &mut reports,
                )?;
            }
        }
        while forced.is_none() && active.len() < settings.parallelism {
            if inbox.cancel_all.load(Ordering::Acquire) || Instant::now() >= deadline {
                break;
            }
            let Some(spec) = pending.pop_front() else {
                break;
            };
            journal.record(Some(&spec.id), "starting", json!({}))?;
            let rpc = RpcSession::new(spec.inference.model().into(), spec.task.clone(), spec.tokens)?;
            let launch = Launch {
                worker_binary: binary.clone(),
                bwrap_binary: bwrap.clone(),
                inference: spec.inference.clone(),
                role: spec.role.clone(),
                wall_time: spec
                    .wall_time
                    .min(deadline.saturating_duration_since(Instant::now())),
                output_bytes: spec.output_bytes,
            };
            match NativeWorker::spawn(launch) {
                Ok(mut process) => {
                    journal.record(Some(&spec.id), "spawned", json!({"processId":process.pid()}))?;
                    let forced = process
                        .queue(&rpc.initial_command())
                        .err()
                        .map(|_| Outcome::Failed);
                    observe(status(&spec, "started", None, 0));
                    active.push(Active {
                        spec,
                        process,
                        rpc,
                        forced,
                    });
                }
                Err(error) => {
                    let mut report = not_started(spec, Outcome::Failed);
                    if let Some((pid, stopped)) = crate::native::startup_evidence(&error) {
                        journal.record(Some(&report.worker), "spawned", json!({"processId":pid}))?;
                        report.cleanup = if stopped {
                            Cleanup::Stopped
                        } else {
                            Cleanup::Unconfirmed
                        };
                        if !stopped {
                            report.outcome = Outcome::Lost;
                        }
                    }
                    publish(&mut journal, &mut observe, report, &mut reports)?;
                }
            }
        }
        let mut index = 0;
        while index < active.len() {
            if let Some(reason) = aggregate_limit(&reports, &active, &settings) {
                forced.get_or_insert(reason);
            }
            if inbox.cancel_all.load(Ordering::Acquire) {
                forced.get_or_insert(Outcome::Canceled);
            }
            if Instant::now() >= deadline {
                forced.get_or_insert(Outcome::Deadline);
            }
            let worker = &mut active[index];
            let mut outcome = forced.or(worker.forced);
            if outcome.is_none() {
                match worker.process.poll() {
                    Ok(records) => {
                        for record in records {
                            match worker.rpc.accept(record) {
                                Ok(update) => {
                                    for command in update.commands {
                                        if command["type"] == "prompt" {
                                            journal.record(
                                                Some(&worker.spec.id),
                                                "policy_verified",
                                                json!({"mode":"plan","toolPolicy":"none"}),
                                            )?;
                                            observe(status(
                                                &worker.spec,
                                                "ready",
                                                worker.rpc.observed_tokens(),
                                                worker.process.output_bytes(),
                                            ));
                                        }
                                        if inbox.cancel_all.load(Ordering::Acquire) {
                                            outcome = Some(Outcome::Canceled);
                                            break;
                                        }
                                        if Instant::now() >= deadline {
                                            outcome = Some(Outcome::Deadline);
                                            break;
                                        }
                                        if let Err(error) = worker.process.queue(&command) {
                                            outcome = Some(classify(&error, &worker.process, &worker.spec));
                                            break;
                                        }
                                        if command["type"] == "prompt" {
                                            journal.record(Some(&worker.spec.id), "working", json!({}))?;
                                            observe(status(
                                                &worker.spec,
                                                "working",
                                                worker.rpc.observed_tokens(),
                                                worker.process.output_bytes(),
                                            ));
                                        }
                                    }
                                    if outcome.is_none() {
                                        outcome = update.outcome.map(from_rpc);
                                    }
                                }
                                Err(_) => outcome = Some(Outcome::Failed),
                            }
                            if outcome.is_some() {
                                break;
                            }
                        }
                        if outcome.is_none()
                            && worker.process.process_stopped()?
                            && worker.process.output_eof()
                        {
                            outcome = Some(Outcome::Lost);
                        }
                    }
                    Err(error) => {
                        outcome = Some(if error.kind() == io::ErrorKind::TimedOut {
                            Outcome::Deadline
                        } else if worker.process.output_bytes() > worker.spec.output_bytes {
                            Outcome::OutputBudget
                        } else {
                            Outcome::Failed
                        })
                    }
                }
            }
            if let Some(outcome) = outcome {
                let mut worker = active.swap_remove(index);
                let cleanup = if worker.process.stop(Duration::from_millis(100)).is_ok()
                    && worker.process.process_stopped()?
                {
                    Cleanup::Stopped
                } else {
                    Cleanup::Unconfirmed
                };
                let report = WorkerReport {
                    worker: worker.spec.id,
                    outcome: if cleanup == Cleanup::Unconfirmed {
                        Outcome::Lost
                    } else {
                        outcome
                    },
                    cleanup,
                    observed_tokens: worker.rpc.observed_tokens(),
                    output_bytes: worker.process.output_bytes(),
                    result: worker.rpc.result().into(),
                };
                publish(&mut journal, &mut observe, report, &mut reports)?;
            } else {
                index += 1;
            }
        }
        if let Some(reason) = aggregate_limit(&reports, &active, &settings) {
            forced.get_or_insert(reason);
        }
        if !pending.is_empty() || !active.is_empty() {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    reports.sort_by_key(|r| order[&r.worker]);
    let report = RunReport {
        run: settings.id,
        workers: reports,
    };
    if report.cleanup_confirmed() {
        journal.record(None, "run_finished", json!({"workers":report.workers.len(),"completed":report.workers.iter().filter(|w|w.outcome==Outcome::Completed).count(),"succeeded":report.succeeded()}))?;
    } else {
        journal.record(None, "cleanup_unconfirmed", json!({}))?;
    }
    Ok(report)
}
pub(crate) fn validate(settings: &Settings, plan: &[WorkerSpec]) -> io::Result<()> {
    if !valid_id(&settings.id)
        || plan.is_empty()
        || plan.len() > 128
        || !(1..=32).contains(&settings.parallelism)
        || settings.wall_time.is_zero()
        || settings.wall_time > Duration::from_secs(86400)
        || !(1024..=16 * 1024 * 1024).contains(&settings.journal_bytes)
    {
        return Err(invalid("invalid run settings"));
    }
    let mut ids = BTreeSet::new();
    let mut bytes = 0usize;
    let mut tokens = 0u64;
    for spec in plan {
        if !valid_id(&spec.id)
            || !ids.insert(&spec.id)
            || spec.role.trim().is_empty()
            || spec.role.len() > 16 * 1024
            || spec.role.contains('\0')
            || spec.wall_time.is_zero()
            || spec.wall_time > Duration::from_secs(86400)
            || spec.output_bytes == 0
            || spec.tokens == 0
        {
            return Err(invalid("invalid worker specification"));
        }
        RpcSession::new(spec.inference.model().into(), spec.task.clone(), spec.tokens)?;
        bytes = bytes
            .checked_add(spec.output_bytes)
            .ok_or_else(|| invalid("output reservation overflow"))?;
        tokens = tokens
            .checked_add(spec.tokens)
            .ok_or_else(|| invalid("token reservation overflow"))?;
    }
    if bytes > settings.output_reservation || tokens > settings.token_reservation {
        return Err(invalid("plan exceeds run reservations"));
    }
    Ok(())
}
fn publish(
    journal: &mut Journal,
    observe: &mut impl FnMut(Status),
    report: WorkerReport,
    reports: &mut Vec<WorkerReport>,
) -> io::Result<()> {
    let event = if report.cleanup == Cleanup::Unconfirmed {
        "cleanup_unconfirmed"
    } else {
        "finished"
    };
    journal.record(Some(&report.worker),event,json!({"outcome":report.outcome.as_str(),"reportedTokens":report.observed_tokens,"outputBytes":report.output_bytes,"processStopped":report.cleanup==Cleanup::Stopped,"cleanup":report.cleanup,"resultSha256":hash(&report.result)}))?;
    observe(Status {
        worker: report.worker.clone(),
        state: if report.cleanup == Cleanup::Unconfirmed {
            "cleanup_unconfirmed"
        } else {
            report.outcome.as_str()
        },
        observed_tokens: report.observed_tokens,
        output_bytes: report.output_bytes,
    });
    reports.push(report);
    Ok(())
}
fn not_started(spec: WorkerSpec, outcome: Outcome) -> WorkerReport {
    WorkerReport {
        worker: spec.id,
        outcome,
        cleanup: Cleanup::NotStarted,
        observed_tokens: None,
        output_bytes: 0,
        result: String::new(),
    }
}
fn status(
    spec: &WorkerSpec,
    state: &'static str,
    observed_tokens: Option<u64>,
    output_bytes: usize,
) -> Status {
    Status {
        worker: spec.id.clone(),
        state,
        observed_tokens,
        output_bytes,
    }
}
fn from_rpc(outcome: RpcOutcome) -> Outcome {
    match outcome {
        RpcOutcome::Completed => Outcome::Completed,
        RpcOutcome::Canceled => Outcome::Canceled,
        RpcOutcome::Truncated => Outcome::Truncated,
        RpcOutcome::TokenBudget => Outcome::TokenBudget,
        RpcOutcome::Failed => Outcome::Failed,
    }
}
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}
fn hash(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}
fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn aggregate_limit(reports: &[WorkerReport], active: &[Active], settings: &Settings) -> Option<Outcome> {
    let output = reports
        .iter()
        .map(|r| r.output_bytes)
        .chain(active.iter().map(|w| w.process.output_bytes()))
        .try_fold(0usize, usize::checked_add);
    let tokens = reports
        .iter()
        .filter_map(|r| r.observed_tokens)
        .chain(active.iter().filter_map(|w| w.rpc.observed_tokens()))
        .try_fold(0u64, u64::checked_add);
    if output.is_none_or(|n| n > settings.output_reservation) {
        Some(Outcome::OutputBudget)
    } else if tokens.is_none_or(|n| n > settings.token_reservation) {
        Some(Outcome::TokenBudget)
    } else {
        None
    }
}

fn classify(error: &io::Error, process: &NativeWorker, spec: &WorkerSpec) -> Outcome {
    if error.kind() == io::ErrorKind::TimedOut {
        Outcome::Deadline
    } else if process.output_bytes() > spec.output_bytes {
        Outcome::OutputBudget
    } else {
        Outcome::Failed
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use oxa_fake::TestEnv;
    use std::fs;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    const RUNTIME: &str = r#"#!/usr/bin/python3 -I
import sys,json,socket
PROMPT_PORT=0
state={'model':{'id':'mock'},'mode':'plan','isStreaming':False,'autoRetryEnabled':True,'autoCompactionEnabled':True}
def send(v): print(json.dumps(v),flush=True)
def finish(text,stop='stop'):
 send({'type':'message_end','message':{'role':'assistant','content':[{'type':'text','text':text}],'stopReason':stop,'usage':{'input':10,'output':2,'cacheRead':3,'cacheWrite':4}}})
 send({'type':'agent_settled','aborted':False})
for line in sys.stdin:
 request=json.loads(line);command=request['type'];data=None
 if command=='abort': break
 if command=='get_state': data=state
 elif command=='set_auto_retry':state['autoRetryEnabled']=request['enabled']
 elif command=='set_auto_compaction':state['autoCompactionEnabled']=request['enabled']
 send({'type':'response','id':request['id'],'command':command,'success':True,'data':data})
 if command=='prompt':
  if PROMPT_PORT:
   lease=socket.create_connection(('127.0.0.1',PROMPT_PORT));lease.sendall(b'p');lease.close()
  task=request['message']
  if task=='wait-for-steer':continue
  if task=='length':finish('attributed partial','length')
  elif task=='flood':finish('x'*8192)
  elif task=='large-exit':finish('x'*100000);sys.exit(0)
  else:finish('reply:'+task)
 elif command=='steer':finish('steered:'+request['message'])
"#;
    pub(crate) fn fixture(tasks: &[&str]) -> (TestEnv, Settings, Vec<WorkerSpec>) {
        let env = TestEnv::new();
        let binary = env.write("runtime.py", RUNTIME);
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
        let private = env.root().join("private");
        fs::DirBuilder::new().mode(0o700).create(&private).unwrap();
        let settings = Settings {
            id: "mock-run".into(),
            worker_binary: binary,
            bwrap_binary: "/usr/bin/bwrap".into(),
            journal: private.join("audit.jsonl"),
            journal_bytes: 64 * 1024,
            parallelism: 2,
            wall_time: Duration::from_secs(5),
            output_reservation: tasks.len() * 128 * 1024,
            token_reservation: tasks.len() as u64 * 1000,
        };
        let plan = tasks
            .iter()
            .enumerate()
            .map(|(i, t)| WorkerSpec {
                id: format!("worker-{i}"),
                role: "synthetic-private-role".into(),
                task: (*t).into(),
                inference: VerifiedLocal::register("127.0.0.1:1".parse().unwrap(), "mock".into()).unwrap(),
                wall_time: Duration::from_secs(5),
                output_bytes: 128 * 1024,
                tokens: 1000,
            })
            .collect();
        (env, settings, plan)
    }
    #[test]
    fn bounded_fanout_retains_every_attributed_result_and_redacts_the_journal() {
        let (_env, mut settings, plan) = fixture(&["synthetic-private-task", "second", "third"]);
        settings.parallelism = 1;
        let path = settings.journal.clone();
        let mut live = 0;
        let mut maximum = 0;
        let (_controller, inbox) = controls();
        let report = run(settings, plan, inbox, |s| {
            if s.state == "started" {
                live += 1;
                maximum = maximum.max(live);
            }
            if s.state == "completed" {
                live -= 1;
            }
        })
        .unwrap();
        assert!(report.succeeded());
        assert!(report.cleanup_confirmed());
        assert_eq!(maximum, 1);
        assert_eq!(live, 0);
        assert_eq!(
            report
                .workers
                .iter()
                .map(|w| w.worker.as_str())
                .collect::<Vec<_>>(),
            ["worker-0", "worker-1", "worker-2"]
        );
        assert_eq!(report.workers[0].result, "reply:synthetic-private-task");
        assert!(report
            .workers
            .iter()
            .all(|w| w.observed_tokens == Some(19) && w.cleanup == Cleanup::Stopped));
        let text = fs::read_to_string(&path).unwrap();
        for raw in ["synthetic-private-task", "synthetic-private-role", "reply:"] {
            assert!(!text.contains(raw));
        }
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        let records: Vec<serde_json::Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        for (i, record) in records.iter().enumerate() {
            assert_eq!(record["sequence"], i);
        }
        assert_eq!(records.last().unwrap()["event"], "run_finished");
        assert_eq!(records.last().unwrap()["details"]["completed"], 3);
    }
    #[test]
    fn partial_fanin_and_large_final_output_are_not_silent_success_or_lost_bytes() {
        let (_env, settings, plan) = fixture(&["normal", "length", "large-exit"]);
        let (_controller, inbox) = controls();
        let report = run(settings, plan, inbox, |_| {}).unwrap();
        assert!(!report.succeeded());
        assert!(report.cleanup_confirmed());
        assert_eq!(report.workers[0].outcome, Outcome::Completed);
        assert_eq!(report.workers[1].outcome, Outcome::Truncated);
        assert_eq!(report.workers[1].result, "attributed partial");
        assert_eq!(report.workers[2].outcome, Outcome::Completed);
        assert_eq!(report.workers[2].result.len(), 100000);
    }
    #[test]
    fn sticky_cancellation_works_with_a_full_control_queue_without_launching() {
        let (_env, settings, plan) = fixture(&["normal", "normal"]);
        let path = settings.journal.clone();
        let (controller, inbox) = controls();
        for _ in 0..32 {
            controller.cancel("unknown".into()).unwrap();
        }
        assert_eq!(
            controller.cancel("unknown".into()).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        controller.cancel_all();
        let report = run(settings, plan, inbox, |_| {}).unwrap();
        assert!(report
            .workers
            .iter()
            .all(|w| w.outcome == Outcome::Canceled && w.cleanup == Cleanup::NotStarted));
        assert!(!fs::read_to_string(path).unwrap().contains("\"starting\""));
        assert_eq!(
            controller.cancel("unknown".into()).unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
    }
    #[test]
    fn cancellation_at_prompt_boundary_prevents_admission_and_mock_side_effect() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let (_env, settings, plan) = fixture(&["normal"]);
        let port = listener.local_addr().unwrap().port();
        fs::write(
            &settings.worker_binary,
            RUNTIME.replace("PROMPT_PORT=0", &format!("PROMPT_PORT={port}")),
        )
        .unwrap();
        let (controller, inbox) = controls();
        let mut boundary = false;
        let report = run(settings, plan, inbox, |s| {
            if s.state == "ready" {
                boundary = true;
                controller.cancel_all();
            }
        })
        .unwrap();
        assert!(boundary);
        assert_eq!(report.workers[0].outcome, Outcome::Canceled);
        assert_eq!(report.workers[0].cleanup, Cleanup::Stopped);
        assert!(report.workers[0].result.is_empty());
        assert!(matches!(listener.accept(),Err(error) if error.kind()==io::ErrorKind::WouldBlock));
    }
    #[test]
    fn steering_acknowledges_queue_admission_and_then_collects_the_actual_result() {
        let (_env, settings, plan) = fixture(&["wait-for-steer"]);
        let (controller, inbox) = controls();
        let mut acknowledgement = None;
        let report = run(settings, plan, inbox, |s| {
            if s.state == "working" {
                acknowledgement = Some(controller.steer(s.worker, "synthetic-control".into()).unwrap());
            }
        })
        .unwrap();
        assert!(acknowledgement.unwrap().try_recv().unwrap());
        assert!(report.succeeded());
        assert_eq!(report.workers[0].result, "steered:synthetic-control");
    }
    #[test]
    fn per_worker_and_run_deadlines_stop_owned_work_and_queued_lanes() {
        let (_env, mut settings, mut plan) = fixture(&["wait-for-steer", "normal"]);
        settings.parallelism = 1;
        plan[0].wall_time = Duration::from_millis(100);
        let (_controller, inbox) = controls();
        let report = run(settings, plan, inbox, |_| {}).unwrap();
        assert_eq!(report.workers[0].outcome, Outcome::Deadline);
        assert_eq!(report.workers[0].cleanup, Cleanup::Stopped);
        assert_eq!(report.workers[1].outcome, Outcome::Completed);
        let (_env, mut settings, plan) = fixture(&["wait-for-steer", "normal"]);
        settings.parallelism = 1;
        settings.wall_time = Duration::from_millis(100);
        let (_controller, inbox) = controls();
        let report = run(settings, plan, inbox, |_| {}).unwrap();
        assert!(report.workers.iter().all(|w| w.outcome == Outcome::Deadline));
        assert_eq!(report.workers[1].cleanup, Cleanup::NotStarted);
        assert!(report.cleanup_confirmed());
    }
    #[test]
    fn observed_output_and_token_overruns_are_attributed_without_clamping() {
        let (_env, mut settings, mut plan) = fixture(&["flood"]);
        plan[0].output_bytes = 4096;
        settings.output_reservation = 4096;
        let (_controller, inbox) = controls();
        let report = run(settings, plan, inbox, |_| {}).unwrap();
        assert_eq!(report.workers[0].outcome, Outcome::OutputBudget);
        assert_eq!(report.workers[0].output_bytes, 4097);
        assert!(report.cleanup_confirmed());
        let (_env, mut settings, mut plan) = fixture(&["normal"]);
        plan[0].tokens = 10;
        settings.token_reservation = 10;
        let (_controller, inbox) = controls();
        let report = run(settings, plan, inbox, |_| {}).unwrap();
        assert_eq!(report.workers[0].outcome, Outcome::TokenBudget);
        assert_eq!(report.workers[0].observed_tokens, Some(19));
        assert!(!report.succeeded());
    }
    #[test]
    fn invalid_reservations_and_duplicate_ids_fail_before_journal_creation() {
        for kind in 0..3 {
            let (_env, mut settings, mut plan) = fixture(&["normal", "normal"]);
            let path = settings.journal.clone();
            match kind {
                0 => settings.output_reservation = 1,
                1 => plan[1].id = plan[0].id.clone(),
                _ => settings.worker_binary = "/nonexistent-a2a-worker".into(),
            }
            let (_controller, inbox) = controls();
            assert!(run(settings, plan, inbox, |_| {}).is_err());
            assert!(!path.exists());
        }
    }
    #[test]
    fn parallel_lanes_are_live_before_the_first_settlement() {
        let (_env, settings, plan) = fixture(&["normal", "normal"]);
        let (_controller, inbox) = controls();
        let mut live = 0;
        let mut maximum = 0;
        let report = run(settings, plan, inbox, |s| {
            if s.state == "started" {
                live += 1;
                maximum = maximum.max(live);
            }
            if s.state == "completed" {
                live -= 1;
            }
        })
        .unwrap();
        assert!(report.succeeded());
        assert_eq!(maximum, 2);
        assert_eq!(live, 0);
    }
    #[test]
    fn targeted_cancel_reaps_the_live_lane_and_never_launches_the_queued_lane() {
        let (_env, mut settings, plan) = fixture(&["wait-for-steer", "normal"]);
        settings.parallelism = 1;
        let (controller, inbox) = controls();
        let report = run(settings, plan, inbox, |s| {
            if s.state == "working" {
                controller.cancel("worker-0".into()).unwrap();
                controller.cancel("worker-1".into()).unwrap();
            }
        })
        .unwrap();
        assert!(report.workers.iter().all(|w| w.outcome == Outcome::Canceled));
        assert_eq!(report.workers[0].cleanup, Cleanup::Stopped);
        assert_eq!(report.workers[1].cleanup, Cleanup::NotStarted);
        assert!(report.cleanup_confirmed());
        assert!(!report.succeeded());
    }
    #[test]
    fn aggregate_overrun_preserves_observation_and_stops_queued_work() {
        let (_env, mut settings, mut plan) = fixture(&["normal", "normal"]);
        settings.parallelism = 1;
        settings.token_reservation = 2;
        for spec in &mut plan {
            spec.tokens = 1;
        }
        let (_controller, inbox) = controls();
        let report = run(settings, plan, inbox, |_| {}).unwrap();
        assert_eq!(report.workers[0].observed_tokens, Some(19));
        assert_eq!(report.workers[0].outcome, Outcome::TokenBudget);
        assert_eq!(report.workers[1].cleanup, Cleanup::NotStarted);
        assert_eq!(report.workers[1].outcome, Outcome::TokenBudget);
        assert!(report.cleanup_confirmed());
        assert!(!report.succeeded());
    }
    #[test]
    fn aggregate_counter_overflow_exhausts_instead_of_wrapping_or_clamping() {
        let (_env, mut settings, _plan) = fixture(&["normal"]);
        settings.token_reservation = u64::MAX;
        let reports = [
            WorkerReport {
                worker: "first".into(),
                outcome: Outcome::TokenBudget,
                cleanup: Cleanup::Stopped,
                observed_tokens: Some(u64::MAX),
                output_bytes: 0,
                result: String::new(),
            },
            WorkerReport {
                worker: "second".into(),
                outcome: Outcome::TokenBudget,
                cleanup: Cleanup::Stopped,
                observed_tokens: Some(1),
                output_bytes: 0,
                result: String::new(),
            },
        ];
        assert_eq!(
            aggregate_limit(&reports, &[], &settings),
            Some(Outcome::TokenBudget)
        );
    }
}
