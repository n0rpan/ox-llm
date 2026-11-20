//! #276, #639: the host state of the git protections. A session's sandbox owns it.
//!
//! * Markers. bwrap must create a mount point for every mask whose destination is missing, and
//!   that mount point lands in the host `.git` (the directory is a bind of the host one). The
//!   plan records each such path ([`Marker`]); the session keeps a ledger file per marker under
//!   `<scratch root>/markers/` with a shared `flock` held for the session, and removes the
//!   marker (and its ledger file) once it holds the exclusive lock, i.e. when no other running
//!   sandbox has a mask mounted on it. Dropping the session and the sweep at the next session
//!   start (a killed oxa) do the same. Only exactly what the sandbox made is removed: an empty
//!   directory, or a regular file with the recorded body.
//! * The git config guard. A bound `config` breaks `git branch -m` (git renames `config.lock`
//!   over it, which fails with EBUSY on a mount point), so an existing plain `config` is left
//!   unbound and verified after each command: changes to allowed keys stay, anything else is
//!   restored from the baseline read before the command.
use crate::gitdirs::parse_entries;
use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{Read, Seek, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

/// What the sandbox made in the host's git directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MarkerKind {
    /// An empty-or-recorded regular file (the body it was created with).
    File(Vec<u8>),
    /// An empty directory (a mount point).
    Dir,
}

/// A path the sandbox causes to exist on the host and removes again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Marker {
    pub path: PathBuf,
    pub kind: MarkerKind,
}

#[derive(Default, Debug)]
struct GuardState {
    /// Each guarded file's bytes and mode before the commands now running.
    baseline: HashMap<PathBuf, (Vec<u8>, u32)>,
    running: usize,
    /// The locked guard file holding each baseline on disk.
    saved: HashMap<PathBuf, File>,
}

/// The session's marker ledger and git config guard state.
#[derive(Debug)]
pub struct Markers {
    dir: PathBuf,
    /// Ledger files this session holds a shared lock on.
    held: Mutex<Vec<(PathBuf, File)>>,
    guard: Mutex<GuardState>,
}

impl PartialEq for Markers {
    fn eq(&self, other: &Self) -> bool {
        self.dir == other.dir
    }
}
impl Eq for Markers {}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn flock(file: &File, op: i32) -> bool {
    // SAFETY: `file` owns a valid descriptor for the duration of the call.
    unsafe { libc::flock(file.as_raw_fd(), op) == 0 }
}

/// A stable name for a ledger file (FNV-1a over the marker's path).
fn ledger_name(p: &Path) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in p.as_os_str().as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

fn encode(m: &Marker) -> Option<Vec<u8>> {
    let path = m.path.as_os_str().as_bytes();
    if path.contains(&b'\n') {
        return None;
    }
    let mut out = Vec::new();
    out.extend_from_slice(match m.kind {
        MarkerKind::File(_) => b"f\n",
        MarkerKind::Dir => b"d\n",
    });
    out.extend_from_slice(path);
    out.push(b'\n');
    if let MarkerKind::File(body) = &m.kind {
        out.extend_from_slice(body);
    }
    Some(out)
}

fn decode(bytes: &[u8]) -> Option<Marker> {
    let mut parts = bytes.splitn(3, |b| *b == b'\n');
    let kind = parts.next()?;
    let path = parts.next()?;
    let body = parts.next().unwrap_or(&[]);
    let path = PathBuf::from(std::ffi::OsStr::from_bytes(path));
    match kind {
        b"f" => Some(Marker {
            path,
            kind: MarkerKind::File(body.to_vec()),
        }),
        b"d" => Some(Marker {
            path,
            kind: MarkerKind::Dir,
        }),
        _ => None,
    }
}

/// Remove `m` when it is still exactly what the sandbox made.
fn remove_if_ours(m: &Marker) {
    let Ok(meta) = std::fs::symlink_metadata(&m.path) else {
        return;
    };
    match &m.kind {
        MarkerKind::File(body) => {
            if meta.file_type().is_file() && std::fs::read(&m.path).is_ok_and(|now| now == *body) {
                let _ = std::fs::remove_file(&m.path);
            }
        }
        MarkerKind::Dir => {
            if meta.file_type().is_dir() {
                // Not recursive: a directory with anything in it is not ours any more.
                let _ = std::fs::remove_dir(&m.path);
            }
        }
    }
}

fn read_ledger(path: &Path) -> Option<Marker> {
    let mut bytes = Vec::new();
    File::open(path).ok()?.read_to_end(&mut bytes).ok()?;
    decode(&bytes)
}

/// Remove every marker nobody holds a lock on (see the module docs), and its ledger file.
fn sweep(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let led = e.path();
        let Ok(file) = File::open(&led) else {
            continue;
        };
        if !flock(&file, libc::LOCK_EX | libc::LOCK_NB) {
            continue;
        }
        if let Some(m) = read_ledger(&led) {
            remove_if_ours(&m);
        }
        let _ = std::fs::remove_file(&led);
    }
}

impl Markers {
    /// The ledger below `scratch_root`; sweeps what a killed session left. `None` when the
    /// ledger directory cannot be created (then nothing is tracked, as before).
    pub fn new(scratch_root: &Path) -> Option<Markers> {
        let dir = scratch_root.join("markers");
        std::fs::create_dir_all(&dir).ok()?;
        sweep_guard(&dir.join("guard"));
        sweep(&dir);
        Some(Markers {
            dir,
            held: Mutex::new(Vec::new()),
            guard: Mutex::new(GuardState::default()),
        })
    }

    /// Record `markers` and hold a shared lock on each until the session ends.
    pub(crate) fn register(&self, markers: &[Marker]) {
        for m in markers {
            self.hold(m);
        }
    }

    fn hold(&self, m: &Marker) {
        let led = self.dir.join(ledger_name(&m.path));
        let Some(body) = encode(m) else {
            return;
        };
        let mut held = lock(&self.held);
        if held.iter().any(|(p, _)| *p == led) {
            return;
        }
        for _ in 0..3 {
            let Ok(file) = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&led)
            else {
                return;
            };
            if !flock(&file, libc::LOCK_SH) {
                return;
            }
            // A sweeper may have unlinked this ledger between our open and our lock.
            let same = match (file.metadata(), std::fs::metadata(&led)) {
                (Ok(a), Ok(b)) => a.ino() == b.ino() && a.dev() == b.dev(),
                _ => false,
            };
            if !same {
                continue;
            }
            if file.metadata().is_ok_and(|md| md.len() == 0) {
                let _ = (&file).write_all(&body);
            }
            held.push((led, file));
            return;
        }
    }

    /// Hold a shared lock on every ledger entry whose marker lies below one of `dirs` (made by
    /// another session of a repository this one masks too), so it is not removed under us.
    pub(crate) fn adopt_under(&self, dirs: &[PathBuf]) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        for e in entries.flatten() {
            if let Some(m) = read_ledger(&e.path()) {
                if dirs.iter().any(|d| m.path.starts_with(d)) && std::fs::symlink_metadata(&m.path).is_ok() {
                    self.hold(&m);
                }
            }
        }
    }

    /// A command starts: with no other command running, the files' current bytes are the
    /// baseline (an edit by the owner between commands is accepted). Each baseline is also
    /// kept in the guard directory under a lock held for the session, so a killed oxa is
    /// repaired by the next sweep like the markers are.
    pub(crate) fn guard_begin(&self, files: &[PathBuf], allow: &[String]) {
        let mut g = lock(&self.guard);
        let fresh = g.running == 0;
        for f in files {
            if fresh || !g.baseline.contains_key(f) {
                match read_plain(f) {
                    Some(state) => {
                        self.persist(&mut g, f, &state, allow);
                        g.baseline.insert(f.clone(), state);
                    }
                    None => {
                        g.baseline.remove(f);
                        g.saved.remove(f);
                    }
                }
            }
        }
        g.running += 1;
    }

    fn guard_dir(&self) -> PathBuf {
        self.dir.join("guard")
    }

    /// Write the baseline of `f` to this session's guard file for it (locked while we live).
    fn persist(&self, g: &mut GuardState, f: &Path, state: &(Vec<u8>, u32), allow: &[String]) {
        let dir = self.guard_dir();
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        if !g.saved.contains_key(f) {
            let name = format!("{}-{}", ledger_name(f), std::process::id());
            let Ok(file) = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(dir.join(name))
            else {
                return;
            };
            if !flock(&file, libc::LOCK_SH) {
                return;
            }
            g.saved.insert(f.to_path_buf(), file);
        }
        if let Some(file) = g.saved.get(f) {
            let body = encode_guard(f, state, allow);
            let _ = file.set_len(0);
            let _ = (&*file).seek(std::io::SeekFrom::Start(0));
            let _ = (&*file).write_all(&body);
        }
    }

    /// A command ended: check each guarded file now, whatever else still runs (git replaces
    /// config atomically by rename, so reading it is safe). Changes to allowed keys become the
    /// new baseline; anything else is restored at once and reported to the command that ended.
    pub(crate) fn guard_end(&self, files: &[PathBuf], allow: &[String]) -> Option<String> {
        let mut g = lock(&self.guard);
        g.running = g.running.saturating_sub(1);
        let mut notes = Vec::new();
        for f in files {
            let Some((base, mode)) = g.baseline.get(f).cloned() else {
                continue;
            };
            match check_guarded(f, &base, mode, allow) {
                Guarded::Same => {}
                Guarded::Accepted(state) => {
                    self.persist(&mut g, f, &state, allow);
                    g.baseline.insert(f.clone(), state);
                }
                Guarded::Restored(what) => notes.push(restored_note(f, &what)),
            }
            if g.running == 0 {
                // Nothing runs: the file is clean, so its guard file is not needed.
                if let Some(file) = g.saved.remove(f) {
                    drop(file);
                    let name = format!("{}-{}", ledger_name(f), std::process::id());
                    let _ = std::fs::remove_file(self.guard_dir().join(name));
                }
            }
        }
        (!notes.is_empty()).then(|| format!("\n{}\n", notes.join("\n")))
    }
}

enum Guarded {
    Same,
    Accepted((Vec<u8>, u32)),
    Restored(String),
}

fn restored_note(f: &Path, what: &str) -> String {
    format!(
        "Sandbox: restored {}: the command changed git config {what}, which can run or redirect programs outside the sandbox. Allow with oxa.sandbox.gitConfigAllow in user settings.",
        f.display()
    )
}

/// Compare `f` with its baseline; restore it when something not allowed changed.
fn check_guarded(f: &Path, base: &[u8], mode: u32, allow: &[String]) -> Guarded {
    let now = read_plain(f);
    if now.as_ref().is_some_and(|(bytes, _)| bytes == base) {
        return Guarded::Same;
    }
    let refused = match &now {
        Some((bytes, _)) => refused_keys(base, bytes, allow),
        None => Err("the file was replaced or removed".to_string()),
    };
    match (refused, now) {
        (Ok(keys), Some(state)) if keys.is_empty() => Guarded::Accepted(state),
        (other, _) => {
            restore(f, base, mode);
            Guarded::Restored(match other {
                Ok(keys) => keys.join(", "),
                Err(why) => why,
            })
        }
    }
}

/// Guard file body: mode, target path, allow patterns, then the baseline bytes. Header lines
/// are separated by newlines; a path or pattern with a newline is not persisted.
fn encode_guard(f: &Path, state: &(Vec<u8>, u32), allow: &[String]) -> Vec<u8> {
    let mut out = format!("{:o}\n", state.1).into_bytes();
    out.extend_from_slice(f.as_os_str().as_bytes());
    out.push(b'\n');
    out.extend_from_slice(allow.join("\u{1f}").as_bytes());
    out.push(b'\n');
    out.extend_from_slice(&state.0);
    out
}

fn decode_guard(bytes: &[u8]) -> Option<(PathBuf, Vec<u8>, u32, Vec<String>)> {
    let mut parts = bytes.splitn(4, |b| *b == b'\n');
    let mode = u32::from_str_radix(std::str::from_utf8(parts.next()?).ok()?, 8).ok()?;
    let path = PathBuf::from(std::ffi::OsStr::from_bytes(parts.next()?));
    let allow = std::str::from_utf8(parts.next()?).ok()?;
    let allow = allow
        .split('\u{1f}')
        .filter(|p| !p.is_empty())
        .map(String::from)
        .collect();
    Some((path, parts.next()?.to_vec(), mode, allow))
}

/// Repair every guarded file whose session is gone (its guard file is not locked).
fn sweep_guard(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        let Ok(file) = File::open(&p) else {
            continue;
        };
        if !flock(&file, libc::LOCK_EX | libc::LOCK_NB) {
            continue;
        }
        let mut bytes = Vec::new();
        if (&file).read_to_end(&mut bytes).is_ok() {
            if let Some((f, base, mode, allow)) = decode_guard(&bytes) {
                let _ = check_guarded(&f, &base, mode, &allow);
            }
        }
        let _ = std::fs::remove_file(&p);
    }
}

impl Drop for Markers {
    fn drop(&mut self) {
        // Release our locks first, then remove what nobody else holds.
        lock(&self.held).clear();
        lock(&self.guard).saved.clear();
        sweep_guard(&self.guard_dir());
        sweep(&self.dir);
    }
}

/// A plain (not symlinked) regular file's bytes and mode.
fn read_plain(p: &Path) -> Option<(Vec<u8>, u32)> {
    let meta = std::fs::symlink_metadata(p).ok()?;
    if !meta.file_type().is_file() {
        return None;
    }
    Some((std::fs::read(p).ok()?, meta.permissions().mode() & 0o7777))
}

/// Put `bytes` back at `path`: a temporary file next to it, then a rename (which replaces a
/// symlink instead of following it).
fn restore(path: &Path, bytes: &[u8], mode: u32) {
    let Some(name) = path.file_name() else {
        return;
    };
    let tmp = path.with_file_name(format!(
        ".{}.oxa-restore-{}",
        name.to_string_lossy(),
        std::process::id()
    ));
    let _ = std::fs::remove_file(&tmp);
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&tmp)
        .and_then(|mut f| {
            f.write_all(bytes)?;
            f.sync_all()
        });
    if written.is_err() || std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

type Values = BTreeMap<String, Vec<Vec<u8>>>;

fn values(bytes: &[u8]) -> Result<Values, String> {
    let mut out = Values::new();
    for (k, v) in parse_entries(bytes)? {
        out.entry(k).or_default().push(v);
    }
    Ok(out)
}

/// The keys whose values differ between `base` and `now` and are not allowed by a pattern.
/// `Err` when either file does not parse (restore it then).
fn refused_keys(base: &[u8], now: &[u8], allow: &[String]) -> Result<Vec<String>, String> {
    let (a, b) = (values(base)?, values(now)?);
    let mut changed: Vec<String> = a
        .keys()
        .chain(b.keys())
        .filter(|k| a.get(*k) != b.get(*k))
        .cloned()
        .collect();
    changed.sort();
    changed.dedup();
    Ok(changed
        .into_iter()
        .filter(|k| !allow.iter().any(|p| key_matches(p, k)))
        .collect())
}

/// Whether the config key `key` (`section[.subsection].name`) matches `pattern`, compared
/// case-insensitively. In a pattern `*` matches one dotted component or a whole subsection;
/// `section.*` matches every key of the section, with or without a subsection.
fn key_matches(pattern: &str, key: &str) -> bool {
    let (pattern, key) = (pattern.to_ascii_lowercase(), key.to_ascii_lowercase());
    let p: Vec<&str> = pattern.split('.').collect();
    let Some((section, rest)) = key.split_once('.') else {
        return false;
    };
    if p[0] != "*" && p[0] != section {
        return false;
    }
    match p.len() {
        1 => false,
        2 => p[1] == "*" || p[1] == rest,
        _ => {
            let Some((sub, name)) = rest.rsplit_once('.') else {
                return false;
            };
            let want = p[1..p.len() - 1].join(".");
            (want == "*" || want == sub) && (p[p.len() - 1] == "*" || p[p.len() - 1] == name)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allow() -> Vec<String> {
        ["branch.*", "user.*", "remote.*.fetch"]
            .map(String::from)
            .to_vec()
    }

    #[test]
    fn patterns_match_components_and_subsections() {
        assert!(key_matches("branch.*", "branch.main.remote"));
        assert!(key_matches("branch.*", "branch.autosetuprebase"));
        assert!(key_matches("user.*", "user.name"));
        assert!(key_matches("remote.*.fetch", "remote.origin.fetch"));
        assert!(key_matches("REMOTE.*.fetch", "remote.origin.fetch"));
        assert!(!key_matches("remote.*.fetch", "remote.origin.url"));
        assert!(!key_matches("branch.*", "core.hookspath"));
        assert!(!key_matches("user.*", "alias.st"));
    }

    #[test]
    fn a_branch_rename_is_allowed_and_a_hooks_path_is_not() {
        let base = b"[core]\n\tbare = false\n[branch \"old\"]\n\tremote = origin\n";
        let renamed = b"[core]\n\tbare = false\n[branch \"new\"]\n\tremote = origin\n";
        assert_eq!(refused_keys(base, renamed, &allow()), Ok(Vec::new()));
        let evil = b"[core]\n\tbare = false\n\thooksPath = /tmp/x\n[branch \"old\"]\n\tremote = origin\n";
        assert_eq!(
            refused_keys(base, evil, &allow()),
            Ok(vec!["core.hookspath".to_string()])
        );
        assert!(refused_keys(base, b"[core\n", &allow()).is_err());
        assert_eq!(refused_keys(base, base, &[]), Ok(Vec::new()));
    }

    const CLEAN: &str = "[core]\n\tbare = false\n";
    const EVIL: &str = "[core]\n\tbare = false\n\thooksPath = /tmp/x\n";

    fn scratch(tag: &str) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("oxa-guard-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("repo")).unwrap();
        let cfg = root.join("repo").join("config");
        std::fs::write(&cfg, CLEAN).unwrap();
        (root, cfg)
    }

    #[test]
    fn a_forbidden_key_is_restored_when_a_background_job_still_runs() {
        let (root, cfg) = scratch("bg");
        let m = Markers::new(&root.join("s")).unwrap();
        let files = vec![cfg.clone()];
        let allow = allow();
        m.guard_begin(&files, &allow); // the background job
        m.guard_begin(&files, &allow); // the foreground command
        std::fs::write(&cfg, EVIL).unwrap();
        let note = m.guard_end(&files, &allow);
        assert!(note.is_some_and(|n| n.contains("core.hookspath")));
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), CLEAN);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_killed_session_leaves_no_forbidden_key_behind() {
        let (root, cfg) = scratch("kill");
        let scratch_root = root.join("s");
        let m = Markers::new(&scratch_root).unwrap();
        let files = vec![cfg.clone()];
        m.guard_begin(&files, &allow());
        std::fs::write(&cfg, EVIL).unwrap();
        // The session ends with the command still running (no guard_end).
        drop(m);
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), CLEAN);
        // A killed oxa: the guard file stays, its lock is gone, and the next start repairs it.
        let m = Markers::new(&scratch_root).unwrap();
        m.guard_begin(&files, &allow());
        std::fs::write(&cfg, EVIL).unwrap();
        lock(&m.guard).saved.clear();
        std::mem::forget(m);
        let _next = Markers::new(&scratch_root).unwrap();
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), CLEAN);
        let _ = std::fs::remove_dir_all(&root);
    }
}
