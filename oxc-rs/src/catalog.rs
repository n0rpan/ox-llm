//! Chooser metadata. Receipt hashes and bounded prefix headers never inspect model contents.
use crate::{models, paths, storage};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::Read,
    os::unix::fs::MetadataExt,
    path::PathBuf,
    sync::{Mutex, OnceLock},
};

const DEFAULT: &str = "ornith-35b-a3b-local";
type ReceiptIdentity = (PathBuf, u64, u64, i64, i64);
static RECEIPTS: OnceLock<Mutex<HashMap<ReceiptIdentity, String>>> = OnceLock::new();
fn hex(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn receipt(path: &str, digest: &str) -> bool {
    if !hex(digest) {
        return false;
    }
    let Ok(info) = fs::metadata(path) else {
        return false;
    };
    let key = (
        PathBuf::from(path),
        info.ino(),
        info.len(),
        info.mtime(),
        info.mtime_nsec(),
    );
    let mut cache = RECEIPTS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(value) = cache.get(&key) {
        return value == digest;
    }
    if !info.is_file() || info.len() > 16 * 1024 * 1024 {
        return false;
    }
    let Ok(file) = File::open(path) else {
        return false;
    };
    let mut bytes = Vec::new();
    if file
        .take(16 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .is_err()
        || bytes.len() > 16 * 1024 * 1024
    {
        return false;
    }
    let actual = format!("{:x}", Sha256::digest(bytes));
    let matches = actual == digest;
    cache.insert(key, actual);
    matches
}
fn qualification(profile: &Value) -> &'static str {
    let validation = &profile["validation"];
    if !validation.is_object() || validation["qualified"] != true {
        return "unqualified";
    }
    if validation["server_sha256"] != profile["server_sha256"] {
        return "stale";
    }
    let mut receipts = Vec::new();
    if let (Some(path), Some(sha)) = (
        validation["report"].as_str(),
        validation["receipt_sha256"].as_str(),
    ) {
        receipts.push((path, sha));
    }
    for row in validation["proof_sources"]
        .as_object()
        .into_iter()
        .flat_map(|r| r.values())
    {
        if let (Some(path), Some(sha)) = (row["path"].as_str(), row["sha256"].as_str()) {
            receipts.push((path, sha));
        }
    }
    if receipts.is_empty() || !receipts.into_iter().all(|(path, sha)| receipt(path, sha)) {
        "receipt-missing"
    } else {
        "qualified"
    }
}
fn prefix_state(profile: &Value) -> &'static str {
    let Some(args) = profile["oxe_args"].as_array() else {
        return "off";
    };
    let mut target = None;
    for (index, arg) in args.iter().enumerate() {
        if arg == "--prefix-cache" && index + 1 < args.len() {
            target = args[index + 1].as_str();
        } else if let Some(path) = arg.as_str().and_then(|s| s.strip_prefix("--prefix-cache=")) {
            target = Some(path);
        }
    }
    let Some(target) = target.filter(|s| !s.is_empty()) else {
        return "off";
    };
    let directory = if target == "default" {
        paths::home().join(".cache/local-ai/snapdragon-engine/prefix")
    } else {
        PathBuf::from(target)
    };
    if !directory.is_absolute() {
        return "off";
    }
    let (Some(model), Some(server)) = (
        profile["sha256"].as_str(),
        profile["server_sha256"].as_str(),
    ) else {
        return "cold";
    };
    if storage::prefixes(&directory, true).contains_key(&(model.to_owned(), server.to_owned())) {
        "warm"
    } else {
        "cold"
    }
}
pub fn view(config: &Value, loaded: Option<&Value>, state: &str) -> Value {
    let base = config["model_alias"].as_str().unwrap_or("qwen-local");
    let selected = models::resolve(config, None)
        .ok()
        .map(|p| p["model_alias"].clone())
        .unwrap_or(Value::Null);
    let mut names = vec![base.to_owned()];
    names.extend(
        config["profiles"]
            .as_object()
            .into_iter()
            .flat_map(|p| p.keys().cloned()),
    );
    let mut seen = HashSet::new();
    let mut rows = Vec::new();
    for alias in names {
        if !seen.insert(alias.clone()) {
            continue;
        }
        let Ok(profile) = models::resolve(config, Some(&alias)) else {
            continue;
        };
        if !models::eligible(&profile) {
            continue;
        }
        // Only the direct Oxe adapter is eligible today. Keep unsupported
        // classic/Flash qualification adapters explicit in models::eligible.
        let (key, name, order) = match alias.as_str() {
            DEFAULT => ("ornith", "Ornith 1.5 35B A3B".to_owned(), 0),
            "qwen3.6-35b-a3b-local" => ("moe", "Qwen 3.6 35B MoE".to_owned(), 1),
            "qwen3.8-flash-next-local" => ("flash", "Qwen3.8 Flash-Next".to_owned(), 2),
            _ => (
                "",
                profile["display_name"]
                    .as_str()
                    .filter(|s| !s.is_empty() && s.chars().count() <= 128)
                    .unwrap_or(&alias)
                    .to_owned(),
                3,
            ),
        };
        let memory = profile["expected_bytes"]
            .as_u64()
            .filter(|n| *n > 0)
            .map(|size| ((size as f64 / 1024_f64.powi(3) + 3.0) * 10.0).round_ties_even() / 10.0);
        let loaded = loaded.is_some_and(|p| p["model_alias"] == alias);
        let row = json!({"id":alias,"key":key,"name":name,"engine":"rust","engine_status":"production",
            "qualification":qualification(&profile),"memory_gib":memory,"battery_start_percent":null,"prefix":prefix_state(&profile),
            "selected":selected==alias,"default":alias==DEFAULT,"state":if loaded && ["ready","loading"].contains(&state) {state} else {"unloaded"}});
        rows.push((order, alias, row));
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    json!({"object":"list","default_model":DEFAULT,"selected_model":selected,"data":rows.into_iter().map(|r|r.2).collect::<Vec<_>>()})
}
