//! Read-only sampling: this runner has no executor or GitHub mutation interface.
use crate::board::{Board, Entry};
use crate::Result;
use serde::Serialize;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::time::{Duration, Instant};

pub const INTERVAL: Duration = Duration::from_secs(300);

#[derive(Serialize)]
struct Sample<'a> {
    schema_version: u32,
    observed_at: u64,
    board: &'a Board,
    would_take: Vec<&'a Entry>,
}

/// Refuse links, shared files and concurrent writers before consulting GitHub.
pub fn open_log(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| "cannot open private shadow log")?;
    let metadata = file.metadata().map_err(|_| "cannot inspect shadow log")?;
    if !metadata.is_file() || metadata.nlink() != 1 || metadata.permissions().mode() & 0o077 != 0 {
        return Err("shadow log must be a private regular file with one link".into());
    }
    file.try_lock().map_err(|_| "shadow log is already in use")?;
    Ok(file)
}

/// Sample immediately, then at five-minute intervals. Fail closed on read/log errors.
/// The callback returns the shared board, including its eligibility decisions.
pub fn run(
    log: &mut File,
    once: bool,
    mut snapshot: impl FnMut() -> Result<Board>,
    mut wait: impl FnMut(Duration),
) -> Result<()> {
    loop {
        let started = Instant::now();
        let board = snapshot()?;
        let sample = Sample {
            schema_version: 1,
            observed_at: board.observed_at,
            would_take: board.entries.iter().filter(|entry| entry.eligible).collect(),
            board: &board,
        };
        let mut line = serde_json::to_vec(&sample).map_err(|_| "cannot encode shadow board")?;
        line.push(b'\n');
        log.write_all(&line).map_err(|_| "cannot append shadow board")?;
        log.sync_data().map_err(|_| "cannot sync shadow board")?;
        if once {
            return Ok(());
        }
        wait(INTERVAL.saturating_sub(started.elapsed()));
    }
}
