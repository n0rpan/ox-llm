//! Atomic publication only. The caller must reserve a core claim and authorize the target first.
use crate::{board::Policy, Result};
use std::fs::{self, File};
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::Path;

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

/// `key` is the lowercase SHA-256 identity of repository, PR, head and action.
/// The consumer atomically moves ready files into `started` and retains them there.
/// No GitHub client, worker execution or parallel lease storage exists in this publisher.
pub fn publish(queue: &Path, key: &str, task: &str, pinned: bool, policy: &Policy) -> Result<bool> {
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
    let prefix = if pinned { "p1-board-" } else { "board-" };
    crate::checkpoint::write_atomic_in(
        &lock,
        &queue.join(format!("{prefix}{key}.txt")),
        task.as_bytes(),
        false,
    )
    .map_err(|error| format!("board queue storage: {error}"))?;
    Ok(true)
}
