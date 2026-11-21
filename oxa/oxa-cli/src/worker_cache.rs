//! #1541: the shared per-repository build cache of a worker, derived by the worker itself.
//!
//! A worker child (`OXA_UNATTENDED` set, by both launchers) works out one cache directory for
//! the main repository it runs in: `<cache root>/<hash>/cargo-target`. Every worktree of one
//! repository gets the same directory, so a build is warm from the second worker on. The
//! sandbox layout grants exactly that path as writable and points `CARGO_TARGET_DIR` at it
//! ([`crate::sandbox_layout`]). The settings are user-only; a project file cannot move, widen
//! or turn off the cache. Nothing here blocks: any error means no cache and one warning.
use oxa_config::{Env, Settings};
use sha2::{Digest, Sha256};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// `oxa.workers.sharedCache` (bool, default true, user settings only).
pub(crate) const SETTING_SHARED: &str = "oxa.workers.sharedCache";
/// `oxa.workers.cacheDir` (path, user settings only).
pub(crate) const SETTING_DIR: &str = "oxa.workers.cacheDir";
/// `oxa.workers.cacheWarnGb` (number, no default, user settings only).
pub(crate) const SETTING_WARN_GB: &str = "oxa.workers.cacheWarnGb";

/// The most directory entries one size walk looks at.
const WALK_CAP: usize = 200_000;

/// The cache root: `oxa.workers.cacheDir` (`~/` expanded), else `<XDG cache home>/oxa/workers`.
pub(crate) fn cache_root(settings: &Settings, home: &Path, env: &Env) -> PathBuf {
    if let Some(raw) = settings.user_value(SETTING_DIR).and_then(|v| v.as_str()) {
        let p = oxa_sandbox::expand_home(raw, home);
        if p.is_absolute() {
            return p;
        }
    }
    let base = env
        .get("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home.join(".cache"));
    base.join("oxa").join("workers")
}

/// The main repository directory of the checkout or worktree that holds `cwd`, found by
/// reading files only (never `git`): a `.git` directory names its own parent; a `.git` file
/// (`gitdir: <path>`) points at a gitdir whose `commondir` names the shared `.git`.
pub(crate) fn main_repo(cwd: &Path) -> Option<PathBuf> {
    let start = cwd.canonicalize().ok()?;
    for dir in start.ancestors() {
        let dot_git = dir.join(".git");
        let Ok(meta) = fs::symlink_metadata(&dot_git) else {
            continue;
        };
        if meta.is_dir() {
            return Some(dir.to_path_buf());
        }
        if !meta.is_file() {
            return None;
        }
        let text = fs::read_to_string(&dot_git).ok()?;
        let target = text.lines().next()?.strip_prefix("gitdir:")?.trim();
        let gitdir = if Path::new(target).is_absolute() {
            PathBuf::from(target)
        } else {
            dir.join(target)
        };
        let gitdir = gitdir.canonicalize().ok()?;
        let common = match fs::read_to_string(gitdir.join("commondir")) {
            Ok(c) => {
                let c = c.trim();
                let p = if Path::new(c).is_absolute() {
                    PathBuf::from(c)
                } else {
                    gitdir.join(c)
                };
                p.canonicalize().ok()?
            }
            Err(_) => return Some(dir.to_path_buf()),
        };
        return common.parent().map(Path::to_path_buf);
    }
    None
}

fn repo_hash(repo: &Path) -> String {
    let digest = Sha256::digest(repo.as_os_str().as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    hex[..16].to_string()
}

/// One private level: created 0700, an existing looser one tightened, a symlink or a
/// directory of another user refused.
fn private_dir(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() || !meta.is_dir() {
                return Err(io::Error::other(format!(
                    "{} is a symlink or not a directory",
                    path.display()
                )));
            }
            // SAFETY: geteuid has no preconditions.
            if meta.uid() != unsafe { libc::geteuid() } {
                return Err(io::Error::other(format!(
                    "{} is not owned by this user",
                    path.display()
                )));
            }
            if meta.permissions().mode() & 0o077 != 0 {
                fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
            }
            Ok(())
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            fs::DirBuilder::new().recursive(true).mode(0o700).create(path)?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        }
        Err(e) => Err(e),
    }
}

/// `<cache_root>/<hash of the main repository>/cargo-target`, created private.
pub(crate) fn cargo_target(cache_root: &Path, repo: &Path) -> io::Result<PathBuf> {
    let repo_dir = cache_root.join(repo_hash(repo));
    let target = repo_dir.join("cargo-target");
    private_dir(cache_root)?;
    private_dir(&repo_dir)?;
    private_dir(&target)?;
    Ok(target)
}

/// What the worker derives for a session, or why it cannot.
pub(crate) enum Derived {
    /// A worker that should not (or cannot) have the cache: not a worker, turned off, or the
    /// owner chose `CARGO_TARGET_DIR` himself.
    None,
    Path(PathBuf),
    Failed(String),
}

/// The cache for a worker process in `cwd`, from the user settings and the environment only.
pub(crate) fn derive(settings: &Settings, home: &Path, env: &Env, cwd: &Path) -> Derived {
    if env.get("OXA_UNATTENDED").is_none()
        || !settings.user_bool(SETTING_SHARED, true)
        || env.get("CARGO_TARGET_DIR").is_some_and(|v| !v.is_empty())
    {
        return Derived::None;
    }
    let repo = main_repo(cwd).unwrap_or_else(|| cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf()));
    match cargo_target(&cache_root(settings, home, env), &repo) {
        Ok(p) => Derived::Path(p),
        Err(e) => Derived::Failed(e.to_string()),
    }
}

/// What the Workers view shows about the cache of the parent's repository.
#[derive(Debug, Clone)]
pub(crate) struct CacheView {
    pub dir: PathBuf,
    pub warn_gb: Option<u64>,
}

/// The view for a parent session in `cwd`; `None` when the cache is turned off.
pub(crate) fn view(settings: &Settings, home: &Path, env: &Env, cwd: &Path) -> Option<CacheView> {
    if !settings.user_bool(SETTING_SHARED, true) {
        return None;
    }
    let repo = main_repo(cwd).unwrap_or_else(|| cwd.to_path_buf());
    Some(CacheView {
        dir: cache_root(settings, home, env)
            .join(repo_hash(&repo))
            .join("cargo-target"),
        warn_gb: settings
            .user_value(SETTING_WARN_GB)
            .and_then(|v| v.as_u64())
            .filter(|n| *n > 0),
    })
}

/// Total bytes below `dir` (symlinks not followed) and whether the entry cap stopped the walk.
fn size_of(dir: &Path) -> (u64, bool) {
    let (mut total, mut seen) = (0u64, 0usize);
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for entry in rd.flatten() {
            seen += 1;
            if seen > WALK_CAP {
                return (total, true);
            }
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                total += meta.len();
            }
        }
    }
    (total, false)
}

fn human(bytes: u64) -> String {
    const GB: f64 = 1_073_741_824.0;
    const MB: f64 = 1_048_576.0;
    if bytes as f64 >= GB {
        format!("{:.1} GB", bytes as f64 / GB)
    } else {
        format!("{:.0} MB", bytes as f64 / MB)
    }
}

/// The one line of the Workers view: the cache path and its size; a warning naming the
/// directory to delete only when the owner set `oxa.workers.cacheWarnGb` and it is exceeded.
pub(crate) fn describe(view: &CacheView) -> Option<String> {
    if !view.dir.is_dir() {
        return None;
    }
    let (bytes, capped) = size_of(&view.dir);
    let size = if capped {
        format!("at least {}", human(bytes))
    } else {
        human(bytes)
    };
    let over = view
        .warn_gb
        .filter(|gb| bytes >= gb.saturating_mul(1_073_741_824));
    Some(match over {
        Some(gb) => format!(
            "warning: the worker build cache is {size}, above {SETTING_WARN_GB} ({gb} GB); delete {} to free the space",
            view.dir.display()
        ),
        None => format!("worker build cache: {} ({size})", view.dir.display()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("oxa-wcache-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d.canonicalize().unwrap()
    }

    /// A main repository `<root>/main` and a linked worktree `<root>/wt<n>`.
    fn repo_with_worktree(root: &Path, n: u32) -> (PathBuf, PathBuf) {
        let main = root.join("main");
        let wt = root.join(format!("wt{n}"));
        let gitdir = main.join(".git/worktrees").join(format!("w{n}"));
        fs::create_dir_all(&gitdir).unwrap();
        fs::create_dir_all(&wt).unwrap();
        fs::write(gitdir.join("commondir"), "../..\n").unwrap();
        fs::write(wt.join(".git"), format!("gitdir: {}\n", gitdir.display())).unwrap();
        (main, wt)
    }

    #[test]
    fn worktrees_of_one_repo_share_one_dir_and_another_repo_does_not() {
        let root = tmp("share");
        let (main, wt1) = repo_with_worktree(&root, 1);
        let (_, wt2) = repo_with_worktree(&root, 2);
        let other = root.join("other");
        fs::create_dir_all(other.join(".git")).unwrap();
        let cache = root.join("cache");
        let key = |p: &Path| main_repo(p).unwrap();
        assert_eq!(key(&wt1), main);
        assert_eq!(key(&wt2), main);
        let a = cargo_target(&cache, &key(&wt1)).unwrap();
        let b = cargo_target(&cache, &key(&wt2)).unwrap();
        let c = cargo_target(&cache, &key(&other)).unwrap();
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(a.ends_with("cargo-target"));
        assert_eq!(fs::metadata(&a).unwrap().permissions().mode() & 0o777, 0o700);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_loose_existing_dir_is_tightened() {
        let root = tmp("loose");
        let cache = root.join("cache");
        let repo = root.join("r");
        fs::create_dir_all(repo.join(".git")).unwrap();
        let target = cargo_target(&cache, &repo).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o777)).unwrap();
        fs::set_permissions(&cache, fs::Permissions::from_mode(0o755)).unwrap();
        cargo_target(&cache, &repo).unwrap();
        assert_eq!(fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(fs::metadata(&cache).unwrap().permissions().mode() & 0o777, 0o700);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_symlinked_cache_dir_is_refused() {
        let root = tmp("link");
        let elsewhere = root.join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        let cache = root.join("cache");
        fs::create_dir_all(&cache).unwrap();
        let repo = root.join("r");
        fs::create_dir_all(repo.join(".git")).unwrap();
        let hashed = cache.join(repo_hash(&repo));
        std::os::unix::fs::symlink(&elsewhere, &hashed).unwrap();
        assert!(cargo_target(&cache, &repo).is_err());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_size_line_names_the_path_and_warns_only_when_the_owner_set_a_limit() {
        let root = tmp("size");
        let dir = root.join("cargo-target");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("f"), vec![0u8; 4096]).unwrap();
        let quiet = CacheView {
            dir: dir.clone(),
            warn_gb: None,
        };
        let line = describe(&quiet).unwrap();
        assert!(
            line.starts_with("worker build cache:") && line.contains("cargo-target"),
            "{line}"
        );
        let (bytes, capped) = size_of(&dir);
        assert_eq!((bytes, capped), (4096, false));
        // A limit that is not reached stays a plain line.
        let roomy = CacheView {
            dir,
            warn_gb: Some(1),
        };
        assert!(!describe(&roomy).unwrap().starts_with("warning"));
        let _ = fs::remove_dir_all(&root);
    }
}
