//! Remote catalog and engine-free status for the merged cloud-only controller.
use crate::paths;
use chrono::Local;
use serde_json::{json, Map, Value};
use std::{
    fs::OpenOptions,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::PathBuf,
};

pub const PROVIDERS: [&str; 3] = ["openrouter", "anthropic", "openai"];
pub fn only(config: &Value) -> bool {
    config["cloud_only"] == true
}
pub fn enabled(config: &Value) -> bool {
    config["cloud"]["enabled"] == true
}
pub fn offline() -> bool {
    std::env::var("OXC_OFFLINE").ok().is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes"
        )
    })
}
fn positive(value: &Value) -> Option<Value> {
    let number = value.as_f64()?;
    (number.is_finite() && number > 0.0).then(|| {
        if value.is_u64() {
            value.clone()
        } else {
            json!(number.trunc())
        }
    })
}
pub fn entry(alias: &str, raw: &Value) -> Option<Value> {
    if alias.is_empty()
        || alias.len() > 128
        || !alias.as_bytes()[0].is_ascii_alphanumeric()
        || !alias
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._:/-".contains(&byte))
        || !raw.is_object()
    {
        return None;
    }
    let kind = raw.get("kind").map_or(Some("remote"), Value::as_str)?;
    let derived = match kind {
        "remote" => None,
        "remote/openrouter" => Some("openrouter"),
        "remote/anthropic" => Some("anthropic"),
        "remote/openai" => Some("openai"),
        _ => return None,
    };
    let provider = match raw.get("provider") {
        Some(Value::String(value)) if !value.is_empty() => Some(value.as_str()),
        None | Some(Value::Null) | Some(Value::Bool(false)) => derived,
        Some(Value::String(_)) => derived,
        _ => return None,
    }?;
    if !PROVIDERS.contains(&provider) {
        return None;
    }
    let base = raw
        .get("base_url")
        .and_then(Value::as_str)
        .or((provider == "openai").then_some("https://api.openai.com/v1"))?;
    let url = reqwest::Url::parse(base).ok()?;
    if !["http", "https"].contains(&url.scheme())
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some_and(|s| !s.is_empty())
        || url.fragment().is_some_and(|s| !s.is_empty())
    {
        return None;
    }
    let upstream = raw
        .get("upstream_model")
        .filter(|v| v.as_str().is_some_and(|s| !s.is_empty()))
        .or_else(|| raw.get("upstream"))?
        .as_str()?
        .trim();
    if upstream.is_empty() {
        return None;
    }
    let context = positive(&raw["context_window"])?;
    let output = positive(&raw["max_output"]);
    if provider == "openai" && output.is_none() {
        return None;
    }
    Some(
        json!({"id":alias,"kind":kind,"provider":provider,"base_url":base.trim_end_matches('/'),"upstream_model":upstream,
        "context_window":context,"max_output":output,"display_name":raw.get("name").filter(|v|v.is_string()).cloned().unwrap_or_else(||json!(alias)),"pricing":pricing(raw)}),
    )
}
pub fn entries(config: &Value) -> Vec<Value> {
    if !enabled(config) {
        return Vec::new();
    }
    let Some(models) = config["cloud"]["models"].as_object() else {
        return Vec::new();
    };
    let mut rows: Vec<_> = models
        .iter()
        .filter_map(|(alias, raw)| entry(alias, raw))
        .collect();
    rows.sort_by(|left, right| left["id"].as_str().cmp(&right["id"].as_str()));
    rows
}
pub fn visible(config: &Value) -> Vec<Value> {
    if offline() {
        Vec::new()
    } else {
        entries(config)
    }
}
pub fn capabilities(item: &Value) -> Value {
    json!({"remote":true,"remote_provider":item["provider"],"echo":false,"cache_hints":false,
        "cancel":item["provider"]!="openai","prefill":false,"preserve_thinking":false,"context_window":item["context_window"]})
}
pub fn row(item: &Value) -> Value {
    json!({"id":item["id"],"object":"model","created":0,"owned_by":item["provider"],"name":item["display_name"],
        "remote":true,"provider":item["provider"],"context_length":item["context_window"],"context_window":item["context_window"],
        "max_output_tokens":item["max_output"],"capabilities":capabilities(item)})
}
pub fn catalog(config: &Value) -> Value {
    let rows:Vec<_>=visible(config).iter().map(|item|json!({"id":item["id"],"key":"cloud","name":item["display_name"],
        "engine":"remote","engine_status":"remote","qualification":null,"memory_gib":null,"battery_start_percent":null,
        "prefix":"off","selected":false,"default":false,"state":"unloaded","remote":true,"provider":item["provider"],
        "context_window":item["context_window"],"max_output":item["max_output"],"capabilities":capabilities(item)})).collect();
    json!({"object":"list","data":rows,"default_model":"ornith-35b-a3b-local",
        "selected_model":config.get("selected_model").or_else(||config.get("model_alias")).cloned().unwrap_or_else(||json!("qwen-local"))})
}
fn local_description(config: &Value) -> Option<&Value> {
    let selected = config["selected_model"]
        .as_str()
        .or_else(|| config["model_alias"].as_str())?;
    config["profiles"]
        .get(selected)
        .filter(|profile| profile.is_object())
}
pub fn local_refusal(config: &Value) -> &'static str {
    if local_description(config)
        .is_some_and(|profile| profile["backend_kind"] == "oxe" && profile["ready"] != true)
    {
        "This model profile is not marked ready; finish its setup and validation first."
    } else {
        "Model setup is still running. Run `oxc status` to follow it."
    }
}
pub fn secret(provider: &str, config: &Value) -> Option<String> {
    secret_result(provider, config).ok()
}
pub fn secret_result(provider: &str, config: &Value) -> Result<String, String> {
    let unavailable = || "Cloud credentials are not configured.".to_owned();
    let refused = |reason: &str| format!("Cloud credentials are not configured: {reason}.");
    let path = config["cloud"]["secrets_file"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            paths::config()
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .join("cloud-secrets.json")
        });
    if !path.is_absolute() {
        return Err(unavailable());
    }
    let resolved = paths::realpath(&path).map_err(|_| unavailable())?;
    if resolved.ancestors().skip(1).any(|parent| {
        parent.join(".git").exists()
            || (parent.join("AGENTS.md").is_file() && parent.join("engine").is_dir())
    }) {
        return Err(unavailable());
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
        .open(path)
        .map_err(|error| match error.raw_os_error() {
            Some(nix::libc::ELOOP) => refused("the configured path is a symbolic link"),
            Some(nix::libc::EACCES | nix::libc::EPERM) => refused("the file cannot be read"),
            _ => unavailable(),
        })?;
    let info = file.metadata().map_err(|_| unavailable())?;
    if !info.is_file() {
        return Err(refused("the configured path is not a regular file"));
    }
    if info.uid() != nix::unistd::Uid::current().as_raw() {
        return Err(refused("the file is owned by another user"));
    }
    if info.mode() & 0o077 != 0 {
        return Err(refused(
            "the file is not mode 0600 (group/other permissions must be off)",
        ));
    }
    if info.len() > 65536 {
        return Err(refused("the file is larger than 65536 bytes"));
    }
    let mut data = Vec::new();
    file.take(65537)
        .read_to_end(&mut data)
        .map_err(|_| unavailable())?;
    if data.len() > 65536 {
        return Err(refused("the file is larger than 65536 bytes"));
    }
    let data: Value = serde_json::from_slice(&data).map_err(|_| unavailable())?;
    let raw = if data[provider].is_object() {
        &data[provider]["api_key"]
    } else {
        &data[provider]
    };
    let key = raw.as_str().ok_or_else(unavailable)?;
    (!key.is_empty() && key.len() <= 8192 && key.bytes().all(|b| (33..=126).contains(&b)))
        .then(|| key.to_owned())
        .ok_or_else(unavailable)
}
pub fn spend() -> Option<f64> {
    let _guard = crate::request_ledger::IO_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let path = crate::ledger::directory()
        .join(format!("requests-{}.jsonl", Local::now().format("%Y%m%d")));
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Some(0.0),
        Err(_) => return None,
    };
    let mut total = 0.0;
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let row: Value = serde_json::from_str(line).ok()?;
        if !row.is_object() {
            return None;
        }
        if row["remote"] != true {
            continue;
        }
        let cost = row["cost_usd"]
            .as_f64()
            .filter(|v| v.is_finite() && *v >= 0.0)?;
        total += cost;
        if !total.is_finite() {
            return None;
        }
    }
    Some((total * 1e8).round_ties_even() / 1e8)
}
pub fn summary(config: &Value) -> Value {
    let spend = spend();
    let cap = config["cloud"]["daily_budget_usd"]
        .as_f64()
        .filter(|v| v.is_finite() && *v > 0.0);
    let providers: Map<_, _> = PROVIDERS
        .iter()
        .map(|p| ((*p).to_owned(), json!(secret(p, config).is_some())))
        .collect();
    json!({"enabled":enabled(config),"offline":offline(),"daily_budget_usd":if spend.is_some(){cap}else{None},
        "spend_today_usd":spend,"ledger_ok":spend.is_some(),"ledger_error":if spend.is_some(){None}else{Some("cloud_ledger_unavailable")},
        "providers":providers,"models":entries(config).len(),"rejected":{}})
}
pub fn status(config: &Value, port: u16) -> Value {
    let limits:Map<_,_>=visible(config).iter().map(|item|(item["id"].as_str().unwrap().to_owned(),json!({"mode":null,"modes":[],
        "cap":item["context_window"],"context_window":item["context_window"],"max_output":item["max_output"],"capabilities":capabilities(item)}))).collect();
    let mut status = json!({"engine_mode":"none","state":"unloaded","validated":true,"download_state":"none","downloaded_bytes":0,"expected_bytes":null,
        "model":null,"selected_model":null,"selection_error":null,"backend":null,"context_size":null,"threads":null,"execution_policy_mode":"stock",
        "memory_policy":null,"memory_guard_setup":{"ok":true,"not_applicable":true,"swap_max":null,"problem":null,"fix":null},"validation":{},
        "flash_memory":null,"gpu_layers":0,"active_requests":0,"queued_requests":0,"idle_unload_seconds":7200.0,"image_active":false,"image_broker_error":null,
        "loaded_model":null,"resident_bytes":null,"idle_unload_remaining_seconds":null,"unload":{"method":"POST","path":"/_local/unload","available":false},
        "prefix_cache":{"enabled":false,"primes":0,"restores":0,"failures":0},"backend_kind":"llama","engine":"llama.cpp","engine_note":null});
    status.as_object_mut().unwrap().extend(json!({
        "flash_qualification":null,"engine_ledger":{"enabled":false},"harnesses":{},
        "harness_learning":{"enabled":false,"persistent":true,"min_requests":3,"min_prefix_chars":3000,"candidates":[],"ignored":[]},
        "flash_idle_qos":{"enabled":false,"unit":null,"started":false,"error":null},
        "capabilities":["models","storage","harness","harness_learning","ledger","active","openai_aliases","state_cache","context_modes","preload","model_preload","cloud"],
        "cloud":summary(config),"active":{"model":null,"phase":"idle","seconds":null,"tokens_out":null,"tokens_per_second":null,"prompt_tokens_done":null,"prompt_tokens_total":null,"queued_requests":0},
        "last_error":null,"last_failure":null,"error_at":null,"error_origin":null,"context":{"mode":null,"modes":[],"cap":null},"model_limits":limits,
        "preload":{"state":"idle","src":null,"ms":null,"age_s":null},"model_preload":null,"stop_pending":null,"image_slot_stale":0,
        "lan_share":crate::sharing::status(config,port),"chat_url":format!("http://127.0.0.1:{port}"),"api_base":format!("http://127.0.0.1:{port}/v1"),
        "keep_warm":{"idle_unload_seconds":7200.0,"psi_some_avg10":10.0,"min_available_mib":1024,"pressure_grace_seconds":60.0,"last_sample":null,"last_unload":null},
        "request_compat":{}}).as_object().unwrap().clone());
    // Main retains descriptive fields for an explicitly configured local
    // profile, even though cloud-only mode has no local engine. Read metadata
    // only; do not qualify, stat or start the described engine/model.
    if let Some(profile) = local_description(config) {
        if let Some(kind) = profile["backend_kind"].as_str() {
            status["backend_kind"] = json!(kind);
            status["engine"] = json!(match kind {
                "oxe" => "rust",
                "llama" => "llama.cpp",
                "bigmoe-cpu-stream" => "bigmoe",
                _ => "unknown",
            });
            if kind == "oxe" && profile["ready"] != true {
                status["engine_note"] = json!(local_refusal(config));
            }
        }
        if profile["context_size"].is_number() {
            status["context"]["cap"] = profile["context_size"].clone();
        }
    }
    status
}

fn rates(raw: &Value) -> Option<Value> {
    let mut result = Map::new();
    for (key, alias) in [
        ("input_per_million", "input"),
        ("output_per_million", "output"),
        ("cache_read_per_million", "cache_read_per_million"),
        ("cache_write_per_million", "cache_write_per_million"),
    ] {
        let value = raw.get(key).or_else(|| raw.get(alias));
        if value.is_none_or(Value::is_null) && key.starts_with("cache_") {
            continue;
        }
        let number = value?.as_f64()?;
        if !(0.0..=1e12).contains(&number) {
            return None;
        }
        result.insert(key.into(), json!(number));
    }
    Some(Value::Object(result))
}
pub fn pricing(raw: &Value) -> Option<Value> {
    let price = &raw["pricing"];
    if price.is_object() {
        if let Some(tiers) = price.get("tiers") {
            let tiers = tiers.as_array()?;
            if tiers.is_empty() || tiers.len() > 64 {
                return None;
            }
            let mut result = Vec::new();
            let mut previous = 0;
            for (index, tier) in tiers.iter().enumerate() {
                let mut merged = price.as_object()?.clone();
                merged.extend(tier.as_object()?.clone());
                let mut normalized = rates(&Value::Object(merged))?;
                let bound = &tier["max_prompt_tokens"];
                if index == tiers.len() - 1 {
                    if !bound.is_null() {
                        return None;
                    }
                } else {
                    let bound = bound.as_u64()?;
                    if bound <= previous || bound > 100_000_000 {
                        return None;
                    }
                    previous = bound;
                }
                normalized["max_prompt_tokens"] = bound.clone();
                result.push(normalized);
            }
            return Some(json!({"tiers":result}));
        }
        if let Some(rates) = rates(price) {
            return Some(rates);
        }
    }
    (raw["free"] == true).then(|| json!({"input_per_million":0.0,"output_per_million":0.0}))
}
