//! Durable reservations only; execution and GitHub writes are outside this API.
use super::{valid_worker_id, Action, AttemptState, Entry, Lease, Policy};
use crate::{facts_io, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

mod activity;
mod controls;
mod dependencies;
pub use activity::{LeaseActivity, LeaseSignal, VerifiedMilestone};
pub use controls::{Assignment, AssignmentBoard, AssignmentData, ReleaseRecord};
pub use dependencies::{Dependency, DependencyWait};
mod imports;
mod native;
pub use native::NativeBinding;
mod ownership;
mod reviews;
mod timing;
pub use ownership::{ExistingWork, ItemLease, ItemOwner};
pub use timing::ActivityTiming;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Saved {
    version: u8,
    number: u64,
    state: AttemptState,
    #[serde(default)]
    data: AssignmentData,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    review_requests: Vec<super::review::RequestKey>,
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
        Self::open_for_owner(root, facts_io::effective_uid())
    }

    fn open_for_owner(root: &Path, expected_owner: u32) -> Result<Self> {
        let metadata = io(fs::symlink_metadata(root))?;
        if facts_io::private_for_owner(&metadata, true, expected_owner).is_err()
            || io(root.canonicalize())? != root
        {
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

    /// Take the store lock and make sure this PR's marker exists. A marker is created
    /// only under the lock, so it never records a reservation that was not attempted.
    fn lock(&self, number: u64) -> Result<File> {
        if Self::open(&self.root)?.uid != self.uid || number == 0 {
            return Err("invalid board claim directory or PR".into());
        }
        // Lock the existing directory inode before creating any per-PR marker.
        // Otherwise a contender could lock a new marker before its creator does.
        let lock = io(File::open(&self.root))?;
        io(lock.lock())?;
        let path = self.root.join(format!("{number}.board-claim.lock"));
        let file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                self.private(&path)?;
                io(File::open(&path))?
            }
            Err(e) => return io(Err(e)),
        };
        drop(file);
        self.private(&path)?;
        Ok(lock)
    }

    /// First access to a PR counts. A state file lost after a marker exists is still
    /// corruption, so a vanished marker must not reset the cap by faking a first access.
    fn load(&self, number: u64) -> Result<Saved> {
        self.load_with_repair(number, true)
    }

    fn load_with_repair(&self, number: u64, repair_markers: bool) -> Result<Saved> {
        let path = self.path(number);
        let created = self.root.join(format!("{number}.board-claim.created"));
        match fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return match fs::symlink_metadata(&created) {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Saved {
                        version: 1,
                        number,
                        state: AttemptState::default(),
                        data: AssignmentData::default(),
                        review_requests: vec![],
                    }),
                    Ok(_) => Err("board claim state is missing; recovery is required".into()),
                    Err(e) => io(Err(e)),
                };
            }
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
            || saved.data.native_binding.as_ref().is_some_and(|binding| {
                !binding.valid()
                    || binding.number != number
                    || binding.lease.generation != (saved.state.utc_day, saved.state.attempts)
                    || saved
                        .state
                        .lease
                        .as_ref()
                        .is_some_and(|lease| lease != &binding.lease)
            })
            || saved.review_requests.iter().any(|key| !key.valid())
            || saved.data.dependency.as_ref().is_some_and(|wait| {
                !wait.valid(number)
                    || saved.data.item_owner.is_none()
                    || saved
                        .data
                        .native_binding
                        .as_ref()
                        .is_none_or(|binding| binding.lease.claimed_at.is_none_or(|at| wait.deadline <= at))
            })
            || saved
                .data
                .lease_activity
                .as_ref()
                .is_some_and(|activity| saved.data.item_owner.is_none() || !activity.valid(&saved.state))
            || saved.data.item_owner.as_ref().is_some_and(|owner| {
                !valid_worker_id(&owner.host_id)
                    || owner.expires_at == 0
                    || saved
                        .state
                        .lease
                        .as_ref()
                        .is_some_and(|lease| owner.expires_at < lease.expires_at)
            })
            || saved.state.lease.as_ref().is_some_and(|l| {
                saved.state.attempts == 0
                    || !valid_worker_id(&l.holder)
                    || l.expires_at == 0
                    || l.generation != (saved.state.utc_day, saved.state.attempts)
                    || l.claimed_at.is_none_or(|at| at >= l.expires_at)
            })
        {
            return Err("invalid board claim identity or lease".into());
        }
        if repair_markers {
            self.mark(number)?;
        } else {
            match fs::symlink_metadata(&created) {
                Ok(_) => self.private(&created)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                Err(error) => return io(Err(error)),
            }
        }
        Ok(saved)
    }

    fn save(&self, number: u64, mut saved: Saved, replacing: bool) -> Result<()> {
        saved.data.revision = saved
            .data
            .revision
            .checked_add(1)
            .ok_or("assignment revision exhausted")?;
        let bytes = serde_json::to_vec(&saved).map_err(|_| "cannot encode board claim state")?;
        if bytes.len() > 4096 {
            return Err("board claim file exceeds capacity".into());
        }
        crate::checkpoint::write_atomic(&self.path(number), &bytes, replacing)
            .map_err(|_| "cannot durably reserve board claim".into())
    }

    /// Whether the authoritative state file is already on disk. The per-PR marker only
    /// records that a reservation started, so it must never decide create-versus-replace:
    /// losing a marker may not brick a PR whose attempts still have to be counted.
    fn established(&self, number: u64) -> Result<bool> {
        match fs::symlink_metadata(self.path(number)) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => io(Err(e)),
        }
    }

    /// Atomic replacements permit a coherent per-PR read without taking execution leases.
    fn rows(&self) -> Result<BTreeMap<u64, Saved>> {
        self.rows_with_repair(true)
    }

    fn rows_with_repair(&self, repair_markers: bool) -> Result<BTreeMap<u64, Saved>> {
        Self::open(&self.root)?;
        let lock = io(File::open(&self.root))?;
        io(lock.lock())?;
        self.rows_locked_with_repair(repair_markers)
    }

    /// Caller holds the directory lock used by every reservation and control update.
    fn rows_locked(&self) -> Result<BTreeMap<u64, Saved>> {
        self.rows_locked_with_repair(true)
    }

    fn rows_locked_with_repair(&self, repair_markers: bool) -> Result<BTreeMap<u64, Saved>> {
        let mut states = BTreeMap::new();
        for entry in io(fs::read_dir(&self.root))? {
            let name = io(entry)?.file_name();
            let Some(id) = name.to_str().and_then(|s| {
                s.strip_suffix(".board-claim.json")
                    .or_else(|| s.strip_suffix(".board-claim.lock"))
                    .or_else(|| s.strip_suffix(".board-claim.created"))
            }) else {
                continue;
            };
            let number = id.parse::<u64>().map_err(|_| "invalid board claim filename")?;
            if number == 0 || id != number.to_string() {
                return Err("invalid board claim filename".into());
            }
            states.insert(number, self.load_with_repair(number, repair_markers)?);
        }
        Ok(states)
    }

    pub fn snapshot(&self) -> Result<BTreeMap<u64, AttemptState>> {
        Ok(self
            .rows()?
            .into_iter()
            .map(|(number, row)| (number, row.state))
            .collect())
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
        self.claim_checked(entry, policy, holder, (now, ttl), None, |row| {
            Ok(entry.action != Action::BuildIssue && row.data.policy.is_none())
        })
    }

    fn claim_checked(
        &self,
        entry: &Entry,
        policy: &Policy,
        holder: &str,
        timing: (u64, u64),
        host_id: Option<&str>,
        authorize: impl FnOnce(&mut Saved) -> Result<bool>,
    ) -> Result<Option<Lease>> {
        let (now, ttl) = timing;
        if !policy.enabled
            || !entry.eligible
            || matches!(
                entry.action,
                Action::WaitReview | Action::WaitRequiredCi | Action::WaitMergeability | Action::WaitIssue
            )
        {
            return Ok(None);
        }
        let expires_at = now
            .checked_add(ttl)
            .filter(|_| ttl > 0)
            .ok_or("invalid board lease duration")?;
        if entry.number == 0 || entry.head.is_empty() || !valid_worker_id(holder) {
            return Err("invalid board claim request".into());
        }
        let _lock = self.lock(entry.number)?;
        let mut row = self.load(entry.number)?;
        if let Some(lease) = row
            .state
            .lease
            .as_ref()
            .filter(|_| host_id.is_some() || row.data.item_owner.is_some())
        {
            return Err(format!("item {} is owned by {}", entry.number, lease.holder));
        }
        if row.data.item_owner.is_some() && host_id.is_none() {
            return Err("host-bound item requires item admission".into());
        }
        if self.established(entry.number)? {
            self.mark(entry.number)?;
        }
        if row.data.import_hold {
            return Ok(None);
        }
        if !authorize(&mut row)? {
            return Ok(None);
        }
        let state = &mut row.state;
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
        // The daily attempt threshold is advisory: a claim past it still
        // reserves, and the caller warns from the returned lease generation.
        state.attempts = state
            .attempts
            .checked_add(1)
            .ok_or("board attempt count exhausted")?;
        let lease = Lease {
            holder: holder.into(),
            expires_at,
            claimed_at: Some(now),
            generation: (day, state.attempts),
        };
        state.lease = Some(lease.clone());
        row.data.item_owner = host_id.map(|host_id| ItemOwner {
            host_id: host_id.into(),
            expires_at,
        });
        row.data.native_binding = None;
        row.data.lease_activity = None;
        row.data.dependency = None;
        let replacing = self.established(entry.number)?;
        self.save(entry.number, row, replacing)?;
        self.mark(entry.number)?;
        Ok(Some(lease))
    }

    /// Record that this PR has established state. Written only after the state file is
    /// durable, so a marker can only ever claim too little, never hide a lost cap.
    fn mark(&self, number: u64) -> Result<()> {
        let path = self.root.join(format!("{number}.board-claim.created"));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(mut file) => {
                io(file.write_all(b"1\n").and_then(|()| file.sync_all()))?;
                io(io(File::open(&self.root))?.sync_all())?;
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => self.private(&path),
            Err(e) => io(Err(e)),
        }
    }

    /// An old worker cannot release a replacement lease. Attempts are never refunded.
    pub fn release(&self, number: u64, lease: &Lease) -> Result<bool> {
        match fs::symlink_metadata(self.path(number)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return io(Err(e)),
            Ok(_) => {}
        }
        let _lock = self.lock(number)?;
        let mut row = self.load(number)?;
        if row.data.policy.is_some() {
            return Err("configured assignments require authenticated release with a reason".into());
        }
        if row.state.lease.as_ref() != Some(lease) {
            return Ok(false);
        }
        row.state.lease = None;
        self.save(number, row, true)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::DirBuilderExt;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn claim_store_requires_the_expected_owner() {
        struct Fixture(PathBuf);
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = fs::remove_dir(&self.0);
            }
        }
        let path = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "oxa-claim-owner-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        let fixture = Fixture(path);
        let metadata = fs::metadata(&fixture.0).unwrap();
        assert!(Claims::open(&fixture.0).is_ok());
        assert!(Claims::open_for_owner(&fixture.0, metadata.uid().wrapping_add(1)).is_err());
    }
}
