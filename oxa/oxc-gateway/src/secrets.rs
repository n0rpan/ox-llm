//! Request-time owner-only credentials. Errors and Debug never contain a path
//! or credential. Key values are never cached, logged or placed in the environment.
use nix::{
    fcntl::{openat, OFlag},
    libc,
    sys::stat::Mode,
    unistd::Uid,
};
use serde_json::Value;
use std::{
    fmt,
    fs::{self, File, OpenOptions},
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::Arc,
};

pub struct Key(String);
impl Key {
    /// Expose only when constructing the provider's request/header or redactor.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Cache-binding key for a sign-in that has no API key: derived from the account, so a
    /// catalog saved for one account is never served to another. It authorizes nothing.
    #[cfg(feature = "upstream-openai")]
    pub(crate) fn for_account(account_id: &str) -> Self {
        Self(format!("chatgpt-account:{account_id}"))
    }

    #[cfg(feature = "anthropic-gateway")]
    pub(crate) fn account_scope(&self) -> [u8; 32] {
        use sha2::{Digest, Sha256};
        Sha256::digest(self.0.as_bytes()).into()
    }
}
impl fmt::Debug for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Key([redacted])")
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Unavailable,
    InRepository,
    SymbolicLink,
    Unreadable,
    NotRegular,
    WrongOwner,
    Permissions,
    TooLarge,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reason = match self {
            Self::Unavailable => return f.write_str("Cloud credentials are not configured."),
            Self::InRepository => "the key file is inside a repository",
            Self::SymbolicLink => "the configured path is a symbolic link",
            Self::Unreadable => "the file cannot be read",
            Self::NotRegular => "the configured path is not a regular file",
            Self::WrongOwner => "the file is owned by another user",
            Self::Permissions => "the file is not mode 0600 (group/other permissions must be off)",
            Self::TooLarge => "the file is larger than 65536 bytes",
        };
        write!(f, "Cloud credentials are not configured: {reason}.")
    }
}
impl std::error::Error for Error {}
/// `path` is chosen by trusted configuration (defaulting belongs to the caller).
/// A path approved for one configuration generation. Prepare when configuration
/// changes; read fresh bytes and descriptor metadata for every request. Parent
/// links bind to the approved location, so retargeting them cannot select a key
/// inside a repository. No key material or opened key descriptor is retained.
#[derive(Clone)]
pub struct KeyFile {
    parent: Arc<File>,
    name: PathBuf,
}
impl fmt::Debug for KeyFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("KeyFile([configured])")
    }
}
impl KeyFile {
    pub fn prepare(path: &Path) -> Result<Self, Error> {
        if !path.is_absolute() {
            return Err(Error::Unavailable);
        }
        let parent = fs::canonicalize(path.parent().unwrap_or(path)).map_err(|_| Error::Unavailable)?;
        if parent.ancestors().any(|parent| {
            parent.join(".git").exists()
                || (parent.join("AGENTS.md").is_file() && parent.join("engine").is_dir())
        }) {
            return Err(Error::InRepository);
        }
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let access = libc::O_PATH;
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        let access = 0;
        let parent = OpenOptions::new()
            .read(true)
            .custom_flags(access | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(parent)
            .map_err(|_| Error::Unavailable)?;
        Ok(Self {
            parent: Arc::new(parent),
            name: path
                .file_name()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(".")),
        })
    }
    pub fn read(&self, provider: &str) -> Result<Key, Error> {
        let file = openat(
            self.parent.as_ref(),
            &self.name,
            OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_NONBLOCK,
            Mode::empty(),
        )
        .map_err(|error| match error {
            nix::errno::Errno::ELOOP => Error::SymbolicLink,
            nix::errno::Errno::EACCES | nix::errno::Errno::EPERM => Error::Unreadable,
            _ => Error::Unavailable,
        })?;
        read_file(File::from(file), provider)
    }
}
/// Convenience for callers without a retained configuration. Runtimes should
/// keep prepare's Result and expose its safe refusal only after policy checks.
pub fn read(path: &Path, provider: &str) -> Result<Key, Error> {
    KeyFile::prepare(path)?.read(provider)
}
fn read_file(file: File, provider: &str) -> Result<Key, Error> {
    let info = file.metadata().map_err(|_| Error::Unavailable)?;
    if !info.is_file() {
        return Err(Error::NotRegular);
    }
    if info.uid() != Uid::current().as_raw() {
        return Err(Error::WrongOwner);
    }
    if info.mode() & 0o077 != 0 {
        return Err(Error::Permissions);
    }
    if info.len() > 65536 {
        return Err(Error::TooLarge);
    }
    let mut data = Vec::new();
    file.take(65537)
        .read_to_end(&mut data)
        .map_err(|_| Error::Unavailable)?;
    if data.len() > 65536 {
        return Err(Error::TooLarge);
    }
    let value: Value = serde_json::from_slice(data.strip_prefix(&[239, 187, 191]).unwrap_or(&data))
        .map_err(|_| Error::Unavailable)?;
    let raw = if value[provider].is_object() {
        &value[provider]["api_key"]
    } else {
        &value[provider]
    };
    let key = raw
        .as_str()
        .filter(|key| !key.is_empty() && key.len() <= 8192 && key.bytes().all(|b| (33..=126).contains(&b)))
        .ok_or(Error::Unavailable)?;
    Ok(Key(key.into()))
}
