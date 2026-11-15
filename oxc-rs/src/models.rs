use serde_json::{json, Map, Value};
use std::path::Path;

pub const UNKNOWN: &str = "Unknown local model. Choose one listed in /v1/models.";

pub fn selected(config: &Value) -> &str {
    config["selected_model"]
        .as_str()
        .or_else(|| config["model_alias"].as_str())
        .unwrap_or("qwen-local")
}
fn base(config: &Value) -> &str {
    config["model_alias"].as_str().unwrap_or("qwen-local")
}

fn aliases(config: &Value) -> Map<String, Value> {
    let mut aliases = Map::new();
    aliases.insert("local-heavy".into(), json!("qwen3.8-flash-next-local"));
    if let Some(extra) = config["openai_api"]["model_aliases"].as_object() {
        for (name, target) in extra {
            if name.is_empty()
                || name.len() > 64
                || !name.as_bytes()[0].is_ascii_alphanumeric()
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b))
            {
                continue;
            }
            if target.is_null() {
                aliases.shift_remove(name);
            } else if target.as_str().is_some_and(|v| !v.is_empty()) {
                aliases.insert(name.clone(), target.clone());
            }
        }
    }
    aliases.retain(|name, _| {
        name != base(config)
            && !["local-default", "qwen-local", "local-auto"].contains(&name.as_str())
            && config["profiles"].get(name).is_none()
    });
    aliases
}

pub fn resolve(config: &Value, name: Option<&str>) -> Result<Value, &'static str> {
    let name = name.unwrap_or("");
    let table = aliases(config);
    let mut requested = name;
    for candidate in std::iter::once(name).chain(
        ["openai/", "openai_compatible/", "local/"]
            .iter()
            .filter_map(|p| name.strip_prefix(p)),
    ) {
        if ["", "local-default", "qwen-local"].contains(&candidate) {
            requested = selected(config);
            break;
        }
        if candidate == base(config) || config["profiles"].get(candidate).is_some() {
            requested = candidate;
            break;
        }
        if let Some(target) = table.get(candidate).and_then(Value::as_str) {
            requested = target;
            break;
        }
    }
    if requested != base(config) && config["profiles"].get(requested).is_none() {
        if config["openai_api"]["unknown_model"] == "default" {
            requested = selected(config);
        } else if config["profiles"]
            .as_object()
            .is_some_and(|p| !p.is_empty())
        {
            return Err(UNKNOWN);
        } else {
            requested = base(config);
        }
    }
    let mut profile = config
        .as_object()
        .cloned()
        .ok_or("Local model setup is still being prepared.")?;
    if let Some(extra) = config["profiles"][requested].as_object() {
        for (key, value) in extra {
            profile.insert(key.clone(), value.clone());
        }
    }
    profile.insert("model_alias".into(), json!(requested));
    if requested != base(config)
        && config["profiles"][requested]
            .get("memlat_overrides")
            .is_none()
    {
        profile.shift_remove("memlat_overrides");
    }
    Ok(apply_mode(
        Value::Object(profile),
        stored_mode(config, requested),
    ))
}

pub fn eligible(profile: &Value) -> bool {
    if profile["ready"] != true {
        return false;
    }
    if profile["backend_kind"] == "oxe" {
        if std::env::var("OXC_ENABLE_OXE").is_ok_and(|v| {
            ["0", "off", "false", "no"].contains(&v.trim().to_ascii_lowercase().as_str())
        }) {
            return false;
        }
        // The Flash qualification adapter is not ported yet.
        // Do not advertise an unvalidated profile as eligible in their place.
        if profile["oxe_flash"].is_object()
            || !profile["model_alias"].as_str().is_some_and(|alias| {
                !alias.is_empty()
                    && alias.chars().count() <= 128
                    && !alias.chars().any(char::is_control)
            })
        {
            return false;
        }
        for key in ["server_binary", "model_path"] {
            if !profile[key]
                .as_str()
                .is_some_and(|v| Path::new(v).is_absolute() && !v.chars().any(char::is_control))
            {
                return false;
            }
        }
        for key in ["server_sha256", "sha256"] {
            if !profile[key].as_str().is_some_and(|v| {
                v.len() == 64
                    && v.bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            }) {
                return false;
            }
        }
        for (key, max) in [
            ("threads", 12_u64),
            ("context_size", 262144),
            ("expected_bytes", 64 * 1024 * 1024 * 1024),
        ] {
            if !profile[key].as_u64().is_some_and(|v| v > 0 && v <= max) {
                return false;
            }
        }
        if rss_proof_reason(profile, profile["context_size"].as_u64().unwrap_or(0)).is_some() {
            return false;
        }
        if profile["prefix_cache"]["enabled"] == true {
            return false;
        }
        let Some(args) = profile["oxe_args"]
            .as_array()
            .filter(|a| !a.is_empty() && a.len() <= 64)
        else {
            return false;
        };
        for arg in args {
            let Some(arg) = arg.as_str() else {
                return false;
            };
            let flag = arg.split('=').next().unwrap_or(arg);
            if arg.chars().any(char::is_control)
                || [
                    "--host",
                    "--port",
                    "--listen",
                    "--bind",
                    "--state-cache",
                    "--state-budget-bytes",
                    "--state-ttl-secs",
                ]
                .contains(&flag)
                || ((arg.contains('{') || arg.contains('}'))
                    && !["{model}", "{context}", "{threads}", "{alias}"].contains(&arg))
            {
                return false;
            }
        }
        return Path::new(profile["server_binary"].as_str().unwrap_or("")).is_file();
    }
    // The classic qualification/receipt adapter is still pending.
    false
}

fn display_name(alias: &str, config: &Value) -> String {
    match alias {
        "ornith-35b-a3b-local" => "Ornith 1.5 35B A3B",
        "qwen3.6-35b-a3b-local" => "Qwen 3.6 35B MoE",
        "qwen3.8-flash-next-local" => "Qwen3.8 Flash-Next",
        _ => config["display_name"]
            .as_str()
            .filter(|v| !v.is_empty() && v.chars().count() <= 128)
            .unwrap_or(alias),
    }
    .to_owned()
}

fn limits(row: &mut Value, profile: &Value) {
    let context = profile["context_size"].as_u64().unwrap_or(131072);
    let output = profile
        .get("max_output_tokens")
        .or_else(|| profile.get("default_max_tokens"));
    let output = match output.and_then(Value::as_u64) {
        Some(0) => Value::Null,
        Some(v) => json!(v.min(context)),
        None => json!(16384_u64.min(context / 4)),
    };
    row["context_length"] = json!(context);
    row["max_output_tokens"] = output;
}

pub fn stored_mode<'a>(config: &'a Value, alias: &str) -> &'a str {
    let valid = |v: &'a Value| v.as_str().filter(|s| ["fast", "deep", "max"].contains(s));
    valid(&config["context_modes"]["models"][alias])
        .or_else(|| valid(&config["context_modes"]["default"]))
        .unwrap_or("fast")
}
fn has_modes(profile: &Value) -> bool {
    matches!(
        profile["model_alias"].as_str(),
        Some("ornith-35b-a3b-local" | "qwen3.6-35b-a3b-local")
    )
}
fn valid_proof(proof: &Value) -> bool {
    let Some(object) = proof.as_object() else {
        return false;
    };
    object.len() == 4
        && ["context", "peak_rss_bytes", "engine_sha256", "date"]
            .iter()
            .all(|k| object.contains_key(*k))
        && proof["context"]
            .as_u64()
            .is_some_and(|n| (131073..=262144).contains(&n))
        && proof["peak_rss_bytes"].as_u64().is_some_and(|n| n > 0)
        && proof["engine_sha256"].as_str().is_some_and(|s| {
            s.len() == 64
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
        && proof["date"].as_str().is_some_and(|s| {
            let b = s.as_bytes();
            b.len() == 10
                && b[0] == b'2'
                && b[1] == b'0'
                && b[2..4].iter().all(u8::is_ascii_digit)
                && b[4] == b'-'
                && (b[5] == b'0' || b[5] == b'1')
                && b[6].is_ascii_digit()
                && b[7] == b'-'
                && (b'0'..=b'3').contains(&b[8])
                && b[9].is_ascii_digit()
        })
}
pub fn rss_proof_reason(profile: &Value, context: u64) -> Option<&'static str> {
    if context > 262144 {
        return Some("above 262144");
    }
    if context <= 131072 {
        return None;
    }
    if profile["model_alias"] != "ornith-35b-a3b-local" {
        return Some("Ornith only");
    }
    if profile["backend_kind"] != "oxe" || profile["oxe_flash"].is_object() {
        return Some("Rust engine only");
    }
    let proof = &profile["rss_proof"];
    if !valid_proof(proof) {
        return Some("needs RSS proof");
    }
    if proof["engine_sha256"] != profile["server_sha256"] {
        return Some("RSS proof is for another engine");
    }
    if proof["context"].as_u64().unwrap_or(0) < context {
        return Some("needs RSS proof");
    }
    if proof["peak_rss_bytes"].as_u64().unwrap_or(0) > 27 * 1024 * 1024 * 1024 {
        return Some("RSS above 27 GiB");
    }
    None
}
pub fn mode_rows(profile: &Value) -> Vec<Value> {
    if !has_modes(profile) {
        return Vec::new();
    }
    [("fast",131072,49536),("deep",131072,102000),("max",262144,199750)].into_iter().map(|(mode,cap,compact)|{
        let reason=rss_proof_reason(profile,cap);
        json!({"id":mode,"context":cap,"compact_at":compact,"cap":cap,"available":reason.is_none(),"reason":reason})
    }).collect()
}
pub fn apply_mode(mut profile: Value, mode: &str) -> Value {
    if !has_modes(&profile) {
        return profile;
    }
    let chosen = if mode == "max" && rss_proof_reason(&profile, 262144).is_some() {
        "fast"
    } else {
        mode
    };
    profile["context_mode"] = json!(chosen);
    if chosen == "max" {
        profile["context_size"] = json!(262144)
    } else if profile["context_size"].as_u64().is_some_and(|n| n > 131072) {
        profile["context_size"] = json!(131072)
    }
    profile
}
pub fn context(profile: &Value, config: &Value) -> Value {
    let mode = stored_mode(config, profile["model_alias"].as_str().unwrap_or(""));
    let applied = apply_mode(profile.clone(), mode);
    let rows = mode_rows(profile);
    let mut cap = if applied["context_size"].is_i64() || applied["context_size"].is_u64() {
        applied["context_size"].clone()
    } else {
        Value::Null
    };
    let mode = if rows.is_empty() {
        Value::Null
    } else {
        applied["context_mode"].clone()
    };
    if !rows.is_empty() && cap.is_null() {
        cap = json!(if mode == "max" { 262144 } else { 131072 });
    }
    json!({"cap":cap,"mode":mode,"modes":rows})
}
pub fn model_limit(profile: &Value, config: &Value, sampling: Option<Vec<String>>) -> Value {
    let mut context = context(profile, config);
    let max_context = context["modes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| r["available"] == true)
        .filter_map(|r| r["cap"].as_u64())
        .max()
        .map(|n| json!(n))
        .unwrap_or_else(|| context["cap"].clone());
    let args = profile["oxe_args"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let spec = args
        .windows(2)
        .filter(|w| w[0] == "--spec-type")
        .map(|w| w[1].clone())
        .next_back();
    let mtp = spec.map(|s| json!(s == "draft-mtp")).unwrap_or_else(|| {
        if args.iter().any(|s| s == "--mtp") {
            json!(true)
        } else {
            Value::Null
        }
    });
    context["capabilities"] = json!({"prefill_only":true,"snapshot_hint":2,"turn_kind":true,"hint_clear":true,
        "thinking":"toggle","preserve_thinking":true,"mtp":mtp,"max_context":max_context,"vision":false,"slots":1,"park":"ram+disk","sampling":sampling});
    let mut row = json!({});
    limits(&mut row, profile);
    context["max_output"] = row["max_output_tokens"].clone();
    context
}

pub fn rows(config: &Value) -> Vec<Value> {
    let mut names = Vec::new();
    if config["profiles"]
        .as_object()
        .is_some_and(|p| !p.is_empty())
    {
        names.push("qwen-local".to_owned());
    }
    names.push(base(config).to_owned());
    if let Some(profiles) = config["profiles"].as_object() {
        names.extend(profiles.keys().cloned());
    }
    let mut raw = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for name in names {
        if !seen.insert(name.clone()) {
            continue;
        }
        let Ok(profile) = resolve(config, Some(&name)) else {
            continue;
        };
        if !eligible(&profile) {
            continue;
        }
        let mut row = json!({"id":name,"object":"model","created":0,"owned_by":"local","name":if name == "qwen-local" {"Selected local model (Local AI)".to_owned()} else {display_name(&name,&profile)}});
        limits(&mut row, &profile);
        raw.push(row);
    }
    let mut result = Vec::new();
    if let Some(chosen) = raw.iter().find(|row| row["id"] == selected(config)) {
        let mut row = chosen.clone();
        row["id"] = json!("local-default");
        row["alias_of"] = json!(selected(config));
        row["name"] = json!(format!(
            "Default: {}",
            chosen["name"].as_str().unwrap_or(selected(config))
        ));
        result.push(row);
    }
    for (alias, target) in aliases(config) {
        if let Some(chosen) = raw.iter().find(|row| row["id"] == target) {
            let mut row = chosen.clone();
            row["id"] = json!(alias);
            row["alias_of"] = target;
            let label = alias.rsplit('-').next().unwrap_or(&alias);
            let label = label
                .chars()
                .enumerate()
                .map(|(i, c)| {
                    if i == 0 {
                        c.to_ascii_uppercase()
                    } else {
                        c.to_ascii_lowercase()
                    }
                })
                .collect::<String>();
            row["name"] = json!(format!(
                "{label}: {}",
                chosen["name"].as_str().unwrap_or("")
            ));
            result.push(row);
        }
    }
    result.extend(raw.iter().filter(|row| row["id"] != "qwen-local").cloned());
    if let Some(legacy) = raw.iter().find(|row| row["id"] == "qwen-local") {
        let mut row = legacy.clone();
        row["alias_of"] = json!(selected(config));
        row["legacy"] = json!(true);
        result.push(row);
    }
    result
}
