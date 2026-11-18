//! Private files under the caller's protected repository root, outside writable Git binds.
use crate::Result;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

fn io<T>(result: std::io::Result<T>, what: &str) -> Result<T> {
    result.map_err(|error| format!("{what}: {:?}", error.kind()))
}
fn private(metadata: &fs::Metadata, directory: bool) -> Result<()> {
    private_for_owner(metadata, directory, effective_uid())
}

pub(crate) fn effective_uid() -> u32 {
    // SAFETY: geteuid has no arguments, pointers or failure mode.
    unsafe { libc::geteuid() }
}

pub(crate) fn private_for_owner(metadata: &fs::Metadata, directory: bool, uid: u32) -> Result<()> {
    if metadata.uid() != uid
        || metadata.mode() & 0o077 != 0
        || if directory {
            !metadata.is_dir()
        } else {
            !metadata.is_file()
        }
    {
        return Err("facts storage must be private and owned by this user".into());
    }
    Ok(())
}

/// Require an existing owner-only directory; a symlink is not a directory proof.
pub fn require_private_directory(path: &Path) -> Result<()> {
    let metadata = io(fs::symlink_metadata(path), "cannot inspect private directory")?;
    private(&metadata, true)
}

#[derive(Clone, Debug)]
pub(crate) struct Root(PathBuf);
impl Root {
    pub(crate) fn open(path: &Path, create: bool) -> Result<Self> {
        if !path.is_absolute() {
            return Err("facts root must be absolute".into());
        }
        let mut parent = PathBuf::new();
        for component in path.components() {
            if matches!(component, Component::ParentDir) {
                return Err("facts root must be canonical".into());
            }
            parent.push(component);
            match fs::symlink_metadata(&parent) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err("facts root contains a symlink".into())
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound && parent == path && create => {
                    io(
                        fs::DirBuilder::new().mode(0o700).create(path),
                        "cannot create facts root",
                    )?;
                }
                Err(error) => return Err(format!("cannot inspect facts root: {:?}", error.kind())),
            }
        }
        let metadata = io(fs::symlink_metadata(path), "cannot inspect facts root")?;
        private(&metadata, true)?;
        if create {
            // A synced child does not persist its entry in the containing directory.
            // Also sync on an initialization retry after an earlier mkdir/sync failure.
            sync_directory(path.parent().ok_or("facts root has no parent directory")?)?;
        }
        Ok(Self(path.to_path_buf()))
    }
    fn path(&self, leaf: &str) -> PathBuf {
        debug_assert!(!leaf.contains('/') && leaf != "." && leaf != "..");
        self.0.join(leaf)
    }
    fn options(&self) -> OpenOptions {
        let mut options = OpenOptions::new();
        options
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .mode(0o600);
        options
    }
    pub(crate) fn exists(&self, leaf: &str) -> Result<bool> {
        match fs::symlink_metadata(self.path(leaf)) {
            Ok(metadata) => {
                private(&metadata, false)?;
                Ok(true)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(format!("cannot inspect facts file: {:?}", error.kind())),
        }
    }
    pub(crate) fn lock(&self, leaf: &str) -> Result<File> {
        let file = io(
            self.options()
                .read(true)
                .write(true)
                .create(true)
                .open(self.path(leaf)),
            "cannot open facts lock",
        )?;
        private(&io(file.metadata(), "cannot inspect facts lock")?, false)?;
        io(file.lock(), "cannot lock facts storage")?;
        Ok(file)
    }
    pub(crate) fn read(&self, leaf: &str, limit: usize) -> Result<Vec<u8>> {
        let file = io(
            self.options().read(true).open(self.path(leaf)),
            "cannot open facts file",
        )?;
        let metadata = io(file.metadata(), "cannot inspect facts file")?;
        private(&metadata, false)?;
        if metadata.len() > limit as u64 {
            return Err("facts file capacity exceeded".into());
        }
        let mut bytes = Vec::new();
        io(
            file.take(limit as u64 + 1).read_to_end(&mut bytes),
            "cannot read facts file",
        )?;
        if bytes.len() > limit {
            return Err("facts file capacity exceeded".into());
        }
        Ok(bytes)
    }
    pub(crate) fn create(&self, leaf: &str, bytes: &[u8]) -> Result<()> {
        let mut file = io(
            self.options().write(true).create_new(true).open(self.path(leaf)),
            "cannot create facts file",
        )?;
        io(
            file.write_all(bytes).and_then(|()| file.sync_all()),
            "cannot save facts file",
        )?;
        self.sync()
    }
    pub(crate) fn atomic(&self, leaf: &str, bytes: &[u8]) -> Result<()> {
        // Inspect the existing leaf without following it; never replace poisoned enforcement data.
        self.exists(leaf)?;
        static NEXT: AtomicU64 = AtomicU64::new(0);
        for _ in 0..16 {
            let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
            let temporary = format!(".{leaf}.tmp-{}-{sequence}", std::process::id());
            let mut file = match self
                .options()
                .write(true)
                .create_new(true)
                .open(self.path(&temporary))
            {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(format!("cannot create facts temporary: {:?}", error.kind())),
            };
            let result = io(
                file.write_all(bytes).and_then(|()| file.sync_all()),
                "cannot save facts temporary",
            )
            .and_then(|()| {
                io(
                    fs::rename(self.path(&temporary), self.path(leaf)),
                    "cannot replace facts file",
                )
            })
            .and_then(|()| self.sync());
            if result.is_err() {
                let _ = fs::remove_file(self.path(&temporary));
            }
            return result;
        }
        Err("cannot reserve facts temporary".into())
    }
    pub(crate) fn append(&self, leaf: &str, bytes: &[u8]) -> Result<()> {
        let mut file = io(
            self.options().append(true).open(self.path(leaf)),
            "cannot open facts journal",
        )?;
        private(&io(file.metadata(), "cannot inspect facts journal")?, false)?;
        io(
            file.write_all(bytes).and_then(|()| file.sync_all()),
            "cannot append facts journal",
        )
    }
    pub(crate) fn truncate(&self, leaf: &str, length: u64) -> Result<()> {
        let file = io(
            self.options().write(true).open(self.path(leaf)),
            "cannot open torn facts journal",
        )?;
        private(&io(file.metadata(), "cannot inspect torn facts journal")?, false)?;
        io(
            file.set_len(length).and_then(|()| file.sync_all()),
            "cannot recover torn facts journal",
        )
    }
    pub(crate) fn rotate(&self, from: &str, to: &str) -> Result<()> {
        if !self.exists(from)? {
            return Err("facts journal is missing".into());
        }
        self.exists(to)?;
        io(
            fs::rename(self.path(from), self.path(to)),
            "cannot rotate facts journal",
        )?;
        self.sync()?;
        self.create(from, b"")
    }
    pub(crate) fn durable(&self, leaf: &str) -> Result<()> {
        let file = io(
            self.options().read(true).write(true).open(self.path(leaf)),
            "cannot open protected metadata",
        )?;
        private(&io(file.metadata(), "cannot inspect protected metadata")?, false)?;
        io(file.sync_all(), "cannot sync protected metadata")?;
        self.sync()
    }
    fn sync(&self) -> Result<()> {
        sync_directory(&self.0)
    }
}

fn sync_directory(path: &Path) -> Result<()> {
    let directory = io(
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(path),
        "cannot open facts directory",
    )?;
    io(directory.sync_all(), "cannot sync facts directory")
}
