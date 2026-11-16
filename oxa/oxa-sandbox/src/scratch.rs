//! #457: the session directory. One host directory per oxa session, mounted at `/tmp` inside
//! every sandboxed command of that session (instead of a fresh tmpfs per command), so scratch
//! work survives from one bash call to the next. `TMPDIR=/tmp` and `XDG_CACHE_HOME=/tmp/.cache`
//! point into it (`Layout::build`), which gives every XDG tool (gh, pip, sccache, mise, ...) a
//! writable cache home without a bind per tool.
//!
//! The user's cache home ([`user_cache_home`]: `$XDG_CACHE_HOME` when set to an absolute path,
//! else `~/.cache`) keeps its shared caches: each entry of it that the layout binds read-write
//! (directly or through a bound parent) is linked from `/tmp/.cache/<name>`, so it stays the
//! cache of the user's own builds. Everything else a tool caches goes to the session.
//!
//! It lives at `~/.cache/oxa/scratch/<pid>-<n>` (mode 0700, never under the agent directory).
//! Each session gets its own (`Bwrap::session`, when a session starts, at `/new`, `/resume`,
//! fork and clone); it is deleted when that session ends: when it is replaced by another one
//! or oxa exits. A directory left by an oxa that was killed is removed when a later oxa
//! creates its next one.
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// The parent of every session directory of `home`.
pub fn scratch_root(home: &Path) -> PathBuf {
    home.join(".cache/oxa/scratch")
}

/// The user's cache home, as the XDG tools outside the sandbox see it: `XDG_CACHE_HOME` from
/// `env` when it is an absolute path (the XDG rule ignores a relative one), else `~/.cache`.
pub fn user_cache_home(home: &Path, env: &[(String, String)]) -> PathBuf {
    env.iter()
        .find(|(k, _)| k == "XDG_CACHE_HOME")
        .map(|(_, v)| PathBuf::from(v))
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home.join(".cache"))
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

    /// Link `/tmp/.cache/<name>` to each entry of the user's cache home `cache` that a
    /// read-write bind in `write` covers (the bind keeps its host path inside, so the link
    /// resolves there). Links only to what is bound read-write; nothing for an empty `cache`.
    pub(crate) fn link_caches(&self, cache: &Path, write: &[PathBuf]) {
        if !cache.is_absolute() {
            return;
        }
        let Ok(entries) = std::fs::read_dir(cache) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            if write.iter().any(|w| p.starts_with(w)) {
                let _ = std::os::unix::fs::symlink(&p, self.path.join(".cache").join(e.file_name()));
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
