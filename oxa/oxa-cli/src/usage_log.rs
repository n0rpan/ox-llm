//! `--usage-log FILE`: one JSONL record per model call. Records hold numbers, ids and timings
//! only; never message text, tool output or request bodies.
use oxa_proto::OxaError;
use serde_json::Value;
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Mutex;

#[derive(Debug)]
pub(crate) struct UsageLog {
    file: Mutex<File>,
}

impl UsageLog {
    /// Install a fresh owner-only log, keeping a handle to the file that was installed.
    pub(crate) fn create(path: &Path) -> Result<Self, OxaError> {
        let write_error = |e: std::io::Error| OxaError::io(&format!("cannot write {}", path.display()), &e);
        match fs::symlink_metadata(path) {
            Ok(info) if !info.is_file() => {
                return Err(write_error(io::Error::other("usage log must be a regular file")));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(write_error(error)),
        }
        let file = crate::status::atomic_file(path, b"").map_err(write_error)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))
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
    use oxa_fake::TestEnv;

    #[test]
    fn usage_log_rejects_links_without_touching_the_target() {
        let env = TestEnv::new();
        let target = env.write("work/keep.txt", "keep this");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
        let link = env.work().join("usage.jsonl");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert!(UsageLog::create(&link).is_err());
        assert!(fs::symlink_metadata(&link).unwrap().is_symlink());
        assert_eq!(fs::read_to_string(&target).unwrap(), "keep this");
        assert_eq!(fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o644);
    }

    #[test]
    fn usage_log_replacement_preserves_hard_linked_files() {
        let env = TestEnv::new();
        let original = env.write("work/keep.txt", "keep this");
        let log = env.work().join("usage.jsonl");
        fs::hard_link(&original, &log).unwrap();

        UsageLog::create(&log)
            .unwrap()
            .write(&serde_json::json!({"request": 1}))
            .unwrap();
        assert_eq!(fs::read_to_string(&original).unwrap(), "keep this");
        assert_eq!(fs::read_to_string(&log).unwrap(), "{\"request\":1}\n");
    }

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
