//! Atomic publication only. The caller must reserve a core claim and authorize the target first.
use crate::{Result, board::Policy};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

const MAX_TASK_BYTES: usize = 64 * 1024;

fn io<T>(value: std::io::Result<T>) -> Result<T> {
    value.map_err(|e| format!("board queue storage: {:?}", e.kind()))
}

fn directory(path: &Path) -> Result<()> {
    let metadata = io(fs::symlink_metadata(path))?;
    // SAFETY: geteuid has no preconditions.
    let owner = unsafe { libc::geteuid() };
    if !metadata.is_dir()
        || metadata.uid() != owner
        || metadata.mode() & 0o022 != 0
        || io(path.canonicalize())? != path
    {
        return Err("board queue requires a canonical owner-controlled directory".into());
    }
    Ok(())
}

struct Staged(PathBuf);
impl Drop for Staged {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// `key` is the lowercase SHA-256 identity of repository, PR, head and action.
/// The consumer atomically moves ready files into `started` and retains them there.
/// No GitHub client, worker execution or parallel lease storage exists in this publisher.
pub fn publish(queue: &Path, key: &str, task: &str, pinned: bool, policy: &Policy) -> Result<bool> {
    publish_reserved(queue, key, task, pinned, policy, || Ok(true))
}

/// Reserve under the publication lock, after packet validation and pending/started dedupe.
/// The callback must not access this queue. Errors after reservation are uncertain publication:
/// retain the reservation until the trusted completion frontend reconciles the exact lease.
pub fn publish_reserved(
    queue: &Path,
    key: &str,
    task: &str,
    pinned: bool,
    policy: &Policy,
    reserve: impl FnOnce() -> Result<bool>,
) -> Result<bool> {
    if !policy.enabled {
        return Ok(false);
    }
    let (header, body) = task
        .split_once('\n')
        .ok_or("queue task needs a header and packet")?;
    let fields: Vec<_> = header.split_ascii_whitespace().collect();
    if key.len() != 64
        || !key
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        || task.len() > MAX_TASK_BYTES
        || task.contains('\0')
        || body.trim().is_empty()
        || body.lines().any(|line| line.trim() == "---")
        || !matches!(fields.len(), 3 | 4)
        || !fields[0].bytes().all(|b| b.is_ascii_alphanumeric())
    {
        return Err("invalid board queue identity or bounded task packet".into());
    }
    directory(queue)?;
    let lock = io(File::open(queue))?;
    io(lock.lock())?;
    let started = queue.join("started");
    match fs::DirBuilder::new().mode(0o700).create(&started) {
        Ok(()) => (),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(e) => return io(Err(e)),
    }
    directory(&started)?;
    for prefix in ["board-", "p1-board-"] {
        let name = format!("{prefix}{key}.txt");
        // Check pending before started: the consumer's one-way rename cannot hide both.
        for path in [queue.join(&name), started.join(&name)] {
            match fs::symlink_metadata(path) {
                Ok(metadata) => {
                    if !metadata.is_file() || metadata.mode() & 0o077 != 0 {
                        return Err("unsafe existing board queue item".into());
                    }
                    return Ok(false);
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => return io(Err(e)),
            }
        }
    }
    if !reserve()? {
        return Ok(false);
    }
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| "invalid board queue clock")?
        .as_nanos();
    let temporary = Staged(queue.join(format!(".board-{key}-{}-{nonce}.tmp", std::process::id())));
    let mut file = io(OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary.0))?;
    io(file.write_all(task.as_bytes()))?;
    io(file.sync_all())?;
    let prefix = if pinned { "p1-board-" } else { "board-" };
    // hard_link publishes a complete inode atomically and never replaces another file.
    io(fs::hard_link(
        &temporary.0,
        queue.join(format!("{prefix}{key}.txt")),
    ))?;
    drop(temporary);
    io(lock.sync_all())?;
    Ok(true)
}
