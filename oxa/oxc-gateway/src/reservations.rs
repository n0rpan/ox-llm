//! Short filesystem transactions shared by every process using one daily ledger.
//! A crash keeps its holds. Broken state refuses admission rather than resetting it.
use crate::billing::Counts;
use nix::fcntl::{Flock, FlockArg};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

const MAX_HOLDS: usize = 16_384;
const MAX_BYTES: u64 = 2 * 1024 * 1024;

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    name.into()
}
fn private(file: &File) -> io::Result<()> {
    let info = file.metadata()?;
    if !info.is_file() || info.uid() != nix::unistd::Uid::current().as_raw() || info.mode() & 0o777 != 0o600 {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    Ok(())
}
fn options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC | nix::libc::O_NONBLOCK);
    options
}

/// A subscription request reserves tokens only; its cost is unknown. The USD
/// field of a `Subscription` hold never reserves headroom: it exists solely so
/// records written by older binaries round-trip unchanged and stay readable.
pub(crate) enum Reserved {
    Metered(f64),
    Subscription(u64, f64),
    Go(u64, f64),
}
impl Reserved {
    fn from_value(value: &Value) -> io::Result<Self> {
        if let Some(cost) = value.as_f64().filter(|cost| cost.is_finite() && *cost >= 0.0) {
            return Ok(Self::Metered(cost));
        }
        let object = value
            .as_object()
            .filter(|o| {
                o.len() == 1 || o.len() == 2 && (o.contains_key("reserved_usd") || o.contains_key("go_usd"))
            })
            .ok_or(io::ErrorKind::InvalidData)?;
        let tokens = object
            .get("subscription_tokens")
            .and_then(Value::as_u64)
            .filter(|n| *n > 0)
            .ok_or(io::ErrorKind::InvalidData)?;
        let positive_usd = |value: &Value| {
            value
                .as_f64()
                .filter(|cost| cost.is_finite() && *cost > 0.0)
                .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))
        };
        if let Some(usd) = object.get("go_usd") {
            return Ok(Self::Go(tokens, positive_usd(usd)?));
        }
        let usd = object
            .get("reserved_usd")
            .map(positive_usd)
            .transpose()?
            .unwrap_or(0.0);
        Ok(Self::Subscription(tokens, usd))
    }
    fn to_value(&self) -> Value {
        match self {
            Self::Metered(cost) => json!(cost),
            Self::Subscription(tokens, usd) if *usd > 0.0 => {
                json!({"subscription_tokens":tokens,"reserved_usd":usd})
            }
            Self::Subscription(tokens, _) => json!({"subscription_tokens":tokens}),
            Self::Go(tokens, usd) => json!({"subscription_tokens":tokens,"go_usd":usd}),
        }
    }
    fn usd(&self) -> f64 {
        match self {
            Self::Metered(cost) | Self::Subscription(_, cost) | Self::Go(_, cost) => *cost,
        }
    }
}
pub(crate) struct Shared {
    _lock: Flock<File>,
    file: File,
    directory: PathBuf,
    fresh: bool,
    pub failed: bool,
    pub holds: BTreeMap<String, Reserved>,
}
impl Shared {
    pub fn open(ledger: &Path) -> io::Result<Self> {
        if !ledger.is_absolute() {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let directory = ledger.parent().ok_or(io::ErrorKind::InvalidInput)?;
        let info = directory.symlink_metadata()?;
        if !info.is_dir()
            || info.uid() != nix::unistd::Uid::current().as_raw()
            || info.mode() & 0o777 != 0o700
        {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        let lock = options()
            .create(true)
            .truncate(false)
            .open(sibling(ledger, ".budget-lock"))?;
        private(&lock)?;
        let lock = Flock::lock(lock, FlockArg::LockExclusive).map_err(|(_, error)| io::Error::from(error))?;
        let path = sibling(ledger, ".holds");
        let (mut file, fresh) = match options().create_new(true).open(&path) {
            Ok(file) => (file, true),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => (options().open(&path)?, false),
            Err(error) => return Err(error),
        };
        private(&file)?;
        let mut failed = false;
        let mut holds = BTreeMap::new();
        if !fresh {
            if file.metadata()?.len() > MAX_BYTES {
                return Err(io::ErrorKind::InvalidData.into());
            }
            let mut bytes = Vec::new();
            (&mut file).take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
            if bytes.len() as u64 > MAX_BYTES {
                return Err(io::ErrorKind::InvalidData.into());
            }
            let value: Value = serde_json::from_slice(&bytes)?;
            let object = value.as_object().ok_or(io::ErrorKind::InvalidData)?;
            failed = value["failed"].as_bool().ok_or(io::ErrorKind::InvalidData)?;
            let pending = value["holds"].as_object().ok_or(io::ErrorKind::InvalidData)?;
            if object.len() != 2 || pending.len() > MAX_HOLDS {
                return Err(io::ErrorKind::InvalidData.into());
            }
            let mut total = 0.0;
            for (id, cost) in pending {
                let amount = Reserved::from_value(cost)?;
                if id.len() != 32 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return Err(io::ErrorKind::InvalidData.into());
                }
                total += amount.usd();
                if !total.is_finite() {
                    return Err(io::ErrorKind::InvalidData.into());
                }
                holds.insert(id.clone(), amount);
            }
        }
        let mut shared = Self {
            _lock: lock,
            file,
            directory: directory.into(),
            fresh,
            failed,
            holds,
        };
        shared.subscription_reserved()?;
        if fresh {
            shared.save()?;
        }
        Ok(shared)
    }
    /// USD headroom held against the capped pools. Legacy subscription
    /// reservation USD stays readable in stored records but reserves nothing.
    pub fn reserved(&self) -> f64 {
        self.holds
            .values()
            .filter_map(|amount| match amount {
                Reserved::Metered(cost) | Reserved::Go(_, cost) => Some(*cost),
                Reserved::Subscription(_, _) => None,
            })
            .sum()
    }
    pub fn metered_reserved(&self) -> f64 {
        self.holds
            .values()
            .filter_map(|amount| match amount {
                Reserved::Metered(cost) => Some(*cost),
                _ => None,
            })
            .sum()
    }
    pub fn go_reserved(&self) -> f64 {
        self.holds
            .values()
            .filter_map(|amount| match amount {
                Reserved::Go(_, cost) => Some(*cost),
                _ => None,
            })
            .sum()
    }
    pub fn subscription_reserved(&self) -> io::Result<Counts> {
        self.holds
            .values()
            .try_fold(Counts::default(), |counts, amount| match amount {
                Reserved::Metered(_) => Ok(counts),
                Reserved::Subscription(tokens, _) | Reserved::Go(tokens, _) => counts
                    .add(Counts {
                        requests: 1,
                        tokens: *tokens,
                    })
                    .map_err(|_| io::ErrorKind::InvalidData.into()),
            })
    }
    pub fn insert(&mut self, cost: f64) -> io::Result<String> {
        self.insert_amount(Reserved::Metered(cost))
    }
    /// A subscription request reserves tokens only. Its cost is unknown, so it
    /// holds no USD headroom; paid Go requests reserve through `insert_go`.
    pub fn insert_subscription(&mut self, tokens: u64) -> io::Result<String> {
        if tokens == 0 {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        self.insert_amount(Reserved::Subscription(tokens, 0.0))
    }
    pub fn insert_go(&mut self, tokens: u64, usd: f64) -> io::Result<String> {
        if tokens == 0 || !usd.is_finite() || usd <= 0.0 {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        self.insert_amount(Reserved::Go(tokens, usd))
    }
    fn insert_amount(&mut self, amount: Reserved) -> io::Result<String> {
        if self.holds.len() >= MAX_HOLDS {
            return Err(io::ErrorKind::OutOfMemory.into());
        }
        let mut bytes = [0; 16];
        getrandom::fill(&mut bytes).map_err(|_| io::ErrorKind::Other)?;
        let id = format!("{:032x}", u128::from_be_bytes(bytes));
        if self.holds.insert(id.clone(), amount).is_some() {
            return Err(io::ErrorKind::InvalidData.into());
        }
        Ok(id)
    }
    pub fn save(&mut self) -> io::Result<()> {
        let holds: BTreeMap<_, _> = self
            .holds
            .iter()
            .map(|(id, amount)| (id, amount.to_value()))
            .collect();
        let bytes = serde_json::to_vec(&json!({"failed":self.failed,"holds":holds}))?;
        if self.fresh {
            // Ledger::open may have created several directory components.
            // Persist their names before writing a valid initial snapshot: a
            // failed sync must leave the empty file untrusted by other processes.
            for directory in self.directory.ancestors() {
                File::open(directory)?.sync_all()?;
            }
            self.fresh = false;
        }
        // Rewrite under the daily lock. An interrupted rewrite is invalid JSON,
        // so the next process fails closed instead of losing holds.
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&bytes)?;
        self.file.sync_data()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::Reserved;
    use serde_json::json;

    #[test]
    fn legacy_subscription_usd_stays_readable_and_new_subscription_holds_are_token_only() {
        for valid in [
            json!(0.01),
            json!({"subscription_tokens":120}),
            json!({"subscription_tokens":120,"reserved_usd":0.01}),
            json!({"subscription_tokens":120,"go_usd":0.25}),
        ] {
            let hold = Reserved::from_value(&valid).unwrap();
            assert_eq!(hold.to_value(), valid);
        }
        // New subscription holds never carry USD. The legacy field is
        // read-only round-trip storage, never a reservation.
        assert_eq!(
            Reserved::Subscription(120, 0.0).to_value(),
            json!({"subscription_tokens":120})
        );
        for invalid in [
            json!({"subscription_tokens":120,"reserved_usd":null}),
            json!({"subscription_tokens":120,"go_usd":0}),
            json!({"subscription_tokens":120,"go_usd":0.25,"reserved_usd":0.25}),
            json!({"subscription_tokens":120,"reserved_usd":"0.01"}),
            json!({"subscription_tokens":120,"reserved_usd":-0.01}),
            json!({"subscription_tokens":120,"reserved_usd":0}),
            json!({"subscription_tokens":0,"reserved_usd":0.01}),
            json!({"subscription_tokens":120,"reserved_usd":0.01,"extra":true}),
            json!({"subscription_tokens":120,"other":0.01}),
        ] {
            assert!(Reserved::from_value(&invalid).is_err(), "{invalid}");
        }
    }
}
