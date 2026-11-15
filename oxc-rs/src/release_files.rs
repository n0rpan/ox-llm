//! Hash-bound staged file transactions and legacy rollback binding data.
//! The legacy script is read as a quoted JSON record; it is never executed.
use crate::paths;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

pub const ABSENT: &str = "absent";
pub fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(v) => *v,
        Value::Number(_) => value.as_f64().is_some_and(|v| v != 0.0),
        Value::String(v) => !v.is_empty(),
        Value::Array(v) => !v.is_empty(),
        Value::Object(v) => !v.is_empty(),
    }
}
pub fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}
pub fn sha(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let mut hash = Sha256::new();
    let mut bytes = vec![0_u8; 1 << 20];
    loop {
        let n = file.read(&mut bytes).ok()?;
        if n == 0 {
            break;
        }
        hash.update(&bytes[..n]);
    }
    Some(format!("{:x}", hash.finalize()))
}
pub fn observed(path: &Path) -> Option<String> {
    if !exists(path) {
        Some(ABSENT.into())
    } else {
        sha(path)
    }
}
pub fn json_file(path: &Path) -> Option<Value> {
    serde_json::from_slice::<Value>(&fs::read(path).ok()?).ok()
}
pub fn manifest(stage: &Path) -> Result<Value, String> {
    let path = stage.join("manifest.json");
    let bytes = fs::read(path).map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            format!(
                "{} is not a staged release (no manifest.json).",
                stage.display()
            )
        } else {
            format!(
                "The release manifest in {} cannot be read.",
                stage.display()
            )
        }
    })?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| {
        format!(
            "The release manifest in {} cannot be read.",
            stage.display()
        )
    })?;
    if !value["paths"].as_object().is_some_and(|p| !p.is_empty()) {
        return Err(format!(
            "The release manifest in {} has no file list.",
            stage.display()
        ));
    }
    Ok(value)
}
pub fn name(stage: &Path, m: &Value) -> String {
    m["release"]
        .as_str()
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            stage
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        })
}
pub fn live_matches(m: &Value, which: &str) -> bool {
    m[which].as_object().is_some_and(|hashes| {
        !hashes.is_empty()
            && m["paths"].as_object().is_some_and(|paths| {
                paths.iter().all(|(k, p)| {
                    p.as_str().and_then(|p| observed(Path::new(p))).as_deref()
                        == hashes.get(k).and_then(Value::as_str)
                })
            })
    })
}
pub fn seal(stage: &Path) -> Result<(), String> {
    fn walk(root: &Path, here: &Path, out: &mut BTreeMap<String, Value>) -> io::Result<()> {
        for entry in fs::read_dir(here)? {
            let entry = entry?;
            let path = entry.path();
            let rel = path
                .strip_prefix(root)
                .map_err(io::Error::other)?
                .to_string_lossy()
                .into_owned();
            if rel.split('/').any(|p| p == "__pycache__")
                || [
                    "receipts",
                    "retired.json",
                    "rollback/receipts",
                    "service-state.json",
                    "SEAL.json",
                ]
                .iter()
                .any(|s| rel == *s || rel.starts_with(&format!("{s}/")))
            {
                continue;
            }
            if entry.file_type()?.is_dir() {
                walk(root, &path, out)?;
            } else if path.is_file() {
                out.insert(rel, sha(&path).map(Value::String).unwrap_or(Value::Null));
            }
        }
        Ok(())
    }
    let value = json_file(&stage.join("SEAL.json"))
        .ok_or("The release is not sealed (SEAL.json missing or unreadable); stage it again.")?;
    let recorded = value["files"]
        .as_object()
        .ok_or("The release is not sealed (SEAL.json missing or unreadable); stage it again.")?;
    let mut actual = BTreeMap::new();
    walk(stage, stage, &mut actual).map_err(|_| {
        "The release is not sealed (SEAL.json missing or unreadable); stage it again."
    })?;
    let a: BTreeSet<_> = actual.keys().collect();
    let r: BTreeSet<_> = recorded.keys().collect();
    let mut bad: Vec<_> = r.symmetric_difference(&a).map(|s| s.as_str()).collect();
    let mut changed: Vec<_> = recorded
        .iter()
        .filter(|(k, v)| actual.get(*k).is_some_and(|a| a != *v))
        .map(|(k, _)| k.as_str())
        .collect();
    changed.sort();
    bad.append(&mut changed);
    if !bad.is_empty() {
        return Err(format!(
            "The staged release changed after it was sealed ({}); stage it again.",
            bad.into_iter().take(3).collect::<Vec<_>>().join(", ")
        ));
    }
    Ok(())
}
fn literal(text: &str) -> Option<String> {
    let mut chars = text.chars();
    let quote = chars.next()?;
    if !['\'', '"'].contains(&quote) || !text.ends_with(quote) {
        return None;
    }
    let mut out = String::new();
    let mut chars = text[quote.len_utf8()..text.len() - quote.len_utf8()].chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let c = chars.next()?;
        match c {
            '\\' | '\'' | '"' => out.push(c),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'b' => out.push('\u{8}'),
            'f' => out.push('\u{c}'),
            'v' => out.push('\u{b}'),
            'a' => out.push('\u{7}'),
            'x' | 'u' | 'U' => {
                let count = match c {
                    'x' => 2,
                    'u' => 4,
                    _ => 8,
                };
                let digits = chars.by_ref().take(count).collect::<String>();
                if digits.len() != count {
                    return None;
                }
                out.push(char::from_u32(u32::from_str_radix(&digits, 16).ok()?)?);
            }
            _ => return None,
        }
    }
    Some(out)
}
pub fn binding(stage: &Path) -> Result<Value, String> {
    let script = fs::read_to_string(stage.join("rollback/rollback.py"))
        .map_err(|_| "rollback binding is unavailable")?;
    let text = script
        .lines()
        .find_map(|l| {
            l.strip_prefix("BINDING = json.loads(")
                .and_then(|l| l.strip_suffix(')'))
        })
        .ok_or("rollback script has no guarded JSON binding")?;
    let value: Value =
        serde_json::from_str(&literal(text).ok_or("rollback binding is not a quoted JSON record")?)
            .map_err(|_| "rollback binding is not JSON")?;
    if value["version"] != 1 || !value["files"].is_object() {
        return Err("unknown rollback binding version".into());
    }
    Ok(value)
}
pub fn parse_mode(value: &Value) -> Option<u32> {
    let text = if let Some(text) = value.as_str() {
        text.to_string()
    } else {
        value.to_string()
    };
    u32::from_str_radix(&text, 8).ok()
}
pub fn rollback_error(code: i32) -> Option<&'static str> {
    match code {3=>Some("the installed files are not the ones this release put there (a newer install or a hand edit)"),5=>Some("the saved copy of the previous version does not match its record"),7=>Some("this release was replaced by a newer one; roll that one back instead"),_=>None}
}
pub fn rollback_checks(stage: &Path, m: &Value, binding: &Value) -> Result<Vec<String>, i32> {
    if stage.join("retired.json").exists() || truthy(&m["retired"]) {
        return Err(7);
    }
    let files = binding["files"].as_object().ok_or(5)?;
    let mut todo = Vec::new();
    for (key, item) in files {
        let pre = item["preimage_sha256"].as_str().ok_or(5)?;
        let new = item["installed_sha256"].as_str().ok_or(5)?;
        let original = stage.join("rollback").join(format!("{key}.original"));
        if pre == ABSENT {
            if exists(&original) {
                return Err(5);
            }
        } else if sha(&original).as_deref() != Some(pre) {
            return Err(5);
        }
        for field in ["preimage_sha256", "installed_sha256"] {
            if m[field].as_object().is_some_and(|v| v.contains_key(key))
                && m[field][key] != item[field]
            {
                return Err(5);
            }
        }
        if let Some(mode) = m["preimage_mode"].as_object().and_then(|v| v.get(key)) {
            if parse_mode(mode) != Some(item["mode"].as_u64().unwrap_or(0o600) as u32) {
                return Err(5);
            }
        }
        let path = item["path"].as_str().ok_or(5)?;
        let state = observed(Path::new(path));
        if state.as_deref() == Some(pre) {
            continue;
        }
        if state.as_deref() == Some(new) {
            todo.push(key.clone());
        } else {
            return Err(3);
        }
    }
    Ok(todo)
}
pub fn atomic(path: &Path, data: &[u8], mode: u32, suffix: &str) -> io::Result<()> {
    let parent = path.parent().ok_or(io::ErrorKind::InvalidInput)?;
    let mut nonce = [0_u8; 8];
    getrandom::fill(&mut nonce)
        .map_err(|_| io::Error::other("temporary-file entropy is unavailable"))?;
    let nonce = nonce.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let temporary = parent.join(format!(
        "{}.{nonce}.{suffix}",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(data)?;
        file.sync_all()?;
        file.set_permissions(fs::Permissions::from_mode(mode))?;
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}
pub fn receipt(directory: &Path, prefix: &str, mut value: Value) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    match fs::DirBuilder::new().mode(0o700).create(directory) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists && directory.is_dir() => {}
        Err(e) => return Err(e),
    }
    let stamp = chrono::Local::now().format("%Y%m%dT%H%M%S").to_string();
    if value.get("time").is_none() {
        value["time"] = json!(stamp);
    }
    let path = directory.join(format!("{prefix}-{stamp}-{}.json", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o666)
        .open(path)?;
    writeln!(file, "{}", serde_json::to_string_pretty(&value)?)
}
struct Item {
    key: String,
    live: PathBuf,
    src: Option<PathBuf>,
    before: String,
    after: String,
    mode: Option<u32>,
    create_mode: Option<u32>,
    preimage: Option<Vec<u8>>,
}
fn plan(stage: &Path, m: &Value) -> Result<Vec<Item>, String> {
    if stage.join("retired.json").exists() || truthy(&m["retired"]) {
        return Err(format!(
            "stage {} is retired; stage a fresh release",
            stage.file_name().unwrap_or_default().to_string_lossy()
        ));
    }
    let mut out = Vec::new();
    for (key, live) in m["paths"].as_object().ok_or("no paths")? {
        let before = m["preimage_sha256"][key]
            .as_str()
            .ok_or_else(|| format!("{key}: no recorded preimage hash"))?
            .to_owned();
        let staged = m["staged_files"][key]
            .as_str()
            .or(match key.as_str() {
                "runtime" => Some("runtime.json"),
                "unit" => Some("unit"),
                "opencode" => Some("opencode.json"),
                _ => None,
            })
            .ok_or_else(|| format!("{key}: no staged file name"))?;
        let removes = m["installed_sha256"][key] == ABSENT;
        let src = if removes {
            if before == ABSENT {
                return Err(format!("{key}: absent before and after; nothing to do"));
            }
            None
        } else {
            let src = stage.join(staged);
            if !src.is_file() {
                return Err(format!("{key}: staged file missing: {}", src.display()));
            }
            Some(src)
        };
        let after = src
            .as_ref()
            .and_then(|p| sha(p))
            .unwrap_or_else(|| ABSENT.into());
        let create_mode = if before == ABSENT {
            let mode = parse_mode(m["installed_mode"].get(key).unwrap_or(&json!("0700")))
                .filter(|v| v & !0o777 == 0 && v & 0o600 == 0o600)
                .ok_or_else(|| format!("{key}: bad installed_mode"))?;
            Some(mode)
        } else {
            None
        };
        let mode = m["preimage_mode"]
            .get(key)
            .map(|v| {
                parse_mode(v)
                    .ok_or_else(|| format!("{key}: bad preimage_mode {}", paths::python_json(v)))
            })
            .transpose()?;
        if !m["installed_sha256"][key].is_null() && m["installed_sha256"][key] != after {
            return Err(format!(
                "{key}: staged file changed since staging (manifest installed_sha256)"
            ));
        }
        let original = stage.join("rollback").join(format!("{key}.original"));
        if stage.join("rollback").is_dir() {
            if before == ABSENT {
                if exists(&original) {
                    return Err(format!(
                        "{key}: preimage is absent but rollback/{key}.original exists"
                    ));
                }
            } else if !original.is_file() || sha(&original).as_deref() != Some(&before) {
                return Err(format!("{key}: rollback/{key}.original missing or not the recorded preimage; rollback would not be exact"));
            }
        }
        out.push(Item {
            key: key.clone(),
            live: PathBuf::from(live.as_str().ok_or("live path is not text")?),
            src,
            before,
            after,
            mode,
            create_mode,
            preimage: None,
        });
    }
    Ok(out)
}
fn verify(plan: &mut [Item]) -> Result<(), String> {
    for item in plan {
        let key = &item.key;
        let path = &item.live;
        if item.before == ABSENT {
            if !exists(path) {
                if !path.parent().is_some_and(Path::is_dir) {
                    return Err(format!(
                        "{key}: directory missing: {}",
                        path.parent().unwrap_or_else(|| Path::new(".")).display()
                    ));
                }
                continue;
            }
            if sha(path).as_deref() == Some(&item.after) {
                return Err(format!(
                    "{key}: already at the new hash (installed?); refusing"
                ));
            }
            return Err(format!(
                "{key}: {path} exists, but this stage expects to create it",
                path = path.display()
            ));
        }
        if !fs::symlink_metadata(path).is_ok_and(|m| m.is_file()) {
            if item.after == ABSENT && !exists(path) {
                return Err(format!("{key}: already removed (installed?); refusing"));
            }
            return Err(format!("{key}: live file missing: {}", path.display()));
        }
        let bytes = fs::read(path).map_err(|_| format!("{key}: live file cannot be read"))?;
        let actual = format!("{:x}", Sha256::digest(&bytes));
        if actual == item.before {
            let mode = fs::metadata(path)
                .map_err(|_| format!("{key}: live mode cannot be read"))?
                .mode()
                & 0o7777;
            if item.mode.is_some_and(|expected| expected != mode) {
                return Err(format!("{key}: live mode {mode:04o} != recorded preimage mode {:04o}; the rollback would not restore it exactly",item.mode.unwrap_or(0)));
            }
            item.preimage = Some(bytes);
        } else if actual == item.after {
            return Err(format!(
                "{key}: already at the new hash (installed?); refusing"
            ));
        } else {
            return Err(format!(
                "{key}: live sha256 {} != recorded preimage {}",
                &actual[..12],
                item.before.get(..12).unwrap_or(&item.before)
            ));
        }
    }
    Ok(())
}
pub fn precheck(stage: &Path, m: &Value) -> Result<(), String> {
    let mut value = plan(stage, m)?;
    verify(&mut value)
}
pub fn install(stage: &Path, m: &Value) -> Result<(), String> {
    let mut plan = plan(stage, m)?;
    verify(&mut plan)?;
    let mut touched = 0;
    let mut done = Vec::new();
    let result = (|| {
        for item in &plan {
            touched += 1;
            if item.after == ABSENT {
                if exists(&item.live) {
                    if fs::symlink_metadata(&item.live).is_ok_and(|m| m.file_type().is_symlink())
                        || sha(&item.live).as_deref() != Some(&item.before)
                    {
                        return Err(format!(
                            "{}: {} changed after the preimage check; not removed",
                            item.key,
                            item.live.display()
                        ));
                    }
                    fs::remove_file(&item.live).map_err(|e| e.to_string())?;
                }
            } else {
                let mode = if !exists(&item.live) {
                    item.create_mode.unwrap_or(0o600)
                } else {
                    fs::metadata(&item.live).map_err(|e| e.to_string())?.mode() & 0o777
                };
                let bytes = fs::read(item.src.as_ref().ok_or("staged bytes are absent")?)
                    .map_err(|e| e.to_string())?;
                if format!("{:x}", Sha256::digest(&bytes)) != item.after {
                    return Err(format!(
                        "{}: temp file hash mismatch before rename",
                        item.key
                    ));
                }
                atomic(&item.live, &bytes, mode, "new").map_err(|e| e.to_string())?;
            }
            if observed(&item.live).as_deref() != Some(&item.after) {
                return Err(format!("{}: post-install sha256 mismatch", item.key));
            }
            if item.after != ABSENT {
                let mode = fs::metadata(&item.live).map_err(|e| e.to_string())?.mode() & 0o7777;
                if item.mode.is_some_and(|m| m != mode)
                    || item.create_mode.is_some_and(|m| m != mode)
                {
                    return Err(format!("{}: post-install mode changed", item.key));
                }
            }
            done.push(item.key.clone());
        }
        Ok(())
    })();
    let hashes: serde_json::Map<String, Value> = plan
        .iter()
        .map(|i| {
            (
                i.key.clone(),
                json!({"preimage":i.before,"installed":i.after}),
            )
        })
        .collect();
    if let Err(error) = result.as_ref() {
        let mut restored = Vec::new();
        let mut failed = Vec::new();
        for item in &plan[..touched] {
            let current = observed(&item.live);
            if current.as_deref() != Some(&item.before) && current.as_deref() != Some(&item.after) {
                failed.push(format!(
                    "{} (live file changed outside this install; refusing to overwrite it)",
                    item.key
                ));
                continue;
            }
            if current.as_deref() != Some(&item.before) {
                if item.before == ABSENT {
                    let _ = fs::remove_file(&item.live);
                } else if let Some(bytes) = item.preimage.as_ref() {
                    let _ = atomic(&item.live, bytes, item.mode.unwrap_or(0o600), "recover");
                }
            }
            if observed(&item.live).as_deref() == Some(&item.before) {
                restored.push(item.key.clone());
            } else {
                failed.push(format!("{} (post-restore hash mismatch)", item.key));
            }
        }
        let _ = receipt(
            &stage.join("receipts"),
            "install",
            json!({"action":"install-failed","stage":stage.file_name().unwrap_or_default().to_string_lossy(),"error":error,"replaced_before_failure":done,"restored":restored,"restore_failed":failed,"sha256":hashes}),
        );
    } else {
        let _ = receipt(
            &stage.join("receipts"),
            "install",
            json!({"action":"install","stage":stage.file_name().unwrap_or_default().to_string_lossy(),"installed":done,"sha256":hashes}),
        );
    }
    result
}
pub fn restore(stage: &Path, binding: &Value, todo: &[String]) -> Result<(), String> {
    for key in todo {
        let item = &binding["files"][key];
        let path = Path::new(item["path"].as_str().ok_or("rollback path is not text")?);
        let pre = item["preimage_sha256"]
            .as_str()
            .ok_or("rollback preimage is not bound")?;
        if pre == ABSENT {
            if exists(path) {
                if fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
                    || sha(path).as_deref() != item["installed_sha256"].as_str()
                {
                    return Err(format!(
                        "{key} changed after the check; not removed; stopped, service left down"
                    ));
                }
                fs::remove_file(path).map_err(|e| e.to_string())?;
            }
        } else {
            let bytes = fs::read(stage.join("rollback").join(format!("{key}.original")))
                .map_err(|e| e.to_string())?;
            if format!("{:x}", Sha256::digest(&bytes)) != pre {
                return Err(format!("preimage {key} changed during rollback; stopped"));
            }
            let mode = item["mode"].as_u64().unwrap_or(0o600) as u32;
            atomic(path, &bytes, mode, "rollback").map_err(|e| e.to_string())?;
            if observed(path).as_deref() != Some(pre)
                || fs::metadata(path).map_err(|e| e.to_string())?.mode() & 0o7777 != mode
            {
                return Err(format!(
                    "post-restore mismatch for {key}; stopped, service left down"
                ));
            }
        }
    }
    Ok(())
}
