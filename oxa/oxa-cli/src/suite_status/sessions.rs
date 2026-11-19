//! The `sessions`, `workers` and `local.running` sections of `oxa status --json`.
//!
//! Read-only: status files and the worker registry are read (regular files, size-limited by
//! `read_snapshot`), nothing is written, and no goal, log, prompt or tool text is copied into a
//! value. What leaves this module is counts, states, times and identifiers (worker name, mode,
//! model, branch).
use crate::status::{proven_local_engine, read_snapshot};
use crate::workers::registry::{self, WorkerRecord, WorkerState};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

/// Worker rows shown: the newest workers by start time. The counts cover every worker.
const ROW_LIMIT: usize = 50;

/// The three sections, computed once per `oxa status --json`.
pub(super) struct Scan {
    sessions: Value,
    workers: Value,
    running: Vec<Value>,
}

/// Adds the sections to the suite object. `local.state` becomes `running` when a proven engine
/// serves a live session and the local setup would otherwise be `ready`.
pub(super) fn attach(value: &mut Value, scan: Scan) {
    value["sessions"] = scan.sessions;
    value["workers"] = scan.workers;
    let running = !scan.running.is_empty();
    value["local"]["running"] = Value::Array(scan.running);
    if running && value["local"]["state"] == "ready" {
        value["local"]["state"] = json!("running");
    }
}

/// The sections for the sessions under `sessions_dir` and the workers under `agent_dir`, judged
/// at `now` (milliseconds since the epoch).
pub(super) fn scan(sessions_dir: &Path, agent_dir: &Path, now: u64) -> Scan {
    let mut live = Live::default();
    let mut engines: BTreeMap<Option<String>, u64> = BTreeMap::new();
    for path in status_files(sessions_dir) {
        let Ok(status) = read_snapshot(&path) else {
            continue;
        };
        // A snapshot whose process is gone describes nothing running now.
        if status["type"] != "oxa_status" || status["version"] != 1 || !pid_of(&status).is_some_and(pid_alive)
        {
            continue;
        }
        live.add(&status, now);
        if proven_local_engine(&status) {
            // The status writer does not record the model yet; a proven engine without one is
            // counted under `null`.
            let model = status["local_model"].as_str().map(str::to_string);
            *engines.entry(model).or_insert(0) += 1;
        }
    }
    Scan {
        sessions: live.value(),
        workers: workers(agent_dir, now),
        running: engines
            .into_iter()
            .map(|(model, sessions)| json!({"model": model, "sessions": sessions}))
            .collect(),
    }
}

/// Counts over the live sessions: the ones whose recorded process is alive.
#[derive(Default)]
struct Live {
    live: u64,
    working: u64,
    stalled: u64,
    last: Option<u64>,
}

impl Live {
    fn add(&mut self, status: &Value, now: u64) {
        self.live += 1;
        if status["phase"]
            .as_str()
            .is_some_and(|phase| phase != "idle" && phase != "finished")
        {
            self.working += 1;
        }
        if deadline_passed(status, now) {
            self.stalled += 1;
        }
        if let Some(progress) = status["last_progress_ms"].as_u64() {
            self.last = Some(self.last.map_or(progress, |last| last.max(progress)));
        }
    }

    fn value(&self) -> Value {
        json!({
            "live": self.live,
            "working": self.working,
            "stalled": self.stalled,
            "last_activity_ms": self.last,
        })
    }
}

/// The status writer sets `stall_deadline_ms` only while a run is active, so a deadline that has
/// passed means a run that has stopped making progress.
fn deadline_passed(status: &Value, now: u64) -> bool {
    status["stall_deadline_ms"]
        .as_u64()
        .is_some_and(|deadline| now >= deadline)
}

/// The workers of every registry under `agent_dir/workers`, with the counts by state and the
/// newest rows.
fn workers(agent_dir: &Path, now: u64) -> Value {
    let mut totals: BTreeMap<&str, u64> = [
        "starting", "working", "idle", "finished", "dead", "stopped", "stalled",
    ]
    .into_iter()
    .map(|key| (key, 0))
    .collect();
    let mut rows: Vec<(u64, Value)> = Vec::new();
    for record in records(agent_dir) {
        let alive = pid_alive(record.pid);
        let status = read_snapshot(&record.status_file)
            .ok()
            .filter(|status| registry::names_worker(&record, status));
        let state = registry::state(&record, status.as_ref(), alive);
        // `Working` is only reached through a snapshot that names this worker, so the deadline
        // read here is this worker's own.
        let stalled =
            state == WorkerState::Working && status.as_ref().is_some_and(|s| deadline_passed(s, now));
        let key = state_word(&state);
        *totals.entry(key).or_insert(0) += 1;
        if stalled {
            *totals.entry("stalled").or_insert(0) += 1;
        }
        let progress = status
            .as_ref()
            .and_then(|s| s["last_progress_ms"].as_u64())
            .unwrap_or(record.started_ms);
        let mut row = json!({
            "name": record.name,
            "state": key,
            "mode": record.mode,
            "model": record.model,
            "branch": record.branch,
            "started_ms": record.started_ms,
            "last_progress_ms": progress,
            "stalled": stalled,
        });
        if let WorkerState::Finished(outcome) = &state {
            row["outcome"] = json!(outcome);
        }
        rows.push((record.started_ms, row));
    }
    rows.sort_by_key(|(started, _)| std::cmp::Reverse(*started));
    let mut section = json!({
        "rows": rows.into_iter().take(ROW_LIMIT).map(|(_, row)| row).collect::<Vec<Value>>(),
    });
    for (key, count) in totals {
        section[key] = json!(count);
    }
    section
}

fn state_word(state: &WorkerState) -> &'static str {
    match state {
        WorkerState::Starting => "starting",
        WorkerState::Working => "working",
        WorkerState::Idle => "idle",
        WorkerState::Finished(_) => "finished",
        WorkerState::Dead => "dead",
        WorkerState::Stopped => "stopped",
    }
}

/// Every worker record in the registries under `agent_dir/workers`, one file per parent session.
/// A registry that cannot be read is skipped, so one bad file never empties the other rows.
fn records(agent_dir: &Path) -> Vec<WorkerRecord> {
    let mut parents: Vec<String> = Vec::new();
    if let Ok(entries) = fs::read_dir(agent_dir.join("workers")) {
        for path in entries.flatten().map(|entry| entry.path()) {
            if path.extension() == Some(OsStr::new("json")) && is_file(&path) {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    parents.push(stem.to_string());
                }
            }
        }
    }
    parents.sort();
    parents
        .iter()
        .filter_map(|parent| registry::load(agent_dir, parent).ok())
        .flatten()
        .collect()
}

/// Every `<id>.status.json` one level below `sessions_dir`, the store's two-level layout. Symlinked
/// folders and files are not followed.
fn status_files(sessions_dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(folders) = fs::read_dir(sessions_dir) else {
        return out;
    };
    for folder in folders.flatten().map(|entry| entry.path()) {
        if !is_dir(&folder) {
            continue;
        }
        let Ok(files) = fs::read_dir(&folder) else {
            continue;
        };
        for file in files.flatten().map(|entry| entry.path()) {
            let named = file
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".status.json"));
            if named && is_file(&file) {
                out.push(file);
            }
        }
    }
    out.sort();
    out
}

fn is_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file())
}

fn is_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir())
}

fn pid_of(status: &Value) -> Option<u32> {
    status["pid"].as_u64().and_then(|pid| u32::try_from(pid).ok())
}

/// A process is alive when Linux still lists it and it is neither a zombie nor dead. A pid that
/// has been reused by another process still reads as alive; the status writer's own timestamps
/// are the only other evidence and are not compared here.
#[cfg(target_os = "linux")]
fn pid_alive(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.rsplit_once(") ")
            .and_then(|(_, rest)| rest.chars().next())
            .is_some_and(|state| state != 'Z' && state != 'X')
    })
}

/// macOS has no procfs. Ask its native `ps` for the process state and apply the same zombie/dead
/// rule as the Linux implementation. An unavailable or unsuccessful `ps` is not proof of life.
#[cfg(target_os = "macos")]
fn pid_alive(pid: u32) -> bool {
    std::process::Command::new("/bin/ps")
        .args(["-o", "state=", "-p", &pid.to_string()])
        .output()
        .is_ok_and(|output| output.status.success() && ps_state_alive(&output.stdout))
}

#[cfg(target_os = "macos")]
fn ps_state_alive(stdout: &[u8]) -> bool {
    stdout
        .iter()
        .copied()
        .find(|byte| !byte.is_ascii_whitespace())
        .is_some_and(|state| state != b'Z' && state != b'X')
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn pid_alive(_pid: u32) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxa_config::{Dirs, Env};
    use oxa_fake::TestEnv;
    use std::time::Instant;

    const NOW: u64 = 1_795_100_000_000;
    /// A pid no live process has: Linux pids stay below 2^22.
    const DEAD: u32 = u32::MAX - 1;

    fn write_status(sessions: &Path, folder: &str, id: &str, body: Value) -> PathBuf {
        let dir = sessions.join(folder);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{id}.status.json"));
        fs::write(&path, body.to_string()).unwrap();
        path
    }

    /// A status body as the status writer emits it for a run of `pid`.
    fn status(id: &str, pid: u32, phase: &str) -> Value {
        json!({
            "type": "oxa_status", "version": 1, "session_id": id, "pid": pid, "phase": phase,
            "last_progress_ms": NOW - 1000, "stall_deadline_ms": null, "outcome": null,
        })
    }

    fn worker(name: &str, pid: u32, status_file: PathBuf) -> WorkerRecord {
        WorkerRecord {
            name: name.into(),
            session_id: format!("{name}-sid"),
            pid,
            pgid: pid,
            worktree: "/work/tree".into(),
            branch: format!("agent/{name}"),
            model: "chatgpt/gpt-6.2-codex".into(),
            mode: "goal".into(),
            goal_file: status_file.with_extension("goal"),
            status_file,
            log_file: "/work/tree/log.txt".into(),
            started_ms: NOW - 60_000,
            stopped: false,
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn workers_rows_use_registry_state() {
        let env = TestEnv::new();
        let dirs = Dirs::under(env.root());
        let sessions = dirs.sessions_dir.clone();
        let me = std::process::id();
        let file = |name: &str| sessions.join("w").join(format!("{name}-sid.status.json"));

        let mut stalled = status("working-sid", me, "working");
        stalled["stall_deadline_ms"] = json!(NOW - 1);
        write_status(&sessions, "w", "working-sid", stalled);
        write_status(&sessions, "w", "idle-sid", status("idle-sid", me, "idle"));
        let mut done = status("finished-sid", me, "finished");
        done["outcome"] = json!({"outcome": "done"});
        write_status(&sessions, "w", "finished-sid", done);
        write_status(&sessions, "w", "dead-sid", status("dead-sid", DEAD, "working"));
        // The "starting" worker has no status file yet.
        let mut stopped = worker("stopped", DEAD, file("stopped"));
        stopped.stopped = true;
        let records = vec![
            worker("working", me, file("working")),
            worker("idle", me, file("idle")),
            worker("finished", me, file("finished")),
            worker("dead", DEAD, file("dead")),
            stopped,
            worker("starting", me, file("starting")),
        ];
        registry::save(&dirs.agent_dir, "lead-1", &records).unwrap();

        let scanned = scan(&sessions, &dirs.agent_dir, NOW);
        let rows = scanned.workers["rows"].as_array().unwrap();
        let row = |name: &str| rows.iter().find(|r| r["name"] == name).unwrap();
        assert_eq!(row("working")["state"], "working");
        assert_eq!(row("working")["stalled"], true);
        assert_eq!(row("idle")["state"], "idle");
        assert_eq!(row("finished")["state"], "finished");
        assert_eq!(row("finished")["outcome"], "done");
        assert_eq!(row("dead")["state"], "dead");
        assert_eq!(row("stopped")["state"], "stopped");
        assert_eq!(row("starting")["state"], "starting");
        for key in ["working", "idle", "finished", "dead", "stopped", "starting"] {
            assert_eq!(scanned.workers[key], 1, "{key}");
        }
        assert_eq!(scanned.workers["stalled"], 1);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn dead_pid_is_not_live() {
        let env = TestEnv::new();
        let sessions = env.root().join("sessions");
        write_status(
            &sessions,
            "a",
            "live",
            status("live", std::process::id(), "working"),
        );
        write_status(&sessions, "a", "gone", status("gone", DEAD, "working"));
        let scanned = scan(&sessions, &env.root().join("agent"), NOW);
        assert_eq!(scanned.sessions["live"], 1);
        assert_eq!(scanned.sessions["working"], 1);
        assert!(!pid_alive(DEAD));
        assert!(pid_alive(std::process::id()));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_process_state_rejects_zombies_and_dead_processes() {
        assert!(ps_state_alive(b" S+\n"));
        assert!(!ps_state_alive(b" Z\n"));
        assert!(!ps_state_alive(b" X\n"));
        assert!(!ps_state_alive(b"\n"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn status_has_no_goal_or_log_text() {
        let env = TestEnv::new();
        let sessions = env.root().join("sessions");
        let me = std::process::id();
        let mut session = status("s-sid", me, "working");
        // Fields the writer never emits must not reach the output either.
        session["last_tool"] = json!("SECRET-TOOL-TEXT");
        session["prompt"] = json!("SECRET-PROMPT-TEXT");
        write_status(&sessions, "a", "s-sid", session);
        let mut record = worker("g", me, sessions.join("a").join("g-sid.status.json"));
        record.goal_file = env.write("secret/goal.md", "SECRET-GOAL-TEXT");
        record.log_file = env.write("secret/log.txt", "SECRET-LOG-TEXT");
        let agent = env.root().join("agent");
        registry::save(&agent, "lead-1", &[record]).unwrap();

        let scanned = scan(&sessions, &agent, NOW);
        let text = format!(
            "{} {} {}",
            scanned.sessions,
            scanned.workers,
            Value::Array(scanned.running.clone())
        );
        for secret in [
            "SECRET-GOAL-TEXT",
            "SECRET-LOG-TEXT",
            "SECRET-TOOL-TEXT",
            "SECRET-PROMPT-TEXT",
            "goal_file",
            "log_file",
            "worktree",
        ] {
            assert!(!text.contains(secret), "{secret} is not in the status sections");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn local_running_needs_a_proven_engine() {
        let env = TestEnv::new();
        let sessions = env.root().join("sessions");
        // The engine pid is claimed but carries no policy and no launcher time: not proven.
        let mut claimed = status("l-sid", std::process::id(), "working");
        claimed["local_engine_pid"] = json!(std::process::id());
        write_status(&sessions, "a", "l-sid", claimed);
        let scanned = scan(&sessions, &env.root().join("agent"), NOW);
        assert!(scanned.running.is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn suite_status_time_on_many_sessions() {
        let env = TestEnv::new();
        let sessions = env.root().join("sessions");
        let me = std::process::id();
        for folder in 0..20 {
            for n in 0..50 {
                let id = format!("s-{folder}-{n}");
                let pid = if n % 2 == 0 { me } else { DEAD };
                write_status(&sessions, &format!("dir-{folder}"), &id, status(&id, pid, "idle"));
            }
        }
        let started = Instant::now();
        let scanned = scan(&sessions, &env.root().join("agent"), NOW);
        // The measured time is printed for the lane log; the count proves every file was read.
        eprintln!(
            "suite status scan of 1000 session status files: {:?}",
            started.elapsed()
        );
        assert_eq!(scanned.sessions["live"], 500);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn suite_envelope_carries_sessions_and_workers() {
        let env = TestEnv::new();
        let dirs = Dirs::under(env.root());
        write_status(
            &dirs.sessions_dir,
            "a",
            "e-sid",
            status("e-sid", std::process::id(), "working"),
        );
        let mut out = Vec::new();
        let code = crate::status::run(
            &["--json".to_string()],
            &dirs,
            env.root(),
            &Env::from_pairs(&[]),
            &mut out,
        )
        .unwrap();
        assert_eq!(code, 0);
        let printed: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(printed["sessions"]["live"], 1);
        assert_eq!(printed["workers"]["rows"], json!([]));
        assert_eq!(printed["local"]["running"], json!([]));
    }
}
