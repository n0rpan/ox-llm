//! #457: the session directory. One host directory per oxa process, mounted at `/tmp` inside
//! every sandboxed command of the session (instead of a fresh tmpfs per command), so scratch
//! work survives from one bash call to the next. `TMPDIR=/tmp` and `XDG_CACHE_HOME=/tmp/.cache`
//! point into it (`Layout::build`), which gives every XDG tool (gh, pip, sccache, mise, ...) a
//! writable cache home without a bind per tool. The package caches the layout already binds
//! read-write under `~/.cache` are linked from `/tmp/.cache/<name>`, so they stay shared with
//! the user's own builds.
//!
//! It lives at `~/.cache/oxa/scratch/<pid>-<n>` (mode 0700, never under the agent directory)
//! and is deleted when the last handle drops; directories of processes that are gone are
//! removed when the next one is created.
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// The parent of every session directory of `home`.
pub fn scratch_root(home: &Path) -> PathBuf {
    home.join(".cache/oxa/scratch")
}

/// A session directory; removed on drop.
#[derive(Debug)]
pub struct Scratch {
    path: PathBuf,
}

impl Scratch {
    /// Create a fresh session directory under `root` (see [`scratch_root`]) and remove the
    /// ones whose process has exited.
    pub fn create(root: &Path) -> std::io::Result<Scratch> {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        static N: AtomicU32 = AtomicU32::new(0);
        std::fs::create_dir_all(root)?;
        prune(root);
        let pid = std::process::id();
        let path = root.join(format!("{pid}-{}", N.fetch_add(1, Ordering::Relaxed)));
        if path.exists() {
            std::fs::remove_dir_all(&path)?;
        }
        std::fs::DirBuilder::new().mode(0o700).create(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
        std::fs::create_dir(path.join(".cache"))?;
        Ok(Scratch { path })
    }

    /// The host path (mounted at `/tmp` inside).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Link `/tmp/.cache/<name>` to each writable bind directly under `~/.cache` (the bind
    /// keeps its host path inside, so the link resolves there). Links only to what is bound.
    pub(crate) fn link_caches(&self, home: &Path, write: &[PathBuf]) {
        let cache = home.join(".cache");
        for w in write.iter().filter(|w| w.parent() == Some(cache.as_path())) {
            if let Some(name) = w.file_name() {
                let _ = std::os::unix::fs::symlink(w, self.path.join(".cache").join(name));
            }
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // `remove_dir_all` never follows a symlink a command left in the directory.
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

impl PartialEq for Scratch {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path
    }
}
impl Eq for Scratch {}

/// Remove the session directories of processes that no longer run.
fn prune(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|n| n.split('-').next())
            .and_then(|p| p.parse::<u32>().ok())
        else {
            continue;
        };
        if !Path::new("/proc").join(pid.to_string()).exists() {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}
