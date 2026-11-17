//! Directory queue: oldest regular JSON files first, with bounded independent workers.
use super::{blocked_record, checked, state, Record, Result, RunPolicy, Task};
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
}
#[derive(Clone)]
struct Job {
    task: Task,
    modified: SystemTime,
    file: PathBuf,
    index: usize,
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
}
fn scan(
    directory: &Path,
    pending: &mut Vec<Job>,
    inputs: &mut Inputs,
    emit: &mut dyn FnMut(QueueEvent) -> Result<()>,
) -> Result<()> {
    let mut files = vec![];
    for entry in fs::read_dir(directory).map_err(|e| e.to_string())? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.to_string()),
        };
        if let Some(file) = input_file(entry)? {
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
    inputs.tasks.retain(|id, _| !withdrawn.contains(id));
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
                        if inputs
                            .tasks
                            .values()
                            .any(|(_, known)| known.branch == task.branch)
                        {
                            problems.push(format!(
                                "branch {} was already queued; use a distinct branch",
                                task.branch
                            ));
                            continue;
                        }
                        inputs.tasks.insert(task.id.clone(), (file.clone(), task.clone()));
                        pending.push(Job {
                            task,
                            file: file.clone(),
                            modified,
                            index,
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
    worker: Arc<dyn Fn(&Task) -> Result<Record> + Send + Sync>,
    emit: &mut dyn FnMut(QueueEvent) -> Result<()>,
) -> Result<QueueStatus> {
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
    let (tx, rx) = mpsc::channel();
    let mut active = Workers::default();
    let (mut pending, mut external, mut inputs) = (vec![], vec![], Inputs::default());
    let (mut finished, mut failed, mut last) = (0, 0, None);
    loop {
        scan(&directory, &mut pending, &mut inputs, emit)?;
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
            let input = job.clone();
            match thread::Builder::new()
                .name(format!("oxa-work-{id}"))
                .spawn(move || {
                    let record =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| worker(&input.task)))
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
            Ok((job, record)) => {
                if let Some(handle) = active.handles.remove(&job.task.id) {
                    let _ = handle.join();
                }
                if busy(&storage, &job.task.id)? {
                    external.push(job);
                    continue;
                }
                let record = record?;
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
        fs::remove_dir_all(&dir).unwrap();
        assert!(pending.is_empty(), "archived input must not be dispatched");
        assert!(!inputs.tasks.contains_key("queued"));
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
