use serde_json::{json, Value};
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::Instant;

/// A bounded private audit journal. It never receives raw prompts, events,
/// model output, environment values or transport error text.
pub(crate) struct Journal {
    file: crate::ownership::Lease,
    sequence: u64,
    bytes: usize,
    limit: usize,
    started: Instant,
    failed: bool,
}

impl Journal {
    pub(crate) fn create(path: &Path, limit: usize) -> io::Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        let file = crate::ownership::Lease::acquire(file)?;
        Ok(Self {
            file,
            sequence: 0,
            bytes: 0,
            limit,
            started: Instant::now(),
            failed: false,
        })
    }

    pub(crate) fn record(&mut self, worker: Option<&str>, kind: &str, details: Value) -> io::Result<()> {
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
        let mut line = serde_json::to_vec(&value)?;
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
    use crate::recovery::ExclusiveRun;
    use oxa_fake::TestEnv;

    #[test]
    fn failed_audit_keeps_ownership_until_the_writer_is_dropped() {
        let env = TestEnv::new();
        let path = env.root().join("transcript.jsonl");
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
            ExclusiveRun::claim(&path).err().unwrap().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(journal);
        let exclusive = ExclusiveRun::claim(&path).unwrap();
        assert!(!exclusive.inspected().snapshot().recorded_run_finished);
        assert_eq!(exclusive.inspected().snapshot().unlisted_workers, 1);
    }
}
