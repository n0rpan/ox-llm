//! Bounded, private diagnostic records. Log failures never fail inference.
use nix::{libc, unistd::Uid};
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
};
fn unsafe_path() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, "unsafe diagnostic path")
}
fn unsafe_file() -> io::Error {
    io::Error::other("unsafe diagnostic file type")
}
pub fn error_name(error: &io::Error) -> &'static str {
    match error.kind() {
        io::ErrorKind::PermissionDenied => "PermissionError",
        io::ErrorKind::NotFound => "FileNotFoundError",
        io::ErrorKind::AlreadyExists => "FileExistsError",
        io::ErrorKind::NotADirectory => "NotADirectoryError",
        io::ErrorKind::IsADirectory => "IsADirectoryError",
        _ => "OSError",
    }
}
fn prepare_parent(path: &Path) -> io::Result<()> {
    if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(unsafe_path());
    }
    let parent = path
        .parent()
        .filter(|p| p.parent().is_some())
        .ok_or_else(unsafe_path)?;
    let mut current = PathBuf::from("/");
    for part in parent.components().filter_map(|c| {
        if let Component::Normal(p) = c {
            Some(p)
        } else {
            None
        }
    }) {
        current.push(part);
        let info = match fs::symlink_metadata(&current) {
            Ok(info) => info,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::DirBuilder::new().mode(0o700).create(&current)?;
                fs::symlink_metadata(&current)?
            }
            Err(error) => return Err(error),
        };
        if !info.is_dir() || info.file_type().is_symlink() {
            return Err(unsafe_file());
        }
        let uid = Uid::effective().as_raw();
        if current == parent {
            if info.uid() != uid {
                return Err(unsafe_path());
            }
            fs::set_permissions(&current, fs::Permissions::from_mode(0o700))?;
        } else if ![0, uid].contains(&info.uid())
            || (info.mode() & 0o022 != 0 && !(info.uid() == 0 && info.mode() & libc::S_ISVTX != 0))
        {
            return Err(unsafe_path());
        }
    }
    Ok(())
}
fn regular_owned(path: &Path) -> io::Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(info) if !info.is_file() || info.file_type().is_symlink() => Err(unsafe_file()),
        Ok(info)
            if info.is_file()
                && !info.file_type().is_symlink()
                && info.uid() == Uid::effective().as_raw()
                && info.nlink() == 1 =>
        {
            Ok(Some(info))
        }
        Ok(_) => Err(unsafe_path()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}
fn truncate_utf8(text: &mut String, limit: usize) {
    if text.len() > limit {
        let mut boundary = limit;
        while !text.is_char_boundary(boundary) {
            boundary -= 1;
        }
        text.truncate(boundary);
    }
}
fn safe_text(bytes: &[u8]) -> Vec<u8> {
    let truncated = bytes.len() > 15 * 1024;
    let text = String::from_utf8_lossy(&bytes[..bytes.len().min(15 * 1024)]);
    let mut out = String::new();
    for ch in text.chars() {
        let code = ch as u32;
        if ch != '\n' && ch != '\t' && (code < 32 || code == 127 || (0x80..0xa0).contains(&code)) {
            out.push_str(&format!("\\x{code:02x}"));
        } else {
            out.push(ch);
        }
    }
    if out.len() > 64 * 1024 {
        truncate_utf8(&mut out, 64 * 1024);
        out.push_str("...[write truncated]\n");
    }
    if truncated {
        out.push_str("...[input truncated]\n");
    }
    truncate_utf8(&mut out, 1024 * 1024);
    out.into_bytes()
}
pub struct Writer {
    path: PathBuf,
    error: Option<&'static str>,
    line: Vec<u8>,
}
impl Writer {
    pub fn new(path: PathBuf) -> Self {
        let error = prepare_parent(&path).err().as_ref().map(error_name);
        Self {
            path,
            error,
            line: Vec::new(),
        }
    }
    pub fn error(&self) -> Option<&'static str> {
        self.error
    }
    pub fn write(&mut self, bytes: &[u8]) {
        if self.error.is_none() {
            self.error = self
                .write_inner(&safe_text(bytes))
                .err()
                .as_ref()
                .map(error_name);
        }
    }
    fn backup(&self, index: usize) -> PathBuf {
        self.path.with_file_name(format!(
            "{}.{index}",
            self.path.file_name().unwrap_or_default().to_string_lossy()
        ))
    }
    fn rotate(&self) -> io::Result<()> {
        regular_owned(&self.path)?;
        let second = self.backup(2);
        let first = self.backup(1);
        regular_owned(&second)?;
        regular_owned(&first)?;
        if second.exists() {
            fs::remove_file(&second)?;
        }
        if first.exists() {
            fs::rename(&first, &second)?;
        }
        if regular_owned(&self.path)?.is_some() {
            fs::rename(&self.path, &first)?;
        }
        Ok(())
    }
    fn write_inner(&self, data: &[u8]) -> io::Result<()> {
        prepare_parent(&self.path)?;
        let info = regular_owned(&self.path)?;
        if let Some(info) = info.as_ref().filter(|info| info.mode() & 0o777 != 0o600) {
            let file = OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
                .open(&self.path)?;
            let opened = file.metadata()?;
            if !opened.is_file()
                || opened.uid() != Uid::effective().as_raw()
                || opened.nlink() != 1
                || opened.dev() != info.dev()
                || opened.ino() != info.ino()
            {
                return Err(unsafe_path());
            }
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        if info
            .as_ref()
            .is_some_and(|m| m.len() + data.len() as u64 > 1024 * 1024)
        {
            self.rotate()?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(&self.path)?;
        let info = file.metadata()?;
        if !info.is_file() || info.uid() != Uid::effective().as_raw() || info.nlink() != 1 {
            return Err(unsafe_path());
        }
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(data)?;
        Ok(())
    }
    fn emit(&mut self, truncated: bool) {
        let mut line = std::mem::take(&mut self.line);
        if truncated {
            line.extend(b"...[line truncated]");
        }
        line.push(b'\n');
        self.write(&line);
    }
    pub fn feed(&mut self, bytes: &[u8]) {
        for byte in bytes {
            if *byte == b'\n' {
                self.emit(false);
            } else {
                self.line.push(*byte);
                if self.line.len() == 8192 {
                    self.emit(true);
                }
            }
        }
    }
    pub fn finish(&mut self) {
        if !self.line.is_empty() {
            self.emit(false);
        }
    }
}
