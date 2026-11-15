//! Filtered panel status, without loading a model or reading conversation content.
use crate::{cli, paths};
use serde_json::{json, Map, Value};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::io::AsyncReadExt;
const TRIAL_UNITS: &[(&str, &str)] = &[
    ("oxc-planning-live.service", "http"),
    ("oxc-flash-final-agent.service", "coding"),
    ("oxc-flash-code-screen.service", "coding"),
    ("oxc-flash-agent.service", "coding"),
    ("oxc-flash-native-http.service", "http"),
    ("oxc-flash-prefill.service", "prefill"),
    ("oxc-flash-wide-prefill.service", "prefill"),
    ("oxc-flash-thp.service", "memory"),
    ("oxc-flash-cache-screen.service", "memory"),
    ("oxc-flash-overlap.service", "overlap"),
    ("oxc-flash-smoke.service", "baseline"),
    ("oxc-flash-profile-cache-abba.service", "memory"),
    ("oxc-flash-profile-hot-parity.service", "baseline"),
    ("oxc-flash-profile-hot-abba.service", "memory"),
    ("oxc-flash-profile-cpu-quota-abba.service", "coding"),
    ("oxc-flash-profile-decode46.service", "coding"),
    ("oxc-flash-profile-decode48.service", "coding"),
    ("oxc-flash-profile-selective-q8.service", "coding"),
    ("oxc-flash-profile-placement.service", "coding"),
    ("oxc-flash-profile-combined-parity.service", "coding"),
    ("oxc-flash-profile-combined-coalescing.service", "coding"),
    ("oxc-flash-profile-combined-predictive.service", "coding"),
    ("oxc-flash-profile-ngram-capture.service", "memory"),
    ("oxc-flash-profile-ngram-screen.service", "coding"),
    ("oxc-flash-profile-cache-full-12288.service", "coding"),
    ("oxc-flash-profile-cache-full-13312.service", "coding"),
    ("oxc-flash-profile-io-full-4.service", "coding"),
    ("oxc-flash-profile-io-full-8.service", "coding"),
];
const PRIMARY: &str = "ornith-35b-a3b-local";
const SECONDARY: &str = "qwen3.6-35b-a3b-local";
const FLASH: &str = "qwen3.8-flash-next-local";
fn choice(value: &Value, choices: &[&str]) -> Value {
    value
        .as_str()
        .filter(|s| choices.contains(s))
        .map_or(Value::Null, |s| json!(s))
}
fn number(value: &Value, low: f64, high: f64) -> Option<Value> {
    value
        .as_f64()
        .filter(|v| v.is_finite() && *v >= low && *v <= high)
        .map(|_| value.clone())
}
fn integer(value: &Value, low: u64, high: u64) -> Option<Value> {
    value
        .as_u64()
        .filter(|v| *v >= low && *v <= high)
        .map(|_| value.clone())
}
fn nonzero(value: &Value, high: f64) -> Value {
    number(value, 0.0, high)
        .filter(|v| v.as_f64() != Some(0.0))
        .unwrap_or(Value::Null)
}
fn clean(value: &str, limit: usize) -> String {
    let mut text = String::new();
    let mut control = false;
    for ch in value.chars() {
        if ch <= '\u{1f}' || ch == '\u{7f}' {
            if !control {
                text.push(' ');
            }
            control = true;
        } else {
            text.push(ch);
            control = false;
        }
    }
    text.trim().chars().take(limit).collect()
}
fn cache_row(value: &Value) -> Option<Value> {
    value.as_object()?;
    let mut result = Map::new();
    if value["preference_control"].is_boolean() {
        result.insert(
            "preference_control".into(),
            value["preference_control"].clone(),
        );
    }
    for (name, allowed) in [
        ("pref", &["auto", "20", "18", "16", "14"][..]),
        (
            "reason",
            &[
                "auto",
                "preferred",
                "preferred-does-not-fit",
                "preferred-swap-tolerant",
                "refuse",
            ][..],
        ),
    ] {
        let chosen = choice(&value[name], allowed);
        if !chosen.is_null() {
            result.insert(name.into(), chosen);
        }
        if name == "pref"
            && value["tier_gib"]
                .as_u64()
                .is_some_and(|n| [20, 18, 16, 14].contains(&n))
        {
            result.insert("tier_gib".into(), value["tier_gib"].clone());
        }
    }
    for (name, low, high) in [
        ("margin_gib", -1024.0, 1024.0),
        ("desktop_floor_gib", 0.0, 16.0),
        ("estimated_working_set_gib", 0.0, 128.0),
    ] {
        if let Some(value) = number(&value[name], low, high) {
            result.insert(name.into(), value);
        }
    }
    (!result.is_empty()).then_some(Value::Object(result))
}
fn context_row(value: &Value) -> Option<Value> {
    value.as_object()?;
    let mut rows = Vec::new();
    for row in value["modes"].as_array().into_iter().flatten() {
        let id = choice(&row["id"], &["fast", "deep", "max"]);
        if !row.is_object()
            || id.is_null()
            || !row["available"].is_boolean()
            || number(&row["compact_at"], 1.0, 4194304.0).is_none()
            || number(&row["cap"], 1.0, 4194304.0).is_none()
        {
            return None;
        }
        rows.push(json!({"id":id,"context":number(&row["context"],1.0,4194304.0).unwrap_or_else(||row["cap"].clone()),
            "compact_at":row["compact_at"],"cap":row["cap"],"available":row["available"],
            "reason":row["reason"].as_str().map(|s|clean(s,80))}));
    }
    rows.truncate(3);
    Some(
        json!({"cap":integer(&value["cap"],1,4194304),"mode":choice(&value["mode"], &["fast","deep","max"]),"modes":rows}),
    )
}
fn preload_row(value: &Value) -> Option<Value> {
    let state = choice(
        &value["state"],
        &["idle", "running", "done", "cancelled", "unavailable"],
    );
    if !value.is_object() || state.is_null() {
        return None;
    }
    Some(
        json!({"state":state,"src":choice(&value["src"], &["disk","live"]),
        "ms":number(&value["ms"],0.0,1e9),"age_s":integer(&value["age_s"],0,1_000_000_000)}),
    )
}
fn harness_rows(value: &Value) -> Option<Value> {
    let mut rows = Vec::new();
    for (name, info) in value.as_object()? {
        if name == "other" || !cli::plain_name(name) || !info.is_object() {
            continue;
        }
        let prefixes: Vec<_> = info["prefixes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|p| p.is_object())
            .collect();
        let mut recent: Option<&Value> = None;
        for prefix in &prefixes {
            let at = |p: &Value| {
                number(&p["last_used"], 0.0, 1e18)
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0)
            };
            if recent.is_none_or(|r| at(prefix) > at(r)) {
                recent = Some(prefix);
            }
        }
        let latest = recent.unwrap_or(&Value::Null);
        let count = |name: &str| integer(&latest[name], 0, 1 << 31).unwrap_or(json!(0));
        rows.push(json!({"name":name,"warm":recent.is_some() && latest["warm"] == true,
            "last_used":number(&info["last_used"],0.0,1e18).filter(|v|v.as_f64()!=Some(0.0)).unwrap_or(json!(0)),
            "tools":count("tools"),"cache_n":count("last_cache_n"),"prompt_n":count("last_prompt_n"),"prefixes":prefixes.len()}));
    }
    rows.sort_by(|a, b| {
        b["last_used"]
            .as_f64()
            .unwrap_or(0.0)
            .total_cmp(&a["last_used"].as_f64().unwrap_or(0.0))
    });
    rows.truncate(12);
    Some(json!(rows))
}
fn panel_key(alias: &Value, flash: bool, ornith: bool) -> Value {
    match alias.as_str() {
        Some(SECONDARY) => json!("moe"),
        Some(PRIMARY) if ornith => json!("ornith"),
        Some(FLASH) if flash => json!("flash"),
        _ => Value::Null,
    }
}
fn failure_row(value: &Value, flash: bool, ornith: bool) -> Option<Value> {
    value.as_object()?;
    let reason = clean(value["reason"].as_str().unwrap_or(""), 200);
    let memory = "The model was stopped to keep the laptop responsive (memory was nearly full). Close a few apps and try again.";
    let reason = match reason.as_str() {
        "host_memory_pressure_sustained"|"host_memory_pressure_full_sustained"|"host_available_below_floor"|"host_available_emergency"|"host_memory_and_swap_exhausted"=>memory,
        "cgroup_oom"=>"The model ran out of its memory allowance and was stopped. Try again, or pick a smaller Flash tier.",
        "host_oom_kill"=>"The system ran out of memory and the model was stopped. Close a few apps and try again.",
        "own_swap_nonzero"=>"The model was stopped because part of it was swapped to disk. Try again in a moment.",
        "own_swap_limit_not_zero"=>"The model was stopped: its swap limit is not set as expected. Run `oxc status` and check the setup.",
        "cgroup_pids_limit"=>"The model was stopped: it started too many processes. Try again; if it repeats, reinstall the runtime.",
        _=>&reason,
    };
    let t = number(&value["t"], 0.0, (1_u64 << 40) as f64).filter(|v| {
        v.as_f64()
            .is_some_and(|v| v > 0.0 && v < (1_u64 << 40) as f64)
    });
    if reason.is_empty() && t.is_none() {
        return None;
    }
    Some(
        json!({"reason":reason,"model":panel_key(&value["alias"],flash,ornith),"t":t,"origin":choice(&value["origin"], &["organic","lab","remote"])}),
    )
}
fn optional_status(chat: &Value, flash: bool, ornith: bool) -> Map<String, Value> {
    let mut result = Map::new();
    if let Some(cache) = cache_row(&chat["flash_cache"]) {
        result.insert("flash_cache".into(), cache);
    }
    for (name, high) in [("queued_requests", 128), ("context_size", 4194304)] {
        if let Some(value) = integer(&chat[name], u64::from(name == "context_size"), high) {
            result.insert(name.into(), value);
        }
    }
    if chat.get("loaded_model").is_some() {
        result.insert(
            "loaded_model".into(),
            panel_key(&chat["loaded_model"], flash, ornith),
        );
    }
    if let Some(resident) = chat.get("resident_bytes") {
        if resident.is_null() {
            result.insert("resident_gib".into(), Value::Null);
        } else if let Some(bytes) = resident.as_u64().filter(|v| *v > 0) {
            result.insert(
                "resident_gib".into(),
                json!((bytes as f64 / 1024_f64.powi(3) * 10.0).round_ties_even() / 10.0),
            );
        }
    }
    if let Some(remaining) = chat.get("idle_unload_remaining_seconds") {
        if remaining.is_null() || number(remaining, 0.0, 86400.0).is_some() {
            result.insert("idle_remaining_seconds".into(), remaining.clone());
        }
    }
    if chat["state"] == "stopping" {
        if let Some(pending) = chat["stop_pending"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
        {
            result.insert("stop_pending".into(), json!(clean(pending, 200)));
        }
    }
    result.insert(
        "has_error".into(),
        json!(
            chat["last_error"].as_str().is_some_and(|s| !s.is_empty())
                || chat["last_error"]
                    .as_object()
                    .is_some_and(|m| !m.is_empty())
        ),
    );
    for (name, value) in [
        ("harnesses", harness_rows(&chat["harnesses"])),
        ("context", context_row(&chat["context"])),
        ("preload", preload_row(&chat["preload"])),
    ] {
        if let Some(value) = value {
            result.insert(name.into(), value);
        }
    }
    if chat.get("error_at").is_some() {
        result.insert(
            "error_at".into(),
            integer(&chat["error_at"], 1, (1 << 40) - 1).unwrap_or(Value::Null),
        );
    }
    if chat.get("error_origin").is_some() {
        result.insert(
            "error_origin".into(),
            choice(&chat["error_origin"], &["organic", "lab"]),
        );
    }
    if let Some(value) = failure_row(&chat["last_failure"], flash, ornith) {
        result.insert("last_failure".into(), value);
    }
    result
}
async fn model_rows() -> Option<Value> {
    let (code, value) = cli::request_timeout("/_local/models", None, 1).await;
    if code != 200 {
        return None;
    }
    let mut rows = Vec::new();
    let mut seen = HashSet::new();
    for row in value["data"].as_array()? {
        if !row.is_object() {
            continue;
        }
        let key = match row["id"].as_str() {
            Some(PRIMARY) => Some("ornith"),
            Some(SECONDARY) => Some("moe"),
            Some(FLASH) => Some("flash"),
            _ => row["key"]
                .as_str()
                .filter(|s| ["ornith", "moe", "flash"].contains(s)),
        };
        let Some(key) = key.filter(|k| seen.insert((*k).to_owned())) else {
            continue;
        };
        let mut item = Map::new();
        if let Some(cache) = cache_row(&row["flash_cache"]) {
            item.insert("flash_cache".into(), cache);
        }
        let basis = choice(
            &row["memory_basis"],
            &["qualified_working_set", "cache_pref_tier"],
        );
        if !basis.is_null() {
            item.insert("memory_basis".into(), basis);
        }
        item.extend(json!({"key":key,"engine":choice(&row["engine"], &["rust","llama","stream"]),
            "engine_status":choice(&row["engine_status"], &["production","classic","fallback"]),"memory_gib":nonzero(&row["memory_gib"],1024.0),
            "battery_start_percent":nonzero(&row["battery_start_percent"],100.0),"prefix":choice(&row["prefix"], &["warm","cold","off"]),
            "selected":row["selected"]==true,"default":row["default"]==true,"state":choice(&row["state"], &["ready","loading","unloaded"])}).as_object().expect("model row").clone());
        rows.push(Value::Object(item));
    }
    Some(json!(rows))
}
fn preferred(installed: &str, legacy: &str) -> PathBuf {
    let path = paths::home().join(installed);
    if path.exists() {
        path
    } else {
        paths::home().join(legacy)
    }
}
fn compute_busy() -> Value {
    let path = std::env::var_os("OXC_LEASE")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            preferred(
                ".local/state/oxc/inference.lock",
                "Work/prompt-studio/generation.lock",
            )
        });
    match File::open(path) {
        Ok(file) => match file.try_lock_shared() {
            Ok(()) => json!(false),
            Err(fs::TryLockError::WouldBlock) => json!(true),
            Err(_) => Value::Null,
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!(false),
        Err(_) => Value::Null,
    }
}
fn memory() -> Result<(f64, Option<f64>), String> {
    let input = fs::read_to_string("/proc/meminfo").map_err(|_| "Memory status is unavailable.")?;
    let (mut available, mut total) = (0.0, None);
    for line in input.lines() {
        let parse = || {
            line.split_whitespace()
                .nth(1)
                .and_then(|s| s.parse::<i128>().ok())
                .map(|v| v as f64 / 1024_f64.powi(2))
        };
        if line.starts_with("MemAvailable:") {
            available = parse()
                .ok_or("Memory status is invalid.")?
                .clamp(0.0, 1024.0);
        } else if line.starts_with("MemTotal:") {
            total = parse();
        }
    }
    Ok((available, total))
}
fn metadata(path: &Path) -> Option<Value> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut data = Vec::new();
    file.take(65537).read_to_end(&mut data).ok()?;
    if data.len() > 65536 {
        return None;
    }
    serde_json::from_slice::<Value>(&data)
        .ok()
        .filter(Value::is_object)
}
fn lower_hex(text: &str, len: usize) -> bool {
    text.len() == len
        && text
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
async fn active_download() -> bool {
    let mut command = tokio::process::Command::new("systemctl");
    command
        .args([
            "--user",
            "is-active",
            "--quiet",
            "oxc-flash-download.service",
        ])
        .stdin(Stdio::inherit())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    matches!(tokio::time::timeout(Duration::from_secs(1),command.status()).await,Ok(Ok(status)) if status.success())
}
async fn download_status(qualification: &Value) -> Value {
    let empty = json!({"flash_download_state":"absent","flash_download_bytes":0,"flash_download_total_bytes":0,
        "flash_download_shards":0,"flash_verified_shards":0,"flash_download_active":false,"flash_waiting_verification":false});
    let state = std::env::var_os("OXC_FLASH_DOWNLOAD_STATE")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            paths::xdg("XDG_STATE_HOME", ".local/state").join("oxc/downloads/flash-next")
        });
    let models = std::env::var_os("OXC_FLASH_MODELS")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("XDG_DATA_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| paths::home().join(".local/share"))
                .join("ai-stack/models/llm/Qwen3.8-Flash-Next")
        });
    let Some(manifest) = metadata(&state.join("manifest.json")) else {
        return empty;
    };
    let Some(entries) = manifest["models"]
        .as_array()
        .filter(|e| !e.is_empty() && e.len() <= 16)
    else {
        return empty;
    };
    if manifest["repository"] != "unsloth/Qwen3.8-Flash-Next-GGUF"
        || !manifest["revision"]
            .as_str()
            .is_some_and(|s| lower_hex(s, 40))
    {
        return empty;
    }
    let mut ids = HashSet::new();
    let mut total = 0;
    for model in entries {
        let id = model["id"]
            .as_str()
            .and_then(|s| s.strip_prefix("flash-next-"))
            .filter(|s| {
                !s.is_empty()
                    && s.len() <= 48
                    && s.bytes()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
            });
        let size = model["bytes"]
            .as_u64()
            .filter(|n| *n >= 1 && *n <= 2 * 1024_u64.pow(4));
        if id.is_none()
            || !ids.insert(model["id"].as_str())
            || !model["sha256"].as_str().is_some_and(|s| lower_hex(s, 64))
            || size.is_none()
        {
            return empty;
        }
        total += size.expect("validated size");
        if total > 2 * 1024_u64.pow(4) {
            return empty;
        }
    }
    if manifest["total_bytes"].as_u64() != Some(total) {
        return empty;
    }
    let active = active_download().await;
    let (mut downloaded, mut verified) = (0, 0);
    let mut phases = Vec::new();
    for model in entries {
        let id = model["id"].as_str().expect("validated identity");
        let size = model["bytes"].as_u64().expect("validated size");
        let marker = metadata(&state.join(format!("{id}-verified.json"))).unwrap_or(Value::Null);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();
        let stamped = marker["verified_at"]
            .as_f64()
            .is_some_and(|t| t.is_finite() && t > 0.0 && t <= now + 300.0);
        let path = model["path"]
            .as_str()
            .filter(|s| s.chars().count() <= 1024)
            .map(Path::new);
        let safe_path = path.is_some_and(|path| {
            let valid_name = path
                .file_name()
                .and_then(|s| s.to_str())
                .and_then(|s| s.strip_suffix(".gguf"))
                .is_some_and(|s| {
                    !s.is_empty()
                        && s.len() <= 230
                        && s.bytes()
                            .all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c))
                });
            path.is_absolute()
                && valid_name
                && paths::realpath(path)
                    .ok()
                    .zip(paths::realpath(&models).ok())
                    .is_some_and(|(p, root)| p.starts_with(root))
                && fs::metadata(path)
                    .ok()
                    .is_some_and(|m| m.is_file() && m.len() == size)
        });
        if marker["id"] == model["id"]
            && marker["sha256"] == model["sha256"]
            && marker["bytes"].as_u64() == Some(size)
            && stamped
            && safe_path
        {
            verified += 1;
            downloaded += size;
            continue;
        }
        let row = metadata(&state.join(format!("{id}-download.json"))).unwrap_or(Value::Null);
        let phase = choice(
            &row["state"],
            &[
                "downloading",
                "downloaded; verification pending",
                "waiting for verification slot",
                "verifying",
                "verified",
            ],
        );
        if row["id"] != model["id"]
            || row["sha256"] != model["sha256"]
            || row["total_bytes"].as_u64() != Some(size)
            || phase.is_null()
            || row["chunk_bytes"].as_u64() != Some(32 * 1024 * 1024)
        {
            continue;
        }
        let Some(done) = row["done"].as_array().filter(|d| d.len() <= 16384) else {
            continue;
        };
        let mut seen = HashSet::new();
        let offsets: Option<Vec<_>> = done
            .iter()
            .map(|n| {
                n.as_u64()
                    .filter(|n| *n < size && *n % (32 * 1024 * 1024) == 0 && seen.insert(*n))
            })
            .collect();
        let Some(offsets) = offsets else {
            continue;
        };
        let count: u64 = offsets
            .iter()
            .map(|n| (32 * 1024 * 1024).min(size - n))
            .sum();
        if row["downloaded_bytes"].as_u64() != Some(count) {
            continue;
        }
        downloaded += count;
        phases.push(phase.as_str().expect("phase").to_owned());
    }
    let phase = if verified == entries.len() {
        if qualification == "qualified" {
            "ready"
        } else {
            "weights-ready-testing"
        }
    } else if !active {
        "paused"
    } else if downloaded == total
        || phases
            .iter()
            .any(|s| ["verifying", "waiting for verification slot"].contains(&s.as_str()))
    {
        "verifying"
    } else {
        "downloading"
    };
    json!({"flash_download_state":phase,"flash_download_bytes":downloaded,"flash_download_total_bytes":total,"flash_download_shards":entries.len(),
        "flash_verified_shards":verified,"flash_download_active":active,"flash_waiting_verification":phase=="verifying" && !phases.iter().any(|s|s=="verifying")})
}
async fn trial_status() -> Value {
    let empty =
        json!({"flash_trial_active":false,"flash_trial_kind":"","flash_trial_elapsed_seconds":0});
    let mut command = tokio::process::Command::new("systemctl");
    command
        .args([
            "--user",
            "show",
            "--no-pager",
            "--property=Id,ActiveState,ActiveEnterTimestampMonotonic",
        ])
        .args(TRIAL_UNITS.iter().map(|(unit, _)| *unit))
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let Ok(mut child) = command.spawn() else {
        return empty;
    };
    let output = tokio::time::timeout(Duration::from_secs(1), async {
        let mut data = Vec::new();
        child
            .stdout
            .take()?
            .take(8193)
            .read_to_end(&mut data)
            .await
            .ok()?;
        if data.len() > 8192 || !data.is_ascii() {
            return None;
        }
        if !child.wait().await.ok()?.success() {
            return None;
        }
        String::from_utf8(data).ok()
    })
    .await
    .ok()
    .flatten();
    let Some(output) = output else {
        return empty;
    };
    let Ok(now) = nix::time::clock_gettime(nix::time::ClockId::CLOCK_MONOTONIC) else {
        return empty;
    };
    let now = now.tv_sec() as i128 * 1_000_000 + now.tv_nsec() as i128 / 1000;
    if !(1..1_i128 << 63).contains(&now) {
        return empty;
    }
    let mut found: HashMap<&str, Option<i128>> = HashMap::new();
    for block in output.trim().split("\n\n") {
        let mut values = HashMap::new();
        let valid = block.lines().all(|line| {
            line.split_once('=').is_some_and(|(k, v)| {
                ["Id", "ActiveState", "ActiveEnterTimestampMonotonic"].contains(&k)
                    && values.insert(k, v).is_none()
            })
        });
        if !valid
            || values.len() != 3
            || values["ActiveState"] != "active"
            || !TRIAL_UNITS.iter().any(|(unit, _)| *unit == values["Id"])
        {
            continue;
        }
        let stamp = values["ActiveEnterTimestampMonotonic"];
        if stamp.is_empty() || stamp.len() > 20 || !stamp.bytes().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let Some(start) = stamp
            .parse::<i128>()
            .ok()
            .filter(|s| *s > 0 && *s <= now && now - *s <= 86400 * 1_000_000)
        else {
            continue;
        };
        found
            .entry(values["Id"])
            .and_modify(|v| *v = None)
            .or_insert(Some((now - start) / 1_000_000));
    }
    for (unit, kind) in TRIAL_UNITS {
        if let Some(Some(elapsed)) = found.get(unit) {
            return json!({"flash_trial_active":true,"flash_trial_kind":kind,"flash_trial_elapsed_seconds":elapsed});
        }
    }
    empty
}
pub async fn project() -> Result<Value, String> {
    let (code, chat) = cli::request_timeout("/_local/status", None, 1).await;
    let (image_code, image) = cli::request_url("http://127.0.0.1:8090/activity", None, 1).await;
    let state = if code == 200 {
        choice(
            &chat["state"],
            &["unloaded", "ready", "loading", "stopping"],
        )
    } else {
        Value::Null
    };
    let active = integer(&chat["active_requests"], 0, 128).unwrap_or(json!(0));
    let ttl = number(
        chat.get("idle_unload_seconds").unwrap_or(&json!(600)),
        1.0,
        86400.0,
    )
    .unwrap_or(json!(120));
    let (available, total) = memory()?;
    let image_state = if image_code == 200 {
        choice(&image["state"], &["idle", "generating"])
    } else {
        Value::Null
    };
    let flash = cli::offered(FLASH).await;
    let ornith = cli::offered(PRIMARY).await;
    let npu = std::env::var_os("OXC_NPU_STATUS")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            preferred(
                ".config/oxc/npu-status.json",
                "Work/local-ai-optimization/npu-status.json",
            )
        });
    let npu = fs::read(npu)
        .ok()
        .and_then(|data| serde_json::from_slice::<Value>(&data).ok())
        .map(|v| {
            choice(
                &v["status"],
                &["testing", "experimental", "ready", "unavailable"],
            )
        })
        .filter(|v| !v.is_null())
        .unwrap_or(json!("experimental"));
    let mut result = json!({"chat_state":if state.is_null(){json!("offline")}else{state},"active_requests":active,"idle_seconds":ttl,"compute_busy":compute_busy(),
        "model":panel_key(&chat["model"],flash,ornith),"selected_model":panel_key(chat.get("selected_model").unwrap_or(&chat["model"]),flash,ornith),
        "prefix_cache_enabled":chat["prefix_cache"].is_object() && chat["prefix_cache"]["enabled"]==true,
        "backend":choice(&chat["backend"], &["cpu","npu","gpu"]).as_str().unwrap_or("cpu"),"npu_status":npu,
        "image_state":if image_state.is_null(){json!("offline")}else{image_state},"image_model":choice(&image["model"], &["q4","q8"]),
        "available_gib":(available*10.0).round_ties_even()/10.0,"flash_available":flash,"ornith_available":ornith,
        "execution_policy_mode":chat.get("execution_policy_mode").cloned().unwrap_or(json!("stock"))});
    let out = result.as_object_mut().expect("status projection");
    if let Some(total) = total.filter(|n| *n > 0.0) {
        out.insert(
            "total_gib".into(),
            json!((total * 10.0).round_ties_even() / 10.0),
        );
    }
    if code == 200 {
        out.extend(optional_status(&chat, flash, ornith));
    }
    if chat["capabilities"]
        .as_array()
        .is_some_and(|c| c.iter().any(|v| v == "models"))
    {
        if let Some(rows) = model_rows().await {
            out.insert("models".into(), rows);
        }
    }
    out.extend(
        download_status(&chat["flash_qualification"])
            .await
            .as_object()
            .expect("download projection")
            .clone(),
    );
    out.extend(
        trial_status()
            .await
            .as_object()
            .expect("trial projection")
            .clone(),
    );
    Ok(result)
}
pub async fn run(as_json: bool) -> i32 {
    let value = match project().await {
        Ok(value) => value,
        Err(error) => {
            eprintln!("oxc: {error}");
            return 1;
        }
    };
    if as_json {
        println!("{}", paths::python_json(&value));
    } else {
        let mut suffix = String::new();
        if value["flash_download_state"] != "absent" {
            suffix = format!(
                " | Flash-Next: {} ({:.1}%)",
                value["flash_download_state"].as_str().unwrap_or(""),
                100.0 * value["flash_download_bytes"].as_f64().unwrap_or(0.0)
                    / value["flash_download_total_bytes"].as_f64().unwrap_or(1.0)
            );
        }
        if value["flash_trial_active"] == true {
            suffix.push_str(&format!(
                " | Flash test: {} ({} min)",
                value["flash_trial_kind"].as_str().unwrap_or(""),
                value["flash_trial_elapsed_seconds"].as_u64().unwrap_or(0) / 60
            ));
        }
        println!(
            "Qwen: {} | Images: {} | Available: {:.1} GiB{suffix}",
            value["chat_state"].as_str().unwrap_or("offline"),
            value["image_state"].as_str().unwrap_or("offline"),
            value["available_gib"].as_f64().unwrap_or(0.0)
        );
    }
    i32::from(value["chat_state"] == "offline")
}
