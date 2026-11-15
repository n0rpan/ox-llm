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
    collections::{HashMap, VecDeque},
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
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
fn read_salt(path: &Path) -> io::Result<[u8; 32]> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC | nix::libc::O_NONBLOCK)
        .open(path)?;
    let info = file.metadata()?;
    if !info.is_file()
        || info.uid() != nix::unistd::Uid::current().as_raw()
        || info.mode() & 0o077 != 0
    {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    let mut bytes = Vec::new();
    file.take(33).read_to_end(&mut bytes)?;
    bytes
        .try_into()
        .map_err(|_| io::ErrorKind::InvalidData.into())
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
#[derive(Default)]
struct State {
    budgets: HashMap<String, Budget>,
    // Only keyed session identifiers, bounded like the Python reference.
    ends: VecDeque<(String, Instant)>,
}
struct Inner {
    directory: PathBuf,
    salt: [u8; 32],
    state: Mutex<State>,
}
#[derive(Clone)]
pub struct Ledger(Arc<Inner>);
#[derive(Clone)]
pub struct Daily {
    ledger: Ledger,
    path: PathBuf,
    budget: Budget,
}
/// Caller metadata, never provider headers. Raw identifiers are not retained.
#[derive(Default)]
pub struct Context<'a> {
    pub origin: Option<&'a str>,
    pub session: Option<&'a str>,
    pub idempotency_key: Option<&'a str>,
}
pub struct Entry {
    daily: Daily,
    row: Value,
    started: Instant,
    admitted: Option<Instant>,
    first: Option<Instant>,
}
fn text(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect(),
        _ => String::new(),
    }
}
fn bounded_hint(hint: Option<&str>) -> Option<&str> {
    hint.filter(|hint| (1..=200).contains(&hint.chars().count()))
}
fn number(value: &Value) -> Value {
    value
        .as_f64()
        .filter(|n| n.is_finite())
        .map_or(Value::Null, |_| value.clone())
}
fn millis(start: Instant, end: Instant) -> f64 {
    (end.duration_since(start).as_secs_f64() * 10_000.0).round_ties_even() / 10.0
}
impl Ledger {
    /// Explicit opt-in, dedicated directory outside source repositories.
    /// Keep one Ledger for the gateway lifetime; cloning preserves all holds.
    pub fn open(directory: &Path) -> io::Result<Self> {
        if !directory.is_absolute() {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        fs::create_dir_all(directory)?;
        let info = fs::symlink_metadata(directory)?;
        if !info.is_dir() || info.uid() != nix::unistd::Uid::current().as_raw() {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
        let directory = fs::canonicalize(directory)?;
        KeyFile::prepare(&directory.join(".salt"))
            .map_err(|_| io::Error::from(io::ErrorKind::PermissionDenied))?;
        let salt = install_salt(&directory)?;
        Ok(Self(Arc::new(Inner {
            directory,
            salt,
            state: Mutex::default(),
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
        let mut state = self
            .0
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let budget = state
            .budgets
            .entry(day.into())
            .or_insert_with(|| Budget::new(path.clone()))
            .clone();
        Ok(Daily {
            ledger: self.clone(),
            path,
            budget,
        })
    }
    fn digest(&self, prefix: &[u8], bytes: &[u8], width: usize) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.0.salt).expect("fixed HMAC key size");
        mac.update(prefix);
        mac.update(bytes);
        hex(&mac.finalize().into_bytes())[..width].into()
    }
}
impl Daily {
    pub fn budget(&self) -> Budget {
        self.budget.clone()
    }
    pub fn entry(&self, model: &str, payload: &Value, context: Context<'_>) -> io::Result<Entry> {
        let started = Instant::now();
        let ledger = &self.ledger;
        let messages = payload["messages"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let session = bounded_hint(context.session)
            .or_else(|| bounded_hint(payload["prompt_cache_key"].as_str()))
            .map(|hint| format!("h:{hint}"))
            .or_else(|| {
                messages
                    .iter()
                    .find(|message| message["role"] == "user")
                    .map(|message| format!("m:{}", text(&message["content"])))
            });
        let session = session.map(|value| ledger.digest(b"", value.as_bytes(), 16));
        let gap = session.as_ref().and_then(|session| {
            ledger
                .0
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .ends
                .iter()
                .find(|(key, _)| key == session)
                .map(|(_, end)| millis(*end, started))
        });
        let agent = messages
            .iter()
            .find(|message| message["role"] == "system")
            .map(|message| {
                ledger.digest(
                    b"ledger-agent-v1\0",
                    text(&message["content"]).as_bytes(),
                    12,
                )
            });
        let tools: Vec<_> = messages
            .iter()
            .filter(|message| message["role"] == "tool")
            .collect();
        let last_role = messages
            .last()
            .and_then(|message| message["role"].as_str())
            .filter(|role| {
                [
                    "system",
                    "developer",
                    "user",
                    "assistant",
                    "tool",
                    "function",
                ]
                .contains(role)
            });
        let origin = if context
            .origin
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("lab"))
        {
            "lab"
        } else {
            "organic"
        };
        let mut row = json!({
            "id":hex(&random::<6>()?), "t":(SystemTime::now().duration_since(UNIX_EPOCH)
                .map_err(|_| io::Error::from(io::ErrorKind::Other))?.as_secs_f64() * 1000.0).round_ties_even() / 1000.0,
            "path":"/v1/chat/completions", "model":model, "origin":origin, "client":null,
            "agent":agent, "session":session, "gap_ms":gap, "n_messages":messages.len(),
            "last_role":last_role, "n_tools":payload["tools"].as_array().map_or(0,Vec::len),
            "n_tool_msgs":tools.len(), "tool_bytes":tools.iter().take(128)
                .map(|message| text(&message["content"]).len()).collect::<Vec<_>>(),
            "stream":payload["stream"].as_bool().unwrap_or(false),
            "max_tokens":number(payload.get("max_tokens").or_else(|| payload.get("n_predict")).unwrap_or(&Value::Null)),
            "temperature":number(&payload["temperature"])
        });
        if let Some(key) = context.idempotency_key.filter(|key| !key.is_empty()) {
            row["idempotency"] =
                json!(ledger.digest(b"ledger-idempotency-v1\0", key.as_bytes(), 12));
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
    pub fn admitted(&mut self) {
        self.admitted = Some(Instant::now());
        self.row["queue_wait_ms"] = json!(0.0);
        self.row["admit_ms"] = json!(millis(self.started, self.admitted.unwrap()));
    }
    /// Call only for response data (not SSE keepalive comments). No body bytes
    /// are retained here; provider accounting observes usage separately.
    pub fn observe(&mut self) {
        self.first.get_or_insert_with(Instant::now);
    }
    /// Consume exactly once inside Hold::finish's callback. Failure propagates
    /// into the shared budget's fail-closed state before admission is released.
    /// Only accounting fields are copied; prompts, replies and raw IDs cannot
    /// be injected through the supplied object. Pass the provider key to redact
    /// even catalog labels or provider finish strings before the append.
    pub fn append(
        mut self,
        fields: &Value,
        status: u16,
        error: Option<&'static str>,
        key: Option<&Key>,
    ) -> io::Result<Value> {
        if fields["remote"] != true
            || !["anthropic", "openai", "openrouter"]
                .contains(&fields["remote_provider"].as_str().unwrap_or(""))
            || fields["cost_usd"]
                .as_f64()
                .is_none_or(|value| !value.is_finite() || value < 0.0)
        {
            return Err(io::ErrorKind::InvalidData.into());
        }
        self.row["remote"] = json!(true);
        self.row["remote_provider"] = fields["remote_provider"].clone();
        self.row["remote_model"] = json!(fields["remote_model"]
            .as_str()
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
        for name in ["cost_usd", "catalog_cost_usd", "reported_cost_usd"] {
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
            .filter(|value| ["usage", "reserved", "estimated", "not_forwarded"].contains(value))
            .ok_or(io::ErrorKind::InvalidData)?;
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
        let now = Instant::now();
        self.row["status"] = json!(status);
        self.row["error"] = json!(error);
        self.row["wall_ms"] = json!(millis(self.started, now));
        if let (Some(admitted), Some(first)) = (self.admitted, self.first) {
            self.row["ttfb_ms"] = json!(millis(admitted, first));
        }
        // Remote usage has no engine cache/timing tail. Preserve the existing
        // ledger classification; settlement is represented separately.
        self.row["completeness"] = json!(if error.is_some() || status != 200 {
            "errored"
        } else {
            "incomplete"
        });
        let mut state = self
            .daily
            .ledger
            .0
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(session) = self.row["session"].as_str() {
            state.ends.retain(|(key, _)| key != session);
            state.ends.push_back((session.into(), now));
            while state.ends.len() > 512 {
                state.ends.pop_front();
            }
        }
        if let Some(key) = key {
            redaction::value(&mut self.row, key);
        }
        append_private(&self.daily.path, &self.row)?;
        Ok(self.row)
    }
}
