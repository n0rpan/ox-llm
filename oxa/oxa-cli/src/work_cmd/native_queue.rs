//! Explicit one-shot execution of an existing, fully bound native board packet.
use super::{protected_storage, Context};
use oxa_work::{
    board::{assignment::Worker, Claims},
    LockedTask, NativeQueueOwner, QueueOptions, QueueRun, Task,
};
use serde_json::Value;
use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::Arc,
};

const MAX_CONFIG_BYTES: u64 = 64 * 1024;

fn paused(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err("cannot establish board pause state".into()),
    }
}

pub(super) fn run(
    context: Arc<Context>,
    common: &Path,
    config: &Path,
    out: &mut dyn Write,
) -> Result<i32, String> {
    if context.env.pairs().iter().any(|(name, _)| {
        matches!(
            name.as_str(),
            "OXA_CODING_AGENT" | "AI_AGENT" | "OXA_SUBAGENT_DEPTH"
        )
    }) {
        return Err("native queue execution requires the owner outside an agent tool".into());
    }
    let root = &context.cwd;
    let config = config.canonicalize().map_err(|_| "board config unavailable")?;
    let owner = context
        .dirs
        .agent_dir
        .canonicalize()
        .map_err(|_| "owner controls unavailable")?;
    if !config.starts_with(&owner) {
        return Err("board config must be in protected owner controls".into());
    }
    protected_storage(&config, root, common)?;
    let uid = fs::metadata(&context.dirs.home)
        .map_err(|_| "owner home unavailable")?
        .uid();
    let mut bytes = Vec::new();
    crate::lead_input::open(&config, uid)?
        .take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "cannot read native board configuration")?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        return Err("native board configuration exceeds capacity".into());
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| "invalid native board configuration")?;
    if value["enabled"] != true || value["nativeExecution"] != true {
        return Ok(0);
    }
    let owner_path = |key: &str| -> Result<PathBuf, String> {
        let path = PathBuf::from(value[key].as_str().ok_or("missing native board path")?);
        protected_storage(&path, root, common)?;
        Ok(path)
    };
    // The legacy check supervisor does not yet inherit the model worker's
    // runner-owned GitHub policy. Offline mode also masks credentials and
    // fences deterministic checkout, acceptance and save subprocesses.
    if !context.policy.offline {
        return Err("native queue execution currently requires --offline".into());
    }
    let source = value["nativeRoot"].as_str().ok_or("missing native source root")?;
    if Path::new(source) != root {
        return Err("native board configuration belongs to a different source root".into());
    }
    let pause = owner_path("pause")?;
    if paused(&pause)? {
        return Ok(0);
    }
    let directory = owner_path("queue")?;
    let claims = Claims::open(&owner_path("claims")?)?;
    let worker: Worker =
        serde_json::from_value(value["worker"].clone()).map_err(|_| "invalid authenticated worker export")?;
    if !oxa_work::board::assignment::valid_worker_id(&worker.id) {
        return Err("invalid native worker identity".into());
    }
    let rows = claims.assignments()?;
    let mut owned = rows.into_values().filter(|row| {
        row.state
            .lease
            .as_ref()
            .is_some_and(|lease| lease.holder == worker.id)
    });
    let Some(row) = owned.next() else { return Ok(0) };
    if owned.next().is_some() {
        return Err("native worker has multiple active assignments".into());
    }
    let binding = row
        .data
        .native_binding
        .filter(|binding| row.state.lease.as_ref() == Some(&binding.lease))
        .ok_or("native worker lacks an exact retained binding")?;
    let owner = NativeQueueOwner {
        claims: &claims,
        worker_id: &worker.id,
    };
    let policy = &context.policy;
    let df = crate::netguard::trusted_executable_for(
        "df",
        Some(&policy.host_path),
        Some(root),
        Some(&context.dirs.home),
    )
    .ok_or("no trusted disk-space executable outside the source project")?;
    let worker_context = Arc::clone(&context);
    let run = Arc::new(move |task: &Task, lock: &mut LockedTask<'_>| {
        if paused(&pause)? {
            return Err("native board execution paused".into());
        }
        match lock.plan_native(&worker_context.cwd, worker_context.reports) {
            Ok(plan) => worker_context.planned_locked(&plan, lock),
            Err(error) => Ok(oxa_work::blocked_record(task, error)),
        }
    });
    let options = QueueOptions {
        once: true,
        ..QueueOptions::default()
    };
    let summary = QueueRun {
        repo: root,
        directory: &directory,
        options: &options,
        policy,
        df: &df,
    }
    .run_native_packet(&owner, &binding, run, &mut |event| {
        serde_json::to_writer(&mut *out, &event).map_err(|e| e.to_string())?;
        writeln!(out).and_then(|_| out.flush()).map_err(|e| e.to_string())
    })?;
    Ok(i32::from(summary.failed > 0 || summary.pending > 0))
}
