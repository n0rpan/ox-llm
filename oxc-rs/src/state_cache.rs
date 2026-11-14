//! Read-only, descriptor-anchored inspection of the disk conversation store.
use crate::paths;
use nix::{
    fcntl::{openat, OFlag},
    sys::stat::Mode,
};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Read},
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
const DEFAULT_BUDGET: u64 = 64_000_000_000;
const DEFAULT_TTL: u64 = 604800;
fn valid_budget(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .filter(|v| *v == 0 || (4_000_000_000..=1 << 50).contains(v))
}
fn valid_ttl(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .filter(|v| (60..=10 * 365 * 86400).contains(v))
}
pub struct Settings {
    budget: u64,
    ttl: u64,
    invalid: Vec<&'static str>,
}
impl Settings {
    pub fn new(config: &Value) -> Self {
        let mut invalid = Vec::new();
        let cache = &config["cache"];
        let budget = cache
            .get("disk_budget_bytes")
            .map(|v| {
                valid_budget(v).unwrap_or_else(|| {
                    invalid.push("disk_budget_bytes");
                    DEFAULT_BUDGET
                })
            })
            .unwrap_or(DEFAULT_BUDGET);
        let ttl = cache
            .get("ttl_secs")
            .map(|v| {
                valid_ttl(v).unwrap_or_else(|| {
                    invalid.push("ttl_secs");
                    DEFAULT_TTL
                })
            })
            .unwrap_or(DEFAULT_TTL);
        Self {
            budget,
            ttl,
            invalid,
        }
    }
}
pub fn root(config: &Value) -> io::Result<PathBuf> {
    let home = paths::home();
    let invalid = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "state_cache_root_override_invalid",
        )
    };
    let environment = std::env::var_os("LOCAL_AI_STATE_CACHE_ROOT").filter(|s| !s.is_empty());
    let raw = environment
        .map(|v| v.into_string().map_err(|_| invalid()))
        .transpose()?
        .map(Value::String)
        .or_else(|| config["cache"].get("root").cloned());
    let base = if let Some(raw) = raw.filter(|v| !v.is_null()) {
        let value = raw
            .as_str()
            .filter(|v| !v.is_empty() && !v.chars().any(|c| (c as u32) < 32))
            .ok_or_else(invalid)?;
        let path = PathBuf::from(value);
        let real_home = paths::realpath(&home)?;
        let real = paths::realpath(&path)?;
        if !path.is_absolute()
            || !home.is_absolute()
            || real == real_home
            || !real.starts_with(&real_home)
        {
            return Err(invalid());
        }
        path
    } else {
        home.join(".cache/local-ai/snapdragon-engine")
    };
    let path = base.join("state");
    let real_home = paths::realpath(&home)?;
    let real = paths::realpath(&path)?;
    if !home.is_absolute() || real == real_home || !real.starts_with(&real_home) {
        return Err(invalid());
    }
    Ok(path)
}
fn hex(text: &str, len: usize) -> bool {
    text.len() == len
        && text
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
fn sid(text: &str) -> bool {
    (1..=128).contains(&text.len())
        && text
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
}
fn pending(name: &str) -> bool {
    name.strip_suffix(".pending").is_some_and(|s| {
        (1..=240).contains(&s.len())
            && s.bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
    })
}
fn session(name: &str) -> Option<&str> {
    name.strip_suffix(".json").filter(|s| sid(s))
}
fn snap(name: &str) -> bool {
    name.strip_suffix(".st")
        .and_then(|s| s.rsplit_once('.'))
        .is_some_and(|(name, checksum)| sid(name) && hex(checksum, 16))
}
fn block(name: &str, prefix: &str) -> bool {
    name.strip_suffix(".kv")
        .and_then(|s| s.split_once('.'))
        .is_some_and(|(hash, checksum)| {
            hex(hash, 64) && hex(checksum, 16) && hash.starts_with(prefix)
        })
}
fn valid_rel(rel: &str) -> bool {
    let parts = rel.split('/').collect::<Vec<_>>();
    match parts.as_slice() {
        [name] => *name == "identity.json" || pending(name),
        ["snaps", name] => snap(name) || pending(name),
        ["sessions", name] => session(name).is_some() || pending(name),
        ["echo", name] => name.strip_suffix(".jsonl").is_some_and(sid) || pending(name),
        ["blocks", prefix, name] => hex(prefix, 2) && (block(name, prefix) || pending(name)),
        _ => false,
    }
}
fn open_dir(path: &Path, parent: Option<&File>) -> io::Result<File> {
    let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
    match parent {
        Some(parent) => openat(parent, path, flags, Mode::empty())
            .map(File::from)
            .map_err(io::Error::from),
        None => OpenOptions::new()
            .read(true)
            .custom_flags(flags.bits())
            .open(path),
    }
}
fn descriptor_path(dir: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()))
}
#[derive(Clone)]
struct Entry {
    size: u64,
    mtime: f64,
}
struct Session {
    atime: f64,
}
struct Model {
    files: BTreeMap<String, Entry>,
    sessions: BTreeMap<String, Session>,
}
fn walk(dir: &File, prefix: &str, depth: u8, out: &mut BTreeMap<String, Entry>) -> io::Result<()> {
    for entry in fs::read_dir(descriptor_path(dir))? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(info) = fs::symlink_metadata(entry.path()) else {
            continue;
        };
        let rel = format!("{prefix}{name}");
        if info.is_file() && valid_rel(&rel) {
            out.insert(
                rel,
                Entry {
                    size: info.len(),
                    mtime: info.mtime() as f64 + info.mtime_nsec() as f64 / 1e9,
                },
            );
        } else if info.is_dir()
            && depth > 0
            && (["snaps", "sessions", "echo", "blocks"].contains(&name.as_str())
                || (prefix == "blocks/" && hex(&name, 2)))
        {
            if let Ok(child) = open_dir(Path::new(&name), Some(dir)) {
                walk(&child, &format!("{rel}/"), depth - 1, out)?;
            }
        }
    }
    Ok(())
}
fn manifest(dir: &File, name: &str) -> Option<Session> {
    let sessions = open_dir(Path::new("sessions"), Some(dir)).ok()?;
    let fd = openat(
        &sessions,
        Path::new(name),
        OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_NONBLOCK,
        Mode::empty(),
    )
    .ok()?;
    let file = File::from(fd);
    let info = file.metadata().ok()?;
    if !info.is_file() || info.len() > 8 << 20 {
        return None;
    }
    let mut raw = Vec::new();
    file.take((8 << 20) + 1).read_to_end(&mut raw).ok()?;
    let value: Value = serde_json::from_slice(&raw).ok()?;
    if value["version"] != 1 && value["version"] != true {
        return None;
    }
    let blocks = value["blocks"].as_array()?;
    for block in blocks {
        let row = block.as_array()?;
        if row.len() != 2
            || !row[0].as_str().is_some_and(|s| hex(s, 64))
            || !row[1].as_str().is_some_and(|s| hex(s, 16))
        {
            return None;
        }
    }
    if let Some(snaps) = value.get("snaps") {
        for row in snaps.as_array()? {
            let key = row["key"].as_str()?;
            let checksum = row["c"].as_str()?;
            if !snap(&format!("{key}.{checksum}.st")) {
                return None;
            }
        }
    }
    let atime = value["atime"].as_f64().filter(|v| v.is_finite())?;
    Some(Session { atime })
}
fn scan(path: &Path) -> io::Result<BTreeMap<String, Model>> {
    let root = match open_dir(path, None) {
        Ok(dir) => dir,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(error),
    };
    let mut models = BTreeMap::new();
    for entry in fs::read_dir(descriptor_path(&root))? {
        let entry = entry?;
        let Some(key) = entry
            .file_name()
            .to_str()
            .filter(|s| hex(s, 16))
            .map(str::to_owned)
        else {
            continue;
        };
        let Ok(dir) = open_dir(Path::new(&key), Some(&root)) else {
            continue;
        };
        let mut files = BTreeMap::new();
        walk(&dir, "", 2, &mut files)?;
        let mut sessions = BTreeMap::new();
        for rel in files.keys() {
            if let Some(name) = rel
                .strip_prefix("sessions/")
                .filter(|name| session(name).is_some())
            {
                if let Some(parsed) = manifest(&dir, name) {
                    sessions.insert(session(name).expect("session name").to_owned(), parsed);
                }
            }
        }
        models.insert(key, Model { files, sessions });
    }
    Ok(models)
}
pub fn usage(config: &Value) -> io::Result<Value> {
    let settings = Settings::new(config);
    let models = scan(&root(config)?)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_secs_f64();
    let mut per = BTreeMap::new();
    let mut atimes = Vec::new();
    for (key, model) in models {
        per.insert(
            key,
            model
                .files
                .iter()
                .filter(|(rel, e)| !rel.ends_with(".pending") || now - e.mtime > 600.0)
                .map(|(_, e)| e.size)
                .sum::<u64>(),
        );
        atimes.extend(model.sessions.values().map(|s| s.atime));
    }
    let oldest = atimes.iter().copied().min_by(f64::total_cmp);
    let newest = atimes.iter().copied().max_by(f64::total_cmp);
    // Preserve integer-valued timestamps as integers, like the manifest JSON.
    let timestamp = |v: Option<f64>| {
        v.map(|n| {
            if n.fract() == 0.0 && n >= i64::MIN as f64 && n < (i64::MAX as f64) {
                json!(n as i64)
            } else {
                json!(n)
            }
        })
        .unwrap_or(Value::Null)
    };
    Ok(
        json!({"budget_bytes":settings.budget,"ttl_secs":settings.ttl,"used_bytes":per.values().sum::<u64>(),"models":per,"sessions":{"count":atimes.len(),"oldest_atime":timestamp(oldest),"newest_atime":timestamp(newest)},"invalid_settings":settings.invalid}),
    )
}
