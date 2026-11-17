//! Owner-selected queue frontend. Existing Reader, Claims, publisher and private-file IO only.
use super::refill_input::{Input, Prepared, text};
use super::*;
use crate::work_refill::{Consumer, Packet, Partition};
use oxa_work::board::{Action, assignment::Worker};
use std::{fs, os::unix::fs::MetadataExt, path::PathBuf};

fn private(path: &Path, uid: u32) -> Result<Value, String> {
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

pub(super) fn run(
    args: &[String],
    root: &Path,
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
    if !crate::work_cmd::trusted(root, env, dirs)? {
        return Err("source project is not trusted".into());
    }
    let config = Path::new(config)
        .canonicalize()
        .map_err(|_| "board config unavailable")?;
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
    let value = private(&config, uid)?;
    if value["enabled"] != true {
        return Ok(0);
    }
    let owner_path = |key| -> Result<PathBuf, String> {
        let path = PathBuf::from(text(&value, key)?);
        if !path.is_absolute() || path.starts_with(root) {
            return Err("board state must be outside the source project".into());
        }
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
    let worker: Worker =
        serde_json::from_value(value["worker"].clone()).map_err(|_| "invalid authenticated worker export")?;
    let partition = Partition {
        modulus: value["partition"]["modulus"]
            .as_u64()
            .ok_or("missing partition modulus")?,
        residues: serde_json::from_value(value["partition"]["residues"].clone())
            .map_err(|_| "invalid partition residues")?,
    };
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
        if task.len() > 65_536
            || !matches!(header.len(), 3 | 4)
            || header[2] != branch
            || header.get(3).copied().unwrap_or("main")
                != if action == Action::BuildIssue {
                    "main"
                } else {
                    branch
                }
        {
            return Err("prepared taskfile branch/base mismatch".into());
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
    let mut source = Input::new(
        &gh,
        &policy.host_path,
        repo,
        &token,
        &packets,
        value["intake"] == true,
    );
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(|_| "invalid system clock")?
        .as_secs();
    let policy = Policy {
        enabled: true,
        ..Policy::default()
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
    consumer.once(&mut source, &inputs, now, |board| {
        oxa_work::shadow::run(&mut log, true, || Ok(board.board.clone()), |_| {})
    })?;
    Ok(0)
}
