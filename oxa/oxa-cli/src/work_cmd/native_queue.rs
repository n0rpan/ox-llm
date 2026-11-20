//! Explicit one-shot execution of an existing, fully bound native board packet.
use super::{protected_storage, registered_native_worker, Context};
use oxa_work::{
    board::{Assignment, Claims, ItemLease, ItemOwner, LeaseSignal, NativeBinding},
    CheckpointKind, LockedTask, NativeQueueOwner, QueueOptions, QueueRun, Record, Task,
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

pub(crate) fn authenticate_native_claim<'a>(
    row: &'a Assignment,
    host: &str,
) -> Result<(&'a NativeBinding, &'a ItemOwner), String> {
    if row.data.import_hold {
        return Err("native claim is held for import".into());
    }
    let owner = row
        .data
        .item_owner
        .as_ref()
        .filter(|owner| owner.host_id == host)
        .ok_or("native claim does not belong to the authenticated host")?;
    let binding = row
        .data
        .native_binding
        .as_ref()
        .filter(|binding| binding.number == row.number && row.state.lease.as_ref() == Some(&binding.lease))
        .ok_or("native worker lacks an exact retained binding")?;
    binding.validate()?;
    Ok((binding, owner))
}

fn runnable_claim<'a>(
    rows: impl Iterator<Item = &'a Assignment>,
    worker: &str,
    host: &str,
) -> Result<Option<(&'a NativeBinding, &'a ItemOwner)>, String> {
    let now = oxa_proto::time::now_ms() / 1000;
    let mut runnable = None;
    for row in rows.filter(|row| {
        row.state
            .lease
            .as_ref()
            .is_some_and(|lease| lease.holder == worker)
    }) {
        let (binding, owner) = authenticate_native_claim(row, host)?;
        if row.data.dependency.is_some() {
            continue;
        }
        if now >= owner.expires_at {
            return Err("native assignment owner has expired".into());
        }
        if runnable.replace((binding, owner)).is_some() {
            return Err("native worker has multiple runnable assignments".into());
        }
    }
    Ok(runnable)
}

pub(super) fn run(
    mut context: Context,
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
    // The existing supervisor receives the runner's offline switch. Keep
    // credential mounts and network off for deterministic steps until the
    // supervisor also inherits the worker's runner-owned GitHub policy.
    context.policy.offline = true;
    let context = Arc::new(context);
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
    let activity = match value.get("nativeActivity") {
        None => false,
        Some(value) => value.as_bool().ok_or("nativeActivity must be a boolean")?,
    };
    let steering = value
        .get("nativeSteering")
        .map(|setting| {
            let attempt = setting["attempt"]
                .as_str()
                .ok_or("missing native steering attempt")?;
            let objective = setting["objective"]
                .as_str()
                .filter(|text| !text.trim().is_empty() && text.len() <= 2048)
                .ok_or("invalid native steering objective")?;
            Ok::<_, &str>((attempt, objective))
        })
        .transpose()?;
    if steering.is_some()
        && (!cfg!(target_os = "linux")
            || !oxa_config::Settings::load(&context.dirs, &context.cwd, false).a2a_enabled())
    {
        return Err(
            "native steering is unavailable: it needs Linux and oxa.a2a.enabled set to true in the user settings"
                .into(),
        );
    }
    let owner_path = |key: &str| -> Result<PathBuf, String> {
        let path = PathBuf::from(value[key].as_str().ok_or("missing native board path")?);
        protected_storage(&path, root, common)?;
        Ok(path)
    };
    let pause = owner_path("pause")?;
    if paused(&pause)? {
        return Ok(0);
    }
    let directory = owner_path("queue")?;
    let claims_path = owner_path("claims")?;
    let claims = Claims::open(&claims_path)?;
    let (worker, host) = registered_native_worker(&value)?;
    let (binding, item_owner) = {
        let rows = claims.assignments()?;
        let Some((binding, item_owner)) = runnable_claim(rows.values(), &worker.id, host)? else {
            return Ok(0);
        };
        (binding.clone(), item_owner.clone())
    };
    if steering.is_some_and(|(attempt, _)| attempt != binding.native_task_id) {
        return Err("native steering objective belongs to a different attempt".into());
    }
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
    let activity_receipt = activity.then(|| ItemLease {
        lease: binding.lease.clone(),
        host_id: item_owner.host_id,
        expires_at: item_owner.expires_at,
    });
    let activity_claims = claims.clone();
    let activity_binding = binding.clone();
    let observe = move |record: &Record| {
        let (Some(receipt), Some(revision)) = (&activity_receipt, record.worker_launch_revision) else {
            return Ok(());
        };
        if record.checkpoint_kind == CheckpointKind::WorkerFinished
            && record.id == activity_binding.native_task_id
            && record.branch == activity_binding.branch
        {
            activity_claims.observe_item_for(
                &receipt.lease.holder,
                &receipt.host_id,
                &activity_binding,
                receipt,
                oxa_proto::time::now_ms() / 1000,
                LeaseSignal::Milestone(record, revision),
            )?;
        }
        Ok(())
    };
    #[cfg(target_os = "linux")]
    let steering_objective = steering.map(|(_, objective)| objective.to_owned());
    #[cfg(target_os = "linux")]
    let steering_binding = binding.clone();
    let admission_claims = claims.clone();
    let admission_binding = binding.clone();
    let admission_host = host.to_owned();
    let run = Arc::new(move |task: &Task, lock: &mut LockedTask<'_>| {
        if paused(&pause)? {
            return Err("native board execution paused".into());
        }
        {
            let rows = admission_claims.assignments()?;
            let current = runnable_claim(rows.values(), &admission_binding.lease.holder, &admission_host)?;
            if !current.is_some_and(|(binding, _)| binding == &admission_binding) {
                return Err("native claim is no longer runnable".into());
            }
        }
        #[cfg(target_os = "linux")]
        if let Some(objective) = &steering_objective {
            lock.bind_managed_steering(&claims_path, &steering_binding, objective)?;
        }
        match lock.plan_native(&worker_context.cwd, worker_context.reports, &admission_binding) {
            Ok(plan) => worker_context.planned_locked(&plan, lock, &observe),
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
