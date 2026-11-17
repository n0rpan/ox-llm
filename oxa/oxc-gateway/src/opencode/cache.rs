//! Bounded, stale-aware cache for the validated Go catalog.
//!
//! One caller (the Go catalog seam) and no scheduler, worker or second HTTP
//! pool: this is deliberately not the generic discovery cache removed by #689.
//! Only the fields the strict wire parser already accepted are stored, so no
//! key, header, transcript or unvalidated upstream field reaches the file, and
//! every load re-runs that same parser against the same trusted base.
use super::{models_diagnostic, Model};
use serde_json::{json, Map, Value};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
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
const FILE: &str = "go-catalog-cache.json";
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

/// How a usable entry was obtained. `Stale` is shown to the user and schedules
/// exactly one bounded background refresh.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Fresh,
    Stale,
}

#[derive(Clone)]
pub struct Cache {
    path: PathBuf,
}

impl Cache {
    /// `directory` is the private agent directory that already holds the ledger
    /// and is denied to file tools and bash.
    pub fn new(directory: &Path) -> Self {
        Self {
            path: directory.join(FILE),
        }
    }

    /// Missing, unreadable, malformed, oversized, future-dated, expired or
    /// base-mismatched files are misses: the caller then does live discovery
    /// exactly as before. A hit is re-validated by the same strict parser as the
    /// wire, so a truncated or tampered file cannot widen routing, limits or
    /// admission.
    pub fn load(&self, base: &str, now: SystemTime) -> Option<(State, Vec<Model>)> {
        let file = File::open(&self.path).ok()?;
        let metadata = file.metadata().ok()?;
        if !metadata.is_file() || metadata.len() > MAX_BYTES {
            return None;
        }
        let mut bytes = Vec::new();
        file.take(MAX_BYTES + 1).read_to_end(&mut bytes).ok()?;
        if bytes.len() as u64 > MAX_BYTES {
            return None;
        }
        let value: Value = serde_json::from_slice(&bytes).ok()?;
        if value["base"].as_str() != Some(base) {
            return None;
        }
        let age = now
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_secs()
            .checked_sub(value["fetched_at"].as_u64()?)?;
        let state = if age < FRESH.as_secs() {
            State::Fresh
        } else if age < STALE.as_secs() {
            State::Stale
        } else {
            return None;
        };
        Some((state, models_diagnostic(&value, base).ok()?))
    }

    /// Atomically publishes one complete validated catalog; last rename wins and
    /// a failure leaves any previous entry untouched. Never fatal to the caller.
    pub fn store(&self, base: &str, models: &[Model], now: SystemTime) -> io::Result<()> {
        let invalid = || io::Error::new(io::ErrorKind::InvalidInput, "Go catalog cache too large");
        let payload = json!({
            "base": base,
            "fetched_at": now.duration_since(UNIX_EPOCH).map_err(|_| invalid())?.as_secs(),
            "data": models.iter().map(entry).collect::<Vec<_>>(),
        });
        let bytes = serde_json::to_vec(&payload).map_err(|_| invalid())?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(invalid());
        }
        let parent = self.path.parent().ok_or_else(invalid)?;
        fs::DirBuilder::new().recursive(true).mode(0o700).create(parent)?;
        let (temporary, mut file) = loop {
            let temporary = parent.join(format!(
                ".{FILE}-{}-{}.tmp",
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
}

/// Only the fields the strict parser accepted are persisted. Unknown API hints
/// are dropped: nothing reads them, and they are the one unvalidated blob.
fn entry(model: &Model) -> Value {
    let api: Map<String, Value> = model
        .api
        .iter()
        .map(|(shape, api)| (shape.clone(), json!({"base_url": api.base_url})))
        .collect();
    json!({
        "id": model.id,
        "canonical_id": model.canonical_id,
        "limit": {"context": model.context_window, "output": model.max_output},
        "api": api,
        "cost": model.cost,
        "capabilities": model.capabilities,
        "modalities": model.modalities,
    })
}

struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
