//! One small atomic snapshot per saved session. Progress never serializes model/tool text.
use oxa_config::{dirs_from_env, Dirs, Env, Settings};
use oxa_core::{Progress, ProgressObserver, RunOutcome};
use oxa_proto::{codes, OxaError, TurnKind};
use oxa_session::{FsStore, SessionRef, SessionStore};
use serde_json::{json, Value};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_BYTES: usize = 4096;
const WRITE_FAILED: &str = "status_write_failed";

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

#[derive(Debug)]
struct Snapshot {
    file: PathBuf,
    session: SessionRef,
    started: u64,
    phase: &'static str,
    step: u64,
    last_tool: Option<String>,
    prompt: u64,
    completion: u64,
    cached: u64,
    cached_missing: bool,
    unreported: u64,
    remote: bool,
    progress: u64,
    running: bool,
    requests: u32,
    tools: u32,
    settled: RunOutcome,
    outcome: Option<RunOutcome>,
    written: Instant,
}

impl Snapshot {
    fn record(&self, timeout: Option<u64>, coalesce_ms: u64) -> Value {
        let active = self.running || self.requests > 0 || self.tools > 0;
        json!({
            "type":"oxa_status", "version":1, "session_id":self.session.id,
            "pid":std::process::id(), "started_ms":self.started,
            "phase":self.phase, "step":self.step, "last_tool":self.last_tool,
            "tokens":{"prompt":self.prompt, "completion":self.completion,
                "cached":(!self.cached_missing).then_some(self.cached),
                "complete":self.unreported == 0 && self.requests == 0,
                "unreported_requests":self.unreported},
            "cost_usd":(!self.remote).then_some(0),
            "last_progress_ms":self.progress, "stall_timeout_ms":timeout,
            // The grace covers progress coalesced since this write: a reader must not flag
            // a healthy session early just because a delta has not reached disk yet.
            "stall_deadline_ms":timeout.filter(|_| active).map(|t| self.progress.saturating_add(t).saturating_add(coalesce_ms)),
            "outcome":self.outcome.map(|outcome| outcome.record())
        })
    }

    fn between(&mut self) {
        self.phase = if self.tools > 0 {
            "tool"
        } else if self.running {
            "working"
        } else {
            "idle"
        };
    }
}

#[derive(Debug, Default)]
struct State {
    active: Option<Snapshot>,
    error: Option<OxaError>,
}

#[derive(Debug)]
pub(crate) struct StatusWriter {
    state: Mutex<State>,
    timeout_ms: Option<u64>,
    interval: Duration,
    billing: Option<oxc_gateway::diagnostics::Diagnostics>,
}

impl StatusWriter {
    pub(crate) fn with_billing(mut self, billing: oxc_gateway::diagnostics::Diagnostics) -> Self {
        self.billing = Some(billing);
        self
    }

    pub(crate) fn new(timeout: Option<Duration>) -> Self {
        let timeout_ms = timeout.map(|d| d.as_millis() as u64);
        Self {
            state: Mutex::new(State::default()),
            billing: None,
            timeout_ms,
            interval: Duration::from_millis(timeout_ms.map(|t| (t / 4).clamp(1, 1000)).unwrap_or(1000)),
        }
    }

    pub(crate) fn check(&self) -> Result<(), OxaError> {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .error
            .clone()
            .map_or(Ok(()), Err)
    }

    pub(crate) fn finish(
        &self,
        result: Result<RunOutcome, OxaError>,
        err: &mut dyn Write,
    ) -> Result<RunOutcome, OxaError> {
        let outcome = result.as_ref().copied().unwrap_or_else(RunOutcome::from_error);
        let mut reported = outcome;
        if let Err(error) = self.check() {
            reported.include(RunOutcome::from_error(&error));
        }
        let id = self
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .active
            .as_ref()
            .map(|s| s.session.clone());
        if let Some(id) = id {
            self.notify(&id, Progress::Finished(reported));
        }
        if let Err(error) = self.check() {
            if matches!(outcome, RunOutcome::Done | RunOutcome::NothingToDo) {
                // A failed final write may be transient. If the retry works, the file must
                // agree with the process's failed result rather than claim success.
                let id = self
                    .state
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .active
                    .as_ref()
                    .map(|s| s.session.clone());
                if let Some(id) = id {
                    self.notify(&id, Progress::Finished(RunOutcome::from_error(&error)));
                }
                return Err(error);
            }
            // A status failure must be visible, but cannot hide an earlier worker failure.
            let _ = writeln!(err, "Error: {}", error.message);
        }
        result
    }

    pub(crate) fn finish_interactive(
        &self,
        result: Result<i32, OxaError>,
        err: &mut dyn Write,
    ) -> Result<i32, OxaError> {
        let outcome = match &result {
            Ok(0) => RunOutcome::Done,
            Ok(130) => RunOutcome::Interrupted,
            Ok(129) => RunOutcome::Failed(oxa_proto::codes::TERMINAL_CLOSED),
            Ok(_) => RunOutcome::Failed("interactive_exit"),
            Err(e) => RunOutcome::from_error(e),
        };
        // Interactive exit describes the UI session, rather than accumulating independent
        // prompt failures the way an unattended batch must.
        if let Some(snapshot) = &mut self.state.lock().unwrap_or_else(|p| p.into_inner()).active {
            snapshot.settled = outcome;
        }
        self.finish(Ok(outcome), err)?;
        result
    }

    fn write(&self, snapshot: &mut Snapshot) -> std::io::Result<()> {
        let mut record = snapshot.record(
            self.timeout_ms,
            self.interval.as_millis().min(u64::MAX as u128) as u64,
        );
        if let Some(billing) = self.billing.as_ref().and_then(|b| b.last_billing()) {
            record["last_billing"] = billing.record();
        }
        let bytes = serde_json::to_vec(&record)?;
        if bytes.len() > MAX_BYTES {
            return Err(std::io::Error::other("status snapshot exceeds its size limit"));
        }
        atomic(&snapshot.file, &bytes)?;
        snapshot.written = Instant::now();
        Ok(())
    }
}

impl ProgressObserver for StatusWriter {
    fn notify(&self, session: &SessionRef, progress: Progress<'_>) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let now = now_ms();
        if let Progress::Session { file } = progress {
            if state.active.as_ref().is_some_and(|s| s.session == *session) {
                if let Some(snapshot) = &mut state.active {
                    if matches!(snapshot.phase, "closing" | "finished") {
                        snapshot.phase = "idle";
                        snapshot.running = false;
                        snapshot.outcome = None;
                        snapshot.progress = now;
                        if let Err(e) = self.write(snapshot) {
                            state.error.get_or_insert_with(|| write_error(&e));
                        }
                    }
                }
                return;
            }
            if let Some(snapshot) = &mut state.active {
                snapshot.phase = "finished";
                snapshot.running = false;
                snapshot.unreported = snapshot.unreported.saturating_add(snapshot.requests as u64);
                snapshot.requests = 0;
                snapshot.tools = 0;
                snapshot.outcome = Some(snapshot.settled);
                if let Err(e) = self.write(snapshot) {
                    state.error.get_or_insert_with(|| write_error(&e));
                }
            }
            state.active = file.map(|file| Snapshot {
                file: file.with_extension("status.json"),
                session: session.clone(),
                started: now,
                phase: "idle",
                step: 0,
                last_tool: None,
                prompt: 0,
                completion: 0,
                cached: 0,
                cached_missing: false,
                unreported: 0,
                remote: false,
                progress: now,
                running: false,
                requests: 0,
                tools: 0,
                settled: RunOutcome::NothingToDo,
                outcome: None,
                written: Instant::now(),
            });
        }
        let Some(snapshot) = &mut state.active else {
            return;
        };
        if snapshot.session != *session {
            return;
        } // A cancelled helper of a session left behind.
        if snapshot.outcome.is_some() && !matches!(progress, Progress::RunStart | Progress::Finished(_)) {
            return;
        }
        let force = match progress {
            Progress::Session { .. } => true,
            Progress::RunStart => {
                snapshot.running = true;
                snapshot.outcome = None;
                snapshot.between();
                true
            }
            Progress::RequestStart { kind, remote } => {
                snapshot.remote |= remote;
                snapshot.requests += 1;
                snapshot.step += 1;
                if snapshot.tools == 0 {
                    snapshot.phase = match kind {
                        TurnKind::Chat | TurnKind::Subagent => "model",
                        TurnKind::Prefill => "prefill",
                        _ => "compaction",
                    };
                }
                true
            }
            Progress::RequestEnd { usage, remote } => {
                snapshot.requests = snapshot.requests.saturating_sub(1);
                snapshot.remote |= remote;
                if let Some(usage) = usage {
                    snapshot.prompt = snapshot.prompt.saturating_add(usage.prompt as u64);
                    snapshot.completion = snapshot.completion.saturating_add(usage.completion as u64);
                    snapshot.cached = snapshot.cached.saturating_add(usage.cached as u64);
                    snapshot.cached_missing |= usage.cached_missing;
                } else {
                    snapshot.unreported += 1;
                }
                snapshot.between();
                true
            }
            Progress::ToolStart(name) => {
                snapshot.tools += 1;
                snapshot.last_tool = Some(name.chars().take(128).collect());
                snapshot.phase = "tool";
                true
            }
            Progress::ToolEnd => {
                snapshot.tools = snapshot.tools.saturating_sub(1);
                snapshot.between();
                true
            }
            Progress::Advanced => false,
            Progress::Closing => {
                snapshot.running = true;
                snapshot.phase = "closing";
                true
            }
            Progress::Settled(outcome) | Progress::Finished(outcome) => {
                snapshot.running = false;
                snapshot.unreported = snapshot.unreported.saturating_add(snapshot.requests as u64);
                snapshot.requests = 0;
                snapshot.tools = 0;
                snapshot.settled.include(outcome);
                snapshot.outcome = matches!(progress, Progress::Finished(_)).then_some(snapshot.settled);
                snapshot.phase = if snapshot.outcome.is_some() {
                    "finished"
                } else {
                    "idle"
                };
                true
            }
        };
        snapshot.progress = now;
        if force || snapshot.written.elapsed() >= self.interval {
            if let Err(e) = self.write(snapshot) {
                state.error.get_or_insert_with(|| write_error(&e));
            }
        }
    }
}

fn write_error(e: &std::io::Error) -> OxaError {
    OxaError::new(WRITE_FAILED, format!("cannot update worker status: {e}"))
}

fn atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("status file has no directory"))?;
    for _ in 0..8 {
        let tmp = parent.join(format!(
            ".oxa-status-{}-{}.tmp",
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
            let _ = fs::remove_file(tmp);
        }
        return result;
    }
    Err(std::io::Error::other("cannot reserve a status temporary file"))
}

pub(crate) fn run(
    words: &[String],
    dirs: &Dirs,
    cwd: &Path,
    env: &Env,
    out: &mut dyn Write,
) -> Result<i32, OxaError> {
    let usage = || {
        OxaError::new(
            codes::USAGE,
            "usage: oxa status <session-id|session-file> [--session-dir <dir>]",
        )
    };
    let Some(id) = words.first().filter(|id| !id.starts_with('-')) else {
        return Err(usage());
    };
    let mut sessions = dirs.sessions_dir.clone();
    if env.get("OXA_SESSION_DIR").is_none() {
        if let Some(configured) = Settings::load(dirs, cwd, false).session_dir(&dirs.home, cwd) {
            sessions = configured;
        }
    }
    match &words[1..] {
        [] => {}
        [flag, dir] if flag == "--session-dir" => {
            sessions = dirs_from_env(env, Some(Path::new(dir)))?.sessions_dir;
        }
        _ => return Err(usage()),
    }
    let sref = FsStore::new(sessions).resolve(id)?;
    let path = sref.path.with_extension("status.json");
    let mut value = read_snapshot(&path)?;
    if value["type"] != "oxa_status" || value["version"] != 1 || value["session_id"] != sref.id {
        return Err(OxaError::new(
            codes::INVALID,
            "worker status does not match this session/version",
        ));
    }
    value["stalled"] = value["stall_deadline_ms"]
        .as_u64()
        .is_some_and(|deadline| now_ms() >= deadline)
        .into();
    serde_json::to_writer(&mut *out, &value).map_err(|e| OxaError::new(codes::IO, e.to_string()))?;
    out.write_all(b"\n")
        .map_err(|e| OxaError::io("cannot write worker status", &e))?;
    Ok(0)
}

fn read_snapshot(path: &Path) -> Result<Value, OxaError> {
    for _ in 0..8 {
        let before = fs::symlink_metadata(path).map_err(|e| OxaError::io("cannot read worker status", &e))?;
        if !before.is_file() {
            return Err(OxaError::new(
                codes::UNSAFE_PATH,
                "worker status is not a regular file",
            ));
        }
        let file = fs::File::open(path).map_err(|e| OxaError::io("cannot open worker status", &e))?;
        let after = file
            .metadata()
            .map_err(|e| OxaError::io("cannot inspect worker status", &e))?;
        if (before.dev(), before.ino()) != (after.dev(), after.ino()) {
            continue; // Atomic replacement may land between stat and open.
        }
        let mut bytes = Vec::new();
        file.take((MAX_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|e| OxaError::io("cannot read worker status", &e))?;
        if bytes.len() > MAX_BYTES {
            return Err(OxaError::new(
                codes::INVALID,
                "worker status exceeds its size limit",
            ));
        }
        return serde_json::from_slice(&bytes)
            .map_err(|_| OxaError::new(codes::INVALID, "invalid worker status JSON"));
    }
    Err(OxaError::new(
        codes::UNSAFE_PATH,
        "worker status changed repeatedly while opening",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxa_fake::TestEnv;
    use oxa_proto::Usage;

    #[test]
    fn interactive_terminal_loss_records_the_process_exit() {
        let env = TestEnv::new();
        let session = SessionRef {
            id: "one".into(),
            path: env.work().join("session.jsonl"),
        };
        let writer = StatusWriter::new(None);
        writer.notify(
            &session,
            Progress::Session {
                file: Some(&session.path),
            },
        );
        assert_eq!(writer.finish_interactive(Ok(129), &mut Vec::new()).unwrap(), 129);
        let record = read_snapshot(&session.path.with_extension("status.json")).unwrap();
        assert_eq!(record["phase"], "finished");
        assert_eq!(record["outcome"]["error_class"], "terminal_closed");
        assert_eq!(record["outcome"]["exit_code"], 129);
    }

    #[test]
    fn stream_updates_coalesce_and_the_final_snapshot_includes_all_usage() {
        let env = TestEnv::new();
        let session = SessionRef {
            id: "one".into(),
            path: env.work().join("session.jsonl"),
        };
        let path = session.path.with_extension("status.json");
        let mut writer = StatusWriter::new(None);
        writer.interval = Duration::MAX; // An explicit clock-independent coalescing window.
        writer.notify(
            &session,
            Progress::Session {
                file: Some(&session.path),
            },
        );
        writer.notify(
            &session,
            Progress::RequestStart {
                kind: TurnKind::Chat,
                remote: true,
            },
        );
        let initial = fs::read(&path).unwrap();
        for _ in 0..1000 {
            writer.notify(&session, Progress::Advanced);
        }
        assert_eq!(fs::read(&path).unwrap(), initial);
        assert_eq!(read_snapshot(&path).unwrap()["cost_usd"], Value::Null);
        assert_eq!(read_snapshot(&path).unwrap()["tokens"]["complete"], false);
        writer.notify(
            &session,
            Progress::RequestEnd {
                usage: Some(Usage {
                    prompt: 100,
                    completion: 8,
                    cached: 60,
                    cached_missing: false,
                }),
                remote: true,
            },
        );
        writer.finish(Ok(RunOutcome::Done), &mut Vec::new()).unwrap();
        let final_status = read_snapshot(&path).unwrap();
        assert_eq!(final_status["tokens"]["prompt"], 100);
        assert_eq!(final_status["tokens"]["completion"], 8);
        assert_eq!(final_status["tokens"]["cached"], 60);
        assert_eq!(final_status["tokens"]["complete"], true);
        assert_eq!(final_status["outcome"]["outcome"], "done");
    }

    #[test]
    fn transient_write_failure_cannot_leave_a_successful_terminal_record() {
        let env = TestEnv::new();
        let session = SessionRef {
            id: "one".into(),
            path: env.work().join("session.jsonl"),
        };
        let path = session.path.with_extension("status.json");
        let writer = StatusWriter::new(None);
        writer.notify(
            &session,
            Progress::Session {
                file: Some(&session.path),
            },
        );
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        writer.notify(&session, Progress::RunStart);
        fs::remove_dir(&path).unwrap();
        let error = writer.finish(Ok(RunOutcome::Done), &mut Vec::new()).unwrap_err();
        assert_eq!(error.code, WRITE_FAILED);
        let value = read_snapshot(&path).unwrap();
        assert_eq!(value["outcome"]["error_class"], WRITE_FAILED);
        assert_eq!(value["outcome"]["exit_code"], 1);
    }

    #[test]
    fn stale_helper_progress_cannot_change_the_session_switched_into() {
        let env = TestEnv::new();
        let one = SessionRef {
            id: "one".into(),
            path: env.work().join("one.jsonl"),
        };
        let two = SessionRef {
            id: "two".into(),
            path: env.work().join("two.jsonl"),
        };
        let writer = StatusWriter::new(None);
        writer.notify(
            &one,
            Progress::Session {
                file: Some(&one.path),
            },
        );
        writer.notify(
            &one,
            Progress::RequestStart {
                kind: TurnKind::Prefill,
                remote: false,
            },
        );
        writer.notify(
            &two,
            Progress::Session {
                file: Some(&two.path),
            },
        );
        let before = fs::read(two.path.with_extension("status.json")).unwrap();
        writer.notify(
            &one,
            Progress::RequestEnd {
                usage: Some(Usage {
                    prompt: 9000,
                    ..Usage::default()
                }),
                remote: false,
            },
        );
        assert_eq!(fs::read(two.path.with_extension("status.json")).unwrap(), before);
        let old = read_snapshot(&one.path.with_extension("status.json")).unwrap();
        assert_eq!(old["tokens"]["complete"], false);
        assert_eq!(old["tokens"]["unreported_requests"], 1);
    }

    #[test]
    fn a_reused_id_does_not_alias_a_different_session_file() {
        let env = TestEnv::new();
        let one = SessionRef {
            id: "same".into(),
            path: env.work().join("one.jsonl"),
        };
        let two = SessionRef {
            id: "same".into(),
            path: env.work().join("two.jsonl"),
        };
        let writer = StatusWriter::new(None);
        writer.notify(
            &one,
            Progress::Session {
                file: Some(&one.path),
            },
        );
        writer.notify(
            &one,
            Progress::RequestStart {
                kind: TurnKind::Prefill,
                remote: false,
            },
        );
        writer.notify(
            &two,
            Progress::Session {
                file: Some(&two.path),
            },
        );
        let before = fs::read(two.path.with_extension("status.json")).unwrap();
        writer.notify(
            &one,
            Progress::RequestEnd {
                usage: Some(Usage::default()),
                remote: false,
            },
        );
        assert_eq!(fs::read(two.path.with_extension("status.json")).unwrap(), before);
        assert_eq!(
            read_snapshot(&two.path.with_extension("status.json")).unwrap()["session_id"],
            "same"
        );
        assert_eq!(
            read_snapshot(&one.path.with_extension("status.json")).unwrap()["phase"],
            "finished"
        );
    }

    #[test]
    fn readers_observe_whole_snapshots_during_atomic_replacement() {
        let env = TestEnv::new();
        let path = env.work().join("status.json");
        atomic(&path, br#"{"step":0}"#).unwrap();
        std::thread::scope(|scope| {
            let writer = scope.spawn(|| {
                for step in 1..50 {
                    atomic(&path, &serde_json::to_vec(&json!({"step":step})).unwrap()).unwrap();
                }
            });
            while !writer.is_finished() {
                assert!(read_snapshot(&path).unwrap()["step"].as_u64().unwrap() <= 50);
            }
            writer.join().unwrap();
        });
        assert_eq!(read_snapshot(&path).unwrap()["step"], 49);
    }

    #[test]
    fn closing_is_active_and_late_helpers_cannot_reopen_a_final_record() {
        let env = TestEnv::new();
        let session = SessionRef {
            id: "one".into(),
            path: env.work().join("one.jsonl"),
        };
        let path = session.path.with_extension("status.json");
        let writer = StatusWriter::new(Some(Duration::from_secs(1)));
        writer.notify(
            &session,
            Progress::Session {
                file: Some(&session.path),
            },
        );
        writer.notify(
            &session,
            Progress::RequestStart {
                kind: TurnKind::Prefill,
                remote: false,
            },
        );
        writer.notify(&session, Progress::Closing);
        let closing = read_snapshot(&path).unwrap();
        assert_eq!(closing["phase"], "closing");
        assert!(closing["stall_deadline_ms"].as_u64().is_some());
        assert!(closing["outcome"].is_null());
        writer.finish(Ok(RunOutcome::Done), &mut Vec::new()).unwrap();
        let final_bytes = fs::read(&path).unwrap();
        writer.notify(
            &session,
            Progress::RequestEnd {
                usage: Some(Usage::default()),
                remote: false,
            },
        );
        writer.notify(&session, Progress::Advanced);
        assert_eq!(fs::read(&path).unwrap(), final_bytes);
        assert_eq!(read_snapshot(&path).unwrap()["tokens"]["complete"], false);
    }
}
