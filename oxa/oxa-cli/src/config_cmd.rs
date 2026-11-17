//! D13: `oxa config`.
use oxa_config::{schema, Dirs, Settings};
use oxa_proto::{codes, OxaError};
use serde_json::Value as Json;
use std::io::Write;
use std::path::Path;

/// `oxa config` (print the resolved settings and their sources). Returns the exit code.
pub fn run_config(words: &[String], dirs: &Dirs, cwd: &Path, out: &mut dyn Write) -> Result<i32, OxaError> {
    if words.first().is_some_and(|word| word == "v-cache-thp") {
        return v_cache_thp(words, dirs, cwd, out);
    }
    for word in words {
        match word.as_str() {
            // Accepted for symmetry with pi's `config -l`; the settings listing is the same.
            "-l" | "--local" => {}
            other => {
                return Err(
                    OxaError::new(codes::USAGE, format!("unknown config option: {other}"))
                        .with_hint("usage: oxa config [-l]"),
                )
            }
        }
    }
    print_settings(dirs, cwd, out)
}

/// One line per settings key of the schema: `key = value  [source]`. The value is the merged
/// value oxa uses (user settings; project settings are not applied in this listing).
fn print_settings(dirs: &Dirs, cwd: &Path, out: &mut dyn Write) -> Result<i32, OxaError> {
    let settings = Settings::load(dirs, cwd, false);
    let user = read_json(&dirs.settings_file());
    let project = read_json(&dirs.project_settings_file(cwd));
    let write_err = |e: std::io::Error| OxaError::io("cannot write the settings listing", &e);
    writeln!(out, "settings from {}", dirs.settings_file().display()).map_err(write_err)?;
    for def in schema() {
        let value = match settings.get(def.key) {
            Some(v) => v.clone(),
            None => serde_json::from_str(def.default).unwrap_or(Json::Null),
        };
        let source = if lookup(&user, def.key).is_some() {
            "user"
        } else {
            "default"
        };
        let note = if lookup(&project, def.key).is_some() {
            " (also set in project settings; not applied unless the project is trusted)"
        } else {
            ""
        };
        let shown = serde_json::to_string(&value).unwrap_or_else(|_| "null".to_string());
        writeln!(out, "{} = {}  [{}]{}", def.key, shown, source, note).map_err(write_err)?;
    }
    Ok(0)
}

/// A settings file as JSON; a missing or unreadable file is `null` (the listing then shows
/// defaults, and the settings loader reports the problem itself).
fn read_json(path: &Path) -> Json {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(Json::Null)
}

/// A dotted key looked up in a JSON object tree.
fn lookup<'a>(root: &'a Json, key: &str) -> Option<&'a Json> {
    let mut cur = root;
    for part in key.split('.') {
        cur = cur.as_object()?.get(part)?;
    }
    Some(cur)
}

/// Narrow widget entry point: same user file and atomic writer as `/settings`.
fn v_cache_thp(words: &[String], dirs: &Dirs, cwd: &Path, out: &mut dyn Write) -> Result<i32, OxaError> {
    let enabled = match words.get(1).map(String::as_str) {
        Some("on") if words.len() == 2 => Some(true),
        Some("off") if words.len() == 2 => Some(false),
        Some("--json") if words.len() == 2 => None,
        _ => {
            return Err(OxaError::new(
                codes::USAGE,
                "usage: oxa config v-cache-thp on|off|--json",
            ))
        }
    };
    let mut settings = Settings::load(dirs, cwd, false);
    if let Some(enabled) = enabled {
        settings.set(dirs, cwd, "oxa.local.vCacheThp", Json::Bool(enabled), false)?;
    }
    writeln!(
        out,
        "{}",
        serde_json::json!({"enabled":settings.local_v_cache_thp(),"applies":"next-model-load"})
    )
    .map_err(|error| OxaError::io("cannot write V-cache setting", &error))?;
    Ok(0)
}
