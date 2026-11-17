//! Read-only sampling: this runner has no executor or GitHub mutation interface.
use crate::board::{review, Board, Entry};
use crate::Result;
use serde::Serialize;
use std::collections::BTreeMap;
use std::fs::{File, Metadata, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::time::{Duration, Instant};

pub const INTERVAL: Duration = Duration::from_secs(300);
pub const MAX_LOG_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Serialize)]
struct Sample<'a> {
    schema_version: u32,
    observed_at: u64,
    board: &'a Board,
    would_take: Vec<&'a Entry>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    reviews: &'a BTreeMap<u64, review::Plan>,
}

pub(crate) const OWNER_FILE_FLAGS: libc::c_int = libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;

pub(crate) fn regular_file_owned_by(metadata: &Metadata, owner: u32) -> bool {
    metadata.is_file() && metadata.uid() == owner
}

/// Refuse links, shared files and concurrent writers before consulting GitHub.
pub fn open_log(path: &Path) -> Result<File> {
    // SAFETY: geteuid has no preconditions and returns this process's effective owner.
    open_log_for_owner(path, unsafe { libc::geteuid() })
}

fn open_log_for_owner(path: &Path, owner: u32) -> Result<File> {
    let mut file = OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(OWNER_FILE_FLAGS)
        .open(path)
        .map_err(|_| "cannot open private shadow log")?;
    let metadata = file.metadata().map_err(|_| "cannot inspect shadow log")?;
    if !regular_file_owned_by(&metadata, owner)
        || metadata.nlink() != 1
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err("shadow log must be a private regular file with one link".into());
    }
    file.try_lock().map_err(|_| "shadow log is already in use")?;
    recover_tail(&mut file, path)?;
    Ok(file)
}

/// Preserve interrupted bytes durably before removing them from the JSONL stream.
fn recover_tail(file: &mut File, path: &Path) -> Result<()> {
    let length = file.metadata().map_err(|_| "cannot inspect shadow log")?.len();
    if length > MAX_LOG_BYTES {
        return Err("shadow log reached its size limit; rotate it while stopped".into());
    }
    if length == 0 {
        return Ok(());
    }
    let mut cursor = length;
    let cut = loop {
        let start = cursor.saturating_sub(8192);
        let mut chunk = vec![0; (cursor - start) as usize];
        file.seek(SeekFrom::Start(start))
            .map_err(|_| "cannot seek shadow log")?;
        file.read_exact(&mut chunk)
            .map_err(|_| "cannot inspect shadow tail")?;
        if let Some(offset) = chunk.iter().rposition(|b| *b == b'\n') {
            break start + offset as u64 + 1;
        }
        if start == 0 {
            break 0;
        }
        cursor = start;
    };
    if cut == length {
        return Ok(());
    }
    let mut fragment = vec![0; (length - cut) as usize];
    file.seek(SeekFrom::Start(cut))
        .map_err(|_| "cannot seek shadow tail")?;
    file.read_exact(&mut fragment)
        .map_err(|_| "cannot read shadow tail")?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| "invalid clock")?
        .as_nanos();
    let quarantine = path.with_extension(format!("jsonl.incomplete-{}-{nonce}", std::process::id()));
    let mut saved = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(quarantine)
        .map_err(|_| "cannot preserve shadow tail")?;
    saved
        .write_all(&fragment)
        .and_then(|_| saved.sync_all())
        .map_err(|_| "cannot preserve shadow tail")?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    File::open(parent)
        .and_then(|dir| dir.sync_all())
        .map_err(|_| "cannot sync preserved shadow tail")?;
    file.set_len(cut)
        .and_then(|_| file.sync_data())
        .map_err(|_| "cannot restore shadow JSONL boundary".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(label: &str) -> std::path::PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("shadow-{label}-{}-{nonce}", std::process::id()))
    }

    #[test]
    fn owner_mismatch_is_refused_before_logging() {
        let path = path("owner");
        drop(open_log(&path).unwrap());
        let other = std::fs::metadata(&path).unwrap().uid().wrapping_add(1);
        let refused = open_log_for_owner(&path, other).is_err();
        std::fs::remove_file(path).unwrap();
        assert!(refused);
    }

    #[test]
    fn a_full_log_is_not_extended() {
        let path = path("limit");
        let mut log = open_log(&path).unwrap();
        log.set_len(MAX_LOG_BYTES).unwrap();
        let refused = run(
            &mut log,
            true,
            || Ok(Board::default()),
            |_| panic!("unexpected wait"),
        )
        .is_err();
        let unchanged = log.metadata().unwrap().len() == MAX_LOG_BYTES;
        drop(log);
        std::fs::remove_file(path).unwrap();
        assert!(refused && unchanged);
    }
}

/// Sample immediately, then at five-minute intervals. Fail closed on read/log errors.
/// The callback returns the shared board, including its eligibility decisions.
pub fn run(
    log: &mut File,
    once: bool,
    mut snapshot: impl FnMut() -> Result<Board>,
    wait: impl FnMut(Duration),
) -> Result<()> {
    run_with_reviews(log, once, || Ok((snapshot()?, BTreeMap::new())), wait)
}

pub fn run_with_reviews(
    log: &mut File,
    once: bool,
    mut snapshot: impl FnMut() -> Result<(Board, BTreeMap<u64, review::Plan>)>,
    mut wait: impl FnMut(Duration),
) -> Result<()> {
    loop {
        let started = Instant::now();
        let (board, reviews) = snapshot()?;
        let sample = Sample {
            schema_version: 1,
            observed_at: board.observed_at,
            would_take: board.entries.iter().filter(|entry| entry.eligible).collect(),
            board: &board,
            reviews: &reviews,
        };
        let mut line = serde_json::to_vec(&sample).map_err(|_| "cannot encode shadow board")?;
        line.push(b'\n');
        let length = log.metadata().map_err(|_| "cannot inspect shadow log")?.len();
        if length.saturating_add(line.len() as u64) > MAX_LOG_BYTES {
            return Err("shadow log reached its size limit; rotate it while stopped".into());
        }
        log.write_all(&line).map_err(|_| "cannot append shadow board")?;
        log.sync_data().map_err(|_| "cannot sync shadow board")?;
        if once {
            return Ok(());
        }
        wait(INTERVAL.saturating_sub(started.elapsed()));
    }
}
