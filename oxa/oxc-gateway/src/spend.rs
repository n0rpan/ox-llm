//! Incremental daily spend. An unfinished final line is not a committed row.
//! Complete malformed rows fail closed. Replacement/truncation resets the cache.
use crate::billing::Counts;
use bigdecimal::BigDecimal;
use num_traits::ToPrimitive;
use serde_json::Value;
use std::{
    fs::OpenOptions,
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    str::FromStr,
};

#[derive(Default, Debug)]
pub struct SpendReader {
    path: PathBuf,
    identity: Option<(u64, u64)>,
    offset: u64,
    total: BigDecimal,
    subscription: Counts,
    tail: Vec<u8>,
    bytes_read: u64,
}
/// Budget accounting accepts an estimate only when it is explicitly classified
/// and no actual cost field is present. Older settled rows keep their format.
pub(crate) fn budget_cost(row: &Value) -> Option<f64> {
    let value = match (row.get("cost_usd"), row.get("estimated_cost_usd")) {
        (Some(cost), None) => cost,
        (None, Some(estimate))
            if matches!(row["settled"].as_str(), Some("estimated" | "reserved"))
                && row["billing"] == "metered"
                && matches!(
                    row["remote_provider"].as_str(),
                    Some("anthropic" | "openai" | "openai-chat" | "openrouter")
                ) =>
        {
            estimate
        }
        _ => return None,
    };
    value.as_f64().filter(|cost| cost.is_finite() && *cost >= 0.0)
}

impl SpendReader {
    pub fn bytes_read(&self) -> u64 {
        self.bytes_read
    }
    pub(crate) fn subscription(&self) -> Counts {
        self.subscription
    }
    pub fn read(&mut self, path: &Path) -> io::Result<f64> {
        let mut file = match OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NONBLOCK)
            .open(path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.identity = None;
                self.offset = 0;
                self.total = BigDecimal::default();
                self.subscription = Counts::default();
                self.tail.clear();
                self.path = path.into();
                return Ok(0.0);
            }
            Err(error) => return Err(error),
        };
        let info = file.metadata()?;
        if !info.is_file() {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let identity = (info.dev(), info.ino());
        if self.path != path || self.identity != Some(identity) || info.len() < self.offset {
            self.path = path.into();
            self.identity = Some(identity);
            self.offset = 0;
            self.total = BigDecimal::default();
            self.subscription = Counts::default();
            self.tail.clear();
        } else if self.offset > 0 {
            // A same-inode rewrite can be at least as long as the cached file.
            // Verify a bounded tail, like the reference, before reusing totals.
            file.seek(SeekFrom::Start(self.offset - self.tail.len() as u64))?;
            let mut tail = vec![0; self.tail.len()];
            file.read_exact(&mut tail)?;
            self.bytes_read += tail.len() as u64;
            if tail != self.tail {
                self.offset = 0;
                self.total = BigDecimal::default();
                self.subscription = Counts::default();
                self.tail.clear();
            }
        }
        let mut reader = BufReader::new(file);
        reader.seek(SeekFrom::Start(self.offset))?;
        let mut offset = self.offset;
        let mut total = self.total.clone();
        let mut subscription = self.subscription;
        let mut tail = self.tail.clone();
        let mut line = Vec::new();
        loop {
            line.clear();
            let count = reader.read_until(b'\n', &mut line)?;
            self.bytes_read += count as u64;
            if count == 0 || !line.ends_with(b"\n") {
                break;
            }
            offset += count as u64;
            tail.extend_from_slice(&line);
            if tail.len() > 64 {
                tail.drain(..tail.len() - 64);
            }
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let row: Value =
                serde_json::from_slice(&line).map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
            if !row.is_object() {
                return Err(io::ErrorKind::InvalidData.into());
            }
            if row["remote"] != true {
                continue;
            }
            let cost = budget_cost(&row).ok_or(io::ErrorKind::InvalidData)?;
            match row.get("billing") {
                None => {
                    if matches!(row["remote_provider"].as_str(), Some("chatgpt" | "opencode"))
                        || row.get("subscription_requests").is_some()
                        || row.get("subscription_tokens").is_some()
                    {
                        return Err(io::ErrorKind::InvalidData.into());
                    }
                }
                Some(value) if value == "metered" && row["remote_provider"] != "opencode" => {
                    if matches!(row["remote_provider"].as_str(), Some("chatgpt" | "opencode"))
                        || row.get("subscription_requests").is_some()
                        || row.get("subscription_tokens").is_some()
                    {
                        return Err(io::ErrorKind::InvalidData.into());
                    }
                }
                Some(value)
                    if value == "subscription"
                        || value == "metered" && row["remote_provider"] == "opencode" =>
                {
                    if (value == "subscription" && cost != 0.0)
                        || (value == "metered" && cost <= 0.0)
                        || !matches!(row["remote_provider"].as_str(), Some("chatgpt" | "opencode"))
                    {
                        return Err(io::ErrorKind::InvalidData.into());
                    }
                    let requests = row["subscription_requests"]
                        .as_u64()
                        .filter(|n| *n <= 1)
                        .ok_or(io::ErrorKind::InvalidData)?;
                    let tokens = row["subscription_tokens"]
                        .as_u64()
                        .ok_or(io::ErrorKind::InvalidData)?;
                    let not_forwarded = row["settled"] == "not_forwarded";
                    let settled = row["settled"]
                        .as_str()
                        .filter(|v| ["usage", "reserved", "not_forwarded", "refused"].contains(v))
                        .ok_or(io::ErrorKind::InvalidData)?;
                    if (requests == 0) != not_forwarded
                        || (matches!(settled, "not_forwarded" | "refused") && tokens != 0)
                    {
                        return Err(io::ErrorKind::InvalidData.into());
                    }
                    subscription = subscription
                        .add(Counts { requests, tokens })
                        .map_err(|_| io::ErrorKind::InvalidData)?;
                }
                _ => return Err(io::ErrorKind::InvalidData.into()),
            }
            // Sum the validated finite ledger amounts as decimals, as pricing
            // does. Fixed currency rounding loses small paid charges, while
            // repeated binary addition can invent spend above an exact cap.
            total += BigDecimal::from_str(&cost.to_string())
                .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
            if total.to_f64().is_none_or(|value| !value.is_finite()) {
                return Err(io::ErrorKind::InvalidData.into());
            }
        }
        let spend = total
            .to_f64()
            .filter(|value| value.is_finite())
            .ok_or(io::ErrorKind::InvalidData)?;
        // Commit the cursor/total together only after every complete row passed.
        self.offset = offset;
        self.total = total;
        self.subscription = subscription;
        self.tail = tail;
        // Admission uses this amount directly. Currency display rounding must
        // never erase a valid positive charge before comparing it with a cap.
        Ok(spend)
    }
}

/// Serialize complete private rows across processes. Durable append must precede
/// releasing a persisted reservation. Readers tolerate an unfinished last write.
pub fn append_private(path: &Path, row: &Value) -> io::Result<()> {
    use std::{
        fs::{self, OpenOptions},
        io::Write,
        os::unix::fs::PermissionsExt,
    };
    let parent = path.parent().ok_or(io::ErrorKind::InvalidInput)?;
    fs::create_dir_all(parent)?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    let file = OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC | nix::libc::O_NONBLOCK)
        .open(path)?;
    let info = file.metadata()?;
    if !info.is_file() || info.uid() != nix::unistd::Uid::current().as_raw() {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    let mut file = nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusive)
        .map_err(|(_, error)| io::Error::from(error))?;
    // A failed/dead writer may have left an unfinished row. Preserve those
    // bytes, but keep this healthy settlement on its own parseable line.
    if file.metadata()?.len() > 0 {
        file.seek(SeekFrom::End(-1))?;
        let mut last = [0];
        file.read_exact(&mut last)?;
        if last != *b"\n" {
            file.write_all(b"\n")?;
        }
    }
    let mut bytes = serde_json::to_vec(row)?;
    bytes.push(b'\n');
    file.write_all(&bytes)?;
    file.sync_data()?;
    // The first row's name must survive as well as its contents before the
    // accounting transaction can durably remove the corresponding hold.
    fs::File::open(parent)?.sync_all()
}
