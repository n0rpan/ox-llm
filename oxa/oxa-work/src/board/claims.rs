//! Durable reservations only; execution and GitHub writes are outside this API.
use super::{Action, AttemptState, Entry, Lease, Policy};
use crate::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Saved {
    version: u8,
    number: u64,
    state: AttemptState,
}

fn io<T>(value: std::io::Result<T>) -> Result<T> {
    value.map_err(|e| format!("board claim storage: {:?}", e.kind()))
}

/// One repository's user-selected private directory, outside worker-writable binds.
/// The caller creates it; opening a store never starts work or contacts GitHub.
#[derive(Clone)]
pub struct Claims {
    root: PathBuf,
    uid: u32,
}

impl Claims {
    pub fn open(root: &Path) -> Result<Self> {
        let metadata = io(fs::symlink_metadata(root))?;
        if !metadata.is_dir() || metadata.mode() & 0o077 != 0 || io(root.canonicalize())? != root {
            return Err("board claims require a canonical private directory".into());
        }
        Ok(Self {
            root: root.into(),
            uid: metadata.uid(),
        })
    }

    fn path(&self, number: u64) -> PathBuf {
        self.root.join(format!("{number}.board-claim.json"))
    }

    fn private(&self, path: &Path) -> Result<()> {
        let metadata = io(fs::symlink_metadata(path))?;
        if !metadata.is_file() || metadata.mode() & 0o077 != 0 || metadata.uid() != self.uid {
            return Err("board claim file is not private".into());
        }
        Ok(())
    }

    fn lock(&self, number: u64) -> Result<(File, bool)> {
        if Self::open(&self.root)?.uid != self.uid || number == 0 {
            return Err("invalid board claim directory or PR".into());
        }
        // Lock the existing directory inode before creating any per-PR marker.
        // Otherwise a contender could lock a new marker before its creator does.
        let lock = io(File::open(&self.root))?;
        io(lock.lock())?;
        let path = self.root.join(format!("{number}.board-claim.lock"));
        let (file, fresh) = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => (file, true),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                self.private(&path)?;
                (io(OpenOptions::new().read(true).write(true).open(&path))?, false)
            }
            Err(e) => return io(Err(e)),
        };
        self.private(&path)?;
        drop(file);
        Ok((lock, fresh))
    }

    fn load(&self, number: u64, fresh: bool) -> Result<AttemptState> {
        let path = self.path(number);
        match fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && fresh => return Ok(AttemptState::default()),
            Err(e) => return io(Err(e)),
            Ok(_) => self.private(&path)?,
        }
        let mut bytes = vec![];
        io(io(File::open(&path))?.take(4097).read_to_end(&mut bytes))?;
        if bytes.len() > 4096 {
            return Err("board claim file exceeds capacity".into());
        }
        let saved: Saved = serde_json::from_slice(&bytes).map_err(|_| "invalid board claim state")?;
        if saved.version != 1
            || saved.number != number
            || saved.state.lease.as_ref().is_some_and(|l| {
                saved.state.attempts == 0 || !crate::valid_task_id(&l.holder) || l.expires_at == 0
            })
        {
            return Err("invalid board claim identity or lease".into());
        }
        Ok(saved.state)
    }

    fn save(&self, number: u64, state: AttemptState, fresh: bool) -> Result<()> {
        let bytes = serde_json::to_vec(&Saved {
            version: 1,
            number,
            state,
        })
        .map_err(|_| "cannot encode board claim state")?;
        crate::checkpoint::write_atomic(&self.path(number), &bytes, !fresh)
            .map_err(|_| "cannot durably reserve board claim".into())
    }

    /// Atomic replacements permit a coherent per-PR read without taking execution leases.
    pub fn snapshot(&self) -> Result<BTreeMap<u64, AttemptState>> {
        Self::open(&self.root)?;
        let mut states = BTreeMap::new();
        for entry in io(fs::read_dir(&self.root))? {
            let name = io(entry)?.file_name();
            let Some(id) = name.to_str().and_then(|s| {
                s.strip_suffix(".board-claim.json")
                    .or_else(|| s.strip_suffix(".board-claim.lock"))
            }) else {
                continue;
            };
            let number = id.parse::<u64>().map_err(|_| "invalid board claim filename")?;
            if number == 0 || id != number.to_string() {
                return Err("invalid board claim filename".into());
            }
            states.insert(number, self.load(number, false)?);
        }
        Ok(states)
    }

    /// Reserve before dispatch. A crash after reservation consumes an attempt conservatively.
    /// The caller must use a fresh GitHub entry and revalidate its head before external effects.
    pub fn claim(
        &self,
        entry: &Entry,
        policy: &Policy,
        holder: &str,
        now: u64,
        ttl: u64,
    ) -> Result<Option<Lease>> {
        if !policy.enabled
            || policy.max_attempts_per_day == 0
            || !entry.eligible
            || matches!(
                entry.action,
                Action::WaitReview | Action::WaitRequiredCi | Action::WaitMergeability
            )
        {
            return Ok(None);
        }
        let expires_at = now
            .checked_add(ttl)
            .filter(|_| ttl > 0)
            .ok_or("invalid board lease duration")?;
        if entry.number == 0 || entry.head.is_empty() || !crate::valid_task_id(holder) {
            return Err("invalid board claim request".into());
        }
        let (_lock, fresh) = self.lock(entry.number)?;
        let mut state = self.load(entry.number, fresh)?;
        let day = now / 86_400;
        if state.utc_day > day {
            return Err("board claim clock moved backwards".into());
        }
        if state.lease.as_ref().is_some_and(|l| l.expires_at > now) {
            return Ok(None);
        }
        if state.utc_day < day {
            state.utc_day = day;
            state.attempts = 0;
        }
        if state.attempts >= policy.max_attempts_per_day {
            return Ok(None);
        }
        state.attempts = state
            .attempts
            .checked_add(1)
            .ok_or("board attempt count exhausted")?;
        let lease = Lease {
            holder: holder.into(),
            expires_at,
        };
        state.lease = Some(lease.clone());
        self.save(entry.number, state, fresh)?;
        Ok(Some(lease))
    }

    /// An old worker cannot release a replacement lease. Attempts are never refunded.
    pub fn release(&self, number: u64, lease: &Lease) -> Result<bool> {
        match fs::symlink_metadata(self.path(number)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return io(Err(e)),
            Ok(_) => {}
        }
        let (_lock, _) = self.lock(number)?;
        let mut state = self.load(number, false)?;
        if state.lease.as_ref() != Some(lease) {
            return Ok(false);
        }
        state.lease = None;
        self.save(number, state, false)?;
        Ok(true)
    }
}
