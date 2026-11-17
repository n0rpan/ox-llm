//! Short-lived locks for fork publication, deletion and spill reclamation.
//! Separate from the long-lived writer lock: forking an open session remains allowed.
use crate::spill::linked_spill_stores;
use crate::store::{check_safe, ensure_dir};
use oxa_proto::{codes, OxaError};
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

static MEMORY_LIFECYCLE: Mutex<()> = Mutex::new(());
const LOCK_FILE: &str = ".spill-lifecycle.lock";

/// Held only while publishing a fork, deleting a session, or reclaiming spills.
/// Dropping it releases the locks, including when an operation fails or unwinds.
#[derive(Default)]
pub struct SpillLifecycleLock {
    _files: Vec<File>,
    _memory: Option<MutexGuard<'static, ()>>,
}

impl SpillLifecycleLock {
    pub(crate) fn memory() -> Self {
        Self {
            _files: Vec::new(),
            _memory: Some(MEMORY_LIFECYCLE.lock().unwrap_or_else(|e| e.into_inner())),
        }
    }

    /// Both endpoints must be locked before loading a source, not just before creating
    /// the child. Sorted canonical roots prevent deadlocks for opposite-direction forks.
    pub(crate) fn publication(root: &Path, session: &Path) -> Result<Self, OxaError> {
        ensure_dir(root)?;
        let mut roots = BTreeSet::from([canonical(root)?]);
        if let Some(source) = session.parent().and_then(Path::parent) {
            check_safe(session)?;
            roots.insert(canonical(source)?);
        }
        Self::roots(&roots)
    }

    pub(crate) fn cleanup(root: &Path) -> Result<Self, OxaError> {
        let root = canonical(root)?;
        loop {
            let roots = component(&root)?;
            let held = Self::roots(&roots)?;
            // A fork may have added a link while we waited. Retry with the expanded
            // component, releasing every lock first to preserve the global lock order.
            if component(&root)?.is_subset(&roots) {
                return Ok(held);
            }
        }
    }

    fn roots(roots: &BTreeSet<PathBuf>) -> Result<Self, OxaError> {
        let mut files = Vec::new();
        for root in roots {
            let path = root.join(LOCK_FILE);
            check_safe(&path)?;
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(&path)
                .map_err(|e| OxaError::io("cannot open spill lifecycle lock", &e))?;
            if !file
                .metadata()
                .map_err(|e| OxaError::io("cannot stat spill lifecycle lock", &e))?
                .is_file()
            {
                return Err(OxaError::new(
                    codes::UNSAFE_PATH,
                    "spill lifecycle lock is not a regular file",
                ));
            }
            loop {
                // SAFETY: file owns the descriptor for the duration of the lock.
                if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
                    break;
                }
                let e = std::io::Error::last_os_error();
                if e.kind() != std::io::ErrorKind::Interrupted {
                    return Err(OxaError::io("cannot lock spill lifecycle", &e));
                }
            }
            // Never unlink these sidecars: every process must lock the same inode.
            files.push(file);
        }
        Ok(Self {
            _files: files,
            _memory: None,
        })
    }
}

fn canonical(path: &Path) -> Result<PathBuf, OxaError> {
    check_safe(path)?;
    fs::canonicalize(path).map_err(|e| OxaError::io("cannot locate spill lifecycle store", &e))
}

fn component(root: &Path) -> Result<BTreeSet<PathBuf>, OxaError> {
    linked_spill_stores(root)?.iter().map(|p| canonical(p)).collect()
}
