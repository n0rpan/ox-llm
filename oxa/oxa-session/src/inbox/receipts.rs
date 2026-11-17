//! Durable pickup is deliberately separate from the frontend's in-memory admission.
use super::*;

fn receipt_exists(dir: &Directory, id: &str) -> io::Result<bool> {
    if !valid_inbox_id(id) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid inbox message id",
        ));
    }
    let path = dir.path().join(format!("{id}.taken"));
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    // Receipts contain no prompt data. Fail closed on malformed markers.
    // SAFETY: geteuid has no preconditions.
    let mine = meta.uid() == unsafe { libc::geteuid() };
    if !meta.is_file() || meta.nlink() != 1 || meta.len() != 0 || !mine {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid inbox receipt",
        ));
    }
    Ok(true)
}

pub(super) fn already_taken(dir: &Directory, path: &Path) -> io::Result<bool> {
    let Some(id) = path
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|id| valid_inbox_id(id))
    else {
        // Legacy watch accepts externally published filenames too.
        return Ok(false);
    };
    receipt_exists(dir, id)
}

impl Inbox {
    /// Query durable pickup, not execution or admission by `watch`.
    /// Message ids are the opaque strings returned by `send_inbox`.
    pub fn receipt(agent_dir: &Path, session: &str, message: &str) -> io::Result<bool> {
        receipt_exists(&directory(agent_dir, session)?, message)
    }

    /// Pick up the oldest valid message with a durable, payload-free receipt.
    /// Exclusive ownership is shared with `watch`. After success, retries and watchers
    /// never deliver this id again, even if a crash left its `.msg` file behind.
    /// A receipt proves pickup only: a crash after publication can lose execution.
    pub fn take(agent_dir: &Path, session: &str) -> io::Result<Option<(String, String)>> {
        let dir = directory(agent_dir, session)?;
        dir.lock()?;
        // Keep memory bounded even for an arbitrarily large stopped inbox.
        let mut first: Option<(PathBuf, String)> = None;
        for entry in fs::read_dir(dir.path())? {
            let path = entry?.path();
            if !path.extension().is_some_and(|s| s == "msg")
                || first.as_ref().is_some_and(|(best, _)| &path >= best)
            {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if !valid_inbox_id(id) {
                continue;
            }
            let meta = fs::symlink_metadata(&path)?;
            if !meta.is_file() || meta.nlink() != 1 || meta.len() > INBOX_MESSAGE_BYTES as u64 {
                continue;
            }
            if already_taken(&dir, &path)? {
                continue;
            }
            let file = match OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
                .open(&path)
            {
                Ok(file) => file,
                Err(_) => continue,
            };
            let meta = file.metadata()?;
            if !meta.is_file() || meta.nlink() != 1 || meta.len() > INBOX_MESSAGE_BYTES as u64 {
                continue;
            }
            let mut text = String::new();
            if file
                .take(INBOX_MESSAGE_BYTES as u64 + 1)
                .read_to_string(&mut text)
                .is_err()
                || text.len() > INBOX_MESSAGE_BYTES
            {
                continue;
            }
            first = Some((path, text));
        }
        let Some((path, text)) = first else {
            return Ok(None);
        };
        let id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .expect("validated message id")
            .to_owned();
        // An empty marker needs no payload write. Sync the inode before publishing it,
        // then the directory before removing the message. Neither path follows links.
        let temp = dir
            .path()
            .join(format!(".{}.receipt.tmp", crate::new_entry_id(&|_| false)));
        let result = (|| {
            let marker = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&temp)?;
            marker.sync_all()?;
            fs::rename(&temp, dir.path().join(format!("{id}.taken")))?;
            sync_directory(&dir.0)?;
            fs::remove_file(path)?;
            sync_directory(&dir.0)?;
            Ok(Some((id, text)))
        })();
        if result.is_err() {
            let _ = fs::remove_file(temp);
        }
        result
    }
}
