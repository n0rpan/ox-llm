//! Private, process-shared guest key registry. Only newly created keys expose a bearer.
//! All reads are fresh and bounded; diagnostics contain neither paths nor file contents.
use hmac::{Hmac, Mac};
use nix::{
    errno::Errno,
    fcntl::{openat, renameat, AtFlags, Flock, FlockArg, OFlag},
    sys::stat::{fstatat, Mode},
    unistd::{unlinkat, Uid, UnlinkatFlags},
};
use serde_json::{json, Value};
use sha2::Sha256;
use std::{
    fmt,
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Component, Path},
    time::{SystemTime, UNIX_EPOCH},
};

const MAX_BYTES: usize = 65536;
const MAX_KEYS: usize = 64;
const REGISTRY: &str = "share-keys.json";
const DOMAIN: &[u8] = b"oxa guest key digest v1";
type Digest = Hmac<Sha256>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Invalid,
    Unsafe,
    Unavailable,
    Corrupt,
    Full,
    Busy,
    RequestLimit,
    TokenLimit,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Invalid => "Invalid guest key input.",
            Self::Unsafe => "Guest key storage is not private and safe.",
            Self::Unavailable => "Guest key storage is unavailable.",
            Self::Corrupt => "Guest key registry is invalid.",
            Self::Full => "Guest key registry is full.",
            Self::Busy => "Guest key storage is busy.",
            Self::RequestLimit => "Guest daily request limit reached.",
            Self::TokenLimit => "Guest daily token limit reached.",
        })
    }
}
impl std::error::Error for Error {}

/// Required positive admission limits; enforcement belongs to the listener.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub daily_requests: u64,
    pub daily_tokens: u64,
}
impl Limits {
    fn valid(self) -> bool {
        self.daily_requests > 0 && self.daily_tokens > 0
    }
}

/// Safe owner-visible metadata, with no bearer or digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyInfo {
    pub id: String,
    pub name: String,
    pub created_at: u64,
    pub revoked: bool,
    pub limits: Limits,
}

pub struct Bearer(String);
impl Bearer {
    /// Present once to the owner after a successful durable create; never log it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}
impl fmt::Debug for Bearer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Bearer([redacted])")
    }
}
#[derive(Debug)]
pub struct Created {
    pub info: KeyInfo,
    pub bearer: Bearer,
}

/// Binds an existing trusted private directory. No key bytes are retained here.
#[derive(Clone)]
pub struct Store {
    directory: std::sync::Arc<File>,
}
impl fmt::Debug for Store {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("GuestKeyStore([private])")
    }
}

struct Record {
    info: KeyInfo,
    digest: [u8; 32],
}

impl Store {
    /// The caller creates this absolute directory with mode 0700 outside repositories.
    /// Refuse parent links too: a trusted spelling must name the actual directory.
    pub fn open(directory: &Path) -> Result<Self, Error> {
        if !directory.is_absolute() {
            return Err(Error::Invalid);
        }
        let mut parent = OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_DIRECTORY | nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
            .open("/")
            .map_err(|_| Error::Unsafe)?;
        // Walk descriptors, not canonical path strings: replacing a parent or
        // retargeting a link cannot redirect a later registry write into a repo.
        for component in directory.components() {
            refuse_repository(&parent)?;
            match component {
                Component::RootDir => continue,
                Component::Normal(name) => {
                    parent = openat(
                        &parent,
                        name,
                        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
                        Mode::empty(),
                    )
                    .map(File::from)
                    .map_err(|_| Error::Unsafe)?;
                }
                _ => return Err(Error::Invalid),
            }
        }
        refuse_repository(&parent)?;
        check_directory(&parent)?;
        Ok(Self {
            directory: std::sync::Arc::new(parent),
        })
    }

    /// Each operation takes the same stable lock, including readers, so revoked
    /// keys cannot be accepted from a cached registry. No lock crosses network I/O.
    fn lock(&self) -> Result<Flock<File>, Error> {
        self.lock_with(FlockArg::LockExclusive)
    }

    pub(super) fn try_lock(&self) -> Result<Flock<File>, Error> {
        self.lock_with(FlockArg::LockExclusiveNonblock)
    }

    fn lock_with(&self, mode: FlockArg) -> Result<Flock<File>, Error> {
        check_directory(&self.directory)?;
        let file = self.file("share-keys.lock", OFlag::O_RDWR | OFlag::O_CREAT)?;
        check_file(&file)?;
        let lock = Flock::lock(file, mode).map_err(|(_, e)| match e {
            Errno::EWOULDBLOCK => Error::Busy,
            _ => Error::Unavailable,
        })?;
        check_file(&lock)?;
        check_directory(&self.directory)?;
        Ok(lock)
    }

    fn file(&self, name: &str, flags: OFlag) -> Result<File, Error> {
        openat(
            &self.directory,
            name,
            flags | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_NONBLOCK,
            Mode::S_IRUSR | Mode::S_IWUSR,
        )
        .map(File::from)
        .map_err(|e| match e {
            Errno::ELOOP => Error::Unsafe,
            _ => Error::Unavailable,
        })
    }

    fn read(&self) -> Result<Vec<Record>, Error> {
        self.read_bytes(REGISTRY)?
            .map_or_else(|| Ok(Vec::new()), |bytes| decode(&bytes))
    }

    pub(super) fn read_bytes(&self, name: &str) -> Result<Option<Vec<u8>>, Error> {
        let file = match openat(
            &self.directory,
            name,
            OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_NONBLOCK,
            Mode::empty(),
        ) {
            Ok(fd) => File::from(fd),
            Err(Errno::ENOENT) => return Ok(None),
            Err(Errno::ELOOP) => return Err(Error::Unsafe),
            Err(_) => return Err(Error::Unavailable),
        };
        check_file(&file)?;
        if file.metadata().map_err(|_| Error::Unavailable)?.len() > MAX_BYTES as u64 {
            return Err(Error::Corrupt);
        }
        let mut bytes = Vec::new();
        file.take(MAX_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| Error::Unavailable)?;
        if bytes.len() > MAX_BYTES {
            return Err(Error::Corrupt);
        }
        Ok(Some(bytes))
    }

    fn write(&self, records: &[Record]) -> Result<(), Error> {
        let rows: Vec<_> = records
            .iter()
            .map(|r| {
                json!({
                    "id": r.info.id, "name": r.info.name,
                    "created_at": r.info.created_at, "revoked": r.info.revoked,
                    "daily_requests": r.info.limits.daily_requests,
                    "daily_tokens": r.info.limits.daily_tokens,
                    "digest": hex(&r.digest),
                })
            })
            .collect();
        let bytes = serde_json::to_vec(&json!({"version":1,"keys":rows})).map_err(|_| Error::Corrupt)?;
        self.write_bytes(REGISTRY, &bytes)
    }

    pub(super) fn write_bytes(&self, destination: &str, bytes: &[u8]) -> Result<(), Error> {
        if bytes.len() > MAX_BYTES {
            return Err(Error::Full);
        }
        let mut random = [0; 16];
        getrandom::fill(&mut random).map_err(|_| Error::Unavailable)?;
        let name = format!(".share-keys-{}.tmp", hex(&random));
        let mut file = self.file(&name, OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL)?;
        let result = (|| {
            check_file(&file)?;
            file.write_all(bytes).map_err(|_| Error::Unavailable)?;
            file.sync_all().map_err(|_| Error::Unavailable)?;
            renameat(&self.directory, name.as_str(), &self.directory, destination)
                .map_err(|_| Error::Unavailable)?;
            self.directory.sync_all().map_err(|_| Error::Unavailable)
        })();
        let _ = unlinkat(&self.directory, name.as_str(), UnlinkatFlags::NoRemoveDir);
        result
    }

    pub fn create(&self, name: &str, limits: Limits) -> Result<Created, Error> {
        if !valid_name(name) || !limits.valid() {
            return Err(Error::Invalid);
        }
        let _lock = self.lock()?;
        let mut records = self.read()?;
        if records.len() >= MAX_KEYS {
            return Err(Error::Full);
        }
        if records.iter().any(|r| r.info.name == name) {
            return Err(Error::Invalid);
        }
        let mut random = [0; 48];
        getrandom::fill(&mut random).map_err(|_| Error::Unavailable)?;
        let bearer = Bearer(hex(&random[..32]));
        let info = KeyInfo {
            id: hex(&random[32..]),
            name: name.into(),
            created_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| Error::Unavailable)?
                .as_secs(),
            revoked: false,
            limits,
        };
        if records.iter().any(|r| r.info.id == info.id) {
            return Err(Error::Unavailable);
        }
        let mut mac = Digest::new_from_slice(DOMAIN).expect("HMAC accepts any key length");
        mac.update(bearer.expose().as_bytes());
        records.push(Record {
            info: info.clone(),
            digest: mac.finalize().into_bytes().into(),
        });
        self.write(&records)?;
        Ok(Created { info, bearer })
    }

    pub fn list(&self) -> Result<Vec<KeyInfo>, Error> {
        let _lock = self.lock()?;
        Ok(self.read()?.into_iter().map(|r| r.info).collect())
    }

    /// Idempotent revoke; false means the id never existed. Retain its audit identity.
    pub fn revoke(&self, id: &str) -> Result<bool, Error> {
        if !valid_hex(id, 32) {
            return Err(Error::Invalid);
        }
        let _lock = self.lock()?;
        let mut records = self.read()?;
        let Some(record) = records.iter_mut().find(|r| r.info.id == id) else {
            return Ok(false);
        };
        // A prior rename may be visible despite its directory sync failing.
        // Retry the durable write even when the visible row is already revoked.
        record.info.revoked = true;
        self.write(&records)?;
        Ok(true)
    }

    /// Verify fixed-size tags using the crypto library's constant-time primitive.
    /// Scan every record, avoiding an early exit based on the matching position.
    pub fn authenticate(&self, bearer: &str) -> Result<Option<KeyInfo>, Error> {
        let _lock = self.lock()?;
        self.find_key(bearer)
    }

    /// Listener admission never waits on another process's storage lock.
    pub fn try_authenticate(&self, bearer: &str) -> Result<Option<KeyInfo>, Error> {
        let _lock = self.try_lock()?;
        self.find_key(bearer)
    }

    pub(super) fn find_key(&self, bearer: &str) -> Result<Option<KeyInfo>, Error> {
        if !valid_hex(bearer, 64) {
            return Ok(None);
        }
        let mut found = None;
        for record in self.read()? {
            let mut mac = Digest::new_from_slice(DOMAIN).expect("HMAC accepts any key length");
            mac.update(bearer.as_bytes());
            let matches = mac.verify_slice(&record.digest).is_ok();
            if matches && !record.info.revoked {
                found = Some(record.info);
            }
        }
        Ok(found)
    }
}

fn check_directory(file: &File) -> Result<(), Error> {
    let m = file.metadata().map_err(|_| Error::Unavailable)?;
    if !m.is_dir() || m.uid() != Uid::current().as_raw() || m.mode() & 0o777 != 0o700 {
        return Err(Error::Unsafe);
    }
    Ok(())
}
fn refuse_repository(directory: &File) -> Result<(), Error> {
    let present = |name: &str| match fstatat(directory, name, AtFlags::AT_SYMLINK_NOFOLLOW) {
        Ok(_) => Ok(true),
        Err(Errno::ENOENT) => Ok(false),
        Err(_) => Err(Error::Unsafe),
    };
    // Separate/bare common directories have no .git child. Linked-worktree
    // metadata uses HEAD + commondir/gitdir instead of its own objects directory.
    if present(".git")?
        || (present("HEAD")? && (present("objects")? || present("commondir")? || present("gitdir")?))
    {
        return Err(Error::Unsafe);
    }
    Ok(())
}
fn check_file(file: &File) -> Result<(), Error> {
    let m = file.metadata().map_err(|_| Error::Unavailable)?;
    if !m.is_file() || m.uid() != Uid::current().as_raw() || m.mode() & 0o777 != 0o600 || m.nlink() != 1 {
        return Err(Error::Unsafe);
    }
    Ok(())
}
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.trim() == name
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b" ._-".contains(&b))
}
fn valid_hex(s: &str, len: usize) -> bool {
    s.len() == len
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn decode(bytes: &[u8]) -> Result<Vec<Record>, Error> {
    let v: Value = serde_json::from_slice(bytes).map_err(|_| Error::Corrupt)?;
    let root = v.as_object().ok_or(Error::Corrupt)?;
    if root.len() != 2 || v["version"] != 1 {
        return Err(Error::Corrupt);
    }
    let rows = v["keys"].as_array().ok_or(Error::Corrupt)?;
    if rows.len() > MAX_KEYS {
        return Err(Error::Corrupt);
    }
    let mut out: Vec<Record> = Vec::new();
    for row in rows {
        if row.as_object().ok_or(Error::Corrupt)?.len() != 7 {
            return Err(Error::Corrupt);
        }
        let text = |key| row[key].as_str().ok_or(Error::Corrupt);
        let id = text("id")?;
        let name = text("name")?;
        let raw = text("digest")?;
        let limits = Limits {
            daily_requests: row["daily_requests"].as_u64().ok_or(Error::Corrupt)?,
            daily_tokens: row["daily_tokens"].as_u64().ok_or(Error::Corrupt)?,
        };
        if !valid_hex(id, 32)
            || !valid_hex(raw, 64)
            || !valid_name(name)
            || !limits.valid()
            || out.iter().any(|r| r.info.id == id || r.info.name == name)
        {
            return Err(Error::Corrupt);
        }
        let mut digest = [0; 32];
        for (i, byte) in digest.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&raw[i * 2..i * 2 + 2], 16).map_err(|_| Error::Corrupt)?;
        }
        if out.iter().any(|r| r.digest == digest) {
            return Err(Error::Corrupt);
        }
        out.push(Record {
            info: KeyInfo {
                id: id.into(),
                name: name.into(),
                created_at: row["created_at"].as_u64().ok_or(Error::Corrupt)?,
                revoked: row["revoked"].as_bool().ok_or(Error::Corrupt)?,
                limits,
            },
            digest,
        });
    }
    Ok(out)
}
