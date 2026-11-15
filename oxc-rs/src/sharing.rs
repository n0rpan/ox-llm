//! Loopback sharing controls. Active shared listeners and their gates remain pending.
use crate::paths;
use nix::{libc, unistd::Uid};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::PathBuf,
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
fn key_metadata() -> Option<(String, bool)> {
    // Match the reference's owner-only, no-follow reader. It permits owner-only
    // read modes and hard links; no key bytes leave this function.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(key_path())
        .ok()?;
    let info = file.metadata().ok()?;
    if !info.is_file() || info.uid() != Uid::current().as_raw() || info.mode() & 0o077 != 0 {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(4097).read_to_end(&mut bytes).ok()?;
    let record: Value = serde_json::from_slice(&bytes).ok()?;
    let key = record["key"].as_str()?;
    let suffix = key.strip_prefix("lai-")?;
    if !(43..=128).contains(&suffix.len())
        || !suffix
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
    {
        return None;
    }
    Some((
        format!(
            "sha256:{:.12}",
            format!("{:x}", Sha256::digest(key.as_bytes()))
        ),
        record["shown"] == true,
    ))
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
