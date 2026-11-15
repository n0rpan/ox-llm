//! Request-time owner-only credentials. Errors and Debug never contain a path
//! or credential. No value is cached, logged or placed in the environment.
use nix::{libc, unistd::Uid};
use serde_json::Value;
use std::{
    fmt,
    fs::{self, OpenOptions},
    io::{self, Read},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Component, Path, PathBuf},
};

pub struct Key(String);
impl Key {
    /// Expose only when constructing the provider's request/header or redactor.
    pub fn expose(&self) -> &str {
        &self.0
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
    SymbolicLink,
    Unreadable,
    NotRegular,
    WrongOwner,
    Permissions,
    TooLarge,
    InRepository,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reason = match self {
            Self::Unavailable => return f.write_str("Cloud credentials are not configured."),
            Self::SymbolicLink => "the configured path is a symbolic link",
            Self::Unreadable => "the file cannot be read",
            Self::NotRegular => "the configured path is not a regular file",
            Self::WrongOwner => "the file is owned by another user",
            Self::Permissions => "the file is not mode 0600 (group/other permissions must be off)",
            Self::TooLarge => "the file is larger than 65536 bytes",
            Self::InRepository => "the key file is inside a repository",
        };
        write!(f, "Cloud credentials are not configured: {reason}.")
    }
}
impl std::error::Error for Error {}
fn resolve(path: &Path, links: &mut usize) -> io::Result<PathBuf> {
    let mut result = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::RootDir => result = PathBuf::from("/"),
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            Component::Normal(name) => {
                let candidate = result.join(name);
                match fs::symlink_metadata(&candidate) {
                    Ok(info) if info.file_type().is_symlink() => {
                        *links += 1;
                        if *links > 40 {
                            // Python 3.13's non-strict resolve retains a loop's
                            // unresolved component. Preserve its real parent for
                            // source-tree exclusion; the no-follow open below
                            // supplies the public symbolic-link refusal.
                            result = candidate;
                            continue;
                        }
                        let target = fs::read_link(candidate)?;
                        result = resolve(
                            &if target.is_absolute() {
                                target
                            } else {
                                result.join(target)
                            },
                            links,
                        )?;
                    }
                    Ok(_) => result = candidate,
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                        ) =>
                    {
                        result = candidate
                    }
                    Err(error) => return Err(error),
                }
            }
            Component::Prefix(_) => return Err(io::ErrorKind::InvalidInput.into()),
        }
    }
    Ok(result)
}
fn decode(bytes: &[u8]) -> Option<String> {
    let (offset, width, little) = if bytes.starts_with(&[0, 0, 254, 255]) {
        (4, 4, false)
    } else if bytes.starts_with(&[255, 254, 0, 0]) {
        (4, 4, true)
    } else if bytes.starts_with(&[254, 255]) {
        (2, 2, false)
    } else if bytes.starts_with(&[255, 254]) {
        (2, 2, true)
    } else if bytes.len() >= 4 && bytes[0] == 0 {
        (0, if bytes[1] == 0 { 4 } else { 2 }, false)
    } else if bytes.len() >= 4 && bytes[1] == 0 {
        (0, if bytes[2] == 0 && bytes[3] == 0 { 4 } else { 2 }, true)
    } else {
        (0, 1, false)
    };
    let data = &bytes[offset..];
    match width {
        1 => std::str::from_utf8(data.strip_prefix(&[239, 187, 191]).unwrap_or(data))
            .ok()
            .map(str::to_owned),
        2 if data.len().is_multiple_of(2) => {
            let units = data
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| {
                    if little {
                        u16::from_le_bytes([b[0], b[1]])
                    } else {
                        u16::from_be_bytes([b[0], b[1]])
                    }
                })
                .collect::<Vec<_>>();
            Some(String::from_utf16_lossy(&units))
        }
        4 if data.len().is_multiple_of(4) => data
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| {
                let n = if little {
                    u32::from_le_bytes(*b)
                } else {
                    u32::from_be_bytes(*b)
                };
                char::from_u32(n)
            })
            .collect(),
        _ => None,
    }
}
fn parse(bytes: &[u8]) -> Option<Value> {
    let text = decode(bytes)?;
    let bytes = text.as_bytes();
    let mut normalized = Vec::with_capacity(bytes.len());
    let (mut index, mut quoted, mut escaped) = (0, false, false);
    while index < bytes.len() {
        let byte = bytes[index];
        if quoted {
            normalized.push(byte);
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
            }
            index += 1;
        } else if byte == b'"' {
            quoted = true;
            normalized.push(byte);
            index += 1;
        } else if let Some(token) = [
            b"NaN".as_slice(),
            b"Infinity".as_slice(),
            b"-Infinity".as_slice(),
        ]
        .iter()
        .find(|token| bytes[index..].starts_with(token))
        {
            normalized.extend_from_slice(b"null");
            index += token.len();
        } else {
            normalized.push(byte);
            index += 1;
        }
    }
    serde_json::from_slice(&normalized).ok()
}
/// `path` is chosen by trusted configuration (defaulting belongs to the caller).
/// The final component is never followed; parent symlinks are resolved for the
/// source-tree exclusion. All checks are repeated on the opened descriptor.
pub fn read(path: &Path, provider: &str) -> Result<Key, Error> {
    if !path.is_absolute() {
        return Err(Error::Unavailable);
    }
    let resolved = resolve(path, &mut 0).map_err(|_| Error::Unavailable)?;
    if resolved.ancestors().skip(1).any(|parent| {
        parent.join(".git").exists()
            || (parent.join("AGENTS.md").is_file() && parent.join("engine").is_dir())
    }) {
        return Err(Error::InRepository);
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| match error.raw_os_error() {
            Some(libc::ELOOP) => Error::SymbolicLink,
            Some(libc::EACCES | libc::EPERM) => Error::Unreadable,
            _ => Error::Unavailable,
        })?;
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
    let value = parse(&data).ok_or(Error::Unavailable)?;
    let raw = if value[provider].is_object() {
        &value[provider]["api_key"]
    } else {
        &value[provider]
    };
    let key = raw
        .as_str()
        .filter(|key| {
            !key.is_empty() && key.len() <= 8192 && key.bytes().all(|b| (33..=126).contains(&b))
        })
        .ok_or(Error::Unavailable)?;
    Ok(Key(key.into()))
}
