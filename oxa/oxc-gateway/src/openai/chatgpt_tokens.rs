//! Python-compatible token records; no transport, sign-in, or credential discovery.
//!
//! Callers supply a path in a trusted directory (including trusted ancestors), normally
//! `chatgpt-auth.json`. Its persistent sibling `chatgpt-auth.lock` is also used by Python.
//! Never unlink the lock: independent opens of that inode serialize threads and processes.
//! Available with `upstream-openai` + `secrets`; unused code adds no default-binary code.
use nix::{
    fcntl::{openat, renameat, renameat2, Flock, FlockArg, OFlag, RenameFlags},
    sys::stat::{mkdirat, Mode},
    unistd::{getuid, unlinkat, UnlinkatFlags},
};
use serde_json::{json, Value};
use std::{
    ffi::{OsStr, OsString},
    fmt,
    fs::{File, Metadata, OpenOptions, Permissions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
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
    Exists,
    Account,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unavailable => "token file unavailable",
            Self::UnsafeFile => "token file is not private and regular",
            Self::TooLarge => "token record exceeds size limit",
            Self::InvalidRecord => "invalid token record",
            Self::Refresh => "token refresh failed",
            Self::Exists => "already signed in; replacement requires explicit consent",
            Self::Account => "sign-in account does not match the configured account",
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
        let expires_at = access_expiry(&access_token).map_or(expires_at, |expiry| expiry.min(expires_at));
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

// Unverified JWT expiry is ONLY a conservative lifetime hint. It never
// authorizes an identity, endpoint or scope. No signature/auth claims are used.
pub(super) fn access_expiry(access: &str) -> Option<f64> {
    if access.len() > MAX_BYTES as usize {
        return None;
    }
    let mut parts = access.split('.');
    parts.next()?;
    let payload = parts.next()?;
    parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let data = payload.trim_end_matches('=');
    let padding = payload.len() - data.len();
    if padding > 2
        || data.len() % 4 == 1
        || (padding != 0 && (payload.len() % 4 != 0 || padding != (4 - data.len() % 4) % 4))
    {
        return None;
    }
    let mut decoded = Vec::with_capacity(data.len() * 3 / 4);
    let (mut pending, mut bits) = (0_u32, 0_u32);
    for byte in data.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        };
        pending = (pending << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            decoded.push((pending >> bits) as u8);
            pending &= (1 << bits) - 1;
        }
    }
    if pending != 0 {
        return None;
    }
    serde_json::from_slice::<Value>(&decoded).ok()?["exp"]
        .as_f64()
        .filter(|n| n.is_finite())
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

/// Fresh authorization-code reply, never a refresh fallback. The authenticated
/// issuer supplies account metadata; these decoded claims are not JWT proof.
/// Neither an ID token nor the reply is stored in the resulting record.
pub fn login_record(reply: &Value, now: f64) -> Result<TokenRecord, Error> {
    let text = |field| reply.get(field).and_then(Value::as_str).filter(|s| !s.is_empty());
    let access = text("access_token").ok_or(Error::InvalidRecord)?;
    let refresh = text("refresh_token").ok_or(Error::InvalidRecord)?;
    let account = text("id_token")
        .map(claims)
        .and_then(|c| {
            c["https://api.openai.com/auth"]["chatgpt_account_id"]
                .as_str()
                .map(str::to_owned)
        })
        .or_else(|| {
            claims(access)["https://api.openai.com/auth"]["chatgpt_account_id"]
                .as_str()
                .map(str::to_owned)
        })
        .filter(|s| !s.is_empty())
        .ok_or(Error::InvalidRecord)?;
    let expires = reply["expires_in"]
        .as_f64()
        .filter(|n| n.is_finite() && *n > 0.0)
        .map(|n| now + n)
        .or_else(|| access_expiry(access))
        .ok_or(Error::InvalidRecord)?;
    let record = TokenRecord::new(access.to_owned(), refresh.to_owned(), account, expires)?;
    if !record.is_fresh_at(now) {
        return Err(Error::InvalidRecord);
    }
    Ok(record)
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
        self.publish(record, false)
    }
    fn publish(&self, record: &TokenRecord, create_only: bool) -> Result<(), Error> {
        self.publish_sync(record, create_only, File::sync_all)
    }
    fn publish_sync(
        &self,
        record: &TokenRecord,
        create_only: bool,
        mut sync: impl FnMut(&File) -> std::io::Result<()>,
    ) -> Result<(), Error> {
        let bytes = serde_json::to_vec(&json!({
            "access_token": record.access_token, "refresh_token": record.refresh_token,
            "account_id": record.account_id, "expires_at": record.expires_at,
        }))
        .map_err(|_| Error::InvalidRecord)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(Error::TooLarge);
        }
        // Check the target again, never write through a replaced link.
        if !create_only {
            self.read()?;
        }
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
            sync(&file).map_err(|_| Error::Unavailable)?;
            if create_only {
                renameat2(
                    &self.file,
                    name.as_os_str(),
                    &self.file,
                    self.name.as_os_str(),
                    RenameFlags::RENAME_NOREPLACE,
                )
                .map_err(|e| {
                    if e == nix::errno::Errno::EEXIST {
                        Error::Exists
                    } else {
                        Error::Unavailable
                    }
                })?;
            } else {
                renameat(&self.file, name.as_os_str(), &self.file, self.name.as_os_str())
                    .map_err(|_| Error::Unavailable)?;
            }
            sync(&self.file).map_err(|_| Error::Unavailable)
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

/// Fixed owner location, beside the existing cloud key file. Create only the
/// fixed private directories; refuse links/foreign or writable ancestors and
/// never relax an existing directory's permissions. Reads no credential bytes.
pub fn prepare_login_path(home: &Path) -> Result<PathBuf, Error> {
    if !home.is_absolute() {
        return Err(Error::Unavailable);
    }
    let mut directory = OpenOptions::new()
        .read(true)
        .custom_flags((FLAGS | OFlag::O_DIRECTORY).bits())
        .open(home)
        .map_err(|_| Error::Unavailable)?;
    let mut path = home.to_owned();
    for component in [".config", "oxc", "llm"] {
        let meta = directory.metadata().map_err(|_| Error::Unavailable)?;
        if meta.uid() != getuid().as_raw() || meta.mode() & 0o022 != 0 {
            return Err(Error::UnsafeFile);
        }
        match mkdirat(&directory, component, Mode::S_IRWXU) {
            Ok(()) => directory.sync_all().map_err(|_| Error::Unavailable)?,
            Err(nix::errno::Errno::EEXIST) => {}
            Err(_) => return Err(Error::Unavailable),
        }
        directory = File::from(
            openat(
                &directory,
                component,
                FLAGS | OFlag::O_RDONLY | OFlag::O_DIRECTORY,
                Mode::empty(),
            )
            .map_err(|_| Error::UnsafeFile)?,
        );
        path.push(component);
    }
    let meta = directory.metadata().map_err(|_| Error::Unavailable)?;
    if meta.uid() != getuid().as_raw() || meta.mode() & 0o022 != 0 {
        return Err(Error::UnsafeFile);
    }
    Ok(path.join("chatgpt-auth.json"))
}

fn try_lock(dir: &Directory) -> Result<Option<Flock<File>>, Error> {
    let file = dir.open_file(OsStr::new(LOCK), OFlag::O_RDWR | OFlag::O_CREAT)?;
    check_file(&file.metadata().map_err(|_| Error::Unavailable)?)?;
    match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        Ok(lock) => Ok(Some(lock)),
        Err((_, e)) if e == nix::errno::Errno::EWOULDBLOCK => Ok(None),
        Err(_) => Err(Error::Unavailable),
    }
}

/// Owner login only. Nonblocking admission shares the permanent refresh lock;
/// `None` means retry under the caller's deadline/cancellation. Creation never
/// replaces an existing entry. Explicit replacement still cannot switch account.
pub fn try_install(
    path: &Path,
    record: &TokenRecord,
    replace: bool,
    expected_accounts: &[&str],
) -> Result<Option<()>, Error> {
    if expected_accounts
        .iter()
        .any(|expected| *expected != record.account_id())
    {
        return Err(Error::Account);
    }
    if !record.fresh()? {
        return Err(Error::InvalidRecord);
    }
    let dir = Directory::open(path)?;
    let Some(_lock) = try_lock(&dir)? else {
        return Ok(None);
    };
    // Inspect the entry without following links; do not mistake an unsafe or
    // malformed existing record for an absent record.
    let present = match nix::sys::stat::fstatat(
        &dir.file,
        dir.name.as_os_str(),
        nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW,
    ) {
        Ok(_) => true,
        Err(nix::errno::Errno::ENOENT) => false,
        Err(_) => return Err(Error::Unavailable),
    };
    if present {
        let previous = dir.read()?;
        if !replace {
            return Err(Error::Exists);
        }
        if previous.account_id() != record.account_id() {
            return Err(Error::Account);
        }
    }
    dir.publish(record, !present)?;
    Ok(Some(()))
}

/// Owner logout only; remove the checked record under the SAME lock as refresh.
/// Never remove the lock inode. A waiting old refresher must re-read and fails.
pub fn try_logout(path: &Path) -> Result<Option<()>, Error> {
    let dir = Directory::open(path)?;
    let Some(_lock) = try_lock(&dir)? else {
        return Ok(None);
    };
    match nix::sys::stat::fstatat(
        &dir.file,
        dir.name.as_os_str(),
        nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW,
    ) {
        Err(nix::errno::Errno::ENOENT) => return Ok(Some(())),
        Err(_) => return Err(Error::Unavailable),
        Ok(_) => {}
    }
    let file = dir.open_file(&dir.name, OFlag::O_RDONLY)?;
    check_file(&file.metadata().map_err(|_| Error::Unavailable)?)?;
    unlinkat(&dir.file, dir.name.as_os_str(), UnlinkatFlags::NoRemoveDir).map_err(|_| Error::Unavailable)?;
    dir.file.sync_all().map_err(|_| Error::Unavailable)?;
    Ok(Some(()))
}

/// Re-read under an exclusive flock before calling the injected refresher.
/// The closure owns all transport policy; its error is deliberately discarded.
/// A refresh must return a record fresh for at least the 120-second margin.
pub fn credentials<E>(
    path: &Path,
    refresh: impl FnOnce(&TokenRecord) -> Result<TokenRecord, E>,
) -> Result<TokenRecord, Error> {
    credentials_locked(path, refresh, true, None)?.ok_or(Error::Unavailable)
}

/// Nonblocking refresh-lock admission. `None` means only WouldBlock: no refresh
/// was attempted. Retry under the caller's cancellation/deadline; all file and
/// record checks are identical to `credentials`. A fresh record needs no lock.
pub fn try_credentials<E>(
    path: &Path,
    refresh: impl FnOnce(&TokenRecord) -> Result<TokenRecord, E>,
) -> Result<Option<TokenRecord>, Error> {
    credentials_locked(path, refresh, false, None)
}

/// After an explicit token_expired rejection, refresh only that access-token
/// generation. A waiter reuses another process's fresh rotation under the lock.
pub fn try_credentials_after_rejection<E>(
    path: &Path,
    rejected_access: &str,
    refresh: impl FnOnce(&TokenRecord) -> Result<TokenRecord, E>,
) -> Result<Option<TokenRecord>, Error> {
    credentials_locked(path, refresh, false, Some(rejected_access))
}

fn credentials_locked<E>(
    path: &Path,
    refresh: impl FnOnce(&TokenRecord) -> Result<TokenRecord, E>,
    blocking: bool,
    rejected_access: Option<&str>,
) -> Result<Option<TokenRecord>, Error> {
    let dir = Directory::open(path)?;
    let record = dir.read()?;
    if record.fresh()? && rejected_access != Some(record.access_token()) {
        return Ok(Some(record));
    }
    let lock = dir.open_file(OsStr::new(LOCK), OFlag::O_RDWR | OFlag::O_CREAT)?;
    check_file(&lock.metadata().map_err(|_| Error::Unavailable)?)?;
    let mode = if blocking {
        FlockArg::LockExclusive
    } else {
        FlockArg::LockExclusiveNonblock
    };
    let _lock = match Flock::lock(lock, mode) {
        Ok(lock) => lock,
        Err((_, error)) if !blocking && error == nix::errno::Errno::EWOULDBLOCK => return Ok(None),
        Err(_) => return Err(Error::Unavailable),
    };
    let record = dir.read()?;
    if record.fresh()? && rejected_access != Some(record.access_token()) {
        return Ok(Some(record));
    }
    let updated = refresh(&record).map_err(|_| Error::Refresh)?;
    if !updated.fresh()? {
        return Err(Error::InvalidRecord);
    }
    dir.replace(&updated)?;
    Ok(Some(updated))
}

#[cfg(test)]
mod conversion_tests {
    use super::*;

    fn previous() -> TokenRecord {
        TokenRecord::new(
            "fake-old-access".into(),
            "fake-old-refresh".into(),
            "fake-old-account".into(),
            10.0,
        )
        .unwrap()
    }

    // Synthetic unsigned metadata, not a credential or authentication proof.
    fn unsigned(value: Value) -> String {
        let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let bytes = serde_json::to_vec(&value).unwrap();
        let mut encoded = String::new();
        for chunk in bytes.chunks(3) {
            let n = (u32::from(chunk[0]) << 16)
                | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
                | u32::from(*chunk.get(2).unwrap_or(&0));
            for shift in (0..chunk.len() + 1).map(|i| 18 - 6 * i) {
                encoded.push(alphabet[((n >> shift) & 63) as usize] as char);
            }
        }
        format!("unsigned.{encoded}.")
    }

    #[test]
    fn refresh_preserves_omitted_null_empty_and_rotates_nonempty() {
        for value in [
            None,
            Some(Value::Null),
            Some(json!("")),
            Some(json!("fake-rotated-refresh")),
        ] {
            let mut reply = json!({"access_token":"fake-new-access"});
            if let Some(value) = value {
                reply["refresh_token"] = value;
            }
            let result = record(&reply, &previous(), 100.0).unwrap();
            assert_eq!(result.access_token(), "fake-new-access");
            assert_eq!(
                result.refresh_token(),
                if reply["refresh_token"] == "fake-rotated-refresh" {
                    "fake-rotated-refresh"
                } else {
                    "fake-old-refresh"
                }
            );
            assert_eq!(result.account_id(), "fake-old-account");
        }
    }

    #[test]
    fn expiry_uses_positive_duration_before_claim_then_one_hour() {
        let access = unsigned(json!({"exp":9000.5}));
        for (duration, expected) in [
            (json!(30.5), 130.5),
            (json!(0), 9000.5),
            (json!(-1), 9000.5),
            (Value::Null, 9000.5),
            (json!("30"), 9000.5),
        ] {
            let result = record(
                &json!({"access_token":access,"expires_in":duration}),
                &previous(),
                100.0,
            )
            .unwrap();
            assert_eq!(result.expires_at(), expected);
        }
        for claims in [json!({}), json!({"exp":"9000"})] {
            assert_eq!(
                record(&json!({"access_token":unsigned(claims)}), &previous(), 100.0)
                    .unwrap()
                    .expires_at(),
                3700.0
            );
        }
    }

    #[test]
    fn account_prefers_id_claim_then_access_then_previous() {
        let access =
            unsigned(json!({"https://api.openai.com/auth":{"chatgpt_account_id":"fake-access-account"}}));
        for (id, expected) in [
            (
                unsigned(json!({"https://api.openai.com/auth":{"chatgpt_account_id":"fake-id-account"}})),
                "fake-id-account",
            ),
            (unsigned(json!({})), "fake-access-account"),
            ("malformed".into(), "fake-access-account"),
        ] {
            assert_eq!(
                record(&json!({"access_token":access,"id_token":id}), &previous(), 100.0)
                    .unwrap()
                    .account_id(),
                expected
            );
        }
        assert_eq!(
            record(&json!({"access_token":"opaque-fake-access"}), &previous(), 100.0)
                .unwrap()
                .account_id(),
            "fake-old-account"
        );
    }

    #[test]
    fn unsigned_claim_decoder_handles_url_alphabet_padding_unicode_and_bad_payloads() {
        let value = json!({"exp":1234,"note":"λ 🦀","https://api.openai.com/auth":{"chatgpt_account_id":"fake-account"}});
        let token = unsigned(value.clone());
        assert_eq!(claims(&token), value);
        let payload = token.split('.').nth(1).unwrap();
        assert_eq!(claims(&format!("unsigned.{payload}==.ignored-signature")), value);
        for bad in [
            "missing".to_owned(),
            "unsigned.!.".into(),
            "unsigned.YQ.".into(),
            format!("unsigned.{}.", "A".repeat(MAX_BYTES as usize + 1)),
        ] {
            assert_eq!(claims(&bad), Value::Null);
            let result = record(&json!({"access_token":bad}), &previous(), 100.0).unwrap();
            assert_eq!(result.expires_at(), 3700.0);
            assert_eq!(result.account_id(), "fake-old-account");
        }
        // Explicit URL-safe alphabet control: this decodes to valid JSON string "࿾".
        assert_eq!(claims("unsigned.IuC_viI."), json!("࿾"));
    }

    #[test]
    fn malformed_replies_and_nonfinite_clocks_have_fixed_errors() {
        for reply in [
            Value::Null,
            json!([]),
            json!({}),
            json!({"access_token":""}),
            json!({"access_token":12}),
            json!({"access_token":"fake-sensitive-access","refresh_token":false}),
        ] {
            let error = record(&reply, &previous(), 100.0).unwrap_err();
            assert_eq!(error, Error::InvalidRecord);
            assert_eq!(error.to_string(), "invalid token record");
            assert_eq!(format!("{error:?}"), "InvalidRecord");
        }
        for now in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(
                record(&json!({"access_token":"fake-sensitive-access"}), &previous(), now).unwrap_err(),
                Error::InvalidRecord
            );
        }
        assert_eq!(
            record(
                &json!({"access_token":"fake-sensitive-access","expires_in":f64::MAX}),
                &previous(),
                f64::MAX
            )
            .unwrap_err(),
            Error::InvalidRecord
        );
    }

    #[test]
    fn exact_refresh_fields_are_borrowed_unencoded_and_debug_is_redacted() {
        let form = refresh_form("fake-refresh+&=%", "fake-client+&=%");
        assert_eq!(
            form.as_fields(),
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", "fake-refresh+&=%"),
                ("client_id", "fake-client+&=%")
            ]
        );
        assert_eq!(format!("{form:?}"), "RefreshForm([redacted])");
        let result = record(
            &json!({"access_token":"fake-sensitive-access","refresh_token":"fake-sensitive-refresh"}),
            &previous(),
            100.0,
        )
        .unwrap();
        assert_eq!(format!("{result:?}"), "TokenRecord([redacted])");
    }
}

#[cfg(test)]
mod publication_tests {
    use super::*;
    #[test]
    fn failed_file_sync_or_noreplace_publication_preserves_original_and_cleans_staging() {
        let root = std::env::temp_dir().join(format!("login-publication-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, Permissions::from_mode(0o700)).unwrap();
        let path = root.join("chatgpt-auth.json");
        let record = TokenRecord::new(
            "synthetic-access".into(),
            "synthetic-refresh".into(),
            "synthetic-account".into(),
            2_000_000_000.0,
        )
        .unwrap();
        try_install(&path, &record, false, &[]).unwrap();
        let previous = std::fs::read(&path).unwrap();
        let directory = Directory::open(&path).unwrap();
        assert_eq!(
            directory.publish_sync(&record, false, |_| Err(std::io::ErrorKind::Other.into())),
            Err(Error::Unavailable)
        );
        assert_eq!(std::fs::read(&path).unwrap(), previous);
        assert_eq!(directory.publish(&record, true), Err(Error::Exists));
        assert_eq!(std::fs::read(&path).unwrap(), previous);
        std::fs::set_permissions(&root, Permissions::from_mode(0o500)).unwrap();
        assert_eq!(directory.publish(&record, false), Err(Error::Unavailable));
        std::fs::set_permissions(&root, Permissions::from_mode(0o700)).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), previous);
        assert!(std::fs::read_dir(&root).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp")));
        drop(directory);
        std::fs::remove_dir_all(root).unwrap();
    }
}
