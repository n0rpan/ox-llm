//! `--usage-log FILE`: one JSONL record per model call. Records hold numbers, ids and timings
//! only; never message text, tool output or request bodies.
use oxa_proto::OxaError;
use serde_json::Value;
use std::fs::File;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::Mutex;

#[derive(Debug)]
pub(crate) struct UsageLog {
    file: Mutex<File>,
}

impl UsageLog {
    /// Creates or truncates the file for this run, owner-only (0600) even when it already existed.
    pub(crate) fn create(path: &Path) -> Result<Self, OxaError> {
        let write_error = |e: std::io::Error| OxaError::io(&format!("cannot write {}", path.display()), &e);
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .map_err(write_error)?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(write_error)?;
        Ok(Self {
            file: Mutex::new(file),
        })
    }

    /// Appends one record as one line. The caller records a failed write; it never panics.
    pub(crate) fn write(&self, record: &Value) -> std::io::Result<()> {
        let mut line = record.to_string();
        line.push('\n');
        let mut file = self.file.lock().unwrap_or_else(|p| p.into_inner());
        file.write_all(line.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_log_file_is_0600() {
        let dir = std::env::temp_dir().join(format!("oxa-ul-mode-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("usage.jsonl");
        // An existing, wider file is narrowed too: the mode is a property of the run's log.
        std::fs::write(&path, "old\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let log = UsageLog::create(&path).unwrap();
        log.write(&serde_json::json!({"request": 1})).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        // Truncated at create: the old line is gone, only the new record remains.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"request\":1}\n");
        let _ = std::fs::remove_dir_all(dir);
    }
}
