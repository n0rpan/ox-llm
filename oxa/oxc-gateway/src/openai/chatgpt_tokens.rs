//! Python-compatible token records; no transport, sign-in, or credential discovery.
//!
//! Callers supply a path in a trusted directory (including trusted ancestors), normally
//! `chatgpt-auth.json`. Its persistent sibling `chatgpt-auth.lock` is also used by Python.
//! Never unlink the lock: independent opens of that inode serialize threads and processes.
//! Available with `upstream-openai` + `secrets`; unused code adds no default-binary code.
use nix::{
    fcntl::{openat, renameat, Flock, FlockArg, OFlag},
    sys::stat::Mode,
    unistd::{getuid, unlinkat, UnlinkatFlags},
};
use serde_json::{json, Value};
use std::{
    ffi::{OsStr, OsString},
    fmt,
    fs::{File, Metadata, OpenOptions, Permissions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

const MAX_BYTES: u64 = 64 * 1024;
const LOCK: &str = "chatgpt-auth.lock";
const FLAGS: OFlag = OFlag::O_NOFOLLOW.union(OFlag::O_CLOEXEC).union(OFlag::O_NONBLOCK);

/// Only fixed diagnostics: paths, input JSON and refresher errors are never retained.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Unavailable,
    UnsafeFile,
    TooLarge,
    InvalidRecord,
    Refresh,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unavailable => "token file unavailable",
            Self::UnsafeFile => "token file is not private and regular",
            Self::TooLarge => "token record exceeds size limit",
            Self::InvalidRecord => "invalid token record",
            Self::Refresh => "token refresh failed",
        })
    }
}
impl std::error::Error for Error {}

/// Sensitive fields are exposed explicitly, never by Debug.
#[derive(Clone)]
pub struct TokenRecord {
    access_token: String,
    refresh_token: String,
    account_id: String,
    expires_at: f64,
}
impl fmt::Debug for TokenRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TokenRecord([redacted])")
    }
}
impl TokenRecord {
    pub fn new(
        access_token: String,
        refresh_token: String,
        account_id: String,
        expires_at: f64,
    ) -> Result<Self, Error> {
        if access_token.is_empty()
            || refresh_token.is_empty()
            || account_id.is_empty()
            || !expires_at.is_finite()
        {
            return Err(Error::InvalidRecord);
        }
        Ok(Self {
            access_token,
            refresh_token,
            account_id,
            expires_at,
        })
    }
    pub fn access_token(&self) -> &str {
        &self.access_token
    }
    pub fn refresh_token(&self) -> &str {
        &self.refresh_token
    }
    pub fn account_id(&self) -> &str {
        &self.account_id
    }
    pub fn expires_at(&self) -> f64 {
        self.expires_at
    }
    pub fn is_fresh_at(&self, unix_seconds: f64) -> bool {
        unix_seconds.is_finite() && self.expires_at - 120.0 > unix_seconds
    }
    fn fresh(&self) -> Result<bool, Error> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| Error::Unavailable)?
            .as_secs_f64();
        Ok(self.is_fresh_at(now))
    }
}

/// Borrowed refresh fields. Debug never exposes the refresh token or client id.
#[derive(Clone, Copy)]
pub struct RefreshForm<'a>([(&'static str, &'a str); 3]);
impl RefreshForm<'_> {
    pub fn as_fields(&self) -> &[(&'static str, &str); 3] {
        &self.0
    }
}
impl fmt::Debug for RefreshForm<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RefreshForm([redacted])")
    }
}

/// Pure form construction; encoding and HTTP policy belong to the caller.
pub fn refresh_form<'a>(refresh_token: &'a str, client_id: &'a str) -> RefreshForm<'a> {
    RefreshForm([
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", client_id),
    ])
}

/// Convert a trusted refresh response into a record using an injected Unix time.
///
/// Like Python's `_record`, preserve an omitted/empty refresh token and fall back
/// to the previous account. A positive numeric `expires_in` takes precedence over
/// the access token's numeric `exp`, then a one-hour default. Claims are decoded,
/// NOT verified: they are metadata from the caller's authenticated token endpoint,
/// never proof of identity. This function does not read credentials or do I/O.
pub fn record(reply: &Value, previous: &TokenRecord, now: f64) -> Result<TokenRecord, Error> {
    if !now.is_finite() {
        return Err(Error::InvalidRecord);
    }
    let access = reply
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or(Error::InvalidRecord)?;
    let refresh = match reply.get("refresh_token") {
        None | Some(Value::Null) => previous.refresh_token(),
        Some(Value::String(s)) if s.is_empty() => previous.refresh_token(),
        Some(Value::String(s)) => s,
        _ => return Err(Error::InvalidRecord),
    };
    let access_claims = claims(access);
    let id_claims = reply.get("id_token").and_then(Value::as_str).map(claims);
    let account = |claims: &Value| {
        claims
            .get("https://api.openai.com/auth")?
            .get("chatgpt_account_id")?
            .as_str()
            .map(str::to_owned)
    };
    let account = id_claims
        .as_ref()
        .and_then(account)
        .or_else(|| account(&access_claims))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| previous.account_id().to_owned());
    let expires_at = match reply
        .get("expires_in")
        .and_then(Value::as_f64)
        .filter(|n| *n > 0.0)
    {
        Some(seconds) => now + seconds,
        None => access_claims
            .get("exp")
            .and_then(Value::as_f64)
            .unwrap_or(now + 3600.0),
    };
    TokenRecord::new(access.to_owned(), refresh.to_owned(), account, expires_at)
}

// A small bounded base64url decoder avoids adding a dependency for metadata-only
// JWT inspection. Invalid payloads behave like absent claims, as in Python.
fn claims(token: &str) -> Value {
    let decode = || -> Option<Value> {
        let payload = token.split('.').nth(1)?;
        if payload.len() > MAX_BYTES as usize {
            return None;
        }
        let mut bytes = Vec::with_capacity(payload.len() * 3 / 4);
        let mut bits = 0u32;
        let mut count = 0;
        for byte in payload.trim_end_matches('=').bytes() {
            let value = match byte {
                b'A'..=b'Z' => byte - b'A',
                b'a'..=b'z' => byte - b'a' + 26,
                b'0'..=b'9' => byte - b'0' + 52,
                b'-' => 62,
                b'_' => 63,
                _ => return None,
            };
            bits = (bits << 6) | u32::from(value);
            count += 6;
            if count >= 8 {
                count -= 8;
                bytes.push((bits >> count) as u8);
            }
        }
        if count >= 6 {
            return None;
        }
        serde_json::from_slice(&bytes).ok()
    };
    decode().unwrap_or(Value::Null)
}

struct Directory {
    file: File,
    name: OsString,
}
impl Directory {
    fn open(path: &Path) -> Result<Self, Error> {
        let name = path.file_name().ok_or(Error::Unavailable)?.to_owned();
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let file = OpenOptions::new()
            .read(true)
            .custom_flags((FLAGS | OFlag::O_DIRECTORY).bits())
            .open(parent)
            .map_err(|_| Error::Unavailable)?;
        let meta = file.metadata().map_err(|_| Error::Unavailable)?;
        if meta.uid() != getuid().as_raw() || meta.mode() & 0o022 != 0 {
            return Err(Error::UnsafeFile);
        }
        Ok(Self { file, name })
    }
    fn open_file(&self, name: &OsStr, flags: OFlag) -> Result<File, Error> {
        openat(&self.file, name, flags | FLAGS, Mode::S_IRUSR | Mode::S_IWUSR)
            .map(File::from)
            .map_err(|_| Error::Unavailable)
    }
    fn read(&self) -> Result<TokenRecord, Error> {
        let file = self.open_file(&self.name, OFlag::O_RDONLY)?;
        let meta = file.metadata().map_err(|_| Error::Unavailable)?;
        check_file(&meta)?;
        if meta.len() > MAX_BYTES {
            return Err(Error::TooLarge);
        }
        let mut bytes = Vec::new();
        file.take(MAX_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| Error::Unavailable)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(Error::TooLarge);
        }
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| Error::InvalidRecord)?;
        let text = |field| {
            value
                .get(field)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or(Error::InvalidRecord)
        };
        TokenRecord::new(
            text("access_token")?,
            text("refresh_token")?,
            text("account_id")?,
            value
                .get("expires_at")
                .and_then(Value::as_f64)
                .ok_or(Error::InvalidRecord)?,
        )
    }
    fn replace(&self, record: &TokenRecord) -> Result<(), Error> {
        let bytes = serde_json::to_vec(&json!({
            "access_token": record.access_token, "refresh_token": record.refresh_token,
            "account_id": record.account_id, "expires_at": record.expires_at,
        }))
        .map_err(|_| Error::InvalidRecord)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(Error::TooLarge);
        }
        // Check the target again, never write through a replaced link.
        self.read()?;
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let name = OsString::from(format!(
            ".chatgpt-auth-{}-{}.tmp",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = self.open_file(&name, OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL)?;
        let result = (|| {
            file.set_permissions(Permissions::from_mode(0o600))
                .map_err(|_| Error::Unavailable)?;
            file.write_all(&bytes).map_err(|_| Error::Unavailable)?;
            file.sync_all().map_err(|_| Error::Unavailable)?;
            renameat(&self.file, name.as_os_str(), &self.file, self.name.as_os_str())
                .map_err(|_| Error::Unavailable)?;
            self.file.sync_all().map_err(|_| Error::Unavailable)
        })();
        if result.is_err() {
            let _ = unlinkat(&self.file, name.as_os_str(), UnlinkatFlags::NoRemoveDir);
        }
        result
    }
}
fn check_file(meta: &Metadata) -> Result<(), Error> {
    if !meta.is_file() || meta.uid() != getuid().as_raw() || meta.mode() & 0o077 != 0 || meta.nlink() != 1 {
        return Err(Error::UnsafeFile);
    }
    Ok(())
}

/// Read a private, bounded Python token record without refreshing it.
pub fn read(path: &Path) -> Result<TokenRecord, Error> {
    Directory::open(path)?.read()
}

/// Re-read under an exclusive flock before calling the injected refresher.
/// The closure owns all transport policy; its error is deliberately discarded.
/// A refresh must return a record fresh for at least the 120-second margin.
pub fn credentials<E>(
    path: &Path,
    refresh: impl FnOnce(&TokenRecord) -> Result<TokenRecord, E>,
) -> Result<TokenRecord, Error> {
    let dir = Directory::open(path)?;
    let record = dir.read()?;
    if record.fresh()? {
        return Ok(record);
    }
    let lock = dir.open_file(OsStr::new(LOCK), OFlag::O_RDWR | OFlag::O_CREAT)?;
    check_file(&lock.metadata().map_err(|_| Error::Unavailable)?)?;
    let _lock = Flock::lock(lock, FlockArg::LockExclusive).map_err(|_| Error::Unavailable)?;
    let record = dir.read()?;
    if record.fresh()? {
        return Ok(record);
    }
    let updated = refresh(&record).map_err(|_| Error::Refresh)?;
    if !updated.fresh()? {
        return Err(Error::InvalidRecord);
    }
    dir.replace(&updated)?;
    Ok(updated)
}
