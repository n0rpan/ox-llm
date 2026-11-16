//! Disk-only discovery metadata; no credentials, transport, refresh worker or catalog policy.
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

/// Both reads and writes are bounded; an oversized snapshot never replaces a good one.
pub const MAX_FILE_BYTES: usize = 1024 * 1024;
const MAGIC: &[u8] = b"OXDC\x01";
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

/// Only public discovery fields belong here, never raw provider responses or headers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Model {
    pub id: String,
    pub display_name: String,
    pub context_window: Option<u32>,
    pub max_output: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub models: Vec<Model>,
    /// Expired entries are returned only when the caller explicitly selects offline mode.
    pub stale: bool,
}

/// The clock returns time since the Unix epoch. The caller owns refresh scheduling.
pub struct DiscoveryCache<C> {
    path: PathBuf,
    ttl: Duration,
    clock: C,
}

impl<C: Fn() -> Duration> DiscoveryCache<C> {
    /// `directory` must be an application-owned cache directory, not a secrets directory.
    pub fn new(directory: &Path, provider: &str, ttl: Duration, clock: C) -> io::Result<Self> {
        if provider.is_empty()
            || provider.len() > 64
            || !provider
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid cache provider",
            ));
        }
        Ok(Self {
            path: directory.join(format!("{provider}.discovery")),
            ttl,
            clock,
        })
    }

    /// Missing, unreadable, malformed, future-dated and oversized files are cache misses.
    /// At age == TTL an entry expires. Offline callers may still use expired entries.
    pub fn read(&self, offline: bool) -> Option<Snapshot> {
        let file = File::open(&self.path).ok()?;
        let metadata = file.metadata().ok()?;
        if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES as u64 {
            return None;
        }
        let mut bytes = Vec::new();
        file.take(MAX_FILE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        if bytes.len() > MAX_FILE_BYTES {
            return None;
        }
        let mut decoder = Decoder(&bytes);
        if decoder.take(MAGIC.len())? != MAGIC {
            return None;
        }
        let seconds = u64::from_le_bytes(decoder.take(8)?.try_into().ok()?);
        let nanos = decoder.number()?;
        if nanos >= 1_000_000_000 {
            return None;
        }
        let age = (self.clock)().checked_sub(Duration::new(seconds, nanos))?;
        let stale = age >= self.ttl;
        if stale && !offline {
            return None;
        }
        let count = decoder.number()? as usize;
        // Each record has at least two string lengths and two option tags.
        if count > decoder.0.len() / 10 {
            return None;
        }
        let mut models = Vec::new();
        for _ in 0..count {
            models.push(Model {
                id: decoder.string()?,
                display_name: decoder.string()?,
                context_window: decoder.optional_number()?,
                max_output: decoder.optional_number()?,
            });
        }
        if !decoder.0.is_empty() {
            return None;
        }
        Some(Snapshot { models, stale })
    }

    /// Atomically publishes one complete provider snapshot, last rename wins.
    /// No key argument or raw response payload is accepted. Existing snapshots survive
    /// encoding/write failures. Parent directories are private when newly created;
    /// existing directories must already be application-owned and trusted.
    pub fn write(&self, models: &[Model]) -> io::Result<()> {
        let invalid = || io::Error::new(io::ErrorKind::InvalidInput, "discovery snapshot too large");
        let now = (self.clock)();
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&now.as_secs().to_le_bytes());
        bytes.extend_from_slice(&now.subsec_nanos().to_le_bytes());
        bytes.extend_from_slice(&u32::try_from(models.len()).map_err(|_| invalid())?.to_le_bytes());
        for model in models {
            for string in [&model.id, &model.display_name] {
                if string.len() > MAX_FILE_BYTES || bytes.len() + 4 + string.len() > MAX_FILE_BYTES {
                    return Err(invalid());
                }
                bytes.extend_from_slice(&(string.len() as u32).to_le_bytes());
                bytes.extend_from_slice(string.as_bytes());
            }
            for value in [model.context_window, model.max_output] {
                bytes.push(u8::from(value.is_some()));
                if let Some(value) = value {
                    bytes.extend_from_slice(&value.to_le_bytes());
                }
            }
            if bytes.len() > MAX_FILE_BYTES {
                return Err(invalid());
            }
        }
        let parent = self.path.parent().expect("cache path has a directory");
        fs::DirBuilder::new().recursive(true).mode(0o700).create(parent)?;
        let (temporary, mut file) = loop {
            let temporary = parent.join(format!(
                ".discovery-{}-{}.tmp",
                std::process::id(),
                NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
            ));
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)
            {
                Ok(file) => break (Temporary(temporary), file),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        };
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temporary.0, &self.path)?;
        File::open(parent)?.sync_all()
    }
}

struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

// Versioned length-delimited encoding deliberately has no generic JSON/headers field.
struct Decoder<'a>(&'a [u8]);
impl<'a> Decoder<'a> {
    fn take(&mut self, count: usize) -> Option<&'a [u8]> {
        if count > self.0.len() {
            return None;
        }
        let (value, rest) = self.0.split_at(count);
        self.0 = rest;
        Some(value)
    }
    fn number(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn string(&mut self) -> Option<String> {
        let len = self.number()? as usize;
        Some(std::str::from_utf8(self.take(len)?).ok()?.to_owned())
    }
    fn optional_number(&mut self) -> Option<Option<u32>> {
        match self.take(1)?[0] {
            0 => Some(None),
            1 => Some(Some(self.number()?)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        sync::{
            atomic::{AtomicU64, Ordering},
            Arc, Barrier,
        },
        thread,
    };

    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "discovery-cache-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn models(id: &str) -> Vec<Model> {
        vec![Model {
            id: id.into(),
            display_name: "Synthetic model".into(),
            context_window: Some(4096),
            max_output: None,
        }]
    }

    #[test]
    fn ttl_edges_and_offline_stale() {
        let dir = Directory::new();
        let now = AtomicU64::new(100);
        let cache = DiscoveryCache::new(&dir.0, "synthetic", Duration::from_secs(10), || {
            Duration::from_secs(now.load(Ordering::Relaxed))
        })
        .unwrap();
        assert_eq!(cache.read(false), None);
        cache.write(&models("one")).unwrap();
        assert_eq!(
            cache.read(false),
            Some(Snapshot {
                models: models("one"),
                stale: false
            })
        );
        now.store(109, Ordering::Relaxed);
        assert!(!cache.read(false).unwrap().stale);
        now.store(110, Ordering::Relaxed);
        assert_eq!(cache.read(false), None);
        assert!(cache.read(true).unwrap().stale);
        now.store(111, Ordering::Relaxed);
        assert!(cache.read(true).unwrap().stale);
        now.store(99, Ordering::Relaxed);
        assert_eq!(cache.read(true), None, "future-dated data is untrusted");
    }

    #[test]
    fn zero_ttl_is_immediately_stale() {
        let dir = Directory::new();
        let cache =
            DiscoveryCache::new(&dir.0, "synthetic", Duration::ZERO, || Duration::from_secs(1)).unwrap();
        cache.write(&[]).unwrap();
        assert_eq!(cache.read(false), None);
        assert_eq!(
            cache.read(true),
            Some(Snapshot {
                models: vec![],
                stale: true
            })
        );
    }

    #[test]
    fn corrupt_and_oversized_are_empty() {
        let dir = Directory::new();
        let cache = DiscoveryCache::new(&dir.0, "synthetic", Duration::from_secs(10), || {
            Duration::from_secs(1)
        })
        .unwrap();
        cache.write(&models("one")).unwrap();
        let valid = fs::read(&cache.path).unwrap();
        for end in 0..valid.len() {
            fs::write(&cache.path, &valid[..end]).unwrap();
            assert_eq!(cache.read(true), None);
        }
        for invalid in [
            b"not a cache".to_vec(),
            vec![0; 1024 * 1024 + 1],
            [valid.clone(), vec![0]].concat(),
        ] {
            fs::write(&cache.path, invalid).unwrap();
            assert_eq!(cache.read(true), None);
        }
        cache.write(&models("recovered")).unwrap();
        assert_eq!(cache.read(false).unwrap().models, models("recovered"));
    }

    #[test]
    fn provider_isolation_permissions_and_atomic_replace() {
        let dir = Directory::new();
        let cache = DiscoveryCache::new(&dir.0, "first", Duration::from_secs(10), || {
            Duration::from_secs(1)
        })
        .unwrap();
        let other = DiscoveryCache::new(&dir.0, "second", Duration::from_secs(10), || {
            Duration::from_secs(1)
        })
        .unwrap();
        cache.write(&models("one")).unwrap();
        let old = fs::File::open(&cache.path).unwrap();
        other.write(&models("two")).unwrap();
        fs::set_permissions(&cache.path, fs::Permissions::from_mode(0o644)).unwrap();
        cache.write(&models("new")).unwrap();
        assert_eq!(
            fs::metadata(&cache.path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(other.read(false).unwrap().models, models("two"));
        assert_eq!(cache.read(false).unwrap().models, models("new"));
        use std::io::Read;
        let mut old_bytes = Vec::new();
        (&old).read_to_end(&mut old_bytes).unwrap();
        assert_ne!(old_bytes, fs::read(&cache.path).unwrap());
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 2);
        for invalid in ["", "../escape", "/absolute", "a/b", ".", "UPPER"] {
            assert!(DiscoveryCache::new(&dir.0, invalid, Duration::ZERO, || Duration::ZERO).is_err());
        }
    }

    #[test]
    fn nanosecond_ttl_and_rejected_write_preserve_snapshot() {
        let dir = Directory::new();
        let now = AtomicU64::new(1);
        let cache = DiscoveryCache::new(&dir.0, "synthetic", Duration::from_nanos(2), || {
            Duration::from_nanos(now.load(Ordering::Relaxed))
        })
        .unwrap();
        cache.write(&models("one")).unwrap();
        now.store(2, Ordering::Relaxed);
        assert!(!cache.read(false).unwrap().stale);
        let mut oversized = models("too-large");
        oversized[0].display_name = "x".repeat(MAX_FILE_BYTES);
        assert_eq!(
            cache.write(&oversized).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(cache.read(false).unwrap().models, models("one"));
        now.store(3, Ordering::Relaxed);
        assert!(cache.read(false).is_none());
        assert!(cache.read(true).unwrap().stale);
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 1);
    }

    #[test]
    fn concurrent_writers_never_expose_partial_files() {
        let dir = Directory::new();
        let cache = Arc::new(
            DiscoveryCache::new(&dir.0, "synthetic", Duration::from_secs(10), || {
                Duration::from_secs(1)
            })
            .unwrap(),
        );
        cache.write(&models("initial")).unwrap();
        let barrier = Arc::new(Barrier::new(5));
        thread::scope(|scope| {
            for n in 0..4 {
                let cache = Arc::clone(&cache);
                let barrier = Arc::clone(&barrier);
                scope.spawn(move || {
                    barrier.wait();
                    for _ in 0..40 {
                        cache.write(&models(&format!("writer-{n}"))).unwrap();
                    }
                });
            }
            barrier.wait();
            for _ in 0..200 {
                let snapshot = cache.read(false).expect("a complete snapshot always exists");
                assert_eq!(snapshot.models.len(), 1);
                assert!(!snapshot.stale);
            }
        });
        assert!(cache.read(false).unwrap().models[0].id.starts_with("writer-"));
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 1);
    }
}
