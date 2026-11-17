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
//! #995: a tool that hardcodes the cache home path (instead of reading `$XDG_CACHE_HOME`) -
//! `CARGO_TARGET_DIR=$HOME/.cache/...`, `touch ~/.cache/x`, a git hook writing its log there -
//! would meet the empty read-only home inside the sandbox, because the host cache home is not
//! writable there. The session therefore also holds an empty `.cache-home`, which the argv
//! binds at the host cache home path: those writes land in the session directory and are
//! deleted with it, while the read-write package caches keep their own binds under
//! `<cache home>/<name>` and stay the user's.
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
        std::fs::create_dir(path.join(CACHE_HOME_DIR))?;
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

    /// #995: the session's own `$HOME/.cache`, bound at the host cache home path by the sandbox
    /// argv: the directory every otherwise-unbound cache write goes to, since the host cache
    /// home itself is not writable inside. The read-write package caches keep their own binds
    /// under `<cache home>/<name>` and stay shared.
    pub fn home_cache_dir(&self) -> PathBuf {
        self.path.join(CACHE_HOME_DIR)
    }
}

/// The session directory's name for the host cache home path (`$HOME/.cache`; #995).
pub(crate) const CACHE_HOME_DIR: &str = ".cache-home";

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

#[cfg(test)]
mod tests {
    use super::*;

    /// #995: a fresh session directory holds its own empty cache home for the argv's
    /// `$HOME/.cache` bind, and it goes away with the session.
    #[test]
    fn a_session_directory_holds_an_own_cache_home() {
        let env = oxa_fake::TestEnv::new();
        let sc = Scratch::create(&env.tmp().join("scratch")).expect("scratch");
        let dir = sc.home_cache_dir();
        assert_eq!(dir, sc.path().join(".cache-home"));
        assert!(dir.is_dir(), "{dir:?}");
        assert_eq!(std::fs::read_dir(&dir).expect("read_dir").count(), 0);
        let path = sc.path().to_path_buf();
        drop(sc);
        assert!(!path.exists(), "the session directory outlived the session");
    }
}
