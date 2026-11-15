//! Durable step checkpoints and a kernel lock that recovers after process death.
use super::{Record, Result, Task};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    Preparing,
    Agent,
    Acceptance,
    Repair,
    Recheck,
    Saving,
    Finished,
    Publishing,
    Reporting,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct Checkpoint {
    pub initial: String,
    pub base: String,
    pub prompt: String,
    pub commands: Vec<String>,
    pub agent_error: Option<String>,
    pub failures: String,
}
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct State {
    #[serde(flatten)]
    pub record: Record,
    pub task: Task,
    pub checkpoint: Option<Checkpoint>,
    pub offline: bool,
    #[serde(default)]
    pub acceptance_outcome: Option<super::Outcome>,
}
pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
pub(crate) fn lock(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .map_err(|e| e.to_string())?;
    file.try_lock()
        .map_err(|e| format!("task is already running: {e}"))?;
    file.set_len(0).map_err(|e| e.to_string())?;
    // Keep the inode: removing a lock file permits two processes to lock different inodes.
    Ok(file)
}
pub(crate) fn read(path: &Path) -> Result<State> {
    serde_json::from_slice(&fs::read(path).map_err(|e| e.to_string())?)
        .map_err(|e| format!("task state: {e}"))
}
pub(crate) fn save(path: &Path, state: &mut State, step: Step) -> Result<()> {
    state.record.step = step;
    state.record.progress_at = now();
    atomic_write(
        path,
        &serde_json::to_vec_pretty(state).map_err(|e| e.to_string())?,
    )
}
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let temporary = path.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|e| e.to_string())?;
        file.write_all(bytes).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        fs::rename(&temporary, path).map_err(|e| e.to_string())?;
        File::open(path.parent().ok_or("state has no parent")?)
            .and_then(|p| p.sync_all())
            .map_err(|e| e.to_string())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}
