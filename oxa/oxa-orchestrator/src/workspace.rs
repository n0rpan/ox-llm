//! Explicitly approved text snapshots and private worker copies. A workspace
//! grant never exposes a source checkout, git metadata or ambient credentials.
//! Returned changes are review data; this module has no apply/merge operation.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::fmt;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::{
    ffi::OsStrExt,
    fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const MAX_FILES: usize = 32;
const MAX_FILE: usize = 256 * 1024;
const MAX_TOTAL: usize = 1024 * 1024;
const MAX_REVIEW: usize = 128 * 1024;
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
    pub(crate) fn with_changes(&self, changes: &[FileChange]) -> io::Result<Self> {
        let mut files = self.files.clone();
        for change in changes {
            match &change.after {
                Some(text) => {
                    files.insert(change.path.clone(), text.clone());
                }
                None => {
                    files.remove(&change.path);
                }
            }
        }
        if files.len() > MAX_FILES
            || files.values().any(|t| t.len() > MAX_FILE)
            || files.values().map(String::len).sum::<usize>() > MAX_TOTAL
        {
            return Err(limit("candidate workspace limit exceeded"));
        }
        let manifest = digest(&serde_json::to_vec(
            &files
                .iter()
                .map(|(p, t)| (p.as_str(), digest(t.as_bytes())))
                .collect::<Vec<_>>(),
        )?);
        Ok(Self { files, manifest })
    }
    pub(crate) fn write_new(&self, directory: &Path) -> io::Result<()> {
        DirBuilder::new().mode(0o700).create(directory)?;
        self.write_contents(directory)
    }
    pub(crate) fn write_contents(&self, directory: &Path) -> io::Result<()> {
        for (path, text) in &self.files {
            let target = directory.join(path.as_str());
            if let Some(parent) = target.parent() {
                private_parents(directory, parent)?;
            }
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(target)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
        }
        Ok(())
    }
    pub(crate) fn entries(&self) -> impl Iterator<Item = (&RelativePath, &String)> {
        self.files.iter()
    }
    pub fn manifest_sha256(&self) -> &str {
        &self.manifest
    }
    pub fn paths(&self) -> impl Iterator<Item = &RelativePath> {
        self.files.keys()
    }
}

#[derive(Clone, Debug)]
pub struct Grant {
    snapshot: Arc<Snapshot>,
    editing: bool,
    new_files: BTreeSet<RelativePath>,
    git_worktrees: bool,
}
impl Grant {
    pub fn read_only(snapshot: Arc<Snapshot>) -> Self {
        Self {
            snapshot,
            editing: false,
            new_files: BTreeSet::new(),
            git_worktrees: false,
        }
    }
    /// Edits affect the entire private copy, never the source checkout. New
    /// paths are eligible for review only when explicitly registered here.
    pub fn editing(snapshot: Arc<Snapshot>, new_files: Vec<RelativePath>) -> io::Result<Self> {
        if new_files.len() > MAX_FILES || new_files.iter().any(|p| snapshot.files.contains_key(p)) {
            return Err(denied("invalid new-file review scope"));
        }
        let new_files: BTreeSet<_> = new_files.into_iter().collect();
        if snapshot.files.len() + new_files.len() > MAX_FILES {
            return Err(limit("workspace path limit exceeded"));
        }
        for path in snapshot.paths().chain(new_files.iter()) {
            if snapshot
                .paths()
                .chain(new_files.iter())
                .any(|other| path != other && other.as_str().starts_with(&format!("{}/", path.as_str())))
            {
                return Err(denied("workspace file/parent path collision"));
            }
        }
        validate_structure(snapshot.paths().chain(new_files.iter()))?;
        Ok(Self {
            snapshot,
            editing: true,
            new_files,
            git_worktrees: false,
        })
    }
    /// Use controller-owned Git worktrees seeded only from this approved snapshot.
    /// No source Git repository is read or mounted. Editing remains explicit.
    pub fn with_git_worktrees(mut self) -> io::Result<Self> {
        if !self.editing {
            return Err(denied("Git worktrees require an editing grant"));
        }
        self.git_worktrees = true;
        Ok(self)
    }
    pub fn uses_git_worktrees(&self) -> bool {
        self.git_worktrees
    }
    pub(crate) fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }
    pub(crate) fn validate_backend(&self) -> io::Result<()> {
        if self.git_worktrees {
            crate::process::checked_program(Path::new("/usr/bin/git"))?;
        }
        Ok(())
    }
    pub fn is_editing(&self) -> bool {
        self.editing
    }
    pub(crate) fn tools(&self) -> &'static [&'static str] {
        if self.editing {
            &["read", "edit", "write"]
        } else {
            &["read"]
        }
    }
    pub(crate) fn role_context(&self) -> String {
        format!("\nApproved workspace files under /work:\n{}\nEligible new files:\n{}\nReturn proposed changes for review; never apply them to the source checkout.",self.snapshot.paths().map(RelativePath::as_str).collect::<Vec<_>>().join("\n"),self.new_files.iter().map(RelativePath::as_str).collect::<Vec<_>>().join("\n"))
    }
    pub(crate) fn materialize(self: &Arc<Self>, directory: &Path) -> io::Result<WorkingCopy> {
        if self.git_worktrees {
            return Err(denied("Git worktree requires an owned controller pool"));
        }
        self.snapshot.write_new(directory)?;
        Ok(WorkingCopy {
            directory: directory.to_path_buf(),
            grant: self.clone(),
            git: None,
        })
    }
}

pub(crate) struct WorkingCopy {
    directory: PathBuf,
    grant: Arc<Grant>,
    git: Option<crate::git_worktrees::Worktree>,
}
impl WorkingCopy {
    pub(crate) fn from_git(
        directory: PathBuf,
        grant: Arc<Grant>,
        git: crate::git_worktrees::Worktree,
    ) -> Self {
        Self {
            directory,
            grant,
            git: Some(git),
        }
    }
    pub(crate) fn git_mask(&self) -> Option<PathBuf> {
        self.git.as_ref().map(|git| git.mask())
    }
    pub(crate) fn base_commit(&self) -> Option<&str> {
        self.git.as_ref().map(|git| git.commit())
    }
    pub(crate) fn path(&self) -> &Path {
        &self.directory
    }
    pub(crate) fn editing(&self) -> bool {
        self.grant.editing
    }
    pub(crate) fn tools(&self) -> &'static [&'static str] {
        self.grant.tools()
    }
    pub(crate) fn manifest(&self) -> &str {
        self.grant.snapshot.manifest_sha256()
    }
    pub(crate) fn role_context(&self) -> String {
        self.grant.role_context()
    }
    /// Call only after the worker namespace has been stopped and reaped.
    pub(crate) fn collect(&self, worker: &str) -> io::Result<ReviewBundle> {
        if let Some(git) = &self.git {
            git.verify(&self.directory)?;
        }
        let root = open_directory(&self.directory)?;
        let mut found = BTreeSet::new();
        let mut count = 0usize;
        scan(
            &self.directory,
            &self.directory,
            &mut found,
            &mut count,
            0,
            self.git.is_some(),
        )?;
        for path in &found {
            if !self.grant.snapshot.files.contains_key(path) && !self.grant.new_files.contains(path) {
                return Err(denied("worker created an unapproved path"));
            }
        }
        let mut changes = Vec::new();
        let mut total = 0usize;
        for path in self
            .grant
            .snapshot
            .files
            .keys()
            .chain(self.grant.new_files.iter())
        {
            let before = self.grant.snapshot.files.get(path).cloned();
            let after = if found.contains(path) {
                Some(read_relative(&root, path, MAX_FILE)?)
            } else {
                None
            };
            total = total
                .checked_add(after.as_ref().map_or(0, String::len))
                .ok_or_else(|| limit("workspace output limit exceeded"))?;
            if total > MAX_TOTAL {
                return Err(limit("workspace output limit exceeded"));
            }
            if before == after {
                continue;
            }
            if !self.grant.editing {
                return Err(denied("read-only workspace changed"));
            }
            let old = before.as_deref().unwrap_or("");
            let new = after.as_deref().unwrap_or("");
            let diff = review_diff(old, new, path);
            changes.push(FileChange {
                path: path.clone(),
                before_sha256: before.as_ref().map(|s| digest(s.as_bytes())),
                after_sha256: after.as_ref().map(|s| digest(s.as_bytes())),
                before,
                after,
                diff,
            });
        }
        let bundle = ReviewBundle {
            version: 1,
            worker: worker.into(),
            manifest_sha256: self.grant.snapshot.manifest.clone(),
            changes,
        };
        bounded_json(&bundle, MAX_REVIEW)?;
        Ok(bundle)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileChange {
    pub path: RelativePath,
    pub before_sha256: Option<String>,
    pub after_sha256: Option<String>,
    pub before: Option<String>,
    pub after: Option<String>,
    pub diff: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReviewBundle {
    pub version: u32,
    pub worker: String,
    pub manifest_sha256: String,
    pub changes: Vec<FileChange>,
}
impl ReviewBundle {
    /// Revalidate review data before a caller presents it as applicable. No
    /// filesystem changes or tool execution happen here.
    pub fn verify(&self, snapshot: &Snapshot, new_files: &[RelativePath]) -> io::Result<()> {
        bounded_json(self, MAX_REVIEW)?;
        if self.version != 1 || self.manifest_sha256 != snapshot.manifest || self.changes.len() > MAX_FILES {
            return Err(denied("review manifest mismatch"));
        }
        let mut seen = BTreeSet::new();
        for change in &self.changes {
            if !seen.insert(&change.path) {
                return Err(denied("duplicate review path"));
            }
            let original = snapshot.files.get(&change.path);
            if original != change.before.as_ref()
                || (original.is_none() && !new_files.contains(&change.path))
                || change.before_sha256 != change.before.as_ref().map(|s| digest(s.as_bytes()))
                || change.after_sha256 != change.after.as_ref().map(|s| digest(s.as_bytes()))
                || change.before == change.after
            {
                return Err(denied("invalid review content or scope"));
            }
            let expected = review_diff(
                change.before.as_deref().unwrap_or(""),
                change.after.as_deref().unwrap_or(""),
                &change.path,
            );
            if change.diff != expected {
                return Err(denied("review diff does not match content"));
            }
        }
        Ok(())
    }
    pub fn overlaps(&self, other: &Self) -> Vec<RelativePath> {
        self.changes
            .iter()
            .filter(|a| other.changes.iter().any(|b| a.path == b.path))
            .map(|c| c.path.clone())
            .collect()
    }
    pub fn verify_current_base(&self, root: &Path) -> io::Result<()> {
        let root = open_directory(&root.canonicalize()?)?;
        for change in &self.changes {
            match read_relative(&root, &change.path, MAX_FILE) {
                Ok(text) if change.before_sha256 == Some(digest(text.as_bytes())) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound && change.before_sha256.is_none() => {}
                _ => {
                    return Err(denied(
                        "source changed since approval; review must be regenerated",
                    ))
                }
            }
        }
        Ok(())
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
fn private_parents(root: &Path, parent: &Path) -> io::Result<()> {
    let relative = parent
        .strip_prefix(root)
        .map_err(|_| denied("workspace path escaped root"))?;
    let mut cursor = root.to_path_buf();
    for part in relative.components() {
        cursor.push(part);
        match DirBuilder::new().mode(0o700).create(&cursor) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && cursor.is_dir() => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
fn scan(
    root: &Path,
    dir: &Path,
    found: &mut BTreeSet<RelativePath>,
    count: &mut usize,
    depth: usize,
    skip_git: bool,
) -> io::Result<()> {
    if depth > MAX_DEPTH {
        return Err(limit("workspace nesting limit exceeded"));
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if skip_git && depth == 0 && entry.file_name() == ".git" {
            continue;
        }
        *count += 1;
        if *count > MAX_ENTRIES {
            return Err(limit("workspace entry limit exceeded"));
        }
        let ty = entry.file_type()?;
        let relative = entry
            .path()
            .strip_prefix(root)
            .map_err(|_| denied("workspace path escaped root"))?
            .to_str()
            .ok_or_else(|| denied("workspace paths must be UTF-8"))?
            .to_owned();
        let path = RelativePath::new(relative)?;
        if ty.is_dir() {
            scan(root, &entry.path(), found, count, depth + 1, skip_git)?;
        } else if ty.is_file() {
            if entry.metadata()?.mode() & 0o111 != 0 {
                return Err(denied("executable workspace output is unsupported"));
            }
            found.insert(path);
            if found.len() > MAX_FILES {
                return Err(limit("workspace file limit exceeded"));
            }
        } else {
            return Err(denied("symlinks and special workspace files are refused"));
        }
    }
    Ok(())
}
// One deterministic hunk around the changed span. Linear time, including for
// adversarial repeated lines; no quadratic diff search on worker-controlled text.
fn review_diff(old: &str, new: &str, path: &RelativePath) -> String {
    let a: Vec<_> = old.split_inclusive('\n').collect();
    let b: Vec<_> = new.split_inclusive('\n').collect();
    let prefix = a.iter().zip(&b).take_while(|(a, b)| a == b).count();
    let suffix = a[prefix..]
        .iter()
        .rev()
        .zip(b[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    if old == new {
        return String::new();
    }
    let start = prefix.saturating_sub(3);
    let tail = suffix.min(3);
    let a_end = a.len() - suffix + tail;
    let b_end = b.len() - suffix + tail;
    let range = |begin: usize, count: usize| {
        if count == 0 {
            format!("{begin},0")
        } else {
            format!("{},{}", begin + 1, count)
        }
    };
    let mut diff = format!(
        "--- {}\n+++ {}\n@@ -{} +{} @@\n",
        diff_path("a", path),
        diff_path("b", path),
        range(start, a_end - start),
        range(start, b_end - start)
    );
    let mut line = |mark: char, text: &str| {
        diff.push(mark);
        diff.push_str(text);
        if !text.ends_with('\n') {
            diff.push_str("\n\\ No newline at end of file\n");
        }
    };
    for text in &a[start..prefix] {
        line(' ', text);
    }
    for text in &a[prefix..a.len() - suffix] {
        line('-', text);
    }
    for text in &b[prefix..b.len() - suffix] {
        line('+', text);
    }
    for text in &a[a.len() - suffix..a_end] {
        line(' ', text);
    }
    diff
}

fn diff_path(prefix: &str, path: &RelativePath) -> String {
    let value = format!("{prefix}/{}", path.as_str());
    if value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"/_-.".contains(&b))
    {
        value
    } else {
        serde_json::to_string(&value).unwrap()
    }
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn bounded_json(value: &impl Serialize, max: usize) -> io::Result<()> {
    struct Count {
        size: usize,
        max: usize,
    }
    impl Write for Count {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > self.max.saturating_sub(self.size) {
                return Err(limit("workspace review limit exceeded"));
            }
            self.size += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Count { size: 0, max }, value).map_err(|_| limit("workspace review limit exceeded"))
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
    use std::os::unix::fs::symlink;

    fn path(value: &str) -> RelativePath {
        RelativePath::new(value.into()).unwrap()
    }
    fn source(env: &TestEnv) -> Arc<Snapshot> {
        fs::write(env.root().join("input.txt"), "before\n").unwrap();
        Arc::new(Snapshot::approve(env.root(), vec![path("input.txt")]).unwrap())
    }
    #[test]
    fn private_changes_are_verifiable_and_never_apply_to_source() {
        let env = TestEnv::new();
        let snapshot = source(&env);
        let grant = Arc::new(Grant::editing(snapshot.clone(), vec![path("nested/new.txt")]).unwrap());
        let work = grant.materialize(&env.root().join("private-copy")).unwrap();
        fs::write(work.path().join("input.txt"), "after\n").unwrap();
        fs::create_dir(work.path().join("nested")).unwrap();
        fs::write(work.path().join("nested/new.txt"), "new\n").unwrap();
        let bundle = work.collect("writer").unwrap();
        assert_eq!(bundle.changes.len(), 2);
        assert!(bundle.changes[0].diff.contains("-before\n+after\n"));
        bundle.verify(&snapshot, &[path("nested/new.txt")]).unwrap();
        bundle.verify_current_base(env.root()).unwrap();
        assert_eq!(
            fs::read_to_string(env.root().join("input.txt")).unwrap(),
            "before\n"
        );
        assert!(!env.root().join("nested/new.txt").exists());
        let mut forged = bundle.clone();
        forged.changes[0].after = Some("forged".into());
        assert!(forged.verify(&snapshot, &[path("nested/new.txt")]).is_err());
        assert_eq!(
            bundle.overlaps(&bundle),
            vec![path("input.txt"), path("nested/new.txt")]
        );
        fs::write(env.root().join("input.txt"), "concurrent change\n").unwrap();
        assert!(bundle.verify_current_base(env.root()).is_err());
    }
    #[test]
    fn inputs_reject_protected_paths_links_special_files_and_nontext() {
        let env = TestEnv::new();
        source(&env);
        for bad in [
            "",
            "../input.txt",
            "/input.txt",
            "a//b",
            "a/./b",
            "a\\b",
            ".env",
            "src/.git/config",
            "x.key",
            "x\nname",
        ] {
            assert!(RelativePath::new(bad.into()).is_err(), "{bad:?}");
        }
        symlink("input.txt", env.root().join("link")).unwrap();
        fs::hard_link(env.root().join("input.txt"), env.root().join("hard")).unwrap();
        fs::write(env.root().join("binary"), [255]).unwrap();
        let fifo = CString::new(env.root().join("fifo").as_os_str().as_bytes()).unwrap();
        // SAFETY: test-owned path and valid NUL-terminated CString.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        for bad in ["link", "hard", "fifo", "binary"] {
            assert!(Snapshot::approve(env.root(), vec![path(bad)]).is_err(), "{bad}");
        }
    }
    #[test]
    fn output_scope_read_only_and_size_limits_fail_closed() {
        let env = TestEnv::new();
        let snapshot = source(&env);
        let read = Arc::new(Grant::read_only(snapshot.clone()))
            .materialize(&env.root().join("read"))
            .unwrap();
        assert!(read.collect("reader").unwrap().changes.is_empty());
        fs::write(read.path().join("input.txt"), "changed").unwrap();
        assert_eq!(
            read.collect("reader").unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        let grant = Arc::new(Grant::editing(snapshot, vec![]).unwrap());
        let work = grant.materialize(&env.root().join("edit")).unwrap();
        fs::write(work.path().join("unapproved"), "x").unwrap();
        assert!(work.collect("writer").is_err());
        fs::remove_file(work.path().join("unapproved")).unwrap();
        fs::remove_file(work.path().join("input.txt")).unwrap();
        symlink(env.root().join("input.txt"), work.path().join("input.txt")).unwrap();
        assert!(work.collect("writer").is_err());
        fs::remove_file(work.path().join("input.txt")).unwrap();
        fs::write(work.path().join("input.txt"), "x".repeat(MAX_REVIEW)).unwrap();
        assert_eq!(
            work.collect("writer").unwrap_err().kind(),
            io::ErrorKind::FileTooLarge
        );
    }
    #[test]
    fn approved_layout_matches_collection_depth_and_entry_boundaries() {
        let env = TestEnv::new();
        let directory = (0..MAX_DEPTH)
            .map(|i| format!("d{i}"))
            .collect::<Vec<_>>()
            .join("/");
        fs::create_dir_all(env.root().join(&directory)).unwrap();
        let valid = format!("{directory}/file.txt");
        fs::write(env.root().join(&valid), "approved").unwrap();
        let snapshot = Arc::new(Snapshot::approve(env.root(), vec![path(&valid)]).unwrap());
        let copy = Arc::new(Grant::read_only(snapshot.clone()))
            .materialize(&env.root().join("depth-copy"))
            .unwrap();
        assert!(copy.collect("reader").unwrap().changes.is_empty());
        let invalid = format!("{directory}/extra/file.txt");
        fs::create_dir_all(env.root().join(&directory).join("extra")).unwrap();
        fs::write(env.root().join(&invalid), "approved").unwrap();
        assert_eq!(
            Snapshot::approve(env.root(), vec![path(&invalid)])
                .unwrap_err()
                .kind(),
            io::ErrorKind::FileTooLarge
        );
        assert_eq!(
            Grant::editing(snapshot, vec![path(&invalid)]).unwrap_err().kind(),
            io::ErrorKind::FileTooLarge
        );
        let mut paths = Vec::new();
        for i in 0..32 {
            let name = format!("e{i}/p/q/file.txt");
            fs::create_dir_all(env.root().join(format!("e{i}/p/q"))).unwrap();
            fs::write(env.root().join(&name), "approved").unwrap();
            paths.push(path(&name));
        }
        let snapshot = Arc::new(Snapshot::approve(env.root(), paths.clone()).unwrap());
        let copy = Arc::new(Grant::read_only(snapshot))
            .materialize(&env.root().join("entry-copy"))
            .unwrap();
        assert!(copy.collect("reader").unwrap().changes.is_empty());
        fs::create_dir_all(env.root().join("e0/p/q/extra")).unwrap();
        fs::write(env.root().join("e0/p/q/extra/file.txt"), "approved").unwrap();
        paths[0] = path("e0/p/q/extra/file.txt");
        let baseline = Arc::new(Snapshot::approve(env.root(), paths[1..].to_vec()).unwrap());
        assert_eq!(
            Grant::editing(baseline, vec![paths[0].clone()])
                .unwrap_err()
                .kind(),
            io::ErrorKind::FileTooLarge
        );
        assert_eq!(
            Snapshot::approve(env.root(), paths).unwrap_err().kind(),
            io::ErrorKind::FileTooLarge
        );
    }
}
