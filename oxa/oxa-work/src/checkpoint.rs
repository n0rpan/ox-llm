//! One repository-wide revision source and a bounded pending checkpoint.
//! A later writer must recover the prior durable event before advancing the source.
use crate::{state, CheckpointObserver, Record, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const COUNTER: &str = ".checkpoint-counter";
const MARKER: &str = ".checkpoint-initialized";

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    version: u64,
    revision: u64,
    pending: Option<String>,
}

pub(crate) struct Repository {
    root: PathBuf,
    _lock: File,
    cursor: Cursor,
}
impl Repository {
    pub(crate) fn open(root: &Path, observer: &dyn CheckpointObserver) -> Result<Self> {
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(root.join(".checkpoint-lock"))
            .map_err(|e| e.to_string())?;
        lock.lock().map_err(|e| e.to_string())?;
        let mut bytes = Vec::new();
        let cursor = match File::open(root.join(COUNTER)) {
            Ok(file) => {
                file.take(4097)
                    .read_to_end(&mut bytes)
                    .map_err(|e| e.to_string())?;
                if bytes.len() > 4096 {
                    return Err("checkpoint counter exceeds capacity".into());
                }
                let value: serde_json::Value =
                    serde_json::from_slice(&bytes).map_err(|e| format!("checkpoint counter: {e}"))?;
                let object = value.as_object().ok_or("invalid checkpoint counter")?;
                if object.len() != 3
                    || ["version", "revision", "pending"]
                        .iter()
                        .any(|key| !object.contains_key(*key))
                {
                    return Err("incomplete or unknown checkpoint counter fields".into());
                }
                let cursor: Cursor =
                    serde_json::from_value(value).map_err(|e| format!("checkpoint counter: {e}"))?;
                if cursor.version != 1
                    || (cursor.pending.is_some() && cursor.revision == 0)
                    || cursor
                        .pending
                        .as_deref()
                        .is_some_and(|id| !crate::valid_task_id(id))
                {
                    return Err("invalid checkpoint counter".into());
                }
                cursor
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if root.join(MARKER).exists() || has_durable_state(root)? {
                    return Err("checkpoint counter is missing; repository recovery is required".into());
                }
                let cursor = Cursor {
                    version: 1,
                    revision: 0,
                    pending: None,
                };
                write_atomic(
                    &root.join(COUNTER),
                    &serde_json::to_vec(&cursor).map_err(|e| e.to_string())?,
                    false,
                )?;
                cursor
            }
            Err(error) => return Err(error.to_string()),
        };
        if !root.join(MARKER).exists() {
            write_atomic(&root.join(MARKER), b"1\n", false)?;
        }
        let mut marker = Vec::new();
        File::open(root.join(MARKER))
            .map_err(|e| e.to_string())?
            .take(3)
            .read_to_end(&mut marker)
            .map_err(|e| e.to_string())?;
        if marker != b"1\n" {
            return Err("invalid checkpoint initialization marker".into());
        }
        let mut repository = Self {
            root: root.to_path_buf(),
            _lock: lock,
            cursor,
        };
        repository.recover(observer)?;
        Ok(repository)
    }

    fn store(&self) -> Result<()> {
        write_atomic(
            &self.root.join(COUNTER),
            &serde_json::to_vec(&self.cursor).map_err(|e| e.to_string())?,
            true,
        )
    }

    fn recover(&mut self, observer: &dyn CheckpointObserver) -> Result<()> {
        let Some(id) = &self.cursor.pending else {
            return Ok(());
        };
        let path = self.root.join(format!("{id}.json"));
        let record = match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.to_string()),
            Ok(metadata) if !metadata.is_file() => {
                return Err("pending checkpoint is not a regular file".into())
            }
            Ok(_) => match state::load(&path)? {
                state::Stored::Current(state) => Some(state.record),
                state::Stored::Legacy(_) => None,
            },
        };
        if let Some(record) = record {
            if record.id != *id || record.checkpoint_revision > self.cursor.revision {
                return Err("pending checkpoint identity disagrees with the counter".into());
            }
            if record.checkpoint_revision == self.cursor.revision {
                // Complete any failed directory sync before emitting the event.
                File::open(&path)
                    .and_then(|file| file.sync_all())
                    .map_err(|e| e.to_string())?;
                File::open(&self.root)
                    .and_then(|directory| directory.sync_all())
                    .map_err(|e| e.to_string())?;
                observer
                    .checkpoint(&record)
                    .map_err(|e| format!("checkpoint event replay: {e}"))?;
            }
        }
        self.acknowledge()
    }

    pub(crate) fn reserve(&mut self, record: &Record) -> Result<u64> {
        if self.cursor.pending.is_some() || !crate::valid_task_id(&record.id) {
            return Err("checkpoint source is not ready".into());
        }
        let revision = self
            .cursor
            .revision
            .checked_add(1)
            .ok_or("task checkpoint revision exhausted")?;
        self.cursor.revision = revision;
        self.cursor.pending = Some(record.id.clone());
        self.store()?;
        Ok(revision)
    }

    pub(crate) fn verify(&self, record: &Record) -> Result<()> {
        if record.checkpoint_revision == 0 || record.checkpoint_revision > self.cursor.revision {
            return Err("task checkpoint is ahead of its repository counter".into());
        }
        Ok(())
    }

    pub(crate) fn acknowledge(&mut self) -> Result<()> {
        self.cursor.pending = None;
        self.store()
    }
}

fn has_durable_state(root: &Path) -> Result<bool> {
    for entry in fs::read_dir(root).map_err(|e| e.to_string())? {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path.extension().is_none_or(|suffix| suffix != "json") {
            continue;
        }
        let mut bytes = Vec::new();
        File::open(path)
            .map_err(|e| e.to_string())?
            .take(4 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        if serde_json::from_slice::<serde_json::Value>(&bytes)
            .is_ok_and(|value| value.get("version").is_some())
        {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn write_atomic(path: &Path, bytes: &[u8], replacing: bool) -> Result<()> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = path.with_extension(format!("tmp-{}-{nonce}", std::process::id()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|e| e.to_string())?;
        file.write_all(bytes).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        if replacing {
            fs::rename(&temporary, path).map_err(|e| e.to_string())?;
        } else {
            fs::hard_link(&temporary, path).map_err(|e| e.to_string())?;
            fs::remove_file(&temporary).map_err(|e| e.to_string())?;
        }
        File::open(path.parent().ok_or("checkpoint has no parent")?)
            .and_then(|directory| directory.sync_all())
            .map_err(|e| e.to_string())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}
