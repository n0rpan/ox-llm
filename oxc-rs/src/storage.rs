//! Read-only cache inventory. Only bounded prefix identity headers are read.
use crate::paths;
use nix::{libc, unistd::Uid};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, OpenOptions},
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};
type Pair = (String, String);
fn hex(s: &str, n: usize) -> bool {
    s.len() == n
        && s.bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
fn regular(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_file())
}
fn directory(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
}
fn names(path: &Path) -> Vec<String> {
    if !directory(path) {
        return Vec::new();
    }
    let mut names = fs::read_dir(path)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().into_string().ok())
        .collect::<Vec<_>>();
    names.sort();
    names
}
fn read_prefix(path: &Path, rust: bool) -> Option<Pair> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    if rust {
        let mut head = [0_u8; 12];
        file.read_exact(&mut head).ok()?;
        if &head[..8] != b"OXEPFX01" {
            return None;
        }
        let length = u32::from_le_bytes(head[8..].try_into().ok()?) as usize;
        if !(1..=4096).contains(&length) {
            return None;
        }
        let mut raw = vec![0_u8; length];
        file.read_exact(&mut raw).ok()?;
        let identity = String::from_utf8_lossy(&raw).into_owned() + " ";
        let rest = identity.strip_prefix("model=sha256:")?;
        let model = rest.get(..64)?;
        let rest = rest.get(64..)?.strip_prefix(" server=sha256:")?;
        let server = rest.get(..64)?;
        if !rest.get(64..)?.starts_with(' ') || !hex(model, 64) || !hex(server, 64) {
            return None;
        }
        Some((model.to_owned(), server.to_owned()))
    } else {
        let mut raw = Vec::new();
        file.take(65537).read_to_end(&mut raw).ok()?;
        if raw.len() > 65536 {
            return None;
        }
        let value: Value = serde_json::from_slice(&raw).ok()?;
        let model = value["model_sha256"].as_str()?;
        let server = value["server_sha256"].as_str()?;
        (hex(model, 64) && hex(server, 64)).then(|| (model.to_owned(), server.to_owned()))
    }
}
fn prefixes(path: &Path, rust: bool) -> BTreeMap<Pair, Vec<PathBuf>> {
    let mut groups = BTreeMap::<Pair, Vec<PathBuf>>::new();
    let suffix = if rust { ".bin" } else { ".json" };
    for name in names(path).into_iter().take(1024) {
        if !name.strip_suffix(suffix).is_some_and(|s| hex(s, 64)) {
            continue;
        }
        let entry = path.join(name);
        if let Some(pair) = read_prefix(&entry, rust) {
            groups.entry(pair).or_default().push(entry);
        }
    }
    groups
}
pub fn clear_legacy_prefix(config: &Value) -> std::io::Result<u64> {
    let state = crate::state_cache::root(config)?;
    let mut directories = HashSet::from([state.parent().expect("state directory").join("prefix")]);
    let override_root = std::env::var_os("LOCAL_AI_STATE_CACHE_ROOT")
        .is_some_and(|s| !s.is_empty())
        || !config["cache"]["root"].is_null();
    if !override_root {
        for profile in config["profiles"]
            .as_object()
            .into_iter()
            .flat_map(|p| p.values())
            .chain(std::iter::once(config))
        {
            if profile["backend_kind"] != "oxe" || profile["oxe_flash"].is_object() {
                continue;
            }
            let args = profile["oxe_args"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default();
            let mut directory = None;
            for (index, arg) in args.iter().enumerate() {
                if arg == "--prefix-cache" {
                    directory = args.get(index + 1).and_then(Value::as_str);
                } else if let Some(value) =
                    arg.as_str().and_then(|s| s.strip_prefix("--prefix-cache="))
                {
                    directory = Some(value);
                }
            }
            if let Some(directory) = directory.filter(|s| !s.is_empty()) {
                if directory == "default" {
                    directories
                        .insert(paths::home().join(".cache/local-ai/snapdragon-engine/prefix"));
                } else if Path::new(directory).is_absolute() {
                    directories.insert(PathBuf::from(directory));
                }
            }
        }
    }
    let mut freed = 0;
    for directory in directories {
        for path in prefixes(&directory, true).into_values().flatten() {
            if let Ok(info) = fs::symlink_metadata(&path) {
                if info.is_file()
                    && info.uid() == Uid::current().as_raw()
                    && fs::remove_file(&path).is_ok()
                {
                    freed += info.len();
                }
            }
        }
    }
    Ok(freed)
}
#[derive(Default)]
struct References {
    models: BTreeMap<String, String>,
    files: BTreeMap<String, String>,
    rust: BTreeMap<Pair, String>,
    llama: BTreeMap<Pair, String>,
}
fn merge(base: &Map<String, Value>, extra: &Map<String, Value>) -> Map<String, Value> {
    let mut out = base.clone();
    out.extend(extra.clone());
    out
}
impl References {
    fn add(&mut self, alias: &str, kind: &str, profile: &Map<String, Value>) {
        if let Some(model) = profile
            .get("sha256")
            .and_then(Value::as_str)
            .filter(|s| hex(s, 64))
        {
            self.models
                .entry(model.to_owned())
                .or_insert_with(|| alias.to_owned());
            if let Some(server) = profile.get("server_sha256").and_then(Value::as_str) {
                let pair = (model.to_owned(), server.to_owned());
                let refs = if kind == "rust" {
                    &mut self.rust
                } else {
                    &mut self.llama
                };
                refs.entry(pair).or_insert_with(|| alias.to_owned());
            }
        }
        if let Some(path) = profile
            .get("model_path")
            .and_then(Value::as_str)
            .and_then(|s| Path::new(s).file_name())
            .and_then(|s| s.to_str())
        {
            self.files
                .entry(path.to_owned())
                .or_insert_with(|| alias.to_owned());
        }
    }
    fn new(config: &Value) -> Self {
        let mut refs = Self::default();
        let mut base = config.as_object().cloned().unwrap_or_default();
        base.shift_remove("profiles");
        let mut variants = vec![(
            base.get("model_alias")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            base.clone(),
        )];
        if let Some(profiles) = config["profiles"].as_object() {
            for (alias, raw) in profiles {
                if let Some(profile) = raw.as_object() {
                    variants.push((alias.clone(), merge(&base, profile)));
                }
            }
        }
        for (alias, profile) in variants {
            refs.add(&alias, "llama", &profile);
            if let Some(overrides) = profile.get("memlat_overrides").and_then(Value::as_object) {
                let mut classic = profile.clone();
                for (key, value) in overrides {
                    if key != "oxe" {
                        classic.insert(key.clone(), value.clone());
                    }
                }
                refs.add(&alias, "llama", &classic);
            }
            let rust = ["rust_engine", "memlat_overrides"].iter().find_map(|key| {
                profile.get(*key).and_then(Value::as_object).filter(|m| {
                    ["oxe", "oxe_flash"]
                        .iter()
                        .any(|k| m.get(*k).is_some_and(Value::is_object))
                })
            });
            if let Some(overrides) = rust.and_then(|m| m.get("oxe")).and_then(Value::as_object) {
                refs.add(&alias, "rust", &merge(&profile, overrides));
            }
        }
        refs
    }
}
fn size(paths: &[PathBuf]) -> u64 {
    let mut total = 0;
    let mut pending = paths.to_vec();
    while let Some(path) = pending.pop() {
        let Ok(info) = fs::symlink_metadata(&path) else {
            continue;
        };
        if info.is_file() {
            total += info.len();
        } else if info.is_dir() {
            if let Ok(entries) = fs::read_dir(&path) {
                pending.extend(entries.filter_map(Result::ok).map(|e| e.path()));
            }
        }
    }
    total
}
fn model_key(owner: Option<&String>) -> &str {
    match owner.map(String::as_str) {
        Some("ornith-35b-a3b-local") => "ornith",
        Some("qwen3.6-35b-a3b-local") => "moe",
        Some("qwen3.8-flash-next-local") => "flash",
        Some("") | None => "",
        Some(_) => "other",
    }
}
struct Row {
    value: Value,
    paths: Vec<PathBuf>,
}
impl Row {
    fn new(
        kind: &str,
        key: &str,
        paths: Vec<PathBuf>,
        name: &str,
        owner: Option<&String>,
        protected: bool,
    ) -> Self {
        let digest = format!("{:x}", Sha256::digest(format!("{kind}\0{key}")));
        let valid_name = (1..=96).contains(&name.len())
            && name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c));
        let state = if protected {
            "protected"
        } else if owner.is_some_and(|v| !v.is_empty()) {
            "referenced"
        } else {
            "unreferenced"
        };
        Self {
            value: json!({"id":format!("{kind}-{}",&digest[..16]),"kind":kind,"name":if valid_name {name} else {""},"bytes":size(&paths),"model":model_key(owner),"state":state}),
            paths,
        }
    }
}
fn arena(name: &str) -> Option<&str> {
    let stem = name.strip_suffix(".bin")?;
    let stem = stem.strip_suffix("-mtp").unwrap_or(stem);
    let parts = stem.split('-').collect::<Vec<_>>();
    if let [model, arch, commit, checksum] = parts.as_slice() {
        if hex(model, 64)
            && (1..=16).contains(&arch.len())
            && arch.bytes().all(|c| c.is_ascii_alphanumeric())
            && (7..=40).contains(&commit.len())
            && hex(commit, commit.len())
            && hex(checksum, 8)
        {
            return Some(model);
        }
    }
    None
}
fn repack(name: &str) -> Option<&str> {
    let (file, checksum) = name.rsplit_once('-')?;
    let stem = file.strip_suffix(".gguf")?;
    (hex(checksum, 12) && (1..=200).contains(&stem.chars().count()) && !stem.contains('\n'))
        .then_some(file)
}
fn active_paths() -> HashSet<String> {
    let mut paths = HashSet::new();
    let Ok(processes) = fs::read_dir("/proc") else {
        return paths;
    };
    for entry in processes.filter_map(Result::ok) {
        if !entry
            .file_name()
            .to_str()
            .is_some_and(|s| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit()))
        {
            continue;
        }
        let process = entry.path();
        if !fs::metadata(&process).is_ok_and(|m| m.uid() == Uid::current().as_raw()) {
            continue;
        }
        if let Ok(entries) = fs::read_dir(process.join("fd")) {
            for fd in entries.filter_map(Result::ok) {
                if let Ok(target) = fs::read_link(fd.path()) {
                    let text = target.to_string_lossy();
                    if text.starts_with('/') {
                        paths.insert(text.strip_suffix(" (deleted)").unwrap_or(&text).to_owned());
                    }
                }
            }
        }
        if let Ok(maps) = fs::read_to_string(process.join("maps")) {
            for line in maps.lines() {
                let mut rest = line;
                for _ in 0..5 {
                    rest = rest.trim_start();
                    rest = rest
                        .find(char::is_whitespace)
                        .map(|i| &rest[i..])
                        .unwrap_or("");
                }
                rest = rest.trim_start();
                if rest.starts_with('/') {
                    paths.insert(rest.strip_suffix(" (deleted)").unwrap_or(rest).to_owned());
                }
            }
        }
    }
    paths
}
pub fn inventory(config: &Value) -> Value {
    let root = paths::home().join(".cache/local-ai");
    let engine = root.join("snapdragon-engine");
    let refs = References::new(config);
    let mut rows = Vec::new();
    for name in names(&engine) {
        if let Some(model) = arena(&name) {
            let path = engine.join(&name);
            if regular(&path) {
                rows.push(Row::new(
                    "rust-weights",
                    &name,
                    vec![path],
                    &name,
                    refs.models.get(model),
                    false,
                ));
            }
        }
    }
    for ((model, server), paths) in prefixes(&engine.join("prefix"), true) {
        let key = format!("{model}:{server}");
        let name = format!("{}-{}", &model[..12], &server[..12]);
        let owner = refs.rust.get(&(model, server));
        rows.push(Row::new("rust-prefix", &key, paths, &name, owner, false));
    }
    let sidecar = engine.join("flash");
    if directory(&sidecar) {
        let owner = "qwen3.8-flash-next-local".to_owned();
        rows.push(Row::new(
            "flash-sidecar",
            "flash",
            vec![sidecar],
            "flash",
            Some(&owner),
            true,
        ));
    }
    for ((model, server), paths) in prefixes(&root.join("prefixes"), false) {
        let files = paths
            .into_iter()
            .flat_map(|path| [path.with_extension("bin"), path])
            .filter(|p| regular(p))
            .collect::<Vec<_>>();
        let key = format!("{model}:{server}");
        let name = format!("{}-{}", &model[..12], &server[..12]);
        let owner = refs.llama.get(&(model, server));
        rows.push(Row::new("llama-prefix", &key, files, &name, owner, false));
    }
    let repacks = root.join("repack");
    for name in names(&repacks) {
        if let Some(file) = repack(&name) {
            let path = repacks.join(&name);
            if directory(&path) {
                rows.push(Row::new(
                    "llama-repack",
                    &name,
                    vec![path],
                    &name,
                    refs.files.get(file),
                    false,
                ));
            }
        }
    }
    let active = if rows.is_empty() {
        HashSet::new()
    } else {
        active_paths()
    };
    for row in &mut rows {
        if row.value["state"] != "protected"
            && row.paths.iter().any(|path| {
                let p = path.to_string_lossy();
                active.iter().any(|a| {
                    a == p.as_ref() || a.starts_with(&format!("{}/", p.trim_end_matches('/')))
                })
            })
        {
            row.value["state"] = json!("in-use");
        }
        row.value["deletable"] = json!(row.value["state"] == "unreferenced");
    }
    json!({"object":"list","data":rows.into_iter().map(|row|row.value).collect::<Vec<_>>()})
}
