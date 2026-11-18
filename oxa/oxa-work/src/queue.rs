//! Directory queue: oldest regular JSON files first, with bounded independent workers.
use super::{blocked_record, board, checked, state, LockedTask, Record, Result, RunPolicy, Task};
use serde::{
    de::{Error, IgnoredAny, SeqAccess, Visitor},
    Deserialize, Deserializer, Serialize,
};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    process::Command,
    sync::{mpsc, Arc},
    thread,
    time::{Duration, SystemTime},
};

mod native;
pub use native::{native_task, NativeQueueOwner};

#[derive(Clone, Debug)]
pub struct QueueOptions {
    pub max: usize,
    pub min_free_bytes: u64,
    pub once: bool,
}
impl Default for QueueOptions {
    fn default() -> Self {
        Self {
            max: 1,
            min_free_bytes: 512 * 1024 * 1024,
            once: false,
        }
    }
}
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct QueueStatus {
    pub time: u64,
    pub active: Vec<String>,
    pub pending: usize,
    pub finished: usize,
    pub failed: usize,
    pub free_bytes: Option<u64>,
    pub disk_blocked: bool,
    pub disk_error: Option<String>,
}
#[derive(Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum QueueEvent {
    State {
        #[serde(flatten)]
        status: QueueStatus,
    },
    Task {
        #[serde(flatten)]
        record: Record,
    },
    Error {
        file: PathBuf,
        detail: String,
    },
    GoalControlReceipt {
        task: String,
        receipt: ControlReceipt,
    },
}

#[derive(Serialize)]
pub struct ControlReceipt {
    id: Option<String>,
    admitted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'static str>,
}

impl ControlReceipt {
    pub(crate) fn from_worker(value: &serde_json::Value) -> Option<Self> {
        if value["type"] != "goal_control_receipt" {
            return None;
        }
        let data = &value["data"];
        let admitted = data["admitted"].as_bool()?;
        let id = match data.get("id") {
            None if !admitted => None,
            Some(id) => Some(
                id.as_str()
                    .filter(|id| !id.is_empty() && id.len() <= 128 && !id.chars().any(char::is_control))?
                    .to_owned(),
            ),
            None => return None,
        };
        let error = if admitted {
            if data.get("error").is_some() {
                return None;
            }
            None
        } else {
            Some(match data["error"].as_str()? {
                "goal_steer_stale" => "goal_steer_stale",
                "control_queue_full" => "control_queue_full",
                "unsupported_control" => "unsupported_control",
                "invalid_request" | "request_too_large" | "invalid_id" | "invalid_steer"
                | "invalid_lease" => "invalid_request",
                _ => "not_admitted",
            })
        };
        Some(Self { id, admitted, error })
    }
}

pub(crate) type ControlSink = dyn Fn(ControlReceipt) + Send + Sync;

enum WorkerEvent {
    Finished(Box<(Job, Result<Record>)>),
    Control { task: String, receipt: ControlReceipt },
}

#[derive(Clone)]
struct Job {
    task: Task,
    modified: SystemTime,
    file: PathBuf,
    index: usize,
    binding: Option<board::NativeBinding>,
}

type WorkerFn = dyn Fn(&Task) -> Result<Record> + Send + Sync;
type NativeWorkerFn = dyn Fn(&Task, &mut LockedTask<'_>) -> Result<Record> + Send + Sync;

#[derive(Clone)]
enum QueueWorker {
    Manual(Arc<WorkerFn>),
    Native(Arc<NativeWorker>),
}

struct NativeWorker {
    claims: board::Claims,
    worker_id: String,
    policy: RunPolicy,
    run: Arc<NativeWorkerFn>,
}

impl QueueWorker {
    fn run(&self, job: &mut Job, control: Box<ControlSink>) -> Result<Record> {
        let worker = match self {
            Self::Manual(run) => return run(&job.task),
            Self::Native(worker) => worker,
        };
        let mut locked = LockedTask::acquire(&job.task, &worker.policy)?;
        locked.control_receipt = Some(control);
        let owner = NativeQueueOwner {
            claims: &worker.claims,
            worker_id: &worker.worker_id,
        };
        job.binding = Some(owner.admit(&job.task)?);
        (worker.run)(&job.task, &mut locked)
    }
}
fn busy(storage: &Path, id: &str) -> Result<bool> {
    match fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(storage.join(format!("{id}.lock")))
    {
        Ok(file) => match file.try_lock() {
            Ok(()) => Ok(false),
            Err(fs::TryLockError::WouldBlock) => Ok(true),
            Err(error) => Err(error.to_string()),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.to_string()),
    }
}
fn available(path: &Path, df: &Path, policy: &RunPolicy) -> Result<u64> {
    let mut command = Command::new(df);
    super::isolate_git_environment(&mut command);
    command.env("PATH", &policy.host_path);
    command.args(["--output=avail", "-B1", "--"]).arg(path);
    checked(command)?
        .lines()
        .last()
        .ok_or("df returned no free-space value")?
        .trim()
        .parse()
        .map_err(|_| "df returned an invalid free-space value".into())
}
fn free_space(repo: &Path, common: &Path, queue: &Path, df: &Path, policy: &RunPolicy) -> Result<u64> {
    let trees = super::worktree_root(common)?;
    let tree_volume = trees
        .ancestors()
        .find(|p| p.is_dir())
        .ok_or("worktree storage has no existing parent")?;
    Ok(available(repo, df, policy)?
        .min(available(common, df, policy)?)
        .min(available(&policy.storage, df, policy)?)
        .min(available(queue, df, policy)?)
        .min(available(tree_volume, df, policy)?))
}
const MAX_INPUT_BYTES: u64 = 1024 * 1024;
const MAX_INPUT_TASKS: usize = 128;
const MAX_QUEUE_FILES: usize = 256;
const MAX_QUEUE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_RETAINED_TASKS: usize = 4096;

fn task_bytes(task: &Task) -> u64 {
    (task.id.len() + task.branch.len() + task.base.len() + task.model.len() + task.prompt.len()) as u64
}

struct QueueTasks(Vec<Task>);
impl<'de> Deserialize<'de> for QueueTasks {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct TasksVisitor;
        impl<'de> Visitor<'de> for TasksVisitor {
            type Value = QueueTasks;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "an array of at most {MAX_INPUT_TASKS} tasks")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<QueueTasks, A::Error> {
                let mut tasks = Vec::new();
                while tasks.len() < MAX_INPUT_TASKS {
                    let Some(task) = seq.next_element()? else {
                        return Ok(QueueTasks(tasks));
                    };
                    tasks.push(task);
                }
                if seq.next_element::<IgnoredAny>()?.is_some() {
                    return Err(A::Error::custom(format!(
                        "task file exceeds {MAX_INPUT_TASKS} tasks"
                    )));
                }
                Ok(QueueTasks(tasks))
            }
        }
        deserializer.deserialize_seq(TasksVisitor)
    }
}

fn read_input(path: &Path) -> Result<Option<Vec<Task>>> {
    // A queued file may be replaced after the directory scan. Open the final
    // component without following links or blocking on a substituted FIFO.
    let file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    let meta = file.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("queue input must be a regular file".into());
    }
    let too_large = || format!("task file exceeds {MAX_INPUT_BYTES} bytes");
    if meta.len() > MAX_INPUT_BYTES {
        return Err(too_large());
    }
    let mut bytes = Vec::new();
    file.take(MAX_INPUT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_INPUT_BYTES {
        return Err(too_large());
    }
    let QueueTasks(tasks) = serde_json::from_slice(&bytes).map_err(|e| format!("task file: {e}"))?;
    super::validate_tasks(tasks).map(Some)
}

fn input_file(entry: fs::DirEntry) -> Result<Option<(PathBuf, SystemTime, u64)>> {
    let path = entry.path();
    if path
        .file_name()
        .is_some_and(|s| s.to_string_lossy().starts_with('.'))
        || path.extension().is_none_or(|s| s != "json")
    {
        return Ok(None);
    }
    // A user may archive an input after read_dir produced this entry.
    let meta = match entry.metadata() {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    if !meta.is_file() {
        return Ok(None);
    }
    Ok(Some((
        path,
        meta.modified().map_err(|e| e.to_string())?,
        meta.len(),
    )))
}
#[derive(Default)]
struct Inputs {
    files: HashMap<PathBuf, (SystemTime, u64)>,
    tasks: HashMap<String, (PathBuf, Task)>,
    errors: HashMap<PathBuf, Vec<String>>,
    branches: HashSet<String>,
    task_bytes: u64,
}
fn scan(
    directory: &Path,
    pending: &mut Vec<Job>,
    inputs: &mut Inputs,
    emit: &mut dyn FnMut(QueueEvent) -> Result<()>,
) -> Result<()> {
    let mut files = vec![];
    let mut bytes = 0;
    for entry in fs::read_dir(directory).map_err(|e| e.to_string())? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.to_string()),
        };
        if let Some(file) = input_file(entry)? {
            if files.len() == MAX_QUEUE_FILES {
                return Err(format!(
                    "queue exceeds {MAX_QUEUE_FILES} task files; archive old inputs"
                ));
            }
            // Oversized files are reported individually by read_input.
            bytes += file.2.min(MAX_INPUT_BYTES);
            if bytes > MAX_QUEUE_BYTES {
                return Err("queue exceeds 16 MiB of task inputs; archive old inputs".into());
            }
            files.push(file);
        }
    }
    let paths: HashSet<_> = files.iter().map(|(p, _, _)| p.clone()).collect();
    let withdrawn: HashSet<_> = pending
        .iter()
        .filter(|job| !paths.contains(&job.file))
        .map(|job| job.task.id.clone())
        .collect();
    pending.retain(|job| paths.contains(&job.file));
    // Active/external workers are not pending and retain their identities.
    for id in withdrawn {
        if let Some((_, task)) = inputs.tasks.remove(&id) {
            inputs.branches.remove(&task.branch);
            inputs.task_bytes -= task_bytes(&task);
        }
    }
    inputs.files.retain(|p, _| paths.contains(p));
    inputs.errors.retain(|p, _| paths.contains(p));
    files.sort_by(|a, b| (&a.1, &a.0).cmp(&(&b.1, &b.0)));
    for (file, modified, len) in files {
        if inputs.files.get(&file) == Some(&(modified, len)) && !inputs.errors.contains_key(&file) {
            continue;
        }
        let tasks = match read_input(&file) {
            Ok(Some(tasks)) => Ok(tasks),
            Ok(None) => continue,
            Err(error) => Err(error),
        };
        let previous = inputs.errors.remove(&file);
        let mut problems = vec![];
        match tasks {
            Ok(tasks) => {
                for (index, task) in tasks.into_iter().enumerate() {
                    if let Some((source, known)) = inputs.tasks.get_mut(&task.id) {
                        if *known == task && (*source == file || !paths.contains(source)) {
                            *source = file.clone();
                            continue;
                        }
                        problems.push(format!("task {} was already queued; use a new id for changed work or remove the duplicate input", task.id));
                    } else {
                        if inputs.branches.contains(&task.branch) {
                            problems.push(format!(
                                "branch {} was already queued; use a distinct branch",
                                task.branch
                            ));
                            continue;
                        }
                        let bytes = task_bytes(&task);
                        if inputs.tasks.len() == MAX_RETAINED_TASKS
                            || inputs.task_bytes + bytes > MAX_QUEUE_BYTES
                        {
                            problems.push("queue task identity capacity reached; archive completed inputs and restart the controller".into());
                            break;
                        }
                        inputs.branches.insert(task.branch.clone());
                        inputs.task_bytes += bytes;
                        inputs.tasks.insert(task.id.clone(), (file.clone(), task.clone()));
                        pending.push(Job {
                            task,
                            file: file.clone(),
                            modified,
                            index,
                            binding: None,
                        });
                    }
                }
            }
            Err(detail) => problems.push(detail),
        }
        inputs.files.insert(file.clone(), (modified, len));
        if !problems.is_empty() {
            if previous.as_ref() != Some(&problems) {
                for detail in &problems {
                    emit(QueueEvent::Error {
                        file: file.clone(),
                        detail: detail.clone(),
                    })?;
                }
            }
            inputs.errors.insert(file, problems);
        }
    }
    Ok(())
}
#[derive(Default)]
struct Workers {
    handles: HashMap<String, thread::JoinHandle<()>>,
}
impl Drop for Workers {
    fn drop(&mut self) {
        // Every error path drains admitted work before the CLI can exit.
        for (_, handle) in self.handles.drain() {
            let _ = handle.join();
        }
    }
}

fn orphan_leases(storage: &Path, active: &Workers, external: &[Job]) -> Result<Vec<String>> {
    let mut ids = vec![];
    for entry in fs::read_dir(storage).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        let Some(id) = name.to_str().and_then(|name| name.strip_suffix(".lock")) else {
            continue;
        };
        if super::valid_task_id(id)
            && !active.handles.contains_key(id)
            && !external.iter().any(|job| job.task.id == id)
            && busy(storage, id)?
        {
            ids.push(id.to_owned());
        }
    }
    Ok(ids)
}

pub fn run_queue(
    repo: &Path,
    directory: &Path,
    options: &QueueOptions,
    policy: &RunPolicy,
    df: &Path,
    worker: Arc<WorkerFn>,
    emit: &mut dyn FnMut(QueueEvent) -> Result<()>,
) -> Result<QueueStatus> {
    run_queue_inner(
        QueueRun {
            repo,
            directory,
            options,
            policy,
            df,
        },
        None,
        None,
        QueueWorker::Manual(worker),
        emit,
    )
}

/// The same execution queue with trusted owner context for a native admitted attempt.
pub struct QueueRun<'a> {
    pub repo: &'a Path,
    pub directory: &'a Path,
    pub options: &'a QueueOptions,
    pub policy: &'a RunPolicy,
    pub df: &'a Path,
}

impl QueueRun<'_> {
    pub fn run_native(
        self,
        owner: &NativeQueueOwner<'_>,
        worker: Arc<NativeWorkerFn>,
        emit: &mut dyn FnMut(QueueEvent) -> Result<()>,
    ) -> Result<QueueStatus> {
        self.run_native_with(owner, None, worker, emit)
    }

    /// Consume one protected spool packet in memory using the existing queue and worker lifecycle.
    pub fn run_native_packet(
        self,
        owner: &NativeQueueOwner<'_>,
        binding: &board::NativeBinding,
        worker: Arc<NativeWorkerFn>,
        emit: &mut dyn FnMut(QueueEvent) -> Result<()>,
    ) -> Result<QueueStatus> {
        if !self.options.once {
            return Err("native packet execution requires a one-shot queue".into());
        }
        if binding.lease.holder != owner.worker_id {
            return Err("native packet belongs to a different worker".into());
        }
        let packet = crate::helper_queue::take_native_packet(self.directory, binding)?;
        let task = native::from_packet(binding, &packet)?;
        self.run_native_with(owner, Some(task), worker, emit)
    }

    fn run_native_with(
        self,
        owner: &NativeQueueOwner<'_>,
        prepared: Option<Task>,
        worker: Arc<NativeWorkerFn>,
        emit: &mut dyn FnMut(QueueEvent) -> Result<()>,
    ) -> Result<QueueStatus> {
        let worker = QueueWorker::Native(Arc::new(NativeWorker {
            claims: owner.claims.clone(),
            worker_id: owner.worker_id.into(),
            policy: self.policy.clone(),
            run: worker,
        }));
        run_queue_inner(self, Some(owner), prepared, worker, emit)
    }
}

fn run_queue_inner(
    queue: QueueRun<'_>,
    native: Option<&NativeQueueOwner<'_>>,
    prepared: Option<Task>,
    worker: QueueWorker,
    emit: &mut dyn FnMut(QueueEvent) -> Result<()>,
) -> Result<QueueStatus> {
    let QueueRun {
        repo,
        directory,
        options,
        policy,
        df,
    } = queue;
    if options.max == 0 {
        return Err("--max must be positive".into());
    }
    let directory = directory.canonicalize().map_err(|e| e.to_string())?;
    if !directory.is_dir() {
        return Err("queue path is not a directory".into());
    }
    let _controller = state::lock(&directory.join(".oxa-queue.lock"))?;
    let common = PathBuf::from(checked(super::git(
        policy,
        repo,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    ))?);
    let storage = &policy.storage;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(storage)
        .map_err(|e| e.to_string())?;
    let mut active = Workers::default();
    // Drop the receiver before joining workers on early output failure.
    let (tx, rx) = mpsc::sync_channel(32);
    let read_directory = prepared.is_none();
    let mut pending: Vec<_> = prepared
        .into_iter()
        .map(|task| Job {
            task,
            modified: SystemTime::UNIX_EPOCH,
            file: directory.clone(),
            index: 0,
            binding: None,
        })
        .collect();
    let (mut external, mut inputs) = (vec![], Inputs::default());
    let (mut finished, mut failed, mut last) = (0, 0, None);
    loop {
        if read_directory {
            scan(&directory, &mut pending, &mut inputs, emit)?;
        }
        // Surviving workers from a dead controller still count toward the concurrency cap.
        let mut waiting = vec![];
        for job in external.drain(..) {
            let job: Job = job;
            if busy(&storage, &job.task.id)? {
                waiting.push(job);
            } else {
                pending.push(job);
            }
        }
        external = waiting;
        let mut ready = vec![];
        for job in pending.drain(..) {
            if busy(&storage, &job.task.id)? {
                external.push(job);
            } else {
                ready.push(job);
            }
        }
        pending = ready;
        let orphans = orphan_leases(storage, &active, &external)?;
        pending.sort_by(|a, b| (&a.modified, &a.file, a.index).cmp(&(&b.modified, &b.file, b.index)));
        let disk = free_space(repo, &common, &directory, df, policy);
        let (mut free_bytes, mut disk_error) = match disk {
            Ok(free) => (Some(free), None),
            Err(error) => (None, Some(error)),
        };
        let mut disk_blocked = free_bytes.is_none_or(|free| free < options.min_free_bytes);
        while !disk_blocked
            && active.handles.len() + external.len() + orphans.len() < options.max
            && !pending.is_empty()
        {
            // Check again for each admission: earlier workers may already be writing.
            match free_space(repo, &common, &directory, df, policy) {
                Ok(free) => {
                    free_bytes = Some(free);
                    disk_error = None;
                    disk_blocked = free < options.min_free_bytes;
                }
                Err(error) => {
                    free_bytes = None;
                    disk_error = Some(error);
                    disk_blocked = true;
                }
            }
            if disk_blocked {
                break;
            }
            let job = pending.remove(0);
            let id = job.task.id.clone();
            let worker = worker.clone();
            let tx = tx.clone();
            let mut input = job.clone();
            let task = id.clone();
            match thread::Builder::new()
                .name(format!("oxa-work-{id}"))
                .spawn(move || {
                    let control_tx = tx.clone();
                    let control = Box::new(move |receipt| {
                        let _ = control_tx.send(WorkerEvent::Control {
                            task: task.clone(),
                            receipt,
                        });
                    });
                    let record = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        worker.run(&mut input, control)
                    }))
                    .unwrap_or_else(|_| {
                        Ok(blocked_record(
                            &input.task,
                            "queue worker panicked; retained state can be resumed".into(),
                        ))
                    });
                    let _ = tx.send(WorkerEvent::Finished(Box::new((input, record))));
                }) {
                Ok(handle) => {
                    active.handles.insert(id, handle);
                }
                Err(error) => {
                    finished += 1;
                    failed += 1;
                    emit(QueueEvent::Task {
                        record: blocked_record(&job.task, error.to_string()),
                    })?;
                }
            }
        }
        let mut ids: Vec<_> = active
            .handles
            .keys()
            .cloned()
            .chain(external.iter().map(|j| j.task.id.clone()))
            .chain(orphans.iter().cloned())
            .collect();
        ids.sort();
        let status = QueueStatus {
            time: state::now_ms() / 1000,
            active: ids,
            pending: pending.len(),
            finished,
            failed: failed + inputs.errors.values().map(Vec::len).sum::<usize>(),
            free_bytes,
            disk_blocked: disk_blocked && !pending.is_empty(),
            disk_error,
        };
        if last.as_ref() != Some(&status) {
            let mut bytes = serde_json::to_vec(&QueueEvent::State {
                status: status.clone(),
            })
            .map_err(|e| e.to_string())?;
            bytes.push(b'\n');
            let status_path = directory.join(".oxa-queue-status.json");
            super::checkpoint::write_atomic(&status_path, &bytes, status_path.exists())?;
            emit(QueueEvent::State {
                status: status.clone(),
            })?;
            last = Some(status.clone());
        }
        if options.once
            && active.handles.is_empty()
            && external.is_empty()
            && orphans.is_empty()
            && (pending.is_empty() || disk_blocked)
        {
            return Ok(status);
        }
        if active.handles.is_empty() {
            thread::sleep(Duration::from_millis(250));
            continue;
        }
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(WorkerEvent::Control { task, receipt }) => {
                emit(QueueEvent::GoalControlReceipt { task, receipt })?
            }
            Ok(WorkerEvent::Finished(completed)) => {
                let (job, record) = *completed;
                if let Some(handle) = active.handles.remove(&job.task.id) {
                    let _ = handle.join();
                }
                let record = if let Some(native) = native {
                    match record.and_then(|record| {
                        let binding = job
                            .binding
                            .as_ref()
                            .ok_or("native admission was not established")?;
                        native.finish(policy, &job.task, binding, &record)?;
                        Ok(record)
                    }) {
                        Ok(record) => record,
                        Err(detail) => {
                            failed += 1;
                            emit(QueueEvent::Error {
                                file: job.file,
                                detail,
                            })?;
                            continue;
                        }
                    }
                } else {
                    if busy(storage, &job.task.id)? {
                        external.push(job);
                        continue;
                    }
                    record?
                };
                finished += 1;
                if record.failed() {
                    failed += 1;
                }
                emit(QueueEvent::Task { record })?;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(error) => return Err(error.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn queue_refuses_oversized_inputs_without_admitting_their_tasks() {
        let dir = std::env::temp_dir().join(format!(
            "oxa-queue-limits-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&dir).unwrap();
        let valid = r#"[{"id":"large","branch":"agent/large","model":"fake","prompt":"Synthetic task"}]"#;
        let mut oversized = valid.to_owned();
        oversized.push_str(&" ".repeat(1024 * 1024));
        fs::write(dir.join("bytes.json"), oversized).unwrap();
        let many: Vec<_> = (0..129)
            .map(|n| {
                serde_json::json!({
                    "id": format!("task-{n}"),
                    "branch": format!("agent/task-{n}"),
                    "model": "fake",
                    "prompt": "Synthetic task"
                })
            })
            .collect();
        fs::write(dir.join("count.json"), serde_json::to_vec(&many).unwrap()).unwrap();
        fs::write(dir.join("outside"), valid.replace("large", "symlink")).unwrap();
        std::os::unix::fs::symlink(dir.join("outside"), dir.join("link.json")).unwrap();

        let mut pending = vec![];
        let mut inputs = Inputs::default();
        let mut errors = vec![];
        let result = scan(&dir, &mut pending, &mut inputs, &mut |event| {
            if let QueueEvent::Error { file, .. } = event {
                errors.push(file);
            }
            Ok(())
        });
        fs::remove_dir_all(&dir).unwrap();
        result.unwrap();
        assert!(
            pending.is_empty(),
            "oversized and linked inputs must never be admitted"
        );
        assert!(errors.contains(&dir.join("bytes.json")));
        assert!(errors.contains(&dir.join("count.json")));
    }

    #[test]
    fn queue_rejects_an_overfull_directory_before_admission() {
        let dir = crate::test_dir::PrivateDir::new("queue-directory-limit");
        for i in 0..257 {
            fs::write(
                dir.path().join(format!("{i}.json")),
                format!(r#"[{{"id":"task-{i}","branch":"agent/task-{i}","model":"fake","prompt":"Synthetic task"}}]"#),
            )
            .unwrap();
        }
        let mut pending = vec![];
        let mut inputs = Inputs::default();
        let result = scan(dir.path(), &mut pending, &mut inputs, &mut |_| Ok(()));
        assert!(result.unwrap_err().contains("256 task files"));
        assert!(pending.is_empty());
        assert!(inputs.tasks.is_empty());
    }

    #[test]
    fn queue_rejects_aggregate_input_bytes_before_admission() {
        let dir = crate::test_dir::PrivateDir::new("queue-input-budget");
        for i in 0..17 {
            let mut task = format!(
                r#"[{{"id":"task-{i}","branch":"agent/task-{i}","model":"fake","prompt":"Synthetic task"}}]"#
            );
            task.push_str(&" ".repeat(1024 * 1024 - task.len()));
            fs::write(dir.path().join(format!("{i}.json")), task).unwrap();
        }
        let mut pending = vec![];
        let mut inputs = Inputs::default();
        let result = scan(dir.path(), &mut pending, &mut inputs, &mut |_| Ok(()));
        assert!(result.unwrap_err().contains("16 MiB"));
        assert!(pending.is_empty());
        assert!(inputs.tasks.is_empty());
    }

    #[test]
    fn archiving_an_unadmitted_file_cancels_its_pending_job() {
        let dir = std::env::temp_dir().join(format!(
            "oxa-queue-pending-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&dir).unwrap();
        let path = dir.join("task.json");
        fs::write(
            &path,
            r#"[{"id":"queued","branch":"agent/queued","model":"fake","prompt":"Synthetic task"}]"#,
        )
        .unwrap();
        let mut pending = vec![];
        let mut inputs = Inputs::default();
        scan(&dir, &mut pending, &mut inputs, &mut |_| Ok(())).unwrap();
        assert_eq!(pending.len(), 1);
        fs::rename(&path, dir.join("archived")).unwrap();
        scan(&dir, &mut pending, &mut inputs, &mut |_| Ok(())).unwrap();
        assert!(pending.is_empty(), "archived input must not be dispatched");
        assert!(!inputs.tasks.contains_key("queued"));
        fs::write(
            &path,
            r#"[{"id":"replacement","branch":"agent/queued","model":"fake","prompt":"Synthetic replacement"}]"#,
        )
        .unwrap();
        scan(&dir, &mut pending, &mut inputs, &mut |_| Ok(())).unwrap();
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].task.id, "replacement");
    }
    #[test]
    fn archiving_between_directory_read_and_metadata_is_not_a_controller_error() {
        let dir = std::env::temp_dir().join(format!(
            "oxa-queue-scan-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&dir).unwrap();
        let path = dir.join("task.json");
        fs::write(&path, "[]").unwrap();
        let entry = fs::read_dir(&dir).unwrap().next().unwrap().unwrap();
        fs::rename(path, dir.join("archived")).unwrap();
        let result = input_file(entry);
        fs::remove_dir_all(dir).unwrap();
        assert!(result.unwrap().is_none());
    }
}
