//! Shared bounded catalog storage carried from the archived Go cache.
use crate::secrets::Key;
use hmac::{Hmac, Mac};
use serde_json::{Map, Value};
use sha2::Sha256;
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// An entry younger than this is consumed without any network call.
pub const FRESH: Duration = Duration::from_secs(15 * 60);
/// An entry older than this is expired: startup does live discovery as before.
pub const STALE: Duration = Duration::from_secs(24 * 60 * 60);
/// Reads and writes are bounded; an oversized file is a miss, never a partial catalog.
pub const MAX_BYTES: u64 = 1024 * 1024;
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

/// How a usable entry was obtained. `Stale` is shown to the user and schedules
/// exactly one bounded background refresh.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Fresh,
    Stale,
}

#[derive(Clone)]
pub(crate) struct Cache {
    path: PathBuf,
}

impl Cache {
    /// `directory` is the private agent directory that already holds the ledger
    /// and is denied to file tools and bash.
    pub(crate) fn new(directory: &Path, filename: &str) -> Self {
        Self {
            path: directory.join(filename),
        }
    }

    pub(crate) fn load(&self, key: &Key, now: SystemTime) -> Option<(State, Value)> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
            .open(&self.path)
            .ok()?;
        let metadata = file.metadata().ok()?;
        if !metadata.is_file()
            || metadata.len() > MAX_BYTES
            || metadata.uid() != nix::unistd::Uid::current().as_raw()
            || metadata.mode() & 0o777 != 0o600
            || metadata.nlink() != 1
        {
            return None;
        }
        let mut bytes = Vec::new();
        file.take(MAX_BYTES + 1).read_to_end(&mut bytes).ok()?;
        if bytes.len() as u64 > MAX_BYTES {
            return None;
        }
        let mut payload: Map<String, Value> = serde_json::from_slice(&bytes).ok()?;
        let tag = payload.shift_remove("auth")?;
        let tag: [u8; 32] = serde_json::from_value(tag).ok()?;
        self.mac(key, &payload).ok()?.verify_slice(&tag).ok()?;
        let age = now
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_secs()
            .checked_sub(payload.get("fetched_at")?.as_u64()?)?;
        let state = if age < FRESH.as_secs() {
            State::Fresh
        } else if age < STALE.as_secs() {
            State::Stale
        } else {
            return None;
        };
        Some((state, Value::Object(payload)))
    }

    /// Atomically publish a validated catalog. Failures before rename preserve
    /// the preceding entry.
    pub(crate) fn store(&self, key: &Key, mut value: Value, now: SystemTime) -> io::Result<()> {
        let invalid = || io::Error::new(io::ErrorKind::InvalidInput, "Invalid catalog cache");
        if crate::redaction::contains_secret(&value, key.expose()) {
            return Err(invalid());
        }
        let payload = value.as_object_mut().ok_or_else(invalid)?;
        payload.insert(
            "fetched_at".into(),
            now.duration_since(UNIX_EPOCH)
                .map_err(|_| invalid())?
                .as_secs()
                .into(),
        );
        let auth: [u8; 32] = self.mac(key, payload)?.finalize().into_bytes().into();
        payload.insert("auth".into(), serde_json::to_value(auth)?);
        let bytes = serde_json::to_vec(payload).map_err(|_| invalid())?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(invalid());
        }
        let parent = self.path.parent().ok_or_else(invalid)?;
        fs::DirBuilder::new().recursive(true).mode(0o700).create(parent)?;
        let filename = self
            .path
            .file_name()
            .and_then(|part| part.to_str())
            .ok_or_else(invalid)?;
        let (temporary, mut file) = loop {
            let temporary = parent.join(format!(
                ".{filename}-{}-{}.tmp",
                std::process::id(),
                NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
            ));
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)
            {
                Ok(file) => break (Temporary(temporary), file),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        };
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temporary.0, &self.path)?;
        File::open(parent)?.sync_all()
    }

    fn mac(&self, key: &Key, payload: &Map<String, Value>) -> io::Result<Hmac<Sha256>> {
        let filename = self
            .path
            .file_name()
            .and_then(|part| part.to_str())
            .ok_or(io::ErrorKind::InvalidInput)?;
        let mut mac =
            Hmac::<Sha256>::new_from_slice(key.expose().as_bytes()).expect("HMAC accepts any key length");
        mac.update(b"oxa-catalog-cache-v1\0");
        mac.update(filename.as_bytes());
        mac.update(b"\0");
        mac.update(&serde_json::to_vec(payload)?);
        Ok(mac)
    }
}

struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
