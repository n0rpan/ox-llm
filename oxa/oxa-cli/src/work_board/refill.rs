//! Owner-selected queue frontend. Existing Reader, Claims, publisher and private-file IO only.
use super::refill_input::{text, Input, Prepared};
use super::*;
use crate::work_refill::{Consumer, Packet, Partition};
use oxa_work::board::Action;
use std::{fs, io::Write, os::unix::fs::MetadataExt, path::PathBuf};

pub(super) fn private(path: &Path, uid: u32) -> Result<Value, String> {
    let mut bytes = Vec::new();
    crate::lead_input::open(path, uid)?
        .take(65_537)
        .read_to_end(&mut bytes)
        .map_err(|_| "cannot read private board input")?;
    if bytes.len() > 65_536 {
        return Err("board input exceeds capacity".into());
    }
    serde_json::from_slice(&bytes).map_err(|_| "invalid private board input".into())
}

pub(super) fn configuration(
    config: &Path,
    root: &Path,
    env: &oxa_config::Env,
    dirs: &oxa_config::Dirs,
) -> Result<(Value, PathBuf, u32), String> {
    if !crate::work_cmd::trusted(root, env, dirs)? {
        return Err("source project is not trusted".into());
    }
    let config = config.canonicalize().map_err(|_| "board config unavailable")?;
    let authority = dirs
        .agent_dir
        .canonicalize()
        .map_err(|_| "owner controls unavailable")?;
    if config.starts_with(root) || !config.starts_with(&authority) {
        return Err("board config must be in protected owner controls outside the source project".into());
    }
    let uid = fs::metadata(&dirs.home)
        .map_err(|_| "owner home unavailable")?
        .uid();
    Ok((private(&config, uid)?, authority, uid))
}

pub(super) fn partition(value: &Value) -> Result<Partition, String> {
    Ok(Partition {
        modulus: value["partition"]["modulus"]
            .as_u64()
            .ok_or("missing partition modulus")?,
        residues: serde_json::from_value(value["partition"]["residues"].clone())
            .map_err(|_| "invalid partition residues")?,
    })
}

pub(super) fn attempt_limit(value: &Value) -> Result<u32, String> {
    let mut value = value;
    for field in ["budget_pools", "limits", "max_attempts_per_day"] {
        let object = value.as_object().ok_or("invalid owner attempt policy")?;
        let Some(next) = object.get(field) else {
            return Ok(Policy::default().max_attempts_per_day);
        };
        value = next;
    }
    serde_json::from_value(value.clone()).map_err(|_| "invalid owner attempt limit".into())
}

pub(super) fn run(
    args: &[String],
    root: &Path,
    common: &Path,
    env: &oxa_config::Env,
    dirs: &oxa_config::Dirs,
    policy: &oxa_work::RunPolicy,
) -> Result<i32, String> {
    // #1062 owns command declarations; WorkBoard's existing Tail seam retains handler validation.
    let [mode, repo_flag, repo, once, token_flag, config_flag, config] = args else {
        return Err(
            "usage: oxa work board --refill --repo OWNER/REPO --once --token-stdin --config PATH".into(),
        );
    };
    if mode != "--refill"
        || repo_flag != "--repo"
        || once != "--once"
        || token_flag != "--token-stdin"
        || config_flag != "--config"
    {
        return Err("invalid board refill options".into());
    }
    if repo.split('/').count() != 2
        || repo.split('/').any(|part| {
            part.is_empty()
                || matches!(part, "." | "..")
                || !part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        })
    {
        return Err("expected OWNER/REPO".into());
    }
    if policy.offline {
        return Err("GitHub board is unavailable in offline mode".into());
    }
    let (value, authority, uid) = configuration(Path::new(config), root, env, dirs)?;
    if value["enabled"] != true {
        return Ok(0);
    }
    let owner_path = |key| -> Result<PathBuf, String> {
        let path = PathBuf::from(text(&value, key)?);
        crate::work_cmd::protected_storage(&path, root, common)?;
        Ok(path)
    };
    let queue = owner_path("queue")?;
    let pause = owner_path("pause")?;
    match fs::symlink_metadata(&pause) {
        Ok(_) => return Ok(0),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(_) => return Err("cannot establish board pause state".into()),
    }
    let claims = oxa_work::board::Claims::open(&owner_path("claims")?)?;
    let (worker, host) = crate::work_cmd::registered_native_worker(&value)?;
    let partition = partition(&value)?;
    let rows = value["packets"]
        .as_array()
        .filter(|rows| rows.len() <= 20)
        .ok_or("expected bounded prepared packets")?;
    let mut packets = vec![];
    let mut seen = BTreeSet::new();
    for row in rows {
        let number = row["number"]
            .as_u64()
            .filter(|n| *n > 0 && seen.insert(*n))
            .ok_or("invalid or duplicate prepared ref")?;
        let action: Action =
            serde_json::from_value(row["action"].clone()).map_err(|_| "invalid prepared action")?;
        let path = Path::new(text(row, "taskfile")?)
            .canonicalize()
            .map_err(|_| "taskfile unavailable")?;
        if path.starts_with(root) || !path.starts_with(&authority) {
            return Err("taskfile must be in protected owner controls outside source project".into());
        }
        let mut task = String::new();
        crate::lead_input::open(&path, uid)?
            .take(65_537)
            .read_to_string(&mut task)
            .map_err(|_| "cannot read prepared taskfile")?;
        let header: Vec<_> = task
            .lines()
            .next()
            .unwrap_or("")
            .split_ascii_whitespace()
            .collect();
        let branch = text(row, "branch")?;
        let task_id = format!("{}{number}", if action == Action::BuildIssue { "i" } else { "c" });
        if task.len() > 65_536
            || !matches!(header.len(), 3 | 4)
            || header[0] != task_id
            || header[2] != branch
            || header.get(3).copied().unwrap_or("main")
                != if action == Action::BuildIssue {
                    "main"
                } else {
                    branch
                }
        {
            return Err("prepared taskfile ref/branch/base mismatch".into());
        }
        packets.push(Prepared {
            number,
            revision: text(row, "revision")?.into(),
            action,
            branch: branch.into(),
            task,
        });
    }
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        return Err("App token input requires a private pipe, not a terminal".into());
    }
    let token = token_line(&mut stdin.lock())?;
    let gh =
        crate::netguard::trusted_executable_for("gh", Some(&policy.host_path), Some(root), Some(&dirs.home))
            .ok_or("no trusted gh executable")?;
    let intake = value["intake"] == true;
    let mut source = Input::new(&gh, &policy.host_path, repo, &token, &packets, intake).with_host(host);
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(|_| "invalid system clock")?
        .as_secs();
    let policy = Policy {
        enabled: true,
        max_attempts_per_day: attempt_limit(&value)?,
    };
    let consumer = Consumer {
        repository: repo,
        queue: &queue,
        pause: &pause,
        claims: &claims,
        worker: &worker,
        partition: &partition,
        policy: &policy,
        ttl: 1800,
    };
    let inputs: Vec<_> = packets
        .iter()
        .map(|p| Packet {
            number: p.number,
            revision: &p.revision,
            action: p.action,
            task: &p.task,
        })
        .collect();
    let mut log = oxa_work::shadow::open_log(&owner_path("log")?)?;
    let receipts = consumer.once(
        &mut source,
        &inputs,
        now,
        |board| oxa_work::shadow::run(&mut log, true, || Ok(board.board.clone()), |_| {}),
        |receipt| {
            let prepared = packets
                .iter()
                .find(|packet| packet.number == receipt.number)
                .ok_or("native admission packet unavailable")?;
            let mut binding = oxa_work::board::NativeBinding::new(
                receipt.number,
                &receipt.key,
                &receipt.lease,
                &prepared.branch,
            )?;
            binding.reviewed_head =
                (prepared.action != Action::BuildIssue).then(|| prepared.revision.clone());
            binding.validate()?;
            if !claims.bind_native(&worker.id, &binding)? {
                return Err("native admission binding refused".into());
            }
            Ok(())
        },
    )?;
    // One operator warning per claimed receipt, once per claim, on stderr: the
    // refill's stdout stays machine-only. The notice is best-effort like every
    // other diagnostic write: a broken sink cannot fail an admission whose
    // claim and packet are already durable.
    let mut err = std::io::stderr().lock();
    for receipt in &receipts {
        if let Some(advisory) = &receipt.advisory {
            let _ = writeln!(err, "{}", advisory.line(receipt.number));
        }
    }
    Ok(0)
}
