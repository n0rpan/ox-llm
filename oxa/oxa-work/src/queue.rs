//! Directory queue: oldest regular JSON files first, with bounded independent workers.
use super::{blocked_record, board, checked, state, LockedTask, Record, Result, RunPolicy, Task};
use serde::{
    de::{Error, IgnoredAny, SeqAccess, Visitor},
    Deserialize, Deserializer, Serialize,
};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
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
        record: Box<Record>,
    },
    Error {
        file: PathBuf,
        detail: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        native_task_id: Option<String>,
    },
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
    selection: NativeSelection,
    policy: RunPolicy,
    run: Arc<NativeWorkerFn>,
}

enum NativeSelection {
    Worker(String),
    Bound(HashMap<String, BoundNative>),
}

/// One attempt selected by the trusted Lead caller from the existing Board.
/// The caller supplies its registered host and selected model independently of the packet.
pub struct NativeQueueTarget<'a> {
    pub binding: &'a board::NativeBinding,
    pub host_id: &'a str,
    pub model: &'a str,
}

struct BoundNative {
    binding: board::NativeBinding,
    host_id: String,
    model: String,
}

impl BoundNative {
    fn verify(&self, rows: &BTreeMap<u64, board::Assignment>) -> Result<()> {
        let now = state::now_ms() / 1000;
        let binding = &self.binding;
        let already_assigned = rows
            .values()
            .any(|row| row.number != binding.number && row.data.busy_for(&row.state, &binding.lease.holder));
        let valid = rows.get(&binding.number).is_some_and(|row| {
            row.state.lease.as_ref() == Some(&binding.lease)
                && row.data.native_binding.as_ref() == Some(binding)
                && !row.data.import_hold
                && row.data.dependency.is_none()
                && row
                    .data
                    .item_owner
                    .as_ref()
                    .is_some_and(|owner| owner.host_id == self.host_id && now < owner.expires_at)
        });
        if !valid || already_assigned {
            return Err("selected native assignment is no longer runnable on its host".into());
        }
        Ok(())
    }

    fn verify_task(&self, task: &Task) -> Result<()> {
        if task.id != self.binding.native_task_id
            || task.branch != self.binding.branch
            || task.model != self.model
            || !task.runner_owns_github()
        {
            return Err("native packet disagrees with its selected attempt or model".into());
        }
        Ok(())
    }
}

impl QueueWorker {
    fn run(&self, job: &mut Job) -> Result<Record> {
        let worker = match self {
            Self::Manual(run) => return run(&job.task),
            Self::Native(worker) => worker,
        };
        let mut locked = LockedTask::acquire(&job.task, &worker.policy)?;
        job.binding = Some(match &worker.selection {
            NativeSelection::Worker(worker_id) => NativeQueueOwner {
                claims: &worker.claims,
                worker_id,
            }
            .admit(&job.task)?,
            NativeSelection::Bound(selected) => {
                let target = selected.get(&job.task.id).ok_or("native task was not selected")?;
                target.verify_task(&job.task)?;
                target.verify(&worker.claims.assignments()?)?;
                target.binding.clone()
            }
        });
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
/// Every directory entry counts, including hidden, non-JSON and non-regular ones,
/// so ignored entries cannot make each watch iteration unbounded.
const MAX_QUEUE_ENTRIES: usize = 4096;
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
    for (index, entry) in fs::read_dir(directory).map_err(|e| e.to_string())?.enumerate() {
        if index == MAX_QUEUE_ENTRIES {
            return Err(format!(
                "queue directory exceeds {MAX_QUEUE_ENTRIES} entries; move other files out of it"
            ));
        }
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
                        native_task_id: None,
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
        self.run_native_with(
            owner.claims,
            NativeSelection::Worker(owner.worker_id.into()),
            None,
            worker,
            emit,
        )
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
        self.run_native_with(
            owner.claims,
            NativeSelection::Worker(owner.worker_id.into()),
            Some(&[binding]),
            worker,
            emit,
        )
    }

    /// Run a bounded selection in the same queue controller. Every job has its
    /// own immutable Claims receipt, task lock, worker and model.
    pub fn run_native_batch(
        self,
        claims: &board::Claims,
        targets: &[NativeQueueTarget<'_>],
        worker: Arc<NativeWorkerFn>,
        emit: &mut dyn FnMut(QueueEvent) -> Result<()>,
    ) -> Result<QueueStatus> {
        if !self.options.once || targets.is_empty() || targets.len() > MAX_INPUT_TASKS {
            return Err("native batch requires a bounded one-shot selection".into());
        }
        let mut selected = HashMap::new();
        let mut items = HashSet::new();
        let mut workers = HashSet::new();
        let mut branches = HashSet::new();
        let mut bindings = Vec::with_capacity(targets.len());
        for target in targets {
            let binding = target.binding;
            binding.validate()?;
            if !items.insert(binding.number)
                || !workers.insert(&binding.lease.holder)
                || !branches.insert(&binding.branch)
                || selected.contains_key(&binding.native_task_id)
            {
                return Err("native batch repeats an item, worker, branch or attempt".into());
            }
            bindings.push(binding);
            selected.insert(
                binding.native_task_id.clone(),
                BoundNative {
                    binding: binding.clone(),
                    host_id: target.host_id.into(),
                    model: target.model.into(),
                },
            );
        }
        self.run_native_with(
            claims,
            NativeSelection::Bound(selected),
            Some(&bindings),
            worker,
            emit,
        )
    }

    fn run_native_with(
        self,
        claims: &board::Claims,
        selection: NativeSelection,
        prepared: Option<&[&board::NativeBinding]>,
        worker: Arc<NativeWorkerFn>,
        emit: &mut dyn FnMut(QueueEvent) -> Result<()>,
    ) -> Result<QueueStatus> {
        let worker = QueueWorker::Native(Arc::new(NativeWorker {
            claims: claims.clone(),
            selection,
            policy: self.policy.clone(),
            run: worker,
        }));
        run_queue_inner(self, prepared, worker, emit)
    }
}

fn run_queue_inner(
    queue: QueueRun<'_>,
    prepared: Option<&[&board::NativeBinding]>,
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
    if prepared.is_some() {
        crate::helper_queue::directory(directory)?;
    }
    let directory = directory.canonicalize().map_err(|e| e.to_string())?;
    if !directory.is_dir() {
        return Err("queue path is not a directory".into());
    }
    let _controller = state::lock(&directory.join(".oxa-queue.lock"))?;
    let selection = match &worker {
        QueueWorker::Native(native) => match &native.selection {
            NativeSelection::Bound(selected) => {
                let rows = native.claims.assignments()?;
                for target in selected.values() {
                    target.verify(&rows)?;
                }
                Some(selected)
            }
            NativeSelection::Worker(_) => None,
        },
        QueueWorker::Manual(_) => None,
    };
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
    let (tx, rx) = mpsc::channel();
    let mut active = Workers::default();
    let read_directory = prepared.is_none();
    let mut pending = if let Some(bindings) = prepared {
        let mut bytes = 0;
        crate::helper_queue::take_native_packets(&directory, bindings, |index, binding, packet| {
            let task = native::from_packet(binding, &packet, policy)?;
            if let Some(selected) = selection {
                selected
                    .get(&binding.native_task_id)
                    .ok_or("native task was not selected")?
                    .verify_task(&task)?;
            }
            bytes += task_bytes(&task);
            if bytes > MAX_QUEUE_BYTES {
                return Err("native queue exceeds the task input capacity".into());
            }
            Ok(Job {
                task,
                modified: SystemTime::UNIX_EPOCH,
                file: directory.clone(),
                index,
                binding: None,
            })
        })?
    } else {
        vec![]
    };
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
            if busy(storage, &job.task.id)? {
                waiting.push(job);
            } else {
                pending.push(job);
            }
        }
        external = waiting;
        let mut ready = vec![];
        for job in pending.drain(..) {
            if busy(storage, &job.task.id)? {
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
            match thread::Builder::new()
                .name(format!("oxa-work-{id}"))
                .spawn(move || {
                    let record =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| worker.run(&mut input)))
                            .unwrap_or_else(|_| {
                                Ok(blocked_record(
                                    &input.task,
                                    "queue worker panicked; retained state can be resumed".into(),
                                ))
                            });
                    let _ = tx.send((input, record));
                }) {
                Ok(handle) => {
                    active.handles.insert(id, handle);
                }
                Err(error) => {
                    finished += 1;
                    failed += 1;
                    emit(QueueEvent::Task {
                        record: Box::new(blocked_record(&job.task, error.to_string())),
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
            Ok((job, record)) => {
                if let Some(handle) = active.handles.remove(&job.task.id) {
                    let _ = handle.join();
                }
                let record = if let QueueWorker::Native(native) = &worker {
                    match record.and_then(|record| {
                        let binding = job
                            .binding
                            .as_ref()
                            .ok_or("native admission was not established")?;
                        NativeQueueOwner {
                            claims: &native.claims,
                            worker_id: &binding.lease.holder,
                        }
                        .finish(policy, &job.task, binding, &record)?;
                        Ok(record)
                    }) {
                        Ok(record) => record,
                        Err(detail) => {
                            failed += 1;
                            emit(QueueEvent::Error {
                                file: job.file,
                                detail,
                                native_task_id: job.binding.map(|binding| binding.native_task_id),
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
                emit(QueueEvent::Task {
                    record: Box::new(record),
                })?;
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
    fn one_controller_runs_two_selected_native_workers_and_settles_each_original_lease() {
        use crate::{CheckpointKind, Outcome, Step};
        use board::{assignment::*, *};
        if crate::lock_process::in_parent() {
            return;
        }
        let directory = crate::test_dir::PrivateDir::new("native-worker-batch");
        let repo = directory.path().join("repo");
        let queue = directory.path().join("queue");
        let claims_dir = directory.path().join("claims");
        for path in [&repo, &queue, &claims_dir] {
            fs::DirBuilder::new().mode(0o700).create(path).unwrap();
        }
        let claims = Claims::open(&claims_dir).unwrap();
        let now = state::now_ms() / 1000;
        claims
            .import_snapshot(
                Some(ManualAuthority::Owner),
                &BTreeMap::from_iter((1..=2).map(|number| {
                    (
                        number,
                        ImportState {
                            state: AttemptState::default(),
                            excluded: false,
                        },
                    )
                })),
                now,
            )
            .unwrap();
        let assignment = Policy {
            enabled: true,
            ..Policy::default()
        };
        let models = ["gpt-fixture", "deepseek-fixture"];
        let bindings: Vec<_> = models
            .iter()
            .enumerate()
            .map(|(index, model)| {
                let number = (index + 1) as u64;
                let worker = Worker {
                    id: format!("oxa:fixture/worker-{number}"),
                    kind: WorkerKind::Oxa,
                    roles: [Role::Builder].into(),
                    skills: Default::default(),
                };
                let entry = Entry {
                    number,
                    head: "fixture-revision".into(),
                    release_pinned: false,
                    action: Action::BuildIssue,
                    lease: None,
                    attempts_today: 0,
                    eligible: true,
                };
                let receipt = claims
                    .claim_item_for(&entry, &assignment, &worker, "host-a", (now, 600), |_| Ok(None))
                    .unwrap()
                    .unwrap();
                let branch = format!("agent/{number}");
                let binding =
                    NativeBinding::new(number, &format!("{number:064x}"), &receipt.lease, &branch).unwrap();
                assert!(claims.bind_native(&worker.id, &binding).unwrap());
                assert!(crate::helper_queue::publish(
                    &queue,
                    &binding.key,
                    &format!("i{number} {model} {branch} main\nSynthetic task\n"),
                    false,
                    &assignment,
                )
                .unwrap());
                binding
            })
            .collect();
        let policy = RunPolicy {
            git_binary: "/usr/bin/git".into(),
            host_path: "/usr/bin".into(),
            terminate_binary: None,
            offline: true,
            runner_owns_github: false,
            autonomy: Default::default(),
            worker_github: Default::default(),
            authority_now: None,
            storage: directory.path().join("runner"),
        };
        assert!(Command::new(&policy.git_binary)
            .current_dir(&repo)
            .env_clear()
            .env("HOME", directory.path())
            .args(["init", "--quiet"])
            .status()
            .unwrap()
            .success());
        let options = QueueOptions {
            max: 2,
            min_free_bytes: 0,
            once: true,
        };
        let run = || QueueRun {
            repo: &repo,
            directory: &queue,
            options: &options,
            policy: &policy,
            df: Path::new("/usr/bin/df"),
        };
        let models_by_id: HashMap<_, _> = bindings
            .iter()
            .zip(models)
            .map(|(binding, model)| (binding.native_task_id.clone(), model))
            .collect();
        let execute: Arc<NativeWorkerFn> = Arc::new(move |task, locked| {
            assert_eq!(task.model, models_by_id[&task.id]);
            let mut value = state::State::new(task.clone(), blocked_record(task, String::new()), true);
            value.record.outcome = Outcome::NothingToDo;
            let mut writer =
                state::Writer::new(locked.policy.storage.join(format!("{}.json", task.id)), false);
            writer
                .save(&mut value, Step::Finished, &|_: &Record| Ok(()))
                .unwrap();
            Ok(value.record)
        });
        let mut targets: Vec<_> = bindings
            .iter()
            .zip(models)
            .map(|(binding, model)| NativeQueueTarget {
                binding,
                host_id: "host-a",
                model,
            })
            .collect();

        targets[1].host_id = "different-host";
        assert!(run()
            .run_native_batch(&claims, &targets, execute.clone(), &mut |_| Ok(()))
            .is_err());
        for binding in &bindings {
            assert!(queue.join(format!("board-{}.txt", binding.key)).is_file());
        }
        targets[1].host_id = "host-a";
        let ready: Vec<_> = bindings
            .iter()
            .map(|binding| queue.join(format!("board-{}.txt", binding.key)))
            .collect();
        let saved: Vec<_> = ready.iter().map(|path| fs::read(path).unwrap()).collect();
        let held = directory.path().join("withheld-packet");
        fs::rename(&ready[1], &held).unwrap();
        assert!(run()
            .run_native_batch(&claims, &targets, execute.clone(), &mut |_| Ok(()))
            .unwrap_err()
            .contains("packet unavailable"));
        assert_eq!(
            fs::read(&ready[0]).expect("missing second packet must leave the first ready"),
            saved[0]
        );
        fs::rename(&held, &ready[1]).unwrap();

        targets[1].model = models[0];
        assert!(run()
            .run_native_batch(&claims, &targets, execute.clone(), &mut |_| Ok(()))
            .is_err());
        for (path, bytes) in ready.iter().zip(&saved) {
            assert_eq!(
                &fs::read(path).expect("rejected batch must leave both packets ready"),
                bytes
            );
        }
        assert_eq!(fs::read_dir(queue.join("started")).unwrap().count(), 0);
        assert!(bindings.iter().all(
            |binding| claims.snapshot().unwrap()[&binding.number].lease.as_ref() == Some(&binding.lease)
        ));

        targets[1].model = models[1];
        let failed_binding = &bindings[0];
        let failed_id = failed_binding.native_task_id.clone();
        let run_successfully = execute.clone();
        let mixed: Arc<NativeWorkerFn> = Arc::new(move |task, locked| {
            if task.id == failed_id {
                return Err("synthetic native worker failure".into());
            }
            run_successfully(task, locked)
        });
        let mut both_active = false;
        let mut completed = HashSet::new();
        let mut failures = 0;
        let mut emit = |event| {
            match event {
                QueueEvent::State { status } => both_active |= status.active.len() == 2,
                QueueEvent::Task { record } => {
                    assert_eq!(record.checkpoint_kind, CheckpointKind::TaskTransition);
                    let binding = bindings
                        .iter()
                        .find(|binding| record.id == binding.native_task_id)
                        .unwrap();
                    let rows = claims.assignments().unwrap();
                    assert!(rows[&binding.number].state.lease.is_none());
                    assert!(rows[&binding.number].data.native_binding.as_ref() == Some(binding));
                    assert!(completed.insert(record.id));
                }
                error @ QueueEvent::Error { .. } => {
                    let event = serde_json::to_value(error).unwrap();
                    assert_eq!(event["detail"], "synthetic native worker failure");
                    assert_eq!(event["native_task_id"], failed_binding.native_task_id);
                    failures += 1;
                }
            }
            Ok(())
        };
        let status = run()
            .run_native_batch(&claims, &targets, mixed, &mut emit)
            .unwrap();
        assert_eq!((status.finished, status.failed), (1, 1));
        let rows = claims.assignments().unwrap();
        assert_eq!(
            rows[&failed_binding.number].state.lease.as_ref(),
            Some(&failed_binding.lease)
        );
        assert!(rows[&bindings[1].number].state.lease.is_none());

        let resumed = run()
            .run_native_batch(&claims, &targets[..1], execute, &mut emit)
            .unwrap();
        assert_eq!((resumed.finished, resumed.failed), (1, 0));
        assert!(both_active);
        assert_eq!((completed.len(), failures), (2, 1));
    }

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
    fn queue_counts_ignored_entries_against_the_directory_limit() {
        let dir = crate::test_dir::PrivateDir::new("queue-entry-limit");
        for i in 0..MAX_QUEUE_ENTRIES {
            let name = match i % 3 {
                0 => format!(".hidden-{i}.json"),
                1 => format!("notes-{i}.txt"),
                _ => format!("dir-{i}.json"),
            };
            if i % 3 == 2 {
                fs::create_dir(dir.path().join(name)).unwrap();
            } else {
                fs::write(dir.path().join(name), "").unwrap();
            }
        }
        fs::write(
            dir.path().join("task.json"),
            r#"[{"id":"task","branch":"agent/task","model":"fake","prompt":"Synthetic task"}]"#,
        )
        .unwrap();
        let mut pending = vec![];
        let mut inputs = Inputs::default();
        let result = scan(dir.path(), &mut pending, &mut inputs, &mut |_| Ok(()));
        assert!(result.unwrap_err().contains("4096 entries"));
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
