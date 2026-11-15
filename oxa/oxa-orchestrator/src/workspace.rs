//! Explicitly selected, bounded text snapshots. No source writes or execution.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::{
    ffi::OsStrExt,
    fs::{MetadataExt, OpenOptionsExt},
};
use std::path::Path;
const MAX_FILES: usize = 32;
const MAX_FILE: usize = 256 * 1024;
const MAX_TOTAL: usize = 1024 * 1024;
const MAX_DEPTH: usize = 16;
const MAX_ENTRIES: usize = 128;
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct RelativePath(String);
impl RelativePath {
    pub fn new(value: String) -> io::Result<Self> {
        if value.is_empty()
            || value.len() > 256
            || value.contains('\\')
            || value.chars().any(char::is_control)
            || value
                .split('/')
                .any(|p| p.is_empty() || p == "." || p == ".." || protected(p))
        {
            return Err(denied("invalid or protected workspace path"));
        }
        Ok(Self(value))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl<'de> Deserialize<'de> for RelativePath {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(de)?).map_err(serde::de::Error::custom)
    }
}

// Count the virtual materialized tree, not the source root's unrelated entries.
// These are the same nesting/entry bounds used when collecting worker output.
fn validate_structure<'a>(paths: impl Iterator<Item = &'a RelativePath>) -> io::Result<()> {
    let mut entries = BTreeSet::new();
    for path in paths {
        let value = path.as_str();
        let mut depth = 0;
        for (index, _) in value.match_indices('/') {
            depth += 1;
            if depth > MAX_DEPTH {
                return Err(limit("workspace nesting limit exceeded"));
            }
            entries.insert(value[..index].to_owned());
        }
        entries.insert(value.to_owned());
        if entries.len() > MAX_ENTRIES {
            return Err(limit("workspace entry limit exceeded"));
        }
    }
    Ok(())
}

fn protected(part: &str) -> bool {
    let p = part.to_ascii_lowercase();
    matches!(
        p.as_str(),
        ".git"
            | ".oxa"
            | ".oxa-review.json"
            | ".ssh"
            | ".gnupg"
            | ".config"
            | ".local"
            | ".netrc"
            | ".npmrc"
            | ".pypirc"
            | "credentials"
            | "credentials.json"
            | "registration.json"
            | ".registration.json.pending"
            | "secrets.json"
            | "id_rsa"
            | "id_ed25519"
    ) || p == ".env"
        || p.starts_with(".env.")
        || [".pem", ".key", ".p12", ".pfx"].iter().any(|s| p.ends_with(s))
}

#[derive(Clone)]
pub struct Snapshot {
    files: BTreeMap<RelativePath, String>,
    manifest: String,
}
impl fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Snapshot")
            .field("files", &self.files.len())
            .field("manifest_sha256", &self.manifest)
            .finish()
    }
}
impl Snapshot {
    /// The caller approves the selected contents for delegation. Filename
    /// filtering is defense in depth, not proof that arbitrary text is secret-free.
    pub fn approve(root: &Path, paths: Vec<RelativePath>) -> io::Result<Self> {
        if paths.is_empty() || paths.len() > MAX_FILES {
            return Err(denied("workspace needs 1..32 approved files"));
        }
        validate_structure(paths.iter())?;
        let root = root.canonicalize()?;
        let directory = open_directory(&root)?;
        let mut files = BTreeMap::new();
        let mut total = 0usize;
        for path in paths {
            if files.contains_key(&path) {
                return Err(denied("duplicate workspace input"));
            }
            let text = read_relative(&directory, &path, MAX_FILE)?;
            total = total
                .checked_add(text.len())
                .ok_or_else(|| limit("workspace input limit exceeded"))?;
            if total > MAX_TOTAL {
                return Err(limit("workspace input limit exceeded"));
            }
            files.insert(path, text);
        }
        let manifest = digest(&serde_json::to_vec(
            &files
                .iter()
                .map(|(p, t)| (p.as_str(), digest(t.as_bytes())))
                .collect::<Vec<_>>(),
        )?);
        Ok(Self { files, manifest })
    }
    /// Stable digest of sorted path/content-digest pairs, independent of input order.
    pub fn manifest_sha256(&self) -> &str {
        &self.manifest
    }
    pub fn paths(&self) -> impl Iterator<Item = &RelativePath> {
        self.files.keys()
    }
    /// Returns only text explicitly selected and approved by the caller.
    pub fn text(&self, path: &RelativePath) -> Option<&str> {
        self.files.get(path).map(String::as_str)
    }
}
fn open_directory(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}
fn child(dir: &File, name: &std::ffi::OsStr, flags: i32) -> io::Result<File> {
    let name = CString::new(name.as_bytes()).map_err(|_| denied("invalid path component"))?;
    // SAFETY: owned live directory FD, valid C name, and no creation flags.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat returned a new owned descriptor.
    Ok(unsafe { File::from_raw_fd(fd) })
}
fn read_relative(root: &File, path: &RelativePath, max: usize) -> io::Result<String> {
    let mut dir = root.try_clone()?;
    let parts: Vec<_> = Path::new(path.as_str()).components().collect();
    for part in &parts[..parts.len() - 1] {
        dir = child(&dir, part.as_os_str(), libc::O_PATH | libc::O_DIRECTORY)?;
    }
    let handle = child(&dir, parts.last().unwrap().as_os_str(), libc::O_PATH)?;
    let before = handle.metadata()?;
    if !before.is_file() || before.nlink() != 1 || before.dev() != root.metadata()?.dev() {
        return Err(denied(
            "workspace files must be plain single-link files on the source filesystem",
        ));
    }
    if before.len() > max as u64 {
        return Err(limit("workspace file limit exceeded"));
    }
    // Reopen the already-validated inode, not an attacker-replaceable path.
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(format!("/proc/self/fd/{}", handle.as_raw_fd()))?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(max as u64 + 1)
        .read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    if bytes.len() > max {
        return Err(limit("workspace file limit exceeded"));
    }
    if (
        before.dev(),
        before.ino(),
        before.len(),
        before.mtime(),
        before.mtime_nsec(),
        before.ctime(),
        before.ctime_nsec(),
        before.nlink(),
    ) != (
        after.dev(),
        after.ino(),
        after.len(),
        after.mtime(),
        after.mtime_nsec(),
        after.ctime(),
        after.ctime_nsec(),
        after.nlink(),
    ) {
        return Err(denied("workspace input changed during capture"));
    }
    String::from_utf8(bytes).map_err(|_| denied("workspace files must be UTF-8 text"))
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn denied(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}
fn limit(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::FileTooLarge, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxa_fake::TestEnv;
    use std::fs;
    use std::os::unix::fs::symlink;

    fn path(s: &str) -> RelativePath {
        RelativePath::new(s.into()).unwrap()
    }
    fn capture(root: &Path, names: &[&str]) -> io::Result<Snapshot> {
        Snapshot::approve(root, names.iter().map(|s| path(s)).collect())
    }

    #[test]
    fn immutable_selection_has_deterministic_manifest_and_redacted_debug() {
        let env = TestEnv::new();
        fs::create_dir(env.root().join("nested")).unwrap();
        fs::write(env.root().join("nested/a"), "selected synthetic text").unwrap();
        fs::write(env.root().join("b"), "other selected text").unwrap();
        fs::write(env.root().join("unselected"), "excluded").unwrap();
        let first = capture(env.root(), &["nested/a", "b"]).unwrap();
        let second = capture(env.root(), &["b", "nested/a"]).unwrap();
        assert_eq!(first.manifest_sha256(), second.manifest_sha256());
        assert_eq!(first.manifest_sha256().len(), 64);
        assert_eq!(
            first.paths().map(RelativePath::as_str).collect::<Vec<_>>(),
            ["b", "nested/a"]
        );
        assert_eq!(first.text(&path("unselected")), None);
        let debug = format!("{first:?}");
        assert!(!debug.contains("selected synthetic text"));
        assert!(!debug.contains("nested/a"));
        fs::write(env.root().join("nested/a"), "changed on disk").unwrap();
        assert_eq!(first.text(&path("nested/a")), Some("selected synthetic text"));
        assert_ne!(
            first.manifest_sha256(),
            capture(env.root(), &["nested/a", "b"]).unwrap().manifest_sha256()
        );
        fs::rename(env.root().join("b"), env.root().join("renamed")).unwrap();
        assert_ne!(
            second.manifest_sha256(),
            capture(env.root(), &["nested/a", "renamed"])
                .unwrap()
                .manifest_sha256()
        );
    }

    #[test]
    fn path_validation_applies_to_construction_and_deserialization() {
        for bad in [
            "",
            "../x",
            "/x",
            "a//b",
            "a/./b",
            "a/../b",
            "a\\b",
            "x\nname",
            "a/\0b",
            ".env",
            "x/.ENV.prod",
            "x/.git/config",
            "x/.oxa/config",
            "REGISTRATION.JSON",
            "credentials",
            "x.PEM",
            "key.pfx",
        ] {
            assert!(RelativePath::new(bad.into()).is_err(), "{bad:?}");
            assert!(
                serde_json::from_value::<RelativePath>(serde_json::json!(bad)).is_err(),
                "{bad:?}"
            );
        }
        assert!(RelativePath::new("x".repeat(257)).is_err());
        for good in ["src/main.rs", "unicode/λ.txt", "space name", "x"] {
            let p = path(good);
            assert_eq!(
                serde_json::from_str::<RelativePath>(&serde_json::to_string(&p).unwrap()).unwrap(),
                p
            );
        }
    }

    #[test]
    fn refuses_links_nontext_and_special_files_without_waiting_for_a_writer() {
        let env = TestEnv::new();
        fs::write(env.root().join("plain"), "text").unwrap();
        symlink("plain", env.root().join("symlink")).unwrap();
        fs::hard_link(env.root().join("plain"), env.root().join("hard")).unwrap();
        fs::write(env.root().join("binary"), [255]).unwrap();
        fs::create_dir(env.root().join("directory")).unwrap();
        fs::write(env.root().join("directory/file"), "text").unwrap();
        symlink("directory", env.root().join("ancestor")).unwrap();
        let fifo = CString::new(env.root().join("fifo").as_os_str().as_bytes()).unwrap();
        // SAFETY: valid NUL-terminated test-owned path; creates no process.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        for bad in [
            "plain",
            "symlink",
            "hard",
            "binary",
            "directory",
            "ancestor/file",
            "fifo",
            "missing",
        ] {
            assert!(capture(env.root(), &[bad]).is_err(), "{bad}");
        }
        assert_eq!(fs::read(env.root().join("plain")).unwrap(), b"text");
    }

    #[test]
    fn file_count_duplicates_and_exact_byte_limits() {
        let env = TestEnv::new();
        assert!(capture(env.root(), &[]).is_err());
        fs::write(env.root().join("one"), "").unwrap();
        assert!(capture(env.root(), &["one", "one"]).is_err());
        assert_eq!(
            capture(env.root(), &["one"]).unwrap().text(&path("one")),
            Some("")
        );
        let mut paths = Vec::new();
        for i in 0..33 {
            let name = format!("f{i}");
            fs::write(env.root().join(&name), "").unwrap();
            paths.push(path(&name));
        }
        assert!(Snapshot::approve(env.root(), paths[..32].to_vec()).is_ok());
        assert!(Snapshot::approve(env.root(), paths).is_err());
        let full = "x".repeat(MAX_FILE);
        for name in ["a", "b", "c", "d"] {
            fs::write(env.root().join(name), &full).unwrap();
        }
        assert!(capture(env.root(), &["a", "b", "c", "d"]).is_ok());
        fs::write(env.root().join("one"), "x").unwrap();
        assert_eq!(
            capture(env.root(), &["a", "b", "c", "d", "one"])
                .unwrap_err()
                .kind(),
            io::ErrorKind::FileTooLarge
        );
        fs::write(env.root().join("a"), format!("{full}x")).unwrap();
        assert_eq!(
            capture(env.root(), &["a"]).unwrap_err().kind(),
            io::ErrorKind::FileTooLarge
        );
    }

    #[test]
    fn virtual_tree_bounds_ignore_unselected_source_entries() {
        let env = TestEnv::new();
        for i in 0..140 {
            fs::write(env.root().join(format!("unselected{i}")), "").unwrap();
        }
        let dirs = (0..MAX_DEPTH)
            .map(|i| format!("d{i}"))
            .collect::<Vec<_>>()
            .join("/");
        fs::create_dir_all(env.root().join(&dirs)).unwrap();
        let valid = format!("{dirs}/file");
        fs::write(env.root().join(&valid), "boundary").unwrap();
        assert!(capture(env.root(), &[&valid]).is_ok());
        let invalid = format!("{dirs}/extra/file");
        fs::create_dir_all(env.root().join(&dirs).join("extra")).unwrap();
        fs::write(env.root().join(&invalid), "too deep").unwrap();
        assert_eq!(
            capture(env.root(), &[&invalid]).unwrap_err().kind(),
            io::ErrorKind::FileTooLarge
        );
        let mut paths = Vec::new();
        for i in 0..32 {
            let name = format!("e{i}/p/q/file");
            fs::create_dir_all(env.root().join(format!("e{i}/p/q"))).unwrap();
            fs::write(env.root().join(&name), "").unwrap();
            paths.push(path(&name));
        }
        assert!(Snapshot::approve(env.root(), paths.clone()).is_ok());
        fs::create_dir_all(env.root().join("e0/p/q/extra")).unwrap();
        fs::write(env.root().join("e0/p/q/extra/file"), "").unwrap();
        paths[0] = path("e0/p/q/extra/file");
        assert_eq!(
            Snapshot::approve(env.root(), paths).unwrap_err().kind(),
            io::ErrorKind::FileTooLarge
        );
    }
}
