//! Loopback sharing controls. Active shared listeners and their gates remain pending.
use crate::paths;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use nix::{libc, unistd::Uid};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const PAUSED: &str = "paused: network changed, re-enable to share on this network";
pub const UNIMPLEMENTED: &str = "Shared listeners are not implemented by the Rust controller yet.";
fn mode(config: &Value) -> &str {
    config["lan_share"]
        .as_str()
        .filter(|s| ["off", "tailscale", "lan"].contains(s))
        .unwrap_or("off")
}
fn key_path() -> PathBuf {
    if let Some(path) = std::env::var_os("LOCAL_AI_SHARE_KEY_FILE")
        .filter(|s| !s.to_string_lossy().trim().is_empty())
    {
        return PathBuf::from(path.to_string_lossy().trim());
    }
    let requested = paths::config();
    let config = paths::realpath(&requested).unwrap_or(requested);
    config
        .parent()
        .and_then(|p| p.parent())
        .unwrap_or_else(|| std::path::Path::new("/"))
        .join("lan-share-key.json")
}
struct KeyRecord {
    key: String,
    created: Value,
    shown: bool,
}
impl KeyRecord {
    fn value(&self) -> Value {
        json!({"key":self.key,"created":self.created,"shown":self.shown})
    }
}
pub struct Revealed {
    key: String,
    pub show: bool,
}
impl Revealed {
    pub fn key(&self) -> &str {
        &self.key
    }
    pub fn fingerprint(&self) -> String {
        fingerprint(&self.key)
    }
}
fn fingerprint(key: &str) -> String {
    format!(
        "sha256:{:.12}",
        format!("{:x}", Sha256::digest(key.as_bytes()))
    )
}
fn read_key(path: &Path) -> Result<Option<KeyRecord>, String> {
    // The reference permits owner-only read modes and hard links.
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("key file is unreadable or a symlink".into()),
    };
    let info = file
        .metadata()
        .map_err(|_| "key file is unreadable or a symlink")?;
    if !info.is_file() || info.uid() != Uid::current().as_raw() || info.mode() & 0o077 != 0 {
        return Err("key file must be a regular file owned by you with mode 0600".into());
    }
    let mut bytes = Vec::new();
    file.take(4097)
        .read_to_end(&mut bytes)
        .map_err(|_| "key file is unreadable or a symlink")?;
    let record: Value = serde_json::from_slice(&bytes).map_err(|_| "key file is not valid JSON")?;
    let key = record["key"]
        .as_str()
        .ok_or("key file holds no valid key")?;
    let suffix = key
        .strip_prefix("lai-")
        .ok_or("key file holds no valid key")?;
    if !(43..=128).contains(&suffix.len())
        || !suffix
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
    {
        return Err("key file holds no valid key".into());
    }
    Ok(Some(KeyRecord {
        key: key.into(),
        created: record["created"].clone(),
        shown: record["shown"] == true,
    }))
}
fn key_metadata() -> Option<(String, bool)> {
    read_key(&key_path())
        .ok()
        .flatten()
        .map(|r| (fingerprint(&r.key), r.shown))
}
fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}
fn path_repr(path: &Path) -> String {
    let text = path.to_string_lossy();
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = quote.to_string();
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch == quote => {
                out.push('\\');
                out.push(ch);
            }
            ch if ch.is_control() => out.push_str(&format!("\\x{:02x}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out.push(quote);
    out
}
fn os_error(error: std::io::Error, path: &Path) -> String {
    let message = error.to_string();
    if let Some(code) = error.raw_os_error() {
        let suffix = format!(" (os error {code})");
        format!(
            "[Errno {code}] {}: {}",
            message.strip_suffix(&suffix).unwrap_or(&message),
            path_repr(path)
        )
    } else {
        message
    }
}
fn private_parent(path: &Path) -> Result<(), String> {
    let parent = parent(path);
    let created = !parent.exists();
    if created {
        let ancestor = parent
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(ancestor).map_err(|e| os_error(e, ancestor))?;
    }
    match fs::DirBuilder::new().mode(0o700).create(parent) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && parent.is_dir() => {}
        Err(error) => return Err(os_error(error, parent)),
    }
    if created {
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
            .map_err(|e| os_error(e, parent))?;
    }
    Ok(())
}
fn entropy<const N: usize>() -> Result<[u8; N], String> {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes).map_err(|_| "operating system randomness is unavailable")?;
    Ok(bytes)
}
fn write_key(path: &Path, record: &KeyRecord) -> Result<(), String> {
    private_parent(path)?;
    let nonce = entropy::<4>()?
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let temporary = parent(path).join(format!(
        ".{}.{nonce}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&temporary)
        .map_err(|e| os_error(e, &temporary))?;
    let result = (|| {
        let mut bytes = paths::python_json(&record.value()).into_bytes();
        bytes.push(b'\n');
        file.write_all(&bytes)
            .map_err(|e| os_error(e, &temporary))?;
        file.sync_all().map_err(|e| os_error(e, &temporary))?;
        // Replace the key path itself. Rotation deliberately replaces a final
        // symlink, while the symlink's target is retained, as in the reference.
        fs::rename(&temporary, path).map_err(|e| os_error(e, &temporary))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    if result.is_ok() {
        if let Ok(directory) = fs::File::open(parent(path)) {
            let _ = directory.sync_all();
        }
    }
    result
}
pub fn reveal(rotate: bool) -> Result<Revealed, String> {
    let path = key_path();
    private_parent(&path)?;
    let lock_path = parent(&path).join(format!(
        ".{}.lock",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&lock_path)
        .map_err(|e| os_error(e, &lock_path))?;
    lock.lock().map_err(|e| os_error(e, &lock_path))?;
    let mut record = if rotate { None } else { read_key(&path)? };
    if record.is_none() {
        let fresh = KeyRecord {
            key: format!("lai-{}", URL_SAFE_NO_PAD.encode(entropy::<32>()?)),
            created: json!(SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()),
            shown: false,
        };
        write_key(&path, &fresh)?;
        record = Some(fresh);
    }
    let mut record = record.expect("existing or generated key");
    let show = !record.shown;
    if show {
        record.shown = true;
        write_key(&path, &record)?;
    }
    // Dropping the descriptor releases the cross-process flock only after the
    // shown marker is durable, before the CLI prints the matching key.
    drop(lock);
    Ok(Revealed {
        key: record.key,
        show,
    })
}
fn setting(config: &Value, name: &str, default: u64, high: u64) -> u64 {
    config["lan_share_options"][name]
        .as_u64()
        .filter(|n| *n >= 1 && *n <= high)
        .unwrap_or(default)
}
pub fn status(config: &Value, port: u16) -> Value {
    let mode = mode(config);
    let key = key_metadata();
    json!({"enabled":mode!="off","mode":mode,"active":false,"addresses":[],"port":port,"urls":[],
        "key_present":key.is_some(),"key_fingerprint":key.as_ref().map(|k|&k.0),"key_shown":key.as_ref().is_some_and(|k|k.1),
        "encryption":match mode {"tailscale"=>Some("wireguard"),"lan"=>Some("none"),_=>None},
        "warning":if mode=="lan" {Some("LAN mode is unencrypted HTTP: anyone on this network can read prompts and replies in transit. Prefer tailscale, or set lan_share_options.tls_cert/tls_key.")} else {None},
        "error":if mode!="off" {Some(UNIMPLEMENTED)} else {None},
        "paused":if mode=="off" && config["lan_share_paused"].is_object() {Some(PAUSED)} else {None},"pinned_network":null,
        "routes":["POST /v1/chat/completions","POST /v1/completions","GET /v1/models","GET /v1/models/{id}"],
        "limits":{"rate_per_min":setting(config,"rate_per_min",120,6000),"burst":setting(config,"burst",30,1000),
            "max_body_bytes":setting(config,"max_body_mb",8,16)*1024*1024,"max_tokens":setting(config,"max_tokens",16384,1_000_000),"concurrency":1},
        "remote_active":0,"remote_requests":0,"remote_refused":0})
}
pub fn disable(port: u16) -> std::io::Result<Value> {
    let mut config = match fs::read(paths::config()) {
        Ok(data) => serde_json::from_slice::<Value>(&data)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(error) => return Err(error),
    };
    let object = config
        .as_object_mut()
        .ok_or(std::io::ErrorKind::InvalidData)?;
    object.insert("lan_share".into(), json!("off"));
    object.remove("lan_share_paused");
    object.remove("lan_share_pin");
    paths::write_config(&config)?;
    Ok(status(&config, port))
}
