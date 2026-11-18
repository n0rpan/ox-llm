//! Private request metadata and one shared budget per local calendar day.
//! Instantiate once per gateway; clones share admission and writer state.
use crate::{
    accounting::Budget,
    redaction,
    secrets::{Key, KeyFile},
    spend::append_private,
};
use chrono::{Local, NaiveDate};
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;
use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

fn random<const N: usize>() -> io::Result<[u8; N]> {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes).map_err(|_| io::Error::from(io::ErrorKind::Other))?;
    Ok(bytes)
}
fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(DIGITS[usize::from(byte >> 4)] as char);
        result.push(DIGITS[usize::from(byte & 15)] as char);
    }
    result
}
fn read_salt(path: &Path) -> io::Result<[u8; 32]> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC | nix::libc::O_NONBLOCK)
        .open(path)?;
    let info = file.metadata()?;
    if !info.is_file() || info.uid() != nix::unistd::Uid::current().as_raw() || info.mode() & 0o077 != 0 {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    let mut bytes = Vec::new();
    file.take(33).read_to_end(&mut bytes)?;
    bytes.try_into().map_err(|_| io::ErrorKind::InvalidData.into())
}
fn install_salt(directory: &Path) -> io::Result<[u8; 32]> {
    let path = directory.join(".salt");
    match read_salt(&path) {
        Ok(salt) => return Ok(salt),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    // Publish only a complete salt. Concurrent gateway starts never see a
    // partly initialized file, and never replace an existing install identity.
    let temporary = directory.join(format!(".salt-{}", hex(&random::<12>()?)));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
            .open(&temporary)?;
        file.write_all(&random::<32>()?)?;
        file.sync_all()?;
        match fs::hard_link(&temporary, &path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        read_salt(&path)
    })();
    let _ = fs::remove_file(temporary);
    result
}
struct Inner {
    directory: PathBuf,
    salt: [u8; 32],
    budgets: Mutex<HashMap<String, Budget>>,
    writer: Mutex<()>,
}
#[cfg(feature = "anthropic-gateway")]
impl Ledger {
    pub(crate) fn history_mac(&self) -> Hmac<Sha256> {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.0.salt).expect("fixed-length ledger salt");
        mac.update(b"oxa-provider-history-v1\0");
        mac
    }
}
#[derive(Clone)]
pub struct Ledger(Arc<Inner>);
#[derive(Clone)]
pub struct Daily {
    ledger: Ledger,
    path: PathBuf,
    budget: Budget,
}
#[derive(Clone, Copy, Debug, Default)]
pub enum Origin {
    #[default]
    Oxa,
    Api,
}
impl Origin {
    fn as_str(self) -> &'static str {
        match self {
            Self::Oxa => "oxa",
            Self::Api => "api",
        }
    }
}
/// Explicit caller identifiers are HMACed; request bodies are never accepted.
#[derive(Default)]
pub struct Context<'a> {
    pub origin: Origin,
    pub session: Option<&'a str>,
    pub idempotency_key: Option<&'a str>,
    /// Validated owner key name, never a bearer/header. API callers only;
    /// persisted in `guest_key` as a separate-domain ledger-salted digest,
    /// never raw text or a session/provider identity. None preserves owner calls.
    pub guest_key_name: Option<&'a str>,
}
/// Completion of a local guest request. Missing usage retains its quota reservation.
pub enum LocalGuestOutcome {
    Completed { tokens: Option<(u64, u64)> },
    Cancelled,
    Failed,
}

pub struct Entry {
    daily: Daily,
    row: Value,
    started: Instant,
    admitted: Option<Instant>,
    first: Option<Instant>,
}
fn millis(start: Instant, end: Instant) -> f64 {
    end.duration_since(start).as_secs_f64() * 1000.0
}

impl Ledger {
    /// Explicit opt-in, dedicated directory outside source repositories.
    /// Keep one Ledger for the gateway lifetime; cloning preserves all holds.
    pub fn open(directory: &Path) -> io::Result<Self> {
        if !directory.is_absolute() {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let mut ancestor = directory;
        while !ancestor.try_exists()? {
            ancestor = ancestor.parent().ok_or(io::ErrorKind::InvalidInput)?;
        }
        KeyFile::prepare(&ancestor.join(".salt"))
            .map_err(|_| io::Error::from(io::ErrorKind::PermissionDenied))?;
        fs::create_dir_all(directory)?;
        let info = fs::symlink_metadata(directory)?;
        if !info.is_dir() || info.uid() != nix::unistd::Uid::current().as_raw() {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        let directory = fs::canonicalize(directory)?;
        KeyFile::prepare(&directory.join(".salt"))
            .map_err(|_| io::Error::from(io::ErrorKind::PermissionDenied))?;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        let salt = install_salt(&directory)?;
        Ok(Self(Arc::new(Inner {
            directory,
            salt,
            budgets: Mutex::default(),
            writer: Mutex::default(),
        })))
    }
    pub fn today(&self) -> io::Result<Daily> {
        self.day(&Local::now().format("%Y%m%d").to_string())
    }
    /// An admitted request keeps its original day across midnight. Tests may
    /// supply a calendar day explicitly without changing the process clock.
    pub fn day(&self, day: &str) -> io::Result<Daily> {
        if day.len() != 8
            || !day.bytes().all(|byte| byte.is_ascii_digit())
            || NaiveDate::parse_from_str(day, "%Y%m%d").is_err()
        {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let path = self.0.directory.join(format!("requests-{day}.jsonl"));
        let mut budgets = self.0.budgets.lock().unwrap_or_else(|error| error.into_inner());
        let budget = budgets
            .entry(day.into())
            .or_insert_with(|| Budget::new(path.clone()))
            .clone();
        Ok(Daily {
            ledger: self.clone(),
            path,
            budget,
        })
    }
    fn digest(&self, prefix: &[u8], bytes: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.0.salt).expect("fixed HMAC key size");
        mac.update(prefix);
        mac.update(bytes);
        hex(&mac.finalize().into_bytes()[..16])
    }
}
impl Daily {
    pub fn budget(&self) -> Budget {
        self.budget.clone()
    }
    pub fn entry(&self, model: &str, context: Context<'_>) -> io::Result<Entry> {
        if model.is_empty()
            || model.len() > 128
            || context.guest_key_name.is_some_and(|name| name.is_empty())
            || (context.guest_key_name.is_some() && !matches!(context.origin, Origin::Api))
        {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let started = Instant::now();
        let mut row = json!({
            "id": hex(&random::<16>()?),
            "t": SystemTime::now().duration_since(UNIX_EPOCH)
                .map_err(|_| io::Error::from(io::ErrorKind::Other))?.as_secs_f64(),
            "model": model, "origin": context.origin.as_str(), "session": null
        });
        for (name, domain, value) in [
            ("session", b"oxc-session-v1\0".as_slice(), context.session),
            (
                "idempotency",
                b"oxc-idempotency-v1\0".as_slice(),
                context.idempotency_key,
            ),
            (
                "guest_key",
                b"oxc-guest-key-name-v1\0".as_slice(),
                context.guest_key_name,
            ),
        ] {
            if let Some(value) = value.filter(|value| !value.is_empty()) {
                if value.len() > 4096 {
                    return Err(io::ErrorKind::InvalidInput.into());
                }
                row[name] = json!(self.ledger.digest(domain, value.as_bytes()));
            }
        }
        Ok(Entry {
            daily: self.clone(),
            row,
            started,
            admitted: None,
            first: None,
        })
    }
}
impl Entry {
    #[cfg(feature = "anthropic-gateway")]
    pub(crate) fn provider_session_hint(&self, provider: &str) -> Option<String> {
        let session = self.row["session"].as_str()?;
        let identity = format!("{provider}\0{session}");
        Some(
            self.daily
                .ledger
                .digest(b"oxa-provider-session-v1\0", identity.as_bytes()),
        )
    }
    #[cfg(feature = "anthropic-gateway")]
    pub(crate) fn ledger(&self) -> &Ledger {
        &self.daily.ledger
    }
    pub fn admitted(&mut self) {
        let admitted = *self.admitted.get_or_insert_with(Instant::now);
        self.row["admit_ms"] = json!(millis(self.started, admitted));
    }
    /// Call only for response data (not SSE keepalive comments). No body bytes
    /// are retained here; provider accounting observes usage separately.
    pub fn observe(&mut self) {
        if self.admitted.is_some() {
            self.first.get_or_insert_with(Instant::now);
        }
    }
    /// Consume exactly once inside Hold::finish's callback. Failure propagates
    /// into the shared budget's fail-closed state before admission is released.
    /// Only accounting fields are copied; prompts, replies and raw IDs cannot
    /// be injected through the supplied object. Pass the provider key to redact
    /// even catalog labels or provider finish strings before the append.
    pub fn append(
        self,
        fields: &Value,
        status: u16,
        error: Option<&'static str>,
        key: Option<&Key>,
    ) -> io::Result<Value> {
        self.append_redacting(fields, status, error, &|row| {
            if let Some(key) = key {
                redaction::value(row, key);
            }
        })
    }
    pub(crate) fn append_redacting(
        mut self,
        fields: &Value,
        status: u16,
        error: Option<&'static str>,
        redact: &dyn Fn(&mut Value),
    ) -> io::Result<Value> {
        if fields["remote"] != true
            || ![
                "anthropic",
                "openai",
                "openai-chat",
                "openrouter",
                "chatgpt",
                "opencode",
            ]
            .contains(&fields["remote_provider"].as_str().unwrap_or(""))
            || crate::spend::budget_cost(fields).is_none()
        {
            return Err(io::ErrorKind::InvalidData.into());
        }
        self.row["remote"] = json!(true);
        self.row["remote_provider"] = fields["remote_provider"].clone();
        if let Some(value) = fields.get("opencode") {
            if fields["remote_provider"] != "opencode"
                || !matches!(fields["billing"].as_str(), Some("subscription" | "metered"))
            {
                return Err(io::ErrorKind::InvalidData.into());
            }
            #[cfg(feature = "anthropic-client")]
            {
                self.row["opencode"] =
                    crate::opencode::ledger_metadata(value).map_err(|_| io::ErrorKind::InvalidData)?;
            }
            #[cfg(not(feature = "anthropic-client"))]
            {
                let _ = value;
                return Err(io::ErrorKind::InvalidData.into());
            }
        }
        self.row["remote_model"] = json!(fields["remote_model"]
            .as_str()
            .filter(|value| !value.is_empty() && value.len() <= 1024)
            .ok_or(io::ErrorKind::InvalidData)?);
        for name in [
            "prompt_tokens",
            "completion_tokens",
            "cached_tokens",
            "cache_write_tokens",
        ] {
            if let Some(value) = fields.get(name) {
                self.row[name] = json!(value.as_u64().ok_or(io::ErrorKind::InvalidData)?);
            }
        }
        for name in [
            "cost_usd",
            "catalog_cost_usd",
            "reported_cost_usd",
            "estimated_cost_usd",
            "go_reserved_usd",
        ] {
            if let Some(value) = fields.get(name) {
                if value
                    .as_f64()
                    .is_none_or(|value| !value.is_finite() || value < 0.0)
                {
                    return Err(io::ErrorKind::InvalidData.into());
                }
                self.row[name] = value.clone();
            }
        }
        if let Some(value) = fields["usage_missing"].as_bool() {
            self.row["usage_missing"] = json!(value);
        }
        let settled = fields["settled"]
            .as_str()
            .filter(|value| ["usage", "reserved", "estimated", "not_forwarded", "refused"].contains(value))
            .ok_or(io::ErrorKind::InvalidData)?;
        if let Some(bound) = fields.get("go_reserved_usd") {
            if fields["remote_provider"] != "opencode"
                || !matches!(settled, "usage" | "reserved")
                || bound.as_f64().is_none_or(|amount| amount <= 0.0)
            {
                return Err(io::ErrorKind::InvalidData.into());
            }
        }
        if settled == "refused" && (fields["cost_usd"].as_f64() != Some(0.0) || !(400..500).contains(&status))
        {
            return Err(io::ErrorKind::InvalidData.into());
        }
        match fields.get("billing") {
            Some(value)
                if value == "subscription"
                    || value == "metered" && fields["remote_provider"] == "opencode" =>
            {
                if !matches!(fields["remote_provider"].as_str(), Some("chatgpt" | "opencode"))
                    || (value == "subscription" && fields["cost_usd"].as_f64() != Some(0.0))
                    || (value == "metered" && crate::spend::budget_cost(fields).is_none_or(|n| n <= 0.0))
                {
                    return Err(io::ErrorKind::InvalidData.into());
                }
                let requests = fields["subscription_requests"]
                    .as_u64()
                    .filter(|n| *n <= 1)
                    .ok_or(io::ErrorKind::InvalidData)?;
                let tokens = fields["subscription_tokens"]
                    .as_u64()
                    .ok_or(io::ErrorKind::InvalidData)?;
                if (requests == 0) != (settled == "not_forwarded")
                    || (matches!(settled, "not_forwarded" | "refused") && tokens != 0)
                {
                    return Err(io::ErrorKind::InvalidData.into());
                }
                self.row["billing"] = value.clone();
                self.row["subscription_requests"] = json!(requests);
                self.row["subscription_tokens"] = json!(tokens);
            }
            None => {
                if matches!(fields["remote_provider"].as_str(), Some("chatgpt" | "opencode"))
                    || fields.get("subscription_requests").is_some()
                    || fields.get("subscription_tokens").is_some()
                {
                    return Err(io::ErrorKind::InvalidData.into());
                }
            }
            Some(value) if value == "metered" => {
                if matches!(fields["remote_provider"].as_str(), Some("chatgpt" | "opencode"))
                    || fields.get("subscription_requests").is_some()
                    || fields.get("subscription_tokens").is_some()
                {
                    return Err(io::ErrorKind::InvalidData.into());
                }
                self.row["billing"] = json!("metered");
            }
            _ => return Err(io::ErrorKind::InvalidData.into()),
        }
        self.row["settled"] = json!(settled);
        if let Some(finish) = fields["finish_reason"].as_str().filter(|value| {
            !value.is_empty()
                && value.len() <= 24
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
        }) {
            self.row["finish_reason"] = json!(finish);
        }
        self.write_row(status, error, |row| {
            redact(row);
            #[cfg(feature = "anthropic-client")]
            if let Some(plan) = row["opencode"].get("billing_source") {
                let plan = crate::opencode::PlanName::from_value(plan);
                row["opencode"]["billing_source"] = plan.as_str().into();
            }
        })
    }

    /// Append one typed terminal row for an authenticated local guest. Local
    /// usage enters the same private file without entering cloud budget totals.
    pub fn append_local_guest(mut self, outcome: LocalGuestOutcome) -> io::Result<Value> {
        if self.row["origin"] != "api"
            || !self.row["guest_key"].is_string()
            || !self.row["session"].is_null()
            || self.row.get("idempotency").is_some()
        {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let (status, error, tokens) = match outcome {
            LocalGuestOutcome::Completed { tokens } => (200, None, tokens),
            LocalGuestOutcome::Cancelled => (499, Some("local_guest_cancelled"), None),
            LocalGuestOutcome::Failed => (502, Some("local_guest_failed"), None),
        };
        self.row["remote"] = json!(false);
        self.row["cost_usd"] = json!(0);
        self.row["usage_missing"] = json!(tokens.is_none());
        self.row["settled"] = json!(if tokens.is_some() { "usage" } else { "reserved" });
        if let Some((input, output)) = tokens {
            self.row["prompt_tokens"] = json!(input);
            self.row["completion_tokens"] = json!(output);
        }
        self.write_row(status, error, |_| {})
    }

    fn write_row(
        mut self,
        status: u16,
        error: Option<&'static str>,
        redact: impl FnOnce(&mut Value),
    ) -> io::Result<Value> {
        let now = Instant::now();
        self.row["status"] = json!(status);
        self.row["error"] = json!(error);
        self.row["wall_ms"] = json!(millis(self.started, now));
        if let (Some(admitted), Some(first)) = (self.admitted, self.first) {
            self.row["ttfb_ms"] = json!(millis(admitted, first));
        }
        let _writer = self
            .daily
            .ledger
            .0
            .writer
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        redact(&mut self.row);
        append_private(&self.daily.path, &self.row)?;
        Ok(self.row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_timing_notifications_preserve_the_first_transition() {
        // Timing notifications do not perform IO. Use a daily handle without
        // opening a ledger or creating any salt or request file.
        let ledger = Ledger(Arc::new(Inner {
            directory: PathBuf::new(),
            salt: [0; 32],
            budgets: Mutex::default(),
            writer: Mutex::default(),
        }));
        let mut entry = ledger
            .day("20260102")
            .unwrap()
            .entry("synthetic", Context::default())
            .unwrap();
        entry.admitted();
        let admitted = entry.admitted;
        entry.observe();
        let first = entry.first;
        entry.admitted();
        entry.observe();
        assert_eq!(entry.admitted, admitted);
        assert_eq!(entry.first, first);
        assert!(first >= admitted);
    }
}
