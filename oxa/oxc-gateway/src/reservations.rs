//! Short filesystem transactions shared by every process using one daily ledger.
//! A crash keeps its holds. Broken state refuses admission rather than resetting it.
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

pub(crate) struct Shared {
    _lock: Flock<File>,
    file: File,
    directory: PathBuf,
    fresh: bool,
    pub failed: bool,
    pub holds: BTreeMap<String, f64>,
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
                let cost = cost
                    .as_f64()
                    .filter(|cost| cost.is_finite() && *cost >= 0.0)
                    .ok_or(io::ErrorKind::InvalidData)?;
                if id.len() != 32 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return Err(io::ErrorKind::InvalidData.into());
                }
                total += cost;
                if !total.is_finite() {
                    return Err(io::ErrorKind::InvalidData.into());
                }
                holds.insert(id.clone(), cost);
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
        if fresh {
            shared.save()?;
        }
        Ok(shared)
    }
    pub fn reserved(&self) -> f64 {
        self.holds.values().sum()
    }
    pub fn insert(&mut self, cost: f64) -> io::Result<String> {
        if self.holds.len() >= MAX_HOLDS {
            return Err(io::ErrorKind::OutOfMemory.into());
        }
        let mut bytes = [0; 16];
        getrandom::fill(&mut bytes).map_err(|_| io::ErrorKind::Other)?;
        let id = format!("{:032x}", u128::from_be_bytes(bytes));
        if self.holds.insert(id.clone(), cost).is_some() {
            return Err(io::ErrorKind::InvalidData.into());
        }
        Ok(id)
    }
    pub fn save(&mut self) -> io::Result<()> {
        let bytes = serde_json::to_vec(&json!({"failed":self.failed,"holds":self.holds}))?;
        // Rewrite under the daily lock. An interrupted rewrite is invalid JSON,
        // so the next process fails closed instead of losing holds.
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&bytes)?;
        self.file.sync_data()?;
        if self.fresh {
            File::open(&self.directory)?.sync_all()?;
            self.fresh = false;
        }
        Ok(())
    }
}
