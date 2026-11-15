//! Descriptor-anchored inspection and lease-protected disk conversation eviction.
use crate::paths;
use nix::{
    fcntl::{openat, OFlag},
    sys::stat::Mode,
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
const DEFAULT_BUDGET: u64 = 64_000_000_000;
const DEFAULT_TTL: u64 = 604800;
pub fn valid_budget(value: &Value) -> Option<u64> {
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
    pub budget: u64,
    pub ttl: u64,
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
    let environment = std::env::var_os("OXC_STATE_CACHE_ROOT").filter(|s| !s.is_empty());
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
pub fn overridden(config: &Value) -> bool {
    std::env::var_os("OXC_STATE_CACHE_ROOT").is_some_and(|s| !s.is_empty())
        || !config["cache"]["root"].is_null()
}
pub fn conversation_args(config: &Value, args: Vec<String>) -> io::Result<Vec<String>> {
    let base = if overridden(config) {
        Some(root(config)?.parent().expect("state parent").to_owned())
    } else {
        None
    };
    let off = !config["oxe_flash"].is_object() && Settings::new(config).budget == 0;
    let mut out = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        let (name, equals) = arg
            .split_once('=')
            .map(|(name, _)| (name, true))
            .unwrap_or((arg.as_str(), false));
        let store = match name {
            "--prefix-cache" => Some("prefix"),
            "--session-cache" => Some("sessions"),
            _ => None,
        };
        let option = !config["oxe_flash"].is_object()
            && [
                "--prefix-cache-max-bytes",
                "--prefix-cache-min-tokens",
                "--prefix-cache-ttl-h",
            ]
            .contains(&name);
        if off && (store.is_some() || option) {
            index += if equals { 1 } else { 2 };
            continue;
        }
        if let (Some(store), Some(base)) = (store, base.as_ref()) {
            let target = base.join(store).to_string_lossy().into_owned();
            if equals {
                out.push(format!("{name}={target}"));
            } else {
                out.extend([name.into(), target]);
            }
            index += if equals { 1 } else { 2 };
            continue;
        }
        out.push(arg.clone());
        index += 1;
    }
    Ok(out)
}
pub fn make_private(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let mut missing = Vec::new();
    let mut current = path;
    while fs::symlink_metadata(current).is_err_and(|e| e.kind() == io::ErrorKind::NotFound) {
        missing.push(current.to_owned());
        let Some(parent) = current.parent().filter(|p| *p != current) else {
            break;
        };
        current = parent;
    }
    for path in missing.into_iter().rev() {
        fs::DirBuilder::new().mode(0o700).create(&path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    let home = paths::realpath(&paths::home())?;
    let real = paths::realpath(path)?;
    if real == home || !real.starts_with(home) || !path.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "state_cache_root_override_invalid",
        ));
    }
    Ok(())
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
    refs: BTreeSet<String>,
    hint: Option<String>,
    pin: bool,
    tokens: u64,
    owners: Vec<String>,
}
struct Model {
    files: BTreeMap<String, Entry>,
    sessions: BTreeMap<String, Session>,
    opaque: bool,
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
fn read_manifest(dir: &File, name: &str) -> Option<Value> {
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
    serde_json::from_slice(&raw).ok()
}
fn manifest(dir: &File, name: &str) -> Option<Session> {
    let value = read_manifest(dir, name)?;
    parse_manifest(&value)
}
fn parse_manifest(value: &Value) -> Option<Session> {
    // Match Python's numeric equality for the persisted version field.
    if value["version"].as_f64() != Some(1.0) && value["version"] != true {
        return None;
    }
    let blocks = value["blocks"].as_array()?;
    let mut refs = BTreeSet::new();
    for block in blocks {
        let row = block.as_array()?;
        if row.len() != 2
            || !row[0].as_str().is_some_and(|s| hex(s, 64))
            || !row[1].as_str().is_some_and(|s| hex(s, 16))
        {
            return None;
        }
        let hash = row[0].as_str()?;
        let checksum = row[1].as_str()?;
        refs.insert(format!("blocks/{}/{hash}.{checksum}.kv", &hash[..2]));
    }
    if let Some(snaps) = value.get("snaps") {
        for row in snaps.as_array()? {
            let key = row["key"].as_str()?;
            let checksum = row["c"].as_str()?;
            if !snap(&format!("{key}.{checksum}.st")) {
                return None;
            }
            refs.insert(format!("snaps/{key}.{checksum}.st"));
        }
    }
    let atime = value["atime"].as_f64().filter(|v| v.is_finite())?;
    let hint = value["hint"]
        .as_str()
        .filter(|s| hex(s, 32))
        .map(str::to_owned);
    let mut owners = Vec::new();
    let mut seen = BTreeSet::new();
    for owner in value["owners"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|s| hex(s, 32) || *s == "-")
    {
        if seen.insert(owner) {
            owners.push(owner.to_owned());
        }
    }
    if owners.is_empty() {
        owners.push(hint.clone().unwrap_or_else(|| "-".into()));
    }
    Some(Session {
        atime,
        refs,
        hint,
        pin: value["pin"] == true,
        tokens: value["n_tokens"].as_u64().unwrap_or(0),
        owners,
    })
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
        let mut opaque = false;
        for rel in files.keys() {
            if let Some(name) = rel
                .strip_prefix("sessions/")
                .filter(|name| session(name).is_some())
            {
                if let Some(parsed) = manifest(&dir, name) {
                    sessions.insert(session(name).expect("session name").to_owned(), parsed);
                } else {
                    opaque = true;
                }
            }
        }
        models.insert(
            key,
            Model {
                files,
                sessions,
                opaque,
            },
        );
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

fn shared(session: &Session) -> BTreeSet<String> {
    let mut refs = session.refs.clone();
    if let Some(hint) = session.hint.as_ref() {
        refs.insert(format!("echo/{hint}.jsonl"));
    }
    refs
}
fn model_bytes(model: &Model, now: f64) -> u64 {
    model
        .files
        .iter()
        .filter(|(rel, e)| !rel.ends_with(".pending") || now - e.mtime > 600.0)
        .map(|(_, e)| e.size)
        .sum()
}
fn file_size(model: &Model, rel: &str) -> u64 {
    model.files.get(rel).map(|e| e.size).unwrap_or(0)
}
fn plan(
    models: &BTreeMap<String, Model>,
    budget: u64,
    ttl: u64,
    now: f64,
) -> Vec<(String, String)> {
    let mut used = models.values().map(|m| model_bytes(m, now)).sum::<u64>();
    let mut refs = BTreeMap::<(String, String), usize>::new();
    for (key, model) in models {
        for session in model.sessions.values() {
            for rel in shared(session) {
                *refs.entry((key.clone(), rel)).or_default() += 1;
            }
        }
    }
    let mut expired = Vec::new();
    let mut rest = Vec::new();
    for (key, model) in models {
        for (id, session) in &model.sessions {
            let shared = shared(session);
            let own = file_size(model, &format!("sessions/{id}.json"));
            let exclusive = own
                + shared
                    .iter()
                    .filter(|rel| refs[&(key.clone(), (*rel).clone())] == 1)
                    .map(|rel| file_size(model, rel))
                    .sum::<u64>();
            let total = own + shared.iter().map(|rel| file_size(model, rel)).sum::<u64>();
            let age = (now - session.atime).max(0.0) / 3600.0;
            let tokens = session.tokens as f64;
            let value = (3.0 * tokens + 0.112 * tokens * tokens / 1000.0)
                / (if exclusive > 0 { exclusive } else { total }).max(1) as f64
                / (1.0 + age);
            if budget > 0 && (session.pin || now - session.atime < 600.0) {
                continue;
            }
            let row = (value, key.clone(), id.clone());
            if now - session.atime > ttl as f64 {
                expired.push(row)
            } else {
                rest.push(row)
            }
        }
    }
    rest.sort_by(|a, b| {
        a.0.total_cmp(&b.0)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2.cmp(&b.2))
    });
    let expired_count = expired.len();
    let mut victims = Vec::new();
    for (index, (_, key, id)) in expired.into_iter().chain(rest).enumerate() {
        if index >= expired_count && used <= budget {
            break;
        }
        let model = &models[&key];
        let session = &model.sessions[&id];
        used = used.saturating_sub(file_size(model, &format!("sessions/{id}.json")));
        for rel in shared(session) {
            let count = refs
                .get_mut(&(key.clone(), rel.clone()))
                .expect("session reference");
            *count -= 1;
            if *count == 0 {
                used = used.saturating_sub(file_size(model, &rel));
            }
        }
        victims.push((key, id));
    }
    victims
}
fn unlink(model: &File, rel: &str) -> u64 {
    use nix::unistd::{unlinkat, UnlinkatFlags};
    if !valid_rel(rel) {
        return 0;
    }
    let mut parts = rel.split('/').collect::<Vec<_>>();
    let name = parts.pop().expect("store name");
    let mut directories = Vec::new();
    for part in &parts {
        let parent = directories.last().unwrap_or(model);
        let Ok(child) = open_dir(Path::new(part), Some(parent)) else {
            return 0;
        };
        directories.push(child);
    }
    let parent = directories.last().unwrap_or(model);
    let Ok(info) = fs::symlink_metadata(descriptor_path(parent).join(name)) else {
        return 0;
    };
    if !info.is_file() || unlinkat(parent, name, UnlinkatFlags::NoRemoveDir).is_err() {
        return 0;
    }
    if matches!(parts.first().copied(), Some("sessions" | "echo")) {
        let _ = parent.sync_all();
    }
    info.len()
}
fn apply(
    path: &Path,
    models: &mut BTreeMap<String, Model>,
    victims: Vec<(String, String)>,
    now: f64,
    everything: &BTreeSet<String>,
) -> io::Result<u64> {
    let root = match open_dir(path, None) {
        Ok(root) => root,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let mut by_model = BTreeMap::<String, Vec<String>>::new();
    for (key, id) in victims {
        by_model.entry(key).or_default().push(id);
    }
    for key in everything {
        by_model.entry(key.clone()).or_default();
    }
    let mut freed = 0;
    for (key, ids) in by_model {
        let Ok(dir) = open_dir(Path::new(&key), Some(&root)) else {
            continue;
        };
        let model = models.get_mut(&key).expect("planned model");
        if everything.contains(&key) {
            for rel in model.files.keys() {
                freed += unlink(&dir, rel);
            }
            prune_dirs(&dir);
            let _ =
                nix::unistd::unlinkat(&root, key.as_str(), nix::unistd::UnlinkatFlags::RemoveDir);
            continue;
        }
        let mut released = BTreeSet::new();
        let mut hints = BTreeSet::new();
        for id in ids {
            freed += unlink(&dir, &format!("sessions/{id}.json"));
            if let Some(session) = model.sessions.remove(&id) {
                released.extend(session.refs);
                hints.insert(session.hint);
            }
        }
        if model.opaque {
            continue;
        }
        let alive = model
            .sessions
            .values()
            .map(|s| s.hint.clone())
            .collect::<BTreeSet<_>>();
        for hint in hints.difference(&alive).flatten() {
            freed += unlink(&dir, &format!("echo/{hint}.jsonl"));
        }
        let kept = model
            .sessions
            .values()
            .flat_map(|s| s.refs.iter())
            .collect::<BTreeSet<_>>();
        for (rel, entry) in &model.files {
            if (rel.starts_with("blocks/") || rel.starts_with("snaps/"))
                && !kept.contains(rel)
                && (released.contains(rel) || now - entry.mtime > 600.0)
            {
                freed += unlink(&dir, rel);
            }
        }
    }
    Ok(freed)
}
pub fn gc(config: &Value) -> io::Result<u64> {
    let settings = Settings::new(config);
    let path = root(config)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_secs_f64();
    let mut models = scan(&path)?;
    let victims = plan(&models, settings.budget, settings.ttl, now);
    let mut freed = apply(&path, &mut models, victims, now, &BTreeSet::new())?;
    // Re-scan after session removal: only orphan echo files older than the TTL
    // can be swept, and an opaque manifest protects its entire model.
    let models = scan(&path)?;
    let root = match open_dir(&path, None) {
        Ok(root) => root,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(freed),
        Err(e) => return Err(e),
    };
    for (key, model) in models {
        if model.opaque {
            continue;
        }
        let named = model
            .sessions
            .values()
            .filter_map(|s| s.hint.as_ref().map(|h| format!("echo/{h}.jsonl")))
            .collect::<BTreeSet<_>>();
        let Ok(dir) = open_dir(Path::new(&key), Some(&root)) else {
            continue;
        };
        for (rel, entry) in model.files {
            if rel.starts_with("echo/")
                && rel.ends_with(".jsonl")
                && !named.contains(&rel)
                && entry.mtime < now - settings.ttl as f64
            {
                freed += unlink(&dir, &rel);
            }
        }
    }
    Ok(freed)
}

fn prune_dirs(model: &File) {
    use nix::unistd::{unlinkat, UnlinkatFlags};
    if let Ok(blocks) = open_dir(Path::new("blocks"), Some(model)) {
        if let Ok(entries) = fs::read_dir(descriptor_path(&blocks)) {
            for entry in entries.filter_map(Result::ok) {
                if let Some(name) = entry.file_name().to_str().filter(|s| hex(s, 2)) {
                    let _ = unlinkat(&blocks, name, UnlinkatFlags::RemoveDir);
                }
            }
        }
    }
    for name in ["snaps", "sessions", "echo", "blocks"] {
        let _ = unlinkat(model, name, UnlinkatFlags::RemoveDir);
    }
}
fn release_owner(path: &Path, key: &str, id: &str, hint: &str) -> io::Result<()> {
    let root = open_dir(path, None)?;
    let model = open_dir(Path::new(key), Some(&root))?;
    let name = format!("{id}.json");
    let Some(mut value) = read_manifest(&model, &name).filter(Value::is_object) else {
        return Ok(());
    };
    let Some(parsed) = parse_manifest(&value) else {
        return Ok(());
    };
    let rest = parsed
        .owners
        .into_iter()
        .filter(|o| o != hint)
        .collect::<Vec<_>>();
    if rest.is_empty() {
        return Ok(());
    }
    value["owners"] = json!(rest);
    if value["hint"].is_null() || value["hint"] == hint {
        value["hint"] = json!(rest.iter().find(|s| *s != "-"));
    }
    let sessions = open_dir(Path::new("sessions"), Some(&model))?;
    let mut random = [0; 4];
    getrandom::fill(&mut random).map_err(|e| io::Error::other(e.to_string()))?;
    let suffix = random
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let temporary = format!("{id}.json.{suffix}.pending");
    let file = openat(
        &sessions,
        temporary.as_str(),
        OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::from_bits_truncate(0o600),
    )
    .map_err(io::Error::from)?;
    let mut file = File::from(file);
    file.write_all(paths::compact_ascii_json(&value).as_bytes())?;
    file.sync_all()?;
    drop(file);
    fs::rename(
        descriptor_path(&sessions).join(&temporary),
        descriptor_path(&sessions).join(name),
    )?;
    let _ = sessions.sync_all();
    Ok(())
}
pub fn clear(
    config: &Value,
    scope: &str,
    model: Option<&str>,
    session: Option<&str>,
    hint: Option<&str>,
) -> io::Result<u64> {
    let path = root(config)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_secs_f64();
    let mut models = scan(&path)?;
    let everything = match scope {
        "all" => models.keys().cloned().collect(),
        "model" => model
            .filter(|k| models.contains_key(*k))
            .map(str::to_owned)
            .into_iter()
            .collect(),
        _ => BTreeSet::new(),
    };
    if matches!(scope, "all" | "model") {
        return apply(&path, &mut models, Vec::new(), now, &everything);
    }
    if scope == "hint" {
        let hint = hint.ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "state_cache_hint_required")
        })?;
        let owned = models
            .iter()
            .filter(|(key, _)| model.is_none_or(|m| m == key.as_str()))
            .flat_map(|(key, m)| {
                m.sessions
                    .iter()
                    .filter(|(_, s)| {
                        s.owners.iter().any(|o| o == hint) || s.hint.as_deref() == Some(hint)
                    })
                    .map(move |(id, s)| {
                        (key.clone(), id.clone(), s.owners.iter().any(|o| o != hint))
                    })
            })
            .collect::<Vec<_>>();
        for (key, id, shared) in &owned {
            if *shared {
                release_owner(&path, key, id, hint)?;
            }
        }
        let victims = owned
            .iter()
            .filter(|(_, _, shared)| !*shared)
            .map(|(key, id, _)| (key.clone(), id.clone()))
            .collect();
        let mut freed = apply(&path, &mut models, victims, now, &BTreeSet::new())?;
        let after = scan(&path)?;
        for key in owned
            .into_iter()
            .map(|(key, _, _)| key)
            .collect::<BTreeSet<_>>()
        {
            let Some(model) = after.get(&key).filter(|m| {
                !m.opaque && !m.sessions.values().any(|s| s.hint.as_deref() == Some(hint))
            }) else {
                continue;
            };
            let _ = model;
            if let Ok(root) = open_dir(&path, None) {
                if let Ok(dir) = open_dir(Path::new(&key), Some(&root)) {
                    freed += unlink(&dir, &format!("echo/{hint}.jsonl"));
                }
            }
        }
        return Ok(freed);
    }
    let victims = models
        .iter()
        .filter(|(key, m)| {
            model.is_none_or(|v| v == key.as_str())
                && session.is_some_and(|s| m.sessions.contains_key(s))
        })
        .map(|(key, _)| (key.clone(), session.expect("session").into()))
        .collect();
    apply(&path, &mut models, victims, now, &BTreeSet::new())
}
pub fn valid_control(control: &Value) -> bool {
    let Some(object) = control.as_object() else {
        return false;
    };
    if object
        .keys()
        .any(|k| !["scope", "model", "session", "hint"].contains(&k.as_str()))
    {
        return false;
    }
    let scope =
        control
            .get("scope")
            .and_then(Value::as_str)
            .unwrap_or(if object.contains_key("scope") {
                ""
            } else {
                "all"
            });
    if !["all", "model", "session", "hint"].contains(&scope) {
        return false;
    }
    if !control["model"].is_null() && !control["model"].as_str().is_some_and(|s| hex(s, 16)) {
        return false;
    }
    if scope == "model" && control["model"].is_null() {
        return false;
    }
    if scope == "session" && !control["session"].as_str().is_some_and(sid) {
        return false;
    }
    if (scope == "hint") != !control["hint"].is_null() {
        return false;
    }
    scope != "hint"
        || control["hint"].as_str().is_some_and(|s| {
            !s.is_empty()
                && s.len() <= 128
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        })
}
