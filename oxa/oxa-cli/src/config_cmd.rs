//! D13: `oxa config`. Owner: task D13. Signatures below are frozen; bodies are yours.
use oxa_config::{schema, Dirs, Settings};
use oxa_proto::{codes, OxaError};
use serde_json::Value as Json;
use std::io::Write;
use std::path::{Path, PathBuf};

/// `oxa config` (print the resolved settings and their sources), `oxa config --import-pi`
/// (copy skills, prompts, themes, keybindings.json and mcp.json from `<home>/.pi/agent` into
/// the oxa agent dir once; existing files are never overwritten; pi directories are never read
/// implicitly anywhere else). Returns the exit code.
pub fn run_config(words: &[String], dirs: &Dirs, cwd: &Path, out: &mut dyn Write) -> Result<i32, OxaError> {
    let mut import = false;
    for word in words {
        match word.as_str() {
            "--import-pi" => import = true,
            // Accepted for symmetry with pi's `config -l`; the settings listing is the same.
            "-l" | "--local" => {}
            other => {
                return Err(
                    OxaError::new(codes::USAGE, format!("unknown config option: {other}"))
                        .with_hint("usage: oxa config [-l] [--import-pi]"),
                )
            }
        }
    }
    if import {
        return import_pi(dirs, out);
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

/// Copy pi's skills, prompt templates, themes, keybindings.json and mcp.json into the agent
/// directory. A file that already exists is kept as it is; a second run copies nothing new.
fn import_pi(dirs: &Dirs, out: &mut dyn Write) -> Result<i32, OxaError> {
    let pi_agent = dirs.home.join(".pi").join("agent");
    let write_err = |e: std::io::Error| OxaError::io("cannot write the import report", &e);
    if !pi_agent.is_dir() {
        writeln!(out, "nothing to import: {} does not exist", pi_agent.display()).map_err(write_err)?;
        return Ok(0);
    }
    let mut tally = Tally::default();
    for name in ["skills", "prompts", "themes"] {
        copy_tree(&pi_agent.join(name), &dirs.agent_dir.join(name), &mut tally)?;
    }
    for name in ["keybindings.json", "mcp.json"] {
        copy_file(&pi_agent.join(name), &dirs.agent_dir.join(name), &mut tally)?;
    }
    writeln!(
        out,
        "imported {} file(s) from {}; kept {} existing file(s)",
        tally.copied,
        pi_agent.display(),
        tally.kept
    )
    .map_err(write_err)?;
    Ok(0)
}

#[derive(Default)]
struct Tally {
    copied: usize,
    kept: usize,
}

/// Recursively copy regular files; symbolic links are skipped.
fn copy_tree(src: &Path, dst: &Path, tally: &mut Tally) -> Result<(), OxaError> {
    if !src.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(dst)
        .map_err(|e| OxaError::io(&format!("cannot create {}", dst.display()), &e))?;
    let entries =
        std::fs::read_dir(src).map_err(|e| OxaError::io(&format!("cannot read {}", src.display()), &e))?;
    for entry in entries {
        let entry = entry.map_err(|e| OxaError::io(&format!("cannot read {}", src.display()), &e))?;
        let file_type = entry
            .file_type()
            .map_err(|e| OxaError::io("cannot stat an import entry", &e))?;
        let from = entry.path();
        let to: PathBuf = dst.join(entry.file_name());
        if file_type.is_symlink() {
            continue;
        } else if file_type.is_dir() {
            copy_tree(&from, &to, tally)?;
        } else {
            copy_file(&from, &to, tally)?;
        }
    }
    Ok(())
}

fn copy_file(src: &Path, dst: &Path, tally: &mut Tally) -> Result<(), OxaError> {
    if !src.is_file() {
        return Ok(());
    }
    if std::fs::symlink_metadata(dst).is_ok() {
        tally.kept += 1;
        return Ok(());
    }
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| OxaError::io(&format!("cannot create {}", parent.display()), &e))?;
    }
    std::fs::copy(src, dst)
        .map_err(|e| OxaError::io(&format!("cannot copy {} to {}", src.display(), dst.display()), &e))?;
    tally.copied += 1;
    Ok(())
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
