//! Reservations and settlement fields. A hold remains reserved until its row is
//! written to the ledger; write failures stop spending.
//!
//! Several processes (every in-process oxa loop is its own gateway) share one daily
//! ledger, so admission is serialized with an exclusive `flock` on `<ledger>.lock`
//! around read-spend + reserve, and every hold is persisted as a file in
//! `<ledger>.holds/` (one file per hold, 0600, in the 0700 ledger directory). The owner
//! keeps an `flock` on its hold file for as long as it lives, so a hold whose lock
//! can be taken belongs to a dead process: an unforwarded one is released, a forwarded
//! one (possibly paid, never recorded) stays reserved for the rest of the day.
use crate::{
    policy::{self, Refusal},
    pricing::{Cache, Price},
    spend::SpendReader,
};
use serde_json::{json, Value};
use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions, TryLockError},
    io::{self, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug)]
pub struct Model {
    pub provider: String,
    pub upstream: String,
    pub context: u64,
    pub max_output: u64,
    pub price: Price,
}
#[derive(Default)]
struct State {
    reader: SpendReader,
    failed: bool,
}
#[derive(Clone)]
pub struct Budget {
    path: PathBuf,
    state: Arc<Mutex<State>>,
}
pub struct Hold {
    budget: Budget,
    model: Model,
    /// Persisted hold; the open file carries the owner's `flock`.
    file: Option<File>,
    path: PathBuf,
    reserved: f64,
    output: u64,
    forwarded: bool,
    settled: bool,
}
#[derive(Clone, Default, Debug)]
pub struct Usage {
    pub prompt: Option<u64>,
    pub output: Option<u64>,
    pub cache_read: Option<u64>,
    pub cache_write: Option<u64>,
    pub invalid_cache: bool,
    pub reported_cost: Option<f64>,
    pub finish: Option<String>,
}
#[derive(Clone, Copy)]
pub enum Outcome {
    Complete,
    Failed,
    Cancelled {
        request_bytes: usize,
        streamed_bytes: usize,
        usage_final: bool,
        /// Include time spent generating hidden reasoning, not just wire bytes.
        elapsed: Duration,
    },
}
impl Usage {
    /// Consume canonical OpenAI usage after provider translation. Anthropic
    /// translators must include cache reads/writes in prompt and keep details.
    pub fn observe(&mut self, value: &Value) {
        let usage = &value["usage"];
        for (field, target) in [
            ("prompt_tokens", &mut self.prompt),
            ("completion_tokens", &mut self.output),
        ] {
            if let Some(number) = usage[field].as_u64() {
                *target = Some(number);
            }
        }
        for (field, target) in [
            ("cached_tokens", &mut self.cache_read),
            ("cache_write_tokens", &mut self.cache_write),
        ] {
            if let Some(value) = usage["prompt_tokens_details"].get(field) {
                if let Some(number) = value.as_u64() {
                    *target = Some(number);
                } else {
                    self.invalid_cache = true;
                }
            }
        }
        if let Some(cost) = usage["cost"].as_f64().filter(|cost| (0.0..=1e100).contains(cost)) {
            self.reported_cost = Some(self.reported_cost.unwrap_or(0.0).max(cost));
        }
        if let Some(finish) = value["choices"][0]["finish_reason"].as_str() {
            self.finish = Some(finish.into());
        }
    }
}
fn exhausted() -> Refusal {
    Refusal {
        status: 429,
        message: "The daily cloud budget is exhausted.",
        code: "budget_exceeded",
        param: "model",
    }
}
const SENT: &str = ".sent";
const HOLD: &str = ".hold";
static HOLD_SEQ: AtomicU64 = AtomicU64::new(0);
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = OsString::from(path.as_os_str());
    name.push(suffix);
    PathBuf::from(name)
}
fn private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}
fn private_file(path: &Path, create_new: bool) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC);
    if create_new {
        options.create_new(true);
    } else {
        options.create(true).truncate(false);
    }
    options.open(path)
}
impl Budget {
    fn lock_path(&self) -> PathBuf {
        sibling(&self.path, ".lock")
    }
    fn holds_dir(&self) -> PathBuf {
        sibling(&self.path, ".holds")
    }
    /// Cross-process admission/settlement lock. Held only for a ledger read and a
    /// handful of tiny files; released when the returned file is dropped.
    fn exclusive(&self) -> io::Result<File> {
        if let Some(parent) = self.path.parent() {
            private_dir(parent)?;
        }
        let file = private_file(&self.lock_path(), false)?;
        file.lock()?;
        Ok(file)
    }
    /// Sum of live holds and of holds left by a dead process after forwarding.
    /// Dead unforwarded holds are removed. Call with the exclusive lock held.
    fn outstanding(&self) -> io::Result<f64> {
        let entries = match fs::read_dir(self.holds_dir()) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0.0),
            Err(error) => return Err(error),
        };
        let mut total = 0.0;
        for entry in entries {
            let path = entry?.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let forwarded = name.ends_with(SENT);
            if !forwarded && !name.ends_with(HOLD) {
                return Err(io::ErrorKind::InvalidData.into());
            }
            let mut file = OpenOptions::new()
                .read(true)
                .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
                .open(&path)?;
            let amount: f64 = io::read_to_string(&mut file)?
                .trim()
                .parse()
                .ok()
                .filter(|n: &f64| n.is_finite() && *n >= 0.0)
                .ok_or(io::ErrorKind::InvalidData)?;
            match file.try_lock() {
                // Nobody holds it: its owner is gone.
                Ok(()) if !forwarded => fs::remove_file(&path)?,
                Ok(()) | Err(TryLockError::WouldBlock) => total += amount,
                Err(TryLockError::Error(error)) => return Err(error),
            }
        }
        Ok(total)
    }
    fn persist(&self, amount: f64) -> io::Result<(File, PathBuf)> {
        let dir = self.holds_dir();
        private_dir(&dir)?;
        let id = format!(
            "{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos()),
            HOLD_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let path = dir.join(format!("{id}{HOLD}"));
        let mut file = private_file(&path, true)?;
        file.lock()?;
        file.write_all(format!("{amount:?}").as_bytes())?;
        Ok((file, path))
    }
    /// One instance per daily ledger path; share it across all request tasks.
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            state: Arc::new(Mutex::new(State::default())),
        }
    }
    pub fn spend(&self) -> Result<f64, Refusal> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.failed {
            return Err(Refusal::ledger());
        }
        state.reader.read(&self.path).map_err(|_| Refusal::ledger())
    }
    pub fn reserve(
        &self,
        cap: &Value,
        ledger_enabled: bool,
        model: &Model,
        output: u64,
    ) -> Result<Hold, Refusal> {
        // The gateway has resolved the request's output-limit field and alias.
        // A zero limit must never become an under-reserved forwarded request.
        if output == 0 {
            return Err(Refusal {
                status: 400,
                message: "The output token limit must be a positive integer.",
                code: "invalid_value",
                param: "max_tokens",
            });
        }
        policy::daily_budget(cap, ledger_enabled, Some(0.0), 0.0, 0.0)?;
        let output = output.min(model.max_output);
        let required = model
            .price
            .reservation(&model.provider, &model.upstream, model.context, output)
            .ok_or_else(exhausted)?;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.failed {
            return Err(Refusal::ledger());
        }
        let _lock = self.exclusive().map_err(|_| Refusal::ledger())?;
        let spent = state.reader.read(&self.path).map_err(|_| Refusal::ledger())?;
        let reserved = self.outstanding().map_err(|_| Refusal::ledger())?;
        policy::daily_budget(cap, ledger_enabled, Some(spent), reserved, required)?;
        let (file, path) = self.persist(required).map_err(|_| Refusal::ledger())?;
        Ok(Hold {
            budget: self.clone(),
            model: model.clone(),
            file: Some(file),
            path,
            reserved: required,
            output,
            forwarded: false,
            settled: false,
        })
    }
}
impl Hold {
    pub fn reserved(&self) -> f64 {
        self.reserved
    }
    /// Persists the flag (a rename that keeps the owner's lock): if this process dies
    /// afterwards, other processes keep the hold reserved. A failure latches the budget.
    pub fn mark_forwarded(&mut self) {
        if self.forwarded {
            return;
        }
        self.forwarded = true;
        let sent = self.path.with_extension("sent");
        let moved = self.budget.exclusive().and_then(|_lock| {
            fs::rename(&self.path, &sent)?;
            self.path = sent;
            Ok(())
        });
        if moved.is_err() {
            let mut state = self.budget.state.lock().unwrap_or_else(|e| e.into_inner());
            state.failed = true;
        }
    }
    /// Remove the persisted hold. Call with the exclusive lock held, after the row.
    fn release(&mut self) {
        let _ = fs::remove_file(&self.path);
        let _ = fs::remove_dir(self.budget.holds_dir());
        self.file = None;
    }
    fn record(&self, usage: &Usage, outcome: Outcome) -> Value {
        let model = &self.model;
        let mut row = json!({"remote":true,"remote_provider":model.provider,"remote_model":model.upstream});
        for (field, value) in [
            ("prompt_tokens", usage.prompt),
            ("completion_tokens", usage.output),
            ("cached_tokens", usage.cache_read),
            ("cache_write_tokens", usage.cache_write),
        ] {
            if let Some(value) = value {
                row[field] = json!(value);
            }
        }
        if let Some(finish) = &usage.finish {
            row["finish_reason"] = json!(finish);
        }
        let invalid = usage.invalid_cache
            || usage.prompt.is_some_and(|prompt| {
                usage
                    .cache_read
                    .unwrap_or(0)
                    .saturating_add(usage.cache_write.unwrap_or(0))
                    > prompt
            });
        let cost = if invalid {
            None
        } else {
            model.price.cost(
                &model.provider,
                &model.upstream,
                usage.prompt.unwrap_or(0),
                usage.output.unwrap_or(0),
                Cache::Tokens {
                    read: usage.cache_read.unwrap_or(0),
                    write: usage.cache_write.unwrap_or(0),
                },
            )
        };
        if let Some(cost) = cost {
            row["cost_usd"] = json!(cost);
            if model.provider != "anthropic" {
                row["catalog_cost_usd"] = json!(cost);
            }
        }
        if model.provider == "openrouter" {
            if let Some(cost) = usage.reported_cost {
                row["reported_cost_usd"] = json!(cost);
                row["cost_usd"] = json!(cost);
            }
        }
        if invalid {
            row["usage_missing"] = json!(true);
        }
        let actual = row["cost_usd"].as_f64();
        let missing = invalid || usage.prompt.is_none() || usage.output.is_none() || actual.is_none();
        match outcome {
            Outcome::Cancelled {
                request_bytes,
                streamed_bytes,
                usage_final,
                elapsed,
            } if !usage_final || usage.prompt.is_none() || usage.output.is_none() || actual.is_none() => {
                let output = self.output.min(
                    (streamed_bytes as u64)
                        .max(usage.output.unwrap_or(0))
                        .max((elapsed.as_secs_f64() * 300.0) as u64),
                );
                let estimate = model
                    .price
                    .cost(
                        &model.provider,
                        &model.upstream,
                        usage.prompt.unwrap_or(request_bytes as u64),
                        output,
                        // Final cache composition is unknown. Include cache-write
                        // premiums together with hidden output in the estimate.
                        Cache::Invalid,
                    )
                    .unwrap_or(self.reserved);
                row["cost_usd"] = json!(self.reserved.min(estimate).max(actual.unwrap_or(0.0)));
                row["usage_missing"] = json!(true);
                row["settled"] = json!("estimated");
            }
            Outcome::Cancelled { .. } => {
                row["settled"] = json!("usage");
            }
            Outcome::Failed => {
                row["cost_usd"] = json!(self.reserved.max(actual.unwrap_or(0.0)));
                row["usage_missing"] = json!(true);
                row["settled"] = json!("reserved");
            }
            _ if missing => {
                row["cost_usd"] = json!(self.reserved.max(actual.unwrap_or(0.0)));
                row["usage_missing"] = json!(true);
                row["settled"] = json!("reserved");
            }
            _ => row["settled"] = json!("usage"),
        }
        row
    }
    /// The writer merges these fixed remote fields with its private request
    /// metadata and appends one complete row. Never put prompts/replies in it.
    pub fn finish(
        mut self,
        usage: &Usage,
        outcome: Outcome,
        write: impl FnOnce(&Value) -> io::Result<()>,
    ) -> Result<Value, Refusal> {
        let row = self.record(usage, outcome);
        let budget = self.budget.clone();
        let mut state = budget.state.lock().unwrap_or_else(|e| e.into_inner());
        // Row first, then release, both under the lock: another process sees the hold,
        // or the row, or briefly both - never neither.
        let lock = budget.exclusive();
        let result = lock.map_err(|_| Refusal::ledger()).and_then(|_lock| {
            let result = write(&row).map_err(|_| Refusal::ledger());
            self.release();
            result
        });
        if result.is_err() {
            state.failed = true;
        }
        self.settled = true;
        result.map(|()| row)
    }
}
impl Drop for Hold {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        let budget = self.budget.clone();
        let mut state = budget.state.lock().unwrap_or_else(|e| e.into_inner());
        // Before forwarding it is safe to release an abandoned admission. A
        // potentially paid request without a recorded settlement fails closed: its
        // file stays (the lock is released with the file) and keeps counting.
        if self.forwarded {
            state.failed = true;
            self.file = None;
        } else if let Ok(_lock) = budget.exclusive() {
            self.release();
        }
    }
}
