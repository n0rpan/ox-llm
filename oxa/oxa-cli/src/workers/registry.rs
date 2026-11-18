//! Worker registry: one JSON file per parent session under `<agent_dir>/workers/`, listing the
//! background workers that session started. Writes are atomic (temp file + rename, mode 0600).
//! A corrupt file is reported as an error and is never overwritten.
use oxa_proto::OxaError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub const CORRUPT: &str = "worker_registry_corrupt";
const IO: &str = "worker_registry_io";
const BAD_PARENT: &str = "worker_registry_bad_parent";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerRecord {
    pub name: String,
    pub session_id: String,
    pub pid: u32,
    pub pgid: u32,
    pub worktree: PathBuf,
    pub branch: String,
    pub model: String,
    pub mode: String,
    pub goal_file: PathBuf,
    pub status_file: PathBuf,
    pub log_file: PathBuf,
    pub started_ms: u64,
    pub stopped: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerState {
    Starting,
    Working,
    Idle,
    Finished(String),
    Dead,
    Stopped,
}

fn io_err(what: &str, e: std::io::Error) -> OxaError {
    OxaError::new(IO, format!("{what}: {e}"))
}

fn file_path(agent_dir: &Path, parent_id: &str) -> Result<PathBuf, OxaError> {
    let valid = !parent_id.is_empty()
        && parent_id.len() <= 128
        && parent_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !valid {
        return Err(OxaError::new(
            BAD_PARENT,
            format!("invalid parent session id: {parent_id:?}"),
        ));
    }
    Ok(agent_dir.join("workers").join(format!("{parent_id}.json")))
}

/// The workers recorded for `parent_id`; no file means no workers.
pub fn load(agent_dir: &Path, parent_id: &str) -> Result<Vec<WorkerRecord>, OxaError> {
    let path = file_path(agent_dir, parent_id)?;
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io_err("cannot read worker registry", e)),
    };
    serde_json::from_slice(&bytes).map_err(|e| {
        OxaError::new(
            CORRUPT,
            format!("worker registry {} is corrupt: {e}", path.display()),
        )
    })
}

/// Replaces the workers recorded for `parent_id` atomically, with mode 0600.
pub fn save(agent_dir: &Path, parent_id: &str, records: &[WorkerRecord]) -> Result<(), OxaError> {
    let path = file_path(agent_dir, parent_id)?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| io_err("cannot create worker registry directory", e))?;
    }
    let bytes = serde_json::to_vec_pretty(records)
        .map_err(|e| OxaError::new(IO, format!("cannot encode worker registry: {e}")))?;
    write_atomic(&path, &bytes).map_err(|e| io_err("cannot write worker registry", e))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("registry file has no directory"))?;
    for _ in 0..8 {
        let tmp = parent.join(format!(
            ".workers-{}-{}.tmp",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
        {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };
        let result = file
            .write_all(bytes)
            .and_then(|_| file.sync_data())
            .and_then(|_| fs::rename(&tmp, path));
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        return result;
    }
    Err(std::io::Error::other("cannot reserve a registry temporary file"))
}

/// The worker's state from its status.json (if any) and whether its process is alive.
/// A finished outcome wins; then a stop request; then liveness; then the reported phase.
pub fn state(rec: &WorkerRecord, status_json: Option<&Value>, alive: bool) -> WorkerState {
    let phase = status_json.and_then(|s| s.get("phase")).and_then(Value::as_str);
    let outcome = status_json
        .and_then(|s| s.get("outcome"))
        .filter(|o| !o.is_null());
    if phase == Some("finished") || outcome.is_some() {
        let name = outcome
            .and_then(|o| o.get("outcome"))
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        return WorkerState::Finished(name.to_string());
    }
    if rec.stopped {
        return WorkerState::Stopped;
    }
    if !alive {
        return WorkerState::Dead;
    }
    match phase {
        Some("idle") => WorkerState::Idle,
        Some("working" | "tool" | "closing") => WorkerState::Working,
        // No status yet, or a phase this build does not know: the worker is still starting up.
        _ => WorkerState::Starting,
    }
}

#[cfg(test)]
mod tests {
    use super::WorkerState::*;
    use super::*;
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;

    fn rec(name: &str) -> WorkerRecord {
        WorkerRecord {
            name: name.into(),
            session_id: format!("{name}-sid"),
            pid: 4242,
            pgid: 4242,
            worktree: "/w/tree".into(),
            branch: "agent/x".into(),
            model: "m".into(),
            mode: "goal".into(),
            goal_file: "/w/goal".into(),
            status_file: "/w/status.json".into(),
            log_file: "/w/log".into(),
            started_ms: 1_700_000_000_000,
            stopped: false,
        }
    }

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("oxa-workers-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn registry_round_trip_is_atomic_and_0600() {
        let dir = scratch("roundtrip");
        assert!(load(&dir, "parent-1").unwrap().is_empty());
        let records = vec![rec("a"), rec("b")];
        save(&dir, "parent-1", &records).unwrap();
        assert_eq!(load(&dir, "parent-1").unwrap(), records);
        let path = dir.join("workers").join("parent-1.json");
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        // A second save replaces the file and leaves no temporary files behind.
        let mut more = records.clone();
        more[0].stopped = true;
        save(&dir, "parent-1", &more).unwrap();
        assert_eq!(load(&dir, "parent-1").unwrap(), more);
        let names: Vec<String> = fs::read_dir(dir.join("workers"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, vec!["parent-1.json".to_string()]);
        // A parent id that could escape the workers directory is refused.
        assert!(save(&dir, "../escape", &[]).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn state_from_status_phase_and_liveness() {
        let live = rec("live");
        let mut stopped = rec("stopped");
        stopped.stopped = true;
        let done = json!({"phase":"finished","outcome":{"type":"oxa_result","outcome":"done","error_class":null,"exit_code":0}});
        let failed = json!({"phase":"finished","outcome":{"type":"oxa_result","outcome":"failed","error_class":"x","exit_code":1}});
        let working = json!({"phase":"working","outcome":null});
        let cases: Vec<(&WorkerRecord, Option<Value>, bool, WorkerState)> = vec![
            (&live, None, true, Starting),
            (&live, None, false, Dead),
            (&live, Some(json!({"phase":"idle","outcome":null})), true, Idle),
            (&live, Some(working.clone()), true, Working),
            (&live, Some(json!({"phase":"tool","outcome":null})), true, Working),
            (
                &live,
                Some(json!({"phase":"closing","outcome":null})),
                true,
                Working,
            ),
            (&live, Some(working.clone()), false, Dead),
            (&live, Some(done.clone()), false, Finished("done".into())),
            (&live, Some(failed), true, Finished("failed".into())),
            (&stopped, Some(working), true, Stopped),
            (&stopped, None, false, Stopped),
            (&stopped, Some(done), false, Finished("done".into())),
        ];
        for (rec, status, alive, want) in cases {
            assert_eq!(
                state(rec, status.as_ref(), alive),
                want,
                "status={status:?} alive={alive} stopped={}",
                rec.stopped
            );
        }
    }

    #[test]
    fn corrupt_registry_is_reported_not_overwritten() {
        let dir = scratch("corrupt");
        fs::create_dir_all(dir.join("workers")).unwrap();
        let path = dir.join("workers").join("parent-2.json");
        fs::write(&path, b"{\"not\": a registry").unwrap();
        let err = load(&dir, "parent-2").unwrap_err();
        assert_eq!(err.code, CORRUPT);
        // Nothing rewrites the bad file: its bytes are still the original.
        assert_eq!(fs::read(&path).unwrap(), b"{\"not\": a registry");
        let _ = fs::remove_dir_all(&dir);
    }
}
