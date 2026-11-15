use serde_json::{json, Value};
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::time::Instant;

/// A bounded private journal with exclusive controller ownership.
///
/// Only explicitly supplied metadata is written. The trusted caller must pass
/// hashes/counts and bounded labels, not raw prompts, outputs or credentials.
/// A failed append permanently closes admission but retains the ownership lease
/// until the writer is dropped. Storage errors can leave an incomplete trailing
/// record; readers must not treat it as committed or replayable. This is audit
/// storage, not a replay protocol.
pub struct Journal {
    file: crate::ownership::Lease,
    sequence: u64,
    bytes: usize,
    limit: usize,
    started: Instant,
    failed: bool,
}

impl Journal {
    pub fn create(path: &Path, limit: usize) -> io::Result<Self> {
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("journal needs a parent directory"))?;
        let namespace = crate::namespace::protected_directory(parent)?;
        let metadata = namespace.metadata()?;
        // SAFETY: geteuid has no arguments or memory access side effects.
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "journal parent must be private and operator-owned",
            ));
        }
        if limit == 0 || limit > 16 * 1024 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "journal limit must be 1..=16 MiB",
            ));
        }
        // Reopen the validated directory inode with a sync-capable descriptor.
        // O_PATH is suitable for namespace validation but rejects fsync. Create
        // through this pinned parent too, so publication and sync name the same
        // directory even if a trusted same-UID caller renames it concurrently.
        let parent = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open(format!("/proc/self/fd/{}", namespace.as_raw_fd()))?;
        let name = path
            .file_name()
            .ok_or_else(|| io::Error::other("journal needs a file name"))?;
        let target = Path::new("/proc/self/fd")
            .join(parent.as_raw_fd().to_string())
            .join(name);
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(target)?;
        let file = crate::ownership::Lease::acquire(file)?;
        // A successful record must not outlive its directory entry after a
        // crash. Persist creation before returning a usable journal; propagate
        // sync errors without acknowledging durable storage.
        parent.sync_all()?;
        Ok(Self {
            file,
            sequence: 0,
            bytes: 0,
            limit,
            started: Instant::now(),
            failed: false,
        })
    }

    pub fn record(&mut self, worker: Option<&str>, kind: &str, details: Value) -> io::Result<()> {
        if self.failed {
            return Err(io::Error::other("orchestrator journal unavailable"));
        }
        let result = self.append(worker, kind, details);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn append(&mut self, worker: Option<&str>, kind: &str, mut details: Value) -> io::Result<()> {
        if kind == "run_started" {
            details
                .as_object_mut()
                .ok_or_else(|| io::Error::other("invalid run start details"))?
                .insert(
                    "ownership".into(),
                    Value::String(crate::ownership::PROTOCOL.into()),
                );
        }
        let value = json!({"schema":1,"sequence":self.sequence,"elapsedMs":self.started.elapsed().as_millis(),"worker":worker,"event":kind,"details":details});
        // Bound the serialization buffer too, rather than allocating an entire
        // oversized record before discovering it cannot be admitted.
        struct Bounded {
            bytes: Vec<u8>,
            limit: usize,
        }
        impl Write for Bounded {
            fn write(&mut self, data: &[u8]) -> io::Result<usize> {
                if data.len() > self.limit.saturating_sub(self.bytes.len()) {
                    return Err(io::Error::other("orchestrator journal limit exceeded"));
                }
                self.bytes.extend_from_slice(data);
                Ok(data.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut buffer = Bounded {
            bytes: Vec::new(),
            limit: self.limit.saturating_sub(self.bytes).saturating_sub(1),
        };
        serde_json::to_writer(&mut buffer, &value)?;
        let mut line = buffer.bytes;
        line.push(b'\n');
        if line.len() > self.limit.saturating_sub(self.bytes) {
            return Err(io::Error::other("orchestrator journal limit exceeded"));
        }
        self.file.write_all(&line)?;
        self.file.flush()?;
        self.file.sync_data()?;
        self.bytes += line.len();
        self.sequence += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxa_fake::TestEnv;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    fn private(env: &TestEnv) -> std::path::PathBuf {
        let directory = env.root().join("private");
        std::fs::DirBuilder::new().mode(0o700).create(&directory).unwrap();
        directory
    }

    #[test]
    fn explicit_records_are_private_ordered_and_bounded() {
        let env = TestEnv::new();
        let path = private(&env).join("audit.jsonl");
        let mut journal = Journal::create(&path, 512).unwrap();
        journal
            .record(None, "run_started", json!({"run":"example","workers":1}))
            .unwrap();
        journal
            .record(
                Some("worker-1"),
                "worker_finished",
                json!({"outcome":"completed","tokens":12}),
            )
            .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let records: Vec<Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["schema"], 1);
        assert_eq!(records[0]["sequence"], 0);
        assert_eq!(records[0]["details"]["ownership"], crate::ownership::PROTOCOL);
        assert_eq!(records[1]["sequence"], 1);
        assert_eq!(records[1]["worker"], "worker-1");
        assert_eq!(records[1]["details"]["tokens"], 12);
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert!(journal
            .record(None, "oversize", json!({"label":"x".repeat(1024)}))
            .is_err());
        assert!(journal.record(None, "small", json!({})).is_err());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            text,
            "refused records must not append bytes"
        );
    }

    #[test]
    fn refuses_existing_names_and_links_without_replacing_data() {
        let env = TestEnv::new();
        let directory = private(&env);
        let path = directory.join("existing");
        std::fs::write(&path, "keep").unwrap();
        assert!(Journal::create(&path, 512).is_err());
        let link = directory.join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(Journal::create(&link, 512).is_err());
        let alias = env.root().join("alias");
        std::os::unix::fs::symlink(&directory, &alias).unwrap();
        assert!(Journal::create(&alias.join("new"), 512).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "keep");
        assert!(!directory.join("new").exists());
    }

    #[test]
    fn private_parent_and_valid_limit_are_required_before_creation() {
        let env = TestEnv::new();
        let directory = private(&env);
        let path = directory.join("audit");
        for limit in [0, 16 * 1024 * 1024 + 1] {
            assert_eq!(
                Journal::create(&path, limit).err().unwrap().kind(),
                io::ErrorKind::InvalidInput
            );
            assert!(!path.exists());
        }
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            Journal::create(&path, 512).err().unwrap().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(!path.exists());
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(Journal::create(&path, 512).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn malformed_start_closes_admission_without_a_partial_record() {
        let env = TestEnv::new();
        let path = private(&env).join("audit");
        let mut journal = Journal::create(&path, 512).unwrap();
        assert!(journal.record(None, "run_started", json!([])).is_err());
        assert!(journal.record(None, "run_started", json!({})).is_err());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
    }

    #[test]
    fn failed_audit_keeps_ownership_until_the_writer_is_dropped() {
        let env = TestEnv::new();
        let path = private(&env).join("transcript.jsonl");
        let mut journal = Journal::create(&path, 512).unwrap();
        use std::os::fd::AsRawFd;
        // SAFETY: querying flags on this test's live journal descriptor.
        let flags = unsafe { libc::fcntl(journal.file.as_raw_fd(), libc::F_GETFD) };
        assert!(flags >= 0 && flags & libc::FD_CLOEXEC != 0);
        journal
            .record(None, "run_started", json!({"run":"failure","workers":1}))
            .unwrap();
        assert!(journal
            .record(None, "control_rejected", json!({"synthetic":"x".repeat(1024)}))
            .is_err());
        assert_eq!(
            crate::ownership::Lease::acquire(std::fs::File::open(&path).unwrap())
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        drop(journal);
        let _exclusive = crate::ownership::Lease::acquire(std::fs::File::open(&path).unwrap()).unwrap();
    }

    #[test]
    fn partial_storage_failure_poisoning_retains_lease_and_committed_prefix() {
        const CHILD: &str = "OXA_AUDIT_PARTIAL_IO_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "journal::tests::partial_storage_failure_poisoning_retains_lease_and_committed_prefix",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(output.status.success(), "child output: {output:?}");
            return;
        }
        // A separate owned test process keeps RLIMIT_FSIZE and SIGXFSZ changes
        // away from concurrent tests. Exercise a real short write then EFBIG.
        let env = TestEnv::new();
        let path = private(&env).join("partial.jsonl");
        let mut journal = Journal::create(&path, 4096).unwrap();
        journal.record(None, "committed", json!({})).unwrap();
        let prefix = std::fs::read(&path).unwrap();
        let bound = prefix.len() as libc::rlim_t + 8;
        let limits = libc::rlimit {
            rlim_cur: bound,
            rlim_max: bound,
        };
        // SAFETY: changes are confined to this disposable child test process.
        unsafe {
            assert_ne!(libc::signal(libc::SIGXFSZ, libc::SIG_IGN), libc::SIG_ERR);
            assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &limits), 0);
        }
        assert!(journal.record(None, "incomplete", json!({"count":12})).is_err());
        let partial = std::fs::read(&path).unwrap();
        assert_eq!(partial.len(), prefix.len() + 8);
        assert!(partial.starts_with(&prefix));
        assert_ne!(partial.last(), Some(&b'\n'));
        assert_eq!(journal.sequence, 1);
        assert_eq!(journal.bytes, prefix.len());
        assert!(journal.record(None, "must_not_retry", json!({})).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), partial);
        assert_eq!(
            crate::ownership::Lease::acquire(std::fs::File::open(&path).unwrap())
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        drop(journal);
        assert!(crate::ownership::Lease::acquire(std::fs::File::open(&path).unwrap()).is_ok());
    }
}
