use serde_json::{json, Value};
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::Instant;

/// A bounded private audit journal. It never receives raw prompts, events,
/// model output, environment values or transport error text.
pub(crate) struct Journal {
    file: File,
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

    fn append(&mut self, worker: Option<&str>, kind: &str, details: Value) -> io::Result<()> {
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
