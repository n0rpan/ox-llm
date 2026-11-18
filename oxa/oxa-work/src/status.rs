//! Read-only checkpoint observations; never acquire, rewrite or delete a task lock.
use crate::{
    load_task_checkpoint, state, valid_task_id, Outcome, Record, Result, RunPolicy, Step, TaskTokens,
};
use serde::Serialize;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Liveness {
    Finished,
    Running,
    Interrupted,
    Unknown,
}

#[derive(Debug, Serialize)]
pub struct Status {
    #[serde(flatten)]
    pub record: Record,
    pub liveness: Liveness,
}

/// A bounded projection of one saved task, with no worker receipt or private paths.
#[derive(Debug, Serialize)]
pub struct TaskUsage {
    pub task: String,
    pub checkpoint_revision: u64,
    pub task_tokens: Option<TaskTokens>,
}

/// Read one validated checkpoint. A missing task and a legacy task with no usage are distinct.
pub fn task_usage(policy: &RunPolicy, id: &str) -> Result<Option<TaskUsage>> {
    if !valid_task_id(id) {
        return Err("invalid task id".into());
    }
    let Some(saved) = load_task_checkpoint(policy, id).map_err(|_| "task usage checkpoint unavailable")?
    else {
        return Ok(None);
    };
    if saved.record.id != id {
        return Err("task usage checkpoint identity disagrees with the requested task".into());
    }
    Ok(Some(TaskUsage {
        task: saved.record.id,
        checkpoint_revision: saved.record.checkpoint_revision,
        task_tokens: saved.record.task_tokens,
    }))
}

pub fn status(storage: &Path, binary: &Path) -> Result<Vec<Status>> {
    let directory = match fs::read_dir(storage) {
        Ok(directory) => directory,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.to_string()),
    };
    let locks = fs::read_to_string("/proc/locks");
    let mut records = Vec::new();
    for entry in directory {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path.extension().is_none_or(|suffix| suffix != "json") {
            continue;
        }
        let mut record = state::load(&path)?.record;
        let liveness = if record.step == Step::Finished {
            Liveness::Finished
        } else {
            match locked(&path.with_extension("lock"), locks.as_deref().ok(), binary) {
                Some(true) => {
                    record.outcome = Outcome::Running;
                    Liveness::Running
                }
                Some(false) => {
                    record.outcome = Outcome::Blocked;
                    record.detail = "task was interrupted; rerun its task file to resume".into();
                    Liveness::Interrupted
                }
                None => Liveness::Unknown,
            }
        };
        records.push(Status { record, liveness });
    }
    records.sort_by(|a, b| a.record.id.cmp(&b.record.id));
    Ok(records)
}

fn locked(path: &Path, locks: Option<&str>, binary: &Path) -> Option<bool> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Some(false),
        Err(_) => return None,
    };
    let device = metadata.dev();
    let major = ((device >> 8) & 0xfff) | ((device >> 32) & 0xfffff000);
    let minor = (device & 0xff) | ((device >> 12) & 0xffffff00);
    let matches_lock = |text: &str| {
        text.lines().any(|line| {
            let fields: Vec<_> = line.split_ascii_whitespace().collect();
            let Some(at) = fields.iter().position(|field| *field == "FLOCK") else {
                return false;
            };
            if fields.get(at + 2).copied() != Some("WRITE") {
                return false;
            }
            let Some(identity) = fields.get(at + 4) else {
                return false;
            };
            let mut parts = identity.split(':');
            let parsed = (|| {
                Some((
                    u64::from_str_radix(parts.next()?, 16).ok()?,
                    u64::from_str_radix(parts.next()?, 16).ok()?,
                    parts.next()?.parse::<u64>().ok()?,
                ))
            })();
            parsed == Some((major, minor, metadata.ino()))
        })
    };
    if locks.is_some_and(matches_lock) {
        return Some(true);
    }
    // /proc/locks omits an inherited flock when its original owner has died.
    // Every worker/supervisor inherits this same description as stdin; fdinfo
    // still exposes the held lock. Inspect only descriptors of the file owner.
    let mut uncertain = locks.is_none();
    let executable = binary.file_name()?.as_encoded_bytes();
    let comm = &executable[..executable.len().min(15)];
    for process in fs::read_dir("/proc").ok()? {
        let process = process.ok()?;
        if !process
            .file_name()
            .as_encoded_bytes()
            .iter()
            .all(u8::is_ascii_digit)
        {
            continue;
        }
        let Ok(owner) = process.metadata() else {
            continue;
        };
        if owner.uid() != metadata.uid() {
            continue;
        }
        // Relevant keepers are the same oxa worker/supervisor executable.
        // Ignore unrelated same-user sandboxes whose descriptors are private.
        let name = match fs::read(process.path().join("comm")) {
            Ok(name) => name,
            Err(_) => {
                uncertain = true;
                continue;
            }
        };
        if name.strip_suffix(b"\n").unwrap_or(&name) != comm {
            continue;
        }
        // Zombies have closed every descriptor, but Linux denies fd lookup
        // for them. Their inaccessible stdin is not an unknown live keeper.
        match fs::read_to_string(process.path().join("stat")) {
            Ok(stat)
                if stat
                    .rsplit_once(')')
                    .and_then(|(_, tail)| tail.split_ascii_whitespace().next())
                    .is_some_and(|state| matches!(state, "Z" | "X")) =>
            {
                continue
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => {
                uncertain = true;
                continue;
            }
        }
        let fd = process.path().join("fd/0");
        let inherited = match fs::metadata(fd) {
            Ok(inherited) => inherited,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                uncertain = true;
                continue;
            }
            Err(_) => continue,
        };
        if (inherited.dev(), inherited.ino()) != (device, metadata.ino()) {
            continue;
        }
        let info = match fs::read_to_string(process.path().join("fdinfo/0")) {
            Ok(info) => info,
            Err(_) => {
                uncertain = true;
                continue;
            }
        };
        if matches_lock(&info) {
            return Some(true);
        }
    }
    if uncertain {
        None
    } else {
        Some(false)
    }
}
