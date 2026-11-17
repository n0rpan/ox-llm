//! Directory queue: oldest regular JSON files first, with bounded independent workers.
use super::{blocked_record, checked, git, state, Record, Result, Task};
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet},
    fs,
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
fn available(path: &Path) -> Result<u64> {
    let mut command = Command::new("df");
    command.args(["--output=avail", "-B1", "--"]).arg(path);
    checked(command)?
        .lines()
        .last()
        .ok_or("df returned no free-space value")?
        .trim()
        .parse()
        .map_err(|_| "df returned an invalid free-space value".into())
}
fn free_space(repo: &Path, common: &Path, queue: &Path) -> Result<u64> {
    let trees = super::worktree_root(common)?;
    let tree_volume = trees
        .ancestors()
        .find(|p| p.is_dir())
        .ok_or("worktree storage has no existing parent")?;
    Ok(available(repo)?
        .min(available(common)?)
        .min(available(queue)?)
        .min(available(tree_volume)?))
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
    inputs.files.retain(|p, _| paths.contains(p));
    inputs.errors.retain(|p, _| paths.contains(p));
    files.sort_by(|a, b| (&a.1, &a.0).cmp(&(&b.1, &b.0)));
    for (file, modified, len) in files {
        if inputs.files.get(&file) == Some(&(modified, len)) && !inputs.errors.contains_key(&file) {
            continue;
        }
        let tasks = match fs::read(&file) {
            Ok(bytes) => super::parse_tasks(&bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => Err(error.to_string()),
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

pub fn run_queue(
    repo: &Path,
    directory: &Path,
    options: &QueueOptions,
    worker: Arc<dyn Fn(&Task) -> Record + Send + Sync>,
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
    let common = PathBuf::from(checked(git(
        repo,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    ))?);
    let storage = common.join("oxa-work");
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
        pending.sort_by(|a, b| (&a.modified, &a.file, a.index).cmp(&(&b.modified, &b.file, b.index)));
        let disk = free_space(repo, &common, &directory);
        let (mut free_bytes, mut disk_error) = match disk {
            Ok(free) => (Some(free), None),
            Err(error) => (None, Some(error)),
        };
        let mut disk_blocked = free_bytes.is_none_or(|free| free < options.min_free_bytes);
        while !disk_blocked && active.handles.len() + external.len() < options.max && !pending.is_empty() {
            // Check again for each admission: earlier workers may already be writing.
            match free_space(repo, &common, &directory) {
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
                                blocked_record(
                                    &input.task,
                                    "queue worker panicked; retained state can be resumed".into(),
                                )
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
            .collect();
        ids.sort();
        let status = QueueStatus {
            time: state::now(),
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
            state::atomic_write(&directory.join(".oxa-queue-status.json"), &bytes)?;
            emit(QueueEvent::State {
                status: status.clone(),
            })?;
            last = Some(status.clone());
        }
        if options.once
            && active.handles.is_empty()
            && external.is_empty()
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
